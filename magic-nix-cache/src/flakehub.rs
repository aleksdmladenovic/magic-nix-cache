use crate::error::{Error, Result};
use attic::nix_store::{NixStore, StorePath};
use attic_client::api::ApiError;
use attic_client::config::ServerTokenConfig;
use attic_client::push::{PushSession, PushSessionConfig};
use attic_client::{
    api::ApiClient,
    config::ServerConfig,
    push::{PushConfig, Pusher},
};
use axum::http::{HeaderMap, HeaderValue};
use reqwest::header::AUTHORIZATION;
use reqwest::Url;
use serde::Deserialize;
use std::env;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

const USER_AGENT: &str = "magic-nix-cache";

pub struct State {
    pub substituter: Url,

    pub push_session: PushSession,
}

pub async fn init_cache(
    flakehub_api_server: &Url,
    flakehub_api_server_netrc: &Path,
    flakehub_cache_server: &Url,
    store: Arc<NixStore>,
) -> Result<State> {
    // Parse netrc to get the credentials for api.flakehub.com.
    let netrc = {
        let mut netrc_file = File::open(flakehub_api_server_netrc).await?;
        let mut netrc_contents = String::new();
        netrc_file.read_to_string(&mut netrc_contents).await?;
        netrc_rs::Netrc::parse(netrc_contents, false).map_err(Error::Netrc)?
    };

    let flakehub_netrc_entry = {
        netrc
            .machines
            .iter()
            .find(|machine| {
                machine.name.as_ref() == flakehub_api_server.host().map(|x| x.to_string()).as_ref()
            })
            .ok_or_else(|| Error::MissingCreds(flakehub_api_server.to_string()))?
            .to_owned()
    };

    let flakehub_cache_server_hostname = flakehub_cache_server
        .host()
        .ok_or_else(|| Error::BadUrl(flakehub_cache_server.to_owned()))?
        .to_string();

    let flakehub_login = flakehub_netrc_entry.login.as_ref().ok_or_else(|| {
        Error::Config(format!(
            "netrc file does not contain a login for '{}'",
            flakehub_api_server
        ))
    })?;

    let flakehub_password = flakehub_netrc_entry.password.as_ref().ok_or_else(|| {
        Error::Config(format!(
            "netrc file does not contain a password for '{}'",
            flakehub_api_server
        ))
    })?;

    // Append an entry for the FlakeHub cache server to netrc.
    if !netrc
        .machines
        .iter()
        .any(|machine| machine.name.as_ref() == Some(&flakehub_cache_server_hostname))
    {
        let mut netrc_file = tokio::fs::OpenOptions::new()
            .create(false)
            .append(true)
            .open(flakehub_api_server_netrc)
            .await?;
        netrc_file
            .write_all(
                format!(
                    "\nmachine {} login {} password {}\n\n",
                    flakehub_cache_server_hostname, flakehub_login, flakehub_password,
                )
                .as_bytes(),
            )
            .await?;
    }

    fn build_http_client(password: &str) -> reqwest::Client {
        let mut headers = HeaderMap::new();

        let auth_header = HeaderValue::from_str(&format!("Bearer {}", password)).unwrap();
        headers.insert(AUTHORIZATION, auth_header);

        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .build()
            .expect("TODO")
    }

    let flakehub_client = build_http_client(flakehub_password);

    // Get the cache UUID for this project.
    let cache_name = {
        let github_repo = env::var("GITHUB_REPOSITORY").map_err(|_| {
            Error::Config("GITHUB_REPOSITORY environment variable is not set".to_owned())
        })?;

        let url = flakehub_api_server
            .join(&format!("project/{}", github_repo))
            .map_err(|_| Error::Config(format!("bad URL '{}'", flakehub_api_server)))?;

        let response = reqwest::Client::new()
            .get(url.to_owned())
            .header("User-Agent", USER_AGENT)
            .basic_auth(flakehub_login, Some(flakehub_password))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(Error::GetCacheName(
                response.status(),
                response.text().await?,
            ));
        }

        #[derive(Deserialize)]
        struct ProjectInfo {
            organization_uuid_v7: Uuid,
            project_uuid_v7: Uuid,
        }

        let project_info = response.json::<ProjectInfo>().await?;

        format!(
            "{}:{}",
            project_info.organization_uuid_v7, project_info.project_uuid_v7,
        )
    };

    tracing::info!("Using cache {:?}", cache_name);

    let cache = cache_name;

    let api = ApiClient::from_server_config(ServerConfig {
        endpoint: flakehub_cache_server.to_string(),
        token: flakehub_netrc_entry
            .password
            .map(|token| ServerTokenConfig::Raw { token })
            .as_ref()
            .cloned(),
    })?;

    let cache_config = {
        let cache = &cache;
        let endpoint = flakehub_cache_server
            .join("_api/v1/cache-config/")
            .expect("TODO")
            .join(cache)
            .expect("TODO");

        let res = flakehub_client.get(endpoint).send().await?;

        if res.status().is_success() {
            let cache_config = res.json().await?;
            Ok(cache_config)
        } else {
            let api_error = ApiError::try_from_response(res).await?;
            Err(api_error.into())
        }
    };

    let push_config = PushConfig {
        num_workers: 5, // FIXME: use number of CPUs?
        force_preamble: false,
    };

    let mp = indicatif::MultiProgress::new();

    let push_session = Pusher::new(
        store.clone(),
        api.clone(),
        cache.to_owned(),
        cache_config,
        mp,
        push_config,
    )
    .into_push_session(PushSessionConfig {
        no_closure: false,
        ignore_upstream_cache_filter: false,
    });

    Ok(State {
        substituter: flakehub_cache_server.to_owned(),
        push_session,
    })
}

pub async fn enqueue_paths(state: &State, store_paths: Vec<StorePath>) -> Result<()> {
    state.push_session.queue_many(store_paths)?;

    Ok(())
}

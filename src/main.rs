//! CLI entry point: parses arguments, builds the App state, and serves it.

mod config;
mod github;
mod review;
mod webhook;

use std::{io::{self, IsTerminal}, net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    routing::{get, post},
    Router,
};
use clap::Parser;
use reqwest::Client;
use tracing::info;

use config::{Args, Config};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) github_app_id: u64,
    pub(crate) github_private_key: Arc<[u8]>,
    pub(crate) github_username: Arc<str>,
    pub(crate) webhook_secret: Arc<str>,
    pub(crate) llm_api_key: Arc<str>,
    pub(crate) llm_base_url: Arc<str>,
    pub(crate) llm_model: Arc<str>,
    pub(crate) client: Client,
    pub(crate) review_lock: Arc<tokio::sync::Mutex<()>>,
}

impl AppState {
    fn from_config(config: Config) -> Result<Self> {
        let private_key = std::fs::read(&config.github_private_key_path).with_context(|| {
            format!(
                "could not read GitHub App private key at {}",
                config.github_private_key_path
            )
        })?;

        Ok(Self {
            github_app_id: config.github_app_id,
            github_private_key: private_key.into(),
            github_username: config.github_username.into(),
            webhook_secret: config.webhook_secret.into(),
            llm_api_key: config.llm_api_key.into(),
            llm_base_url: config.llm_base_url.trim_end_matches('/').to_owned().into(),
            llm_model: config.llm_model.into(),
            client: Client::builder()
                .user_agent("github-pr-review-bot")
                .build()?,
            review_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let log_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(log_filter)
        .with_ansi(io::stderr().is_terminal())
        .init();

    let config = Args::parse().into_config()?;
    let address = SocketAddr::from(([0, 0, 0, 0], config.port));
    info!(
        username = %config.github_username,
        github_app_id = config.github_app_id,
        model = %config.llm_model,
        base_url = %config.llm_base_url,
        engine = "ocr",
        "starting review bot"
    );
    let state = AppState::from_config(config)?;

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/webhooks/github", post(webhook::github_webhook))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(address).await?;
    info!(address = %listener.local_addr()?, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
}

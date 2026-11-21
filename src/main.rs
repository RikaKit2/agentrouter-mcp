// WHY: User invariant in rust-workflow explicitly enforces match-first style over if let
#![allow(clippy::single_match)]

mod auth;
mod client;
mod config;
mod server;
mod types;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{self, EnvFilter};

use crate::{config::AppConfig, server::AgentRouterServer};

#[tokio::main]
async fn main() -> Result<()> {
    // Write logs strictly to stderr; stdout is reserved exclusively for MCP JSON-RPC protocol
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let config = AppConfig::from_args();
    let key_source = match config.api_key.is_some() {
        true => "static AGENTROUTER_API_KEY".to_string(),
        false => format!("key_cmd: '{}'", config.key_cmd),
    };
    tracing::info!(
        "Initializing AgentRouter MCP server (default_model: '{}', key_source: '{}', base_url: '{}', timeout: {}s, api_mode: '{:?}')",
        config.default_model,
        key_source,
        config.base_url,
        config.timeout_secs,
        config.api_mode
    );

    let server = AgentRouterServer::new(config)?;
    let service = server.serve(stdio()).await.inspect_err(|e| {
        tracing::error!("Serving error: {:?}", e);
    })?;

    service.waiting().await?;
    Ok(())
}

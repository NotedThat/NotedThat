use crate::config::Config;
use anyhow::Context;
use notedthat_mcp::{McpHttpService, McpHttpServiceConfig, client::NotedThatClient};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub(crate) async fn bind_listener(config: &Config) -> anyhow::Result<Option<TcpListener>> {
    if !config.mcp_http_enabled {
        return Ok(None);
    }

    let listener = TcpListener::bind(config.mcp_http_bind)
        .await
        .with_context(|| {
            format!(
                "failed to bind MCP HTTP listener on {}",
                config.mcp_http_bind
            )
        })?;
    info!(mcp = %listener.local_addr()?, "MCP HTTP listener bound");
    Ok(Some(listener))
}

pub(crate) fn build_router(
    config: &Config,
    internal_api_url: &str,
    cancellation_token: CancellationToken,
) -> anyhow::Result<axum::Router> {
    let client = NotedThatClient::new(internal_api_url, &config.api_token)
        .context("failed to build MCP HTTP API client")?;
    let mcp_config = McpHttpServiceConfig::new(
        config.mcp_http_allowed_hosts.clone(),
        config.mcp_http_allowed_origins.clone(),
        cancellation_token,
    )
    .context("failed to build MCP HTTP service config")?;
    let mcp_service = McpHttpService::new(client, mcp_config);
    Ok(axum::Router::new().route_service("/mcp", mcp_service.into_service()))
}

pub(crate) fn internal_http_api_url(addr: SocketAddr) -> String {
    let mapped_addr = match addr {
        SocketAddr::V4(addr) if addr.ip().is_unspecified() => {
            SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, addr.port()))
        }
        SocketAddr::V6(addr) if addr.ip().is_unspecified() => {
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, addr.port()))
        }
        addr => addr,
    };
    format!("http://{mapped_addr}")
}

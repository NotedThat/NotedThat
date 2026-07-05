use crate::config::Config;
use anyhow::Context;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::Response,
};
use notedthat_mcp::{
    McpHttpService, McpHttpServiceConfig,
    auth::require_bearer_auth,
    client::NotedThatClient,
    sse_refusal::{refusal_body, should_refuse_request},
};
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
    let token = config.api_token.clone();
    Ok(axum::Router::new()
        .route_service("/mcp", mcp_service.into_service())
        .route_layer(middleware::from_fn_with_state(token, require_bearer_auth))
        .layer(middleware::from_fn(sse_refusal_check)))
}

async fn sse_refusal_check(request: Request<Body>, next: Next) -> Response {
    if should_refuse_request(request.method().as_str(), request.uri().path()) {
        Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header("content-type", "application/json")
            .body(Body::from(refusal_body().to_vec()))
            .expect("SSE refusal response is infallible")
    } else {
        next.run(request).await
    }
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

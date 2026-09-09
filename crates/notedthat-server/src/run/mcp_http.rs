use crate::config::Config;
use anyhow::Context;
use axum::{
    body::Body,
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{any, get, post_service},
};
use notedthat_mcp::{
    McpHttpService, McpHttpServiceConfig, auth::require_bearer_auth, client::NotedThatClient,
    sse_refusal::refusal_body,
};
use std::net::SocketAddr;
use tokio_util::sync::CancellationToken;

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
    let mcp_service = McpHttpService::new(client, &mcp_config);
    let token = config.api_token.clone();
    let authenticated_mcp = post_service(mcp_service.into_service())
        .route_layer(middleware::from_fn_with_state(token, require_bearer_auth));
    Ok(axum::Router::new()
        .route(
            "/mcp",
            get(legacy_transport_refusal)
                .delete(legacy_transport_refusal)
                .merge(authenticated_mcp),
        )
        .route(
            "/sse",
            get(legacy_transport_refusal).post(legacy_transport_refusal),
        )
        .route("/sse/", any(legacy_transport_refusal))
        .route("/sse/{*path}", any(legacy_transport_refusal)))
}

async fn legacy_transport_refusal() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [("content-type", "application/json")],
        Body::from(refusal_body()),
    )
        .into_response()
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn trailing_slash_sse_path_refuses_every_method() {
        let router = axum::Router::new().route("/sse/", any(legacy_transport_refusal));

        for method in ["GET", "POST", "PUT", "DELETE"] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/sse/")
                        .body(Body::empty())
                        .expect("test request is valid"),
                )
                .await
                .expect("refusal route is infallible");
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        }
    }
}

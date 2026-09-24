use crate::config::{Config, McpAnonymous};
use anyhow::Context;
use axum::{
    body::Body,
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{MethodFilter, any, get, on_service},
};
use notedthat_core::{AccessPolicy, Authenticator, Principal};
use notedthat_mcp::{
    McpHttpService, McpHttpServiceConfig,
    auth::{McpAuth, authenticate_caller},
    client::NotedThatClient,
    http::bind_session,
    sse_refusal::refusal_body,
};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub(crate) fn build_router(
    config: &Config,
    authenticator: Arc<Authenticator>,
    access_policies: &BTreeMap<String, Arc<AccessPolicy>>,
    internal_api_url: &str,
    cancellation_token: CancellationToken,
    events_enabled: bool,
) -> anyhow::Result<axum::Router> {
    let client = NotedThatClient::new(internal_api_url, &config.api_token)
        .context("failed to build MCP HTTP API client")?
        .with_max_read_bytes(config.mcp_max_read_bytes);
    let mcp_config = McpHttpServiceConfig::new(
        config.mcp_http_allowed_hosts.clone(),
        config.mcp_http_allowed_origins.clone(),
        cancellation_token,
    )
    .context("failed to build MCP HTTP service config")?
    .with_max_sessions(config.mcp_max_sessions);
    let mcp_service = McpHttpService::new(client, &mcp_config, events_enabled);
    let sessions = mcp_service.sessions();
    let anonymous = anonymous_admitted(config.mcp_anonymous, access_policies);
    let auth = Arc::new(McpAuth {
        authenticator,
        anonymous,
    });
    // The stateful transport (D66): POST for requests, GET for the
    // server-to-client notification leg, DELETE to end a session. Auth is the
    // outermost layer, so a missing credential is 401 before rmcp answers
    // 400/404 about the session; the session bound sits between the two.
    let mcp = on_service(
        MethodFilter::GET
            .or(MethodFilter::POST)
            .or(MethodFilter::DELETE),
        mcp_service.into_service(),
    )
    .route_layer(middleware::from_fn_with_state(sessions, bind_session))
    .route_layer(middleware::from_fn_with_state(auth, authenticate_caller));
    Ok(axum::Router::new()
        .route("/mcp", mcp)
        .route(
            "/sse",
            get(legacy_transport_refusal).post(legacy_transport_refusal),
        )
        .route("/sse/", any(legacy_transport_refusal))
        .route("/sse/{*path}", any(legacy_transport_refusal)))
}

/// Whether `/mcp` lets a request with no credential through, decided once.
///
/// Policies are a startup snapshot, so this is exact for the life of the
/// process. It is on when some declared knowledge base grants `anyone`
/// something — the same test that makes a knowledge base appear in an
/// anonymous listing — unless the operator said `never`. Off means a missing
/// credential answers `401` with the bearer challenge, which is what an
/// OAuth-capable MCP client needs to see before it signs in; that is why
/// `never` exists for a deployment that has both public knowledge bases and
/// an identity provider. Logged as `MCP_ANONYMOUS` so an operator can tell
/// which of the three cases they are in.
fn anonymous_admitted(
    mode: McpAnonymous,
    access_policies: &BTreeMap<String, Arc<AccessPolicy>>,
) -> bool {
    let granted = access_policies
        .values()
        .any(|policy| policy.visible_in_listing(&Principal::Anyone));
    let (admitted, reason) = match (mode, granted) {
        (McpAnonymous::Never, _) => (false, "disabled_by_setting"),
        (McpAnonymous::Auto, false) => (false, "disabled_no_anonymous_grants"),
        (McpAnonymous::Auto, true) => (true, "enabled"),
    };
    info!(
        mode = %mode,
        anonymous_grants = granted,
        admitted,
        "MCP_ANONYMOUS {reason}"
    );
    admitted
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
    use notedthat_core::{AccessRule, Verb, Who};
    use tower::ServiceExt as _;

    fn policies(rules: Vec<AccessRule>) -> BTreeMap<String, Arc<AccessPolicy>> {
        BTreeMap::from([(
            "notes".to_string(),
            Arc::new(rules.into_iter().collect::<AccessPolicy>()),
        )])
    }

    #[test]
    fn anonymous_is_admitted_only_under_auto_with_an_anonymous_grant() {
        let public = policies(vec![
            AccessRule::new(Who::SignedIn, Verb::ALL),
            AccessRule::new(Who::Anyone, [Verb::Read]),
        ]);
        let private = policies(vec![AccessRule::new(Who::SignedIn, Verb::ALL)]);

        assert!(anonymous_admitted(McpAnonymous::Auto, &public));
        assert!(!anonymous_admitted(McpAnonymous::Auto, &private));
        assert!(!anonymous_admitted(McpAnonymous::Auto, &BTreeMap::new()));
        assert!(!anonymous_admitted(McpAnonymous::Never, &public));
    }

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

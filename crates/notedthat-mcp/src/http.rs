//! Stateless JSON-response Streamable HTTP transport for the MCP tool handler.

use std::sync::Arc;

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{NotedThatMcp, client::NotedThatClient};

/// Axum/tower-compatible rmcp Streamable HTTP service type for `NotedThatMcp`.
pub type McpHttpInnerService = StreamableHttpService<NotedThatMcp, NeverSessionManager>;

/// Validated Streamable HTTP server configuration for stateless JSON responses.
#[derive(Clone, Debug)]
pub struct McpHttpServiceConfig {
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    cancellation_token: CancellationToken,
}

impl McpHttpServiceConfig {
    /// Build validated HTTP transport config.
    pub fn new(
        allowed_hosts: impl IntoIterator<Item = impl Into<String>>,
        allowed_origins: impl IntoIterator<Item = impl Into<String>>,
        cancellation_token: CancellationToken,
    ) -> Result<Self, McpHttpServiceConfigError> {
        let allowed_hosts = allowed_hosts
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        let allowed_origins = allowed_origins
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();

        if allowed_hosts.is_empty() {
            return Err(McpHttpServiceConfigError::EmptyAllowedHosts);
        }
        if allowed_origins.is_empty() {
            return Err(McpHttpServiceConfigError::EmptyAllowedOrigins);
        }

        Ok(Self {
            allowed_hosts,
            allowed_origins,
            cancellation_token,
        })
    }

    fn streamable_http_config(&self) -> StreamableHttpServerConfig {
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true)
            .with_allowed_hosts(self.allowed_hosts.clone())
            .with_allowed_origins(self.allowed_origins.clone())
            .with_cancellation_token(self.cancellation_token.clone())
    }
}

/// Errors from constructing [`McpHttpServiceConfig`].
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum McpHttpServiceConfigError {
    /// Empty host lists disable rmcp Host validation and are not allowed here.
    #[error("allowed_hosts must not be empty")]
    EmptyAllowedHosts,
    /// Empty origin lists disable rmcp Origin validation and are not allowed here.
    #[error("allowed_origins must not be empty")]
    EmptyAllowedOrigins,
}

/// Reusable Streamable HTTP service wrapper for `NotedThatMcp`.
#[derive(Clone)]
pub struct McpHttpService {
    inner: McpHttpInnerService,
}

impl McpHttpService {
    /// Create a stateless JSON-response Streamable HTTP service.
    pub fn new(client: NotedThatClient, config: &McpHttpServiceConfig) -> Self {
        let streamable_config = config.streamable_http_config();
        let service_factory = move || Ok(NotedThatMcp::for_http(client.clone()));
        let inner = StreamableHttpService::new(
            service_factory,
            Arc::new(NeverSessionManager::default()),
            streamable_config,
        );

        Self { inner }
    }

    /// Return the rmcp Streamable HTTP service for use with axum/tower routing.
    pub fn into_service(self) -> McpHttpInnerService {
        self.inner
    }

    /// Inspect the effective rmcp Streamable HTTP server config.
    pub fn config(&self) -> &StreamableHttpServerConfig {
        &self.inner.config
    }
}

#[cfg(test)]
mod mcp_http_service {
    use super::*;

    fn test_client() -> NotedThatClient {
        NotedThatClient::new("http://127.0.0.1:8080", "test-token")
            .expect("test client config is valid")
    }

    fn test_config(token: CancellationToken) -> McpHttpServiceConfig {
        McpHttpServiceConfig::new(["127.0.0.1", "localhost"], ["http://127.0.0.1:8080"], token)
            .expect("test HTTP service config is valid")
    }

    #[test]
    fn creates_service_with_configured_hosts_and_origins() {
        // Given: explicit non-empty host and origin allow-lists.
        let token = CancellationToken::new();
        let config = test_config(token);

        // When: the MCP HTTP service is created.
        let service = McpHttpService::new(test_client(), &config);

        // Then: rmcp receives the caller-supplied allow-lists.
        assert_eq!(
            service.config().allowed_hosts,
            ["127.0.0.1".to_string(), "localhost".to_string()]
        );
        assert_eq!(
            service.config().allowed_origins,
            ["http://127.0.0.1:8080".to_string()]
        );
    }

    #[test]
    fn explicitly_sets_stateless_json_response_mode() {
        // Given: valid HTTP service config.
        let token = CancellationToken::new();
        let config = test_config(token);

        // When: the MCP HTTP service is created.
        let service = McpHttpService::new(test_client(), &config);

        // Then: stateful mode is disabled and JSON responses are enabled.
        assert!(!service.config().stateful_mode);
        assert!(service.config().json_response);
    }

    #[test]
    fn uses_caller_supplied_cancellation_token() {
        // Given: a cancellation token owned by the caller.
        let token = CancellationToken::new();
        let config = test_config(token.clone());
        let service = McpHttpService::new(test_client(), &config);

        // When: the caller cancels the original token.
        token.cancel();

        // Then: the rmcp config observes the same cancellation hook.
        assert!(service.config().cancellation_token.is_cancelled());
    }

    #[test]
    fn rejects_empty_host_or_origin_allow_lists() {
        // Given: rmcp treats empty allow-lists as validation disabled.
        let token = CancellationToken::new();

        // When/Then: empty host and origin lists are rejected before rmcp sees them.
        let empty_hosts =
            McpHttpServiceConfig::new(Vec::<&str>::new(), ["http://127.0.0.1:8080"], token.clone());
        assert!(matches!(
            empty_hosts,
            Err(McpHttpServiceConfigError::EmptyAllowedHosts)
        ));
        let empty_origins = McpHttpServiceConfig::new(["127.0.0.1"], Vec::<&str>::new(), token);
        assert!(matches!(
            empty_origins,
            Err(McpHttpServiceConfigError::EmptyAllowedOrigins)
        ));
    }

    #[test]
    fn mcp_http_service_is_send_sync() {
        const _: fn() = || {
            fn assert_send_sync<T: Send + Sync>() {}
            assert_send_sync::<McpHttpService>();
        };
    }

    #[test]
    fn exposes_rmcp_service_for_axum_tower_routing() {
        // Given: a constructed wrapper.
        let token = CancellationToken::new();
        let config = test_config(token);
        let service = McpHttpService::new(test_client(), &config);

        // When: callers request the underlying rmcp service.
        let inner = service.into_service();

        // Then: it preserves the required stateless JSON rmcp config.
        assert!(!inner.config.stateful_mode);
        assert!(inner.config.json_response);
    }
}

#[cfg(test)]
mod caller_identity {
    //! The MCP service acts as its caller: the bearer presented to `/mcp` is
    //! the bearer the loopback API call carries.

    use super::*;
    use crate::auth::{McpAuth, authenticate_caller};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post_service;
    use axum::{Router, middleware};
    use notedthat_core::Authenticator;
    use notedthat_core::testing::StubTokenVerifier;
    use std::sync::Arc;
    use tower::ServiceExt as _;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SERVICE_TOKEN: &str = "service-token";
    const ALICE_TOKEN: &str = "jwt-alice";

    /// A fake API that answers the knowledge-base index only for `bearer`.
    async fn api_expecting(bearer: &str) -> MockServer {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases"))
            .and(header("authorization", format!("Bearer {bearer}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"knowledgebases": ["notes"]})),
            )
            .expect(1)
            .mount(&api)
            .await;
        api
    }

    fn app(api_url: &str) -> Router {
        let client = NotedThatClient::new(api_url, SERVICE_TOKEN).expect("client");
        let config = McpHttpServiceConfig::new(
            ["127.0.0.1", "localhost"],
            ["http://127.0.0.1:8080"],
            CancellationToken::new(),
        )
        .expect("config");
        let service = McpHttpService::new(client, &config);
        let authenticator = Arc::new(Authenticator::new(SERVICE_TOKEN).with_token_verifier(
            Arc::new(StubTokenVerifier::default().accepting(ALICE_TOKEN, "alice", [])),
        ));
        Router::new().route(
            "/mcp",
            post_service(service.into_service()).route_layer(middleware::from_fn_with_state(
                Arc::new(McpAuth {
                    authenticator,
                    anonymous: false,
                }),
                authenticate_caller,
            )),
        )
    }

    async fn list_knowledgebases(app: Router, bearer: &str) -> StatusCode {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "list_knowledgebases", "arguments": {} },
        });
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "127.0.0.1")
            .header("authorization", format!("Bearer {bearer}"))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");
        app.oneshot(request).await.expect("response").status()
    }

    /// What a client actually receives for a `search` whose arguments carry a
    /// key the tool does not know. rmcp refuses the call while deserializing
    /// `Parameters<SearchArgs>`, before the tool runs, and answers it the way
    /// MCP reports a tool failure: a *result* with `isError: true` whose text
    /// is rmcp's own message — not a JSON-RPC error, and not the tool's
    /// `invalid_request` string. `docs/API.md` says so; this pins it.
    #[tokio::test]
    async fn an_unknown_search_argument_is_refused_before_the_tool_runs() {
        // Given — an API that would answer, so a call that got through would
        // succeed. It must never be reached.
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/knowledgebases/notes/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"hits": []})))
            .expect(0)
            .mount(&api)
            .await;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "search", "arguments": {
                "kb": ["notes"], "query": "q", "filter": { "mime": "text/markdown" }
            } },
        });
        let request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "127.0.0.1")
            .header("authorization", format!("Bearer {SERVICE_TOKEN}"))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");

        // When
        let response = app(&api.uri()).oneshot(request).await.expect("response");

        // Then — a tool error naming the key, and the API untouched.
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json-rpc");
        assert!(json.get("error").is_none(), "not a JSON-RPC error: {json}");
        assert_eq!(json["result"]["isError"], true, "{json}");
        let message = json["result"]["content"][0]["text"].as_str().expect("text");
        assert!(
            message.contains("unknown field `filter`") && message.contains("`filters`"),
            "names the unknown key and the accepted one: {message}"
        );
        api.verify().await;
    }

    #[tokio::test]
    async fn a_caller_token_in_the_http_parts_is_forwarded_to_the_api() {
        // Given — the API will only answer alice's own token.
        let api = api_expecting(ALICE_TOKEN).await;

        // When
        let status = list_knowledgebases(app(&api.uri()), ALICE_TOKEN).await;

        // Then — `expect(1)` on the mock is the assertion; a call carrying the
        // service token would have found no matching mock.
        assert_eq!(status, StatusCode::OK);
        api.verify().await;
    }

    #[tokio::test]
    async fn the_service_token_is_forwarded_as_itself() {
        let api = api_expecting(SERVICE_TOKEN).await;
        let status = list_knowledgebases(app(&api.uri()), SERVICE_TOKEN).await;
        assert_eq!(status, StatusCode::OK);
        api.verify().await;
    }

    #[test]
    fn without_http_parts_the_configured_token_is_used() {
        // Given — a handler built with the service token and no request context
        // extensions to draw on, which is the stdio transport's situation.
        let client = NotedThatClient::new("http://127.0.0.1:1", SERVICE_TOKEN).expect("client");
        let with_caller = client.with_token(ALICE_TOKEN);

        // When / Then — `with_token` swaps only the credential.
        assert_eq!(with_caller.base_url_display(), client.base_url_display());
        assert_eq!(with_caller.token.as_deref(), Some(ALICE_TOKEN));
        assert_eq!(client.token.as_deref(), Some(SERVICE_TOKEN));
    }
}

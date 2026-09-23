//! Stateful streamable HTTP transport for the MCP tool handler (D31, D66).
//!
//! `initialize` opens a session that rmcp keeps in process: one handler
//! instance per session, every later `POST` carrying its `Mcp-Session-Id`,
//! every answer SSE-framed, `GET` as the server-to-client notification leg and
//! `DELETE` to end it. Sessions are what let the server push
//! `notifications/resources/updated`; they are also what an anonymous or
//! misbehaving client could open without bound, so [`admit_session`] caps
//! them at [`MAX_SESSIONS`] per process.
//!
//! **A session id is not bound to a credential.** rmcp binds nothing to a
//! session, and auth is outermost, so any valid credential — including the
//! anonymous caller where `anyone` is granted — that presents a session id can
//! attach that session's `GET` leg or `unsubscribe` from it. Tool calls are
//! unaffected: each acts as the credential presented on its own request. The
//! notification leg is not, and this is why it matters more since
//! subscriptions: a forwarder runs as the *session owner's* credential, so
//! whoever attaches the leg reads which object URIs that principal subscribed
//! to and exactly when those objects change — object keys and change timing
//! for objects the attacher may not read. `list_changed` leaks write timing
//! the same way. Session ids are rmcp-generated UUIDs, so this is not
//! guessable; the binding is a disclosed follow-up (§7.4), recorded as a
//! confidentiality limitation rather than as tidiness.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{NotedThatMcp, client::NotedThatClient};

/// Axum/tower-compatible rmcp Streamable HTTP service type for `NotedThatMcp`.
pub type McpHttpInnerService = StreamableHttpService<NotedThatMcp, LocalSessionManager>;

/// The most sessions one process holds at once. A `POST` that would open
/// another — one carrying no `Mcp-Session-Id` — answers `503` until a session
/// ends or idles out (five minutes, rmcp's default), so a client that keeps
/// its session id costs one slot for as long as it is active.
pub const MAX_SESSIONS: usize = 256;

/// The session header, as rmcp spells it.
pub const SESSION_HEADER: &str = "mcp-session-id";

/// Validated streamable HTTP server configuration.
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

    /// rmcp's defaults are the stateful transport: sessions on, JSON-response
    /// mode off (it is only consulted when stateless), a keep-alive comment
    /// every 15 s on every open stream and a 3 s `retry:` hint.
    fn streamable_http_config(&self) -> StreamableHttpServerConfig {
        StreamableHttpServerConfig::default()
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

/// Reusable streamable HTTP service wrapper for `NotedThatMcp`.
#[derive(Clone)]
pub struct McpHttpService {
    inner: McpHttpInnerService,
    sessions: Arc<LocalSessionManager>,
}

impl McpHttpService {
    /// Create a stateful streamable HTTP service with its own session table.
    ///
    /// `events_enabled` says whether the deployment has an events backend:
    /// with one, each session's handler may take resource subscriptions,
    /// whose forwarders stop with the config's cancellation token (D66).
    pub fn new(
        client: NotedThatClient,
        config: &McpHttpServiceConfig,
        events_enabled: bool,
    ) -> Self {
        let streamable_config = config.streamable_http_config();
        let events = events_enabled.then(|| config.cancellation_token.clone());
        let service_factory = move || Ok(NotedThatMcp::for_http(client.clone(), events.as_ref()));
        let sessions = Arc::new(LocalSessionManager::default());
        let inner =
            StreamableHttpService::new(service_factory, sessions.clone(), streamable_config);

        Self { inner, sessions }
    }

    /// Return the rmcp Streamable HTTP service for use with axum/tower routing.
    pub fn into_service(self) -> McpHttpInnerService {
        self.inner
    }

    /// The session table, for [`admit_session`].
    #[must_use]
    pub fn session_manager(&self) -> Arc<LocalSessionManager> {
        self.sessions.clone()
    }

    /// Inspect the effective rmcp Streamable HTTP server config.
    pub fn config(&self) -> &StreamableHttpServerConfig {
        &self.inner.config
    }
}

/// Refuse to open a session past [`MAX_SESSIONS`].
///
/// Only a `POST` with no `Mcp-Session-Id` can open one (rmcp answers anything
/// but `initialize` there with `422`), so that is the only request counted;
/// a request on an existing session, the `GET` leg and `DELETE` pass. The
/// refusal is `503` with `Retry-After`, the shape every other capacity
/// refusal on this server takes (D38).
///
/// A **soft** cap, not a hard one: the count and `next.run` are not atomic, so
/// concurrent sessionless `POST`s can all read a count below the limit and
/// overshoot it by however many were in flight. Bounded by concurrency and
/// harmless — the point is to stop unbounded growth, not to hold an exact
/// number — but the constant reads like a hard cap and is not one. Making it
/// exact means reserving a slot before rmcp creates the session and releasing
/// it if rmcp does not, which is more machinery than the overshoot costs.
pub async fn admit_session(
    State(sessions): State<Arc<LocalSessionManager>>,
    request: Request,
    next: Next,
) -> Response {
    let opens_session = request.method() == axum::http::Method::POST
        && !request.headers().contains_key(SESSION_HEADER);
    if opens_session && sessions.sessions.read().await.len() >= MAX_SESSIONS {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("application/json")),
                (header::RETRY_AFTER, HeaderValue::from_static("5")),
            ],
            Body::from(format!(
                r#"{{"error":"backend_unavailable","message":"this server holds its maximum of {MAX_SESSIONS} MCP sessions; retry when one ends"}}"#
            )),
        )
            .into_response();
    }
    next.run(request).await
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
        let service = McpHttpService::new(test_client(), &config, false);

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
    fn serves_stateful_sessions_without_json_response_mode() {
        // Given: valid HTTP service config.
        let token = CancellationToken::new();
        let config = test_config(token);

        // When: the MCP HTTP service is created.
        let service = McpHttpService::new(test_client(), &config, false);

        // Then: sessions are on — the notification leg needs them — and the
        // JSON-response switch, which rmcp reads only when stateless, is off.
        assert!(service.config().stateful_mode);
        assert!(!service.config().json_response);
        assert!(service.config().sse_keep_alive.is_some());
    }

    #[test]
    fn uses_caller_supplied_cancellation_token() {
        // Given: a cancellation token owned by the caller.
        let token = CancellationToken::new();
        let config = test_config(token.clone());
        let service = McpHttpService::new(test_client(), &config, false);

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
        let service = McpHttpService::new(test_client(), &config, false);

        // When: callers request the underlying rmcp service.
        let inner = service.into_service();

        // Then: it preserves the stateful rmcp config.
        assert!(inner.config.stateful_mode);
        assert!(!inner.config.json_response);
    }
}

#[cfg(test)]
mod caller_identity {
    //! The MCP service acts as its caller: the bearer presented to `/mcp` is
    //! the bearer the loopback API call carries.

    use super::*;
    use crate::auth::{McpAuth, authenticate_caller};
    use crate::sse::SseParser;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::{MethodFilter, on_service};
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
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "knowledgebases": [{ "kb_slug": "notes", "display_name": "notes" }]
            })))
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
        let service = McpHttpService::new(client, &config, false);
        let sessions = service.session_manager();
        let authenticator = Arc::new(Authenticator::new(SERVICE_TOKEN).with_token_verifier(
            Arc::new(StubTokenVerifier::default().accepting(ALICE_TOKEN, "alice", [])),
        ));
        // The same stack `notedthat-server` mounts: auth outermost, then the
        // session bound, then rmcp for POST, GET and DELETE.
        Router::new().route(
            "/mcp",
            on_service(
                MethodFilter::GET
                    .or(MethodFilter::POST)
                    .or(MethodFilter::DELETE),
                service.into_service(),
            )
            .route_layer(middleware::from_fn_with_state(sessions, admit_session))
            .route_layer(middleware::from_fn_with_state(
                Arc::new(McpAuth {
                    authenticator,
                    anonymous: false,
                }),
                authenticate_caller,
            )),
        )
    }

    /// One `POST /mcp` as `bearer`, on `session` when there is one; the
    /// answer with the JSON-RPC message it framed, if any.
    async fn post(
        app: &Router,
        bearer: &str,
        session: Option<&str>,
        body: serde_json::Value,
    ) -> (axum::response::Response, Option<serde_json::Value>) {
        let mut request = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "127.0.0.1")
            .header("authorization", format!("Bearer {bearer}"))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json");
        if let Some(id) = session {
            request = request.header(SESSION_HEADER, id);
        }
        let request = request.body(Body::from(body.to_string())).expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, 64 * 1024).await.expect("body");
        let mut parser = SseParser::new();
        let mut frames = parser.feed(&bytes);
        frames.extend(parser.feed(b"\n\n"));
        let message = frames
            .into_iter()
            .find(crate::sse::SseEvent::has_data)
            .and_then(|frame| serde_json::from_str(&frame.data).ok());
        (
            axum::response::Response::from_parts(parts, Body::empty()),
            message,
        )
    }

    /// `initialize` as `bearer`; the session id the transport issued.
    async fn open_session(app: &Router, bearer: &str) -> String {
        let (response, message) = post(
            app,
            bearer,
            None,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" }
                },
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{message:?}");
        response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
            .expect("a stateful transport issues a session id")
            .to_owned()
    }

    async fn list_knowledgebases(app: Router, bearer: &str) -> StatusCode {
        let session = open_session(&app, bearer).await;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "list_knowledgebases", "arguments": {} },
        });
        post(&app, bearer, Some(&session), body).await.0.status()
    }

    #[tokio::test]
    async fn a_tool_call_without_a_session_is_refused_and_one_with_it_answers() {
        let api = api_expecting(SERVICE_TOKEN).await;
        let app = app(&api.uri());
        let call = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "list_knowledgebases", "arguments": {} },
        });

        // Without a session rmcp answers 422 before any handler runs; the
        // API is not reached (its `expect(1)` is satisfied by the second call).
        let (response, _) = post(&app, SERVICE_TOKEN, None, call.clone()).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let session = open_session(&app, SERVICE_TOKEN).await;
        let (response, message) = post(&app, SERVICE_TOKEN, Some(&session), call).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/event-stream")),
            "a stateful answer is SSE-framed"
        );
        let message = message.expect("the frame carries the JSON-RPC answer");
        assert!(message.get("result").is_some(), "{message}");
        api.verify().await;
    }

    #[tokio::test]
    async fn a_new_session_is_refused_above_the_bound() {
        let api = MockServer::start().await;
        let app = app(&api.uri());
        for _ in 0..MAX_SESSIONS {
            open_session(&app, SERVICE_TOKEN).await;
        }

        let (response, _) = post(
            &app,
            SERVICE_TOKEN,
            None,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" }
                },
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "5");

        // An existing session is not counted against anyone.
        let session = open_session_or_none(&app).await;
        assert!(session.is_none(), "the bound holds for new sessions only");
    }

    /// `initialize` that may be refused: `Some(id)` when a session opened.
    async fn open_session_or_none(app: &Router) -> Option<String> {
        let (response, _) = post(
            app,
            SERVICE_TOKEN,
            None,
            serde_json::json!({
                "jsonrpc": "2.0", "id": 0, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" }
                },
            }),
        )
        .await;
        response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
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

        // When
        let app = app(&api.uri());
        let session = open_session(&app, SERVICE_TOKEN).await;
        let (response, json) = post(&app, SERVICE_TOKEN, Some(&session), body).await;

        // Then — a tool error naming the key, and the API untouched.
        assert_eq!(response.status(), StatusCode::OK);
        let json = json.expect("json-rpc");
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
    fn with_token_swaps_only_the_credential() {
        // Given — a client built with the service token.
        let client = NotedThatClient::new("http://127.0.0.1:1", SERVICE_TOKEN).expect("client");
        let with_caller = client.with_token(ALICE_TOKEN);

        // When / Then — `with_token` swaps only the credential.
        assert_eq!(with_caller.base_url_display(), client.base_url_display());
        assert_eq!(with_caller.token.as_deref(), Some(ALICE_TOKEN));
        assert_eq!(client.token.as_deref(), Some(SERVICE_TOKEN));
    }
}

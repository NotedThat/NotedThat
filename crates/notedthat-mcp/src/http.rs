//! Stateful streamable HTTP transport for the MCP tool handler (D31, D66).
//!
//! `initialize` opens a session that rmcp keeps in process: one handler
//! instance per session, every later `POST` carrying its `Mcp-Session-Id`,
//! every answer SSE-framed, `GET` as the server-to-client notification leg and
//! `DELETE` to end it. Sessions are what let the server push
//! `notifications/resources/updated`; they are also what an anonymous or
//! misbehaving client could open without bound, so [`bind_session`] caps them
//! at `NOTEDTHAT_MCP_MAX_SESSIONS` ([`DEFAULT_MAX_SESSIONS`]) per process.
//!
//! **A session belongs to the principal that opened it** (D68). rmcp binds
//! nothing to a session, so before D68 any valid credential — including the
//! anonymous caller where `anyone` is granted — that presented a session id
//! could attach that session's `GET` leg, `unsubscribe` from it, or `DELETE`
//! it. Tool calls were never the problem: each acts as the credential
//! presented on its own request. The notification leg is, and subscriptions
//! are what gave it value: a forwarder runs as the *session owner's*
//! credential, so whoever attached the leg read which object URIs that
//! principal subscribed to and exactly when those objects changed — object
//! keys and change timing for objects the attacher may not read, with
//! `list_changed` leaking write timing the same way.
//!
//! rmcp holds no room for a caller, and [`LocalSessionManager::create_session`]
//! receives no request at all, so the record is ours: [`bind_session`] reads
//! `Mcp-Session-Id` off the `initialize` **response** — the first place the id
//! is visible to us — and stores the session against a [`SessionOwner`]
//! derived from the `Principal` the auth layer resolved. A later request
//! presenting that id as anyone else is refused.
//!
//! Three things about that are worth stating plainly, because each is a choice
//! rather than a consequence:
//!
//! - **The key is the subject, never the bearer and never the groups.** The
//!   ordinary reason a session's credential changes mid-life is not an attacker
//!   but OIDC access-token refresh — a client keeps one session for its whole
//!   connection, which is the point of the stateful transport, and rotates its
//!   token underneath it every few minutes, possibly with different groups (see
//!   the `WatchKey` rationale in [`crate::subscriptions`]). Binding either
//!   would cost a client its own session on every refresh.
//! - **A mismatch answers exactly what an unknown id answers**, byte for byte,
//!   so the check is not an oracle for which session ids exist. That is `404`
//!   with rmcp's own plain-text body on `POST` and `GET`, and — the one place
//!   the two differ — `202` on `DELETE`, which is what rmcp answers for an id
//!   it does not know; the session is left untouched.
//! - **Every anonymous caller is one owner**, because there is no credential to
//!   tell two of them apart. On a deployment admitting them, one anonymous
//!   client can still attach another's leg. Both hold identical read authority
//!   — whatever `anyone` grants — so what leaks is which public URIs the other
//!   subscribed to, and nothing either could not already read. The service
//!   token is likewise one owner: whatever holds it holds all of its sessions.
//!
//! rmcp offers no close callback, and a session can end through `DELETE`, the
//! idle timeout, an init timeout or a worker error, of which only the first is
//! visible here. So the record is reclaimed on `DELETE` directly and otherwise
//! by reconciling against rmcp's own table before each new session is recorded
//! — which bounds it by that table for one pass per `initialize`. What is still
//! **not** done is budgeting sessions per principal rather than per process:
//! one caller can hold every slot (§7.4, issue #179). This record is the
//! prerequisite for it.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use notedthat_core::{Identity, Principal};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::{SessionId, local::LocalSessionManager},
};
use thiserror::Error;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::{NotedThatMcp, client::NotedThatClient};

/// Axum/tower-compatible rmcp Streamable HTTP service type for `NotedThatMcp`.
pub type McpHttpInnerService = StreamableHttpService<NotedThatMcp, LocalSessionManager>;

/// The most sessions one process holds at once, absent
/// `NOTEDTHAT_MCP_MAX_SESSIONS`. A `POST` that would open another — one
/// carrying no `Mcp-Session-Id` — answers `503` until a session ends or idles
/// out (five minutes, rmcp's default), so a client that keeps its session id
/// costs one slot for as long as it is active.
pub const DEFAULT_MAX_SESSIONS: usize = 256;

/// The session header, as rmcp spells it.
pub const SESSION_HEADER: &str = "mcp-session-id";

/// Validated streamable HTTP server configuration.
#[derive(Clone, Debug)]
pub struct McpHttpServiceConfig {
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    cancellation_token: CancellationToken,
    max_sessions: usize,
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
            max_sessions: DEFAULT_MAX_SESSIONS,
        })
    }

    /// Hold at most `max_sessions` sessions in this process
    /// (`NOTEDTHAT_MCP_MAX_SESSIONS`). The caller owns the number: this crate
    /// reads no environment of its own, exactly as it takes its allow-lists
    /// and cancellation token from the caller.
    #[must_use]
    pub fn with_max_sessions(mut self, max_sessions: usize) -> Self {
        self.max_sessions = max_sessions;
        self
    }

    /// The configured session bound.
    #[must_use]
    pub fn max_sessions(&self) -> usize {
        self.max_sessions
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

/// Whose a session is: the part of a [`Principal`] a session is keyed on, and
/// no more of it.
///
/// Deliberately not a `Principal`. `Principal` carries groups, and hashing
/// those would make a token refresh that gains or drops one look like a
/// different caller — costing a client its own session for the most ordinary
/// reason a credential changes. `notedthat_core`'s own private `Reach` is the
/// same idea: the part of a principal one consumer keys on is that consumer's
/// business.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum SessionOwner {
    /// Every caller that presented no credential, under one marker: they hold
    /// identical read authority (whatever `anyone` grants), so the binding
    /// cannot tell them apart and does not pretend to.
    Anonymous,
    /// The deployment's own credential, which has no subject of its own.
    ServiceToken,
    /// An identity provider's subject, verbatim — never the bearer.
    Subject(String),
}

impl From<&Principal> for SessionOwner {
    fn from(principal: &Principal) -> Self {
        match principal {
            Principal::Anyone => Self::Anonymous,
            Principal::SignedIn(Identity::ServiceToken) => Self::ServiceToken,
            Principal::SignedIn(Identity::User(user)) => Self::Subject(user.subject.clone()),
        }
    }
}

/// rmcp's session table, who owns each session in it, and the bound on how many
/// this process holds: the state [`bind_session`] runs on.
///
/// rmcp is the only writer of the table itself; this type adds what rmcp holds
/// no room for.
#[derive(Debug)]
pub struct McpSessions {
    /// Handed to rmcp as its session manager, and read here to count sessions
    /// and to reconcile [`Self::owners`] against what rmcp still knows.
    manager: Arc<LocalSessionManager>,
    /// Session id → the principal that opened it, recorded on the `initialize`
    /// response and reclaimed with the session.
    owners: RwLock<HashMap<SessionId, SessionOwner>>,
    max_sessions: usize,
}

impl McpSessions {
    fn new(max_sessions: usize) -> Self {
        Self {
            manager: Arc::new(LocalSessionManager::default()),
            owners: RwLock::new(HashMap::new()),
            max_sessions,
        }
    }

    /// The session table, for rmcp.
    #[must_use]
    pub fn manager(&self) -> Arc<LocalSessionManager> {
        self.manager.clone()
    }

    /// The configured session bound.
    #[must_use]
    pub fn max_sessions(&self) -> usize {
        self.max_sessions
    }

    /// Whether this process already holds as many sessions as it may.
    async fn at_capacity(&self) -> bool {
        self.manager.sessions.read().await.len() >= self.max_sessions
    }

    /// Whether `presented` may act on the session `id` names.
    ///
    /// A session we hold no record of is admitted: rmcp answers for it, and
    /// inventing an owner for a session we did not see opened would refuse the
    /// caller that did open it. A recorded session with no principal on the
    /// request is refused — unreachable under the layer order the server mounts
    /// (auth is outermost and always resolves one, the anonymous caller
    /// included), and the safe answer if someone mounts this without it.
    async fn admits(&self, id: &str, presented: Option<&SessionOwner>) -> bool {
        match self.owners.read().await.get(id) {
            None => true,
            Some(owner) => presented == Some(owner),
        }
    }

    /// Record who opened the session `id` names, reclaiming the records of
    /// sessions rmcp no longer holds while we are here.
    ///
    /// The sweep is the eviction path for every ending rmcp does not tell us
    /// about — the idle timeout, an init timeout, a worker error. It runs once
    /// per new session, which is the only place this map grows, and is `O(n)`
    /// in a map rmcp's own table bounds.
    ///
    /// Lock order matters and is the reason the write lock spans the read:
    /// rmcp inserts into its table strictly before `bind_session` gets the
    /// response, so holding `owners` across the snapshot makes the two
    /// orderings exhaustive — a concurrent `record` either finished before we
    /// took the lock, in which case rmcp's insert did too and the id is in the
    /// snapshot, or it is waiting for the lock and inserts after we are done.
    /// Taking the snapshot without the lock held would leave a window in which
    /// a session recorded after it is swept away. Nothing anywhere takes rmcp's
    /// lock before this one.
    async fn record(&self, id: &str, owner: SessionOwner) {
        let mut owners = self.owners.write().await;
        {
            let live = self.manager.sessions.read().await;
            owners.retain(|session, _| live.contains_key(session));
        }
        owners.insert(SessionId::from(id), owner);
    }

    /// Forget the session `id` names — rmcp has already dropped it.
    async fn forget(&self, id: &str) {
        self.owners.write().await.remove(id);
    }

    /// How many sessions we hold an owner for.
    #[cfg(test)]
    async fn owner_count(&self) -> usize {
        self.owners.read().await.len()
    }
}

/// Reusable streamable HTTP service wrapper for `NotedThatMcp`.
#[derive(Clone)]
pub struct McpHttpService {
    inner: McpHttpInnerService,
    sessions: Arc<McpSessions>,
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
        let sessions = Arc::new(McpSessions::new(config.max_sessions));
        let inner =
            StreamableHttpService::new(service_factory, sessions.manager(), streamable_config);

        Self { inner, sessions }
    }

    /// Return the rmcp Streamable HTTP service for use with axum/tower routing.
    pub fn into_service(self) -> McpHttpInnerService {
        self.inner
    }

    /// The session state, for [`bind_session`].
    #[must_use]
    pub fn sessions(&self) -> Arc<McpSessions> {
        self.sessions.clone()
    }

    /// Inspect the effective rmcp Streamable HTTP server config.
    pub fn config(&self) -> &StreamableHttpServerConfig {
        &self.inner.config
    }
}

/// Bind each session to the principal that opened it (D68), and refuse to open
/// one past the configured bound.
///
/// Three paths, in the order a request meets them:
///
/// - **A request carrying a session id** is refused unless the principal on it
///   owns that session; see [`McpSessions::admits`]. A `DELETE` that gets
///   through also drops the record, since rmcp removes the session before
///   answering.
/// - **A `POST` carrying none** is the only request that can open a session
///   (rmcp answers anything but `initialize` there with `422`), so it is the
///   only one counted against the bound, and the only one whose response can
///   carry a new `Mcp-Session-Id` to record. The capacity refusal is `503` with
///   `Retry-After`, the shape every other capacity refusal on this server takes
///   (D38).
/// - **A sessionless `GET` or `DELETE`** has nothing to bind; rmcp answers
///   `400`.
///
/// The bound is a **soft** cap, not a hard one: the count and `next.run` are not
/// atomic, so concurrent sessionless `POST`s can all read a count below the
/// limit and overshoot it by however many were in flight. Bounded by
/// concurrency and harmless — the point is to stop unbounded growth, not to
/// hold an exact number — but the setting reads like a hard cap and is not one.
/// Making it exact means reserving a slot before rmcp creates the session and
/// releasing it if rmcp does not, which travels with the per-principal budget
/// (#179).
pub async fn bind_session(
    State(sessions): State<Arc<McpSessions>>,
    mut request: Request,
    next: Next,
) -> Response {
    let owner = request
        .extensions()
        .get::<Principal>()
        .map(SessionOwner::from);
    let presented = request
        .headers()
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    if let Some(id) = presented {
        if !sessions.admits(&id, owner.as_ref()).await {
            tracing::warn!(
                target: "notedthat::mcp",
                method = %request.method(),
                "MCP_SESSION_OWNER_MISMATCH a session was presented by a principal that does not own it"
            );
            // Hand rmcp an id it cannot know and let it answer, rather than
            // synthesizing the answer here.
            //
            // Synthesizing it short-circuited ahead of every precondition rmcp
            // validates *before* its own session lookup — the `Host`/`Origin`
            // allow-list, `Accept`, `Content-Type`, the JSON parse,
            // `MCP-Protocol-Version` — so a malformed request told the two
            // apart: rmcp answered `403`/`406`/`415`/`400` for an id it did not
            // know, while this layer answered `404`/`202` for one it refused.
            // That difference named which session ids exist, which is the one
            // thing this branch is for. Falling through makes the two answers
            // identical by construction instead of by a copy that has to be
            // kept in step with the crate.
            //
            // `DELETE` stays safe: rmcp's `close_session` on an id it does not
            // hold removes nothing and still answers `202`, so the owner's
            // session is untouched — and `forget` below is not reached, so the
            // record survives the refusal too.
            request
                .headers_mut()
                .insert(SESSION_HEADER, HeaderValue::from_static(NO_SUCH_SESSION));
            return next.run(request).await;
        }
        let closing = request.method() == Method::DELETE;
        let response = next.run(request).await;
        if closing && response.status().is_success() {
            sessions.forget(&id).await;
        }
        return response;
    }

    if request.method() == Method::POST {
        if sessions.at_capacity().await {
            return capacity_refusal(sessions.max_sessions());
        }
        let response = next.run(request).await;
        // rmcp sets this header on one response only — the one that opened a
        // session — so a response carrying it is unambiguously a new session,
        // and it is recorded before the client can present the id back.
        let opened = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        match (opened, owner) {
            (Some(id), Some(owner)) => sessions.record(&id, owner).await,
            // The write side of `admits`'s no-principal case, and the one that
            // fails *open*: an unrecorded session is admitted for everyone, so
            // a session created here without an owner is drivable by every
            // caller — exactly what D68 exists to prevent. Unreachable under
            // the layer order `build_router` mounts, but `bind_session` is
            // `pub` and nothing fails if that pairing is broken, so it is said
            // out loud rather than left silent. rmcp offers no handle to end a
            // session it has just created, which is why this logs instead of
            // closing it.
            (Some(_), None) => tracing::error!(
                target: "notedthat::mcp",
                "MCP_SESSION_UNBOUND a session opened with no principal on the request; \
                 bind_session is mounted without an auth layer and the session is unowned"
            ),
            _ => {}
        }
        return response;
    }

    next.run(request).await
}

/// A session id rmcp cannot have issued, substituted onto a refused request so
/// that rmcp produces its own unknown-session answer.
///
/// rmcp mints ids with `Uuid::new_v4()`, whose version nibble is always `4`.
/// The nil UUID's is `0`, so no session rmcp holds can ever carry this id and
/// the substitution cannot collide with a live one.
const NO_SUCH_SESSION: &str = "00000000-0000-0000-0000-000000000000";

/// The D38 capacity refusal: `503`, `Retry-After: 5`, and a body naming the
/// bound the caller ran into.
fn capacity_refusal(max_sessions: usize) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/json")),
            (header::RETRY_AFTER, HeaderValue::from_static("5")),
        ],
        Body::from(format!(
            r#"{{"error":"backend_unavailable","message":"this server holds its maximum of {max_sessions} MCP sessions; retry when one ends"}}"#
        )),
    )
        .into_response()
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
    fn the_session_bound_defaults_to_the_documented_two_hundred_and_fifty_six() {
        // Given / When: a config that says nothing about the bound.
        let config = test_config(CancellationToken::new());

        // Then: the documented default, and a caller-supplied value is kept.
        assert_eq!(DEFAULT_MAX_SESSIONS, 256);
        assert_eq!(config.max_sessions(), DEFAULT_MAX_SESSIONS);
        assert_eq!(config.with_max_sessions(7).max_sessions(), 7);
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
    /// Alice again after a refresh: a different bearer, the same subject, and
    /// different groups — which is what a refresh may also change.
    const ALICE_REFRESHED: &str = "jwt-alice-refreshed";
    const BOB_TOKEN: &str = "jwt-bob";

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
        app_parts(api_url, DEFAULT_MAX_SESSIONS).0
    }

    /// The same harness admitting the anonymous caller (D59).
    fn anonymous_app(api_url: &str) -> Router {
        app_with(api_url, DEFAULT_MAX_SESSIONS, true).0
    }

    /// The router and the session state behind it, so a test can assert on
    /// both — and with a session bound of its own, so the bound can be reached
    /// in two `initialize`s rather than in two hundred and fifty-seven.
    fn app_parts(api_url: &str, max_sessions: usize) -> (Router, Arc<McpSessions>) {
        app_with(api_url, max_sessions, false)
    }

    fn app_with(api_url: &str, max_sessions: usize, anonymous: bool) -> (Router, Arc<McpSessions>) {
        let client = NotedThatClient::new(api_url, SERVICE_TOKEN).expect("client");
        let config = McpHttpServiceConfig::new(
            ["127.0.0.1", "localhost"],
            ["http://127.0.0.1:8080"],
            CancellationToken::new(),
        )
        .expect("config")
        .with_max_sessions(max_sessions);
        let service = McpHttpService::new(client, &config, false);
        let sessions = service.sessions();
        let authenticator = Arc::new(
            Authenticator::new(SERVICE_TOKEN).with_token_verifier(Arc::new(
                StubTokenVerifier::default()
                    .accepting(ALICE_TOKEN, "alice", [])
                    .accepting(ALICE_REFRESHED, "alice", ["editors"])
                    .accepting(BOB_TOKEN, "bob", []),
            )),
        );
        // The same stack `notedthat-server` mounts: auth outermost, then the
        // session bound, then rmcp for POST, GET and DELETE.
        let router = Router::new().route(
            "/mcp",
            on_service(
                MethodFilter::GET
                    .or(MethodFilter::POST)
                    .or(MethodFilter::DELETE),
                service.into_service(),
            )
            .route_layer(middleware::from_fn_with_state(
                sessions.clone(),
                bind_session,
            ))
            .route_layer(middleware::from_fn_with_state(
                Arc::new(McpAuth {
                    authenticator,
                    anonymous,
                }),
                authenticate_caller,
            )),
        );
        (router, sessions)
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

    /// A way to make a request malformed that rmcp rejects *before* it looks
    /// the session up. One per precondition the refusal used to short-circuit
    /// past, and each was its own oracle for which session ids exist.
    #[derive(Clone, Copy, Debug)]
    enum Flaw {
        /// An `Accept` rmcp will not serve.
        Accept,
        /// A `Content-Type` rmcp will not parse.
        ContentType,
        /// An `MCP-Protocol-Version` rmcp does not know.
        ProtocolVersion,
        /// A `Host` outside the allow-list — the first thing rmcp checks.
        Host,
    }

    /// One request of any method, with `bearer` when there is one and `session`
    /// when there is one: the status, the headers and the body bytes, none of
    /// them interpreted. The refusal assertions compare all three.
    async fn raw(
        app: &Router,
        method: Method,
        bearer: Option<&str>,
        session: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, axum::body::Bytes) {
        raw_with(app, method, bearer, session, None).await
    }

    /// [`raw`], optionally malformed in one of the ways rmcp rejects ahead of
    /// its own session lookup.
    async fn raw_with(
        app: &Router,
        method: Method,
        bearer: Option<&str>,
        session: Option<&str>,
        flaw: Option<Flaw>,
    ) -> (StatusCode, axum::http::HeaderMap, axum::body::Bytes) {
        let host = match flaw {
            Some(Flaw::Host) => "attacker.example.com",
            _ => "127.0.0.1",
        };
        let accept = match (&method, flaw) {
            (_, Some(Flaw::Accept)) => "application/json",
            (&Method::GET, _) => "text/event-stream",
            _ => "application/json, text/event-stream",
        };
        let mut request = Request::builder()
            .method(method.clone())
            .uri("/mcp")
            .header("host", host)
            .header("accept", accept);
        if method != Method::GET {
            request = request.header(
                "content-type",
                match flaw {
                    Some(Flaw::ContentType) => "text/plain",
                    _ => "application/json",
                },
            );
        }
        if matches!(flaw, Some(Flaw::ProtocolVersion)) {
            request = request.header("mcp-protocol-version", "bogus");
        }
        if let Some(bearer) = bearer {
            request = request.header("authorization", format!("Bearer {bearer}"));
        }
        if let Some(id) = session {
            request = request.header(SESSION_HEADER, id);
        }
        let body = if method == Method::POST {
            Body::from(
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": { "name": "list_knowledgebases", "arguments": {} },
                })
                .to_string(),
            )
        } else {
            Body::empty()
        };
        let request = request.body(body).expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        let (parts, body) = response.into_parts();
        // A `GET` that opened the notification leg never ends on its own, so it
        // is read with a deadline and whatever arrived is the answer.
        let bytes = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            axum::body::to_bytes(body, 64 * 1024),
        )
        .await
        .map(|body| body.expect("body"))
        .unwrap_or_default();
        (parts.status, parts.headers, bytes)
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
    async fn a_new_session_is_refused_above_the_configured_bound() {
        // Given: a process that may hold two sessions, holding two.
        let api = MockServer::start().await;
        let (app, _) = app_parts(&api.uri(), 2);
        let first = open_session(&app, SERVICE_TOKEN).await;
        open_session(&app, SERVICE_TOKEN).await;

        // When: a third `initialize` arrives.
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

        // Then: the D38 capacity refusal, naming the configured bound rather
        // than the default.
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "5");

        // And: the bound holds for new sessions only — an existing one still
        // answers, while another `initialize` is still refused.
        let call = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "list_knowledgebases", "arguments": {} },
        });
        let (existing, _) = post(&app, SERVICE_TOKEN, Some(&first), call).await;
        assert_eq!(existing.status(), StatusCode::OK);
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
    mod session_binding {
        //! A session id is a handle to one principal's session (D68), not a bearer
        //! of its own: presented by anyone else it answers as if it did not exist.

        use super::*;
        // `close_session` is the trait's, and it is how every ending rmcp does
        // not report — the idle timeout, an init timeout, a worker error —
        // reaches the session table.
        use rmcp::transport::streamable_http_server::session::SessionManager as _;

        const UNKNOWN_SESSION: &str = "00000000-0000-4000-8000-000000000000";

        fn call() -> serde_json::Value {
            serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "list_knowledgebases", "arguments": {} },
            })
        }

        #[tokio::test]
        async fn a_session_answers_only_to_the_principal_that_opened_it() {
            // Given: alice's session, on a server whose API would answer anyone.
            let api = MockServer::start().await;
            let app = app(&api.uri());
            let alice = open_session(&app, ALICE_TOKEN).await;

            // When / Then: bob presents it on each of the three methods. `POST` and
            // `GET` are refused as an unknown session is; `DELETE` is accepted, as
            // rmcp accepts a `DELETE` for any id — and does nothing.
            let (status, _, _) = raw(&app, Method::POST, Some(BOB_TOKEN), Some(&alice)).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = raw(&app, Method::GET, Some(BOB_TOKEN), Some(&alice)).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = raw(&app, Method::DELETE, Some(BOB_TOKEN), Some(&alice)).await;
            assert_eq!(status, StatusCode::ACCEPTED);

            // And: alice still has her session — bob's `DELETE` did not end it,
            // which before D68 it would have.
            let (response, message) = post(&app, ALICE_TOKEN, Some(&alice), call()).await;
            assert_eq!(response.status(), StatusCode::OK, "{message:?}");
        }

        #[tokio::test]
        async fn the_service_token_and_a_user_never_share_a_session() {
            let api = MockServer::start().await;
            let app = app(&api.uri());

            // Given: one session each.
            let alice = open_session(&app, ALICE_TOKEN).await;
            let service = open_session(&app, SERVICE_TOKEN).await;

            // When / Then: neither may drive the other's, in either direction.
            let (status, _, _) = raw(&app, Method::POST, Some(SERVICE_TOKEN), Some(&alice)).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = raw(&app, Method::POST, Some(ALICE_TOKEN), Some(&service)).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn a_refused_session_is_indistinguishable_from_an_unknown_one() {
            // Given: alice's session, and an id that was never issued.
            let api = MockServer::start().await;
            let app = app(&api.uri());
            let alice = open_session(&app, ALICE_TOKEN).await;

            // Well-formed, and malformed in each of the ways rmcp rejects
            // before it looks a session up. The malformed ones are the point: a
            // refusal synthesized ahead of those preconditions answered
            // `404`/`202` where an unknown id got `403`/`406`/`415`/`400`, and
            // one `DELETE` carrying a bogus protocol version was enough to ask
            // whether an id was live.
            let flaws = [
                None,
                Some(Flaw::Accept),
                Some(Flaw::ContentType),
                Some(Flaw::ProtocolVersion),
                Some(Flaw::Host),
            ];
            for method in [Method::POST, Method::GET, Method::DELETE] {
                for flaw in flaws {
                    // When: bob presents each of them.
                    let refused =
                        raw_with(&app, method.clone(), Some(BOB_TOKEN), Some(&alice), flaw).await;
                    let unknown = raw_with(
                        &app,
                        method.clone(),
                        Some(BOB_TOKEN),
                        Some(UNKNOWN_SESSION),
                        flaw,
                    )
                    .await;

                    // Then: the same answer, down to the bytes and the headers — so the
                    // refusal is not an oracle for which session ids exist.
                    assert_eq!(refused.0, unknown.0, "{method} {flaw:?} status");
                    assert_eq!(refused.2, unknown.2, "{method} {flaw:?} body");
                    assert_eq!(
                        refused.1.get(header::CONTENT_TYPE),
                        unknown.1.get(header::CONTENT_TYPE),
                        "{method} {flaw:?} content-type"
                    );
                }
            }

            // And: none of it touched alice's session, the refused `DELETE`s
            // included — the refusal now reaches rmcp, so this pins that it
            // reaches it carrying an id rmcp cannot act on.
            let (status, _, _) = raw(&app, Method::GET, Some(ALICE_TOKEN), Some(&alice)).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "alice's session must survive bob's refused DELETEs"
            );
        }

        #[tokio::test]
        async fn the_owner_is_the_subject_not_the_bearer_and_not_the_groups() {
            // Given: a session opened with one of alice's bearers.
            let api = MockServer::start().await;
            let app = app(&api.uri());
            let alice = open_session(&app, ALICE_TOKEN).await;

            // When: she refreshes — a different bearer for the same subject,
            // carrying different groups, which is the ordinary way a long-lived
            // session's credential changes.
            let (response, message) = post(&app, ALICE_REFRESHED, Some(&alice), call()).await;

            // Then: it is still her session, on every method.
            assert_eq!(response.status(), StatusCode::OK, "{message:?}");
            let (status, _, _) = raw(&app, Method::GET, Some(ALICE_REFRESHED), Some(&alice)).await;
            assert_eq!(status, StatusCode::OK);
            let (status, _, _) =
                raw(&app, Method::DELETE, Some(ALICE_REFRESHED), Some(&alice)).await;
            assert_eq!(status, StatusCode::ACCEPTED);
        }

        #[tokio::test]
        async fn two_anonymous_callers_share_one_session_owner() {
            // Given: a deployment that admits the anonymous caller (D59), and an
            // anonymous session.
            let api = MockServer::start().await;
            let app = anonymous_app(&api.uri());
            let session = open_anonymous_session(&app).await;

            // When / Then: a second credential-less request drives it. There is no
            // credential to tell two anonymous callers apart, so the binding does
            // not pretend to — the disclosed residual, pinned rather than left to
            // prose.
            let (status, _, _) = raw(&app, Method::GET, None, Some(&session)).await;
            assert_eq!(status, StatusCode::OK);

            // And: a signed-in caller is still refused it.
            let (status, _, _) = raw(&app, Method::GET, Some(ALICE_TOKEN), Some(&session)).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn an_owner_record_ends_with_its_session() {
            // Given: three sessions and their records.
            let api = MockServer::start().await;
            let (app, sessions) = app_parts(&api.uri(), DEFAULT_MAX_SESSIONS);
            let ids = [
                open_session(&app, ALICE_TOKEN).await,
                open_session(&app, BOB_TOKEN).await,
                open_session(&app, SERVICE_TOKEN).await,
            ];
            assert_eq!(sessions.owner_count().await, 3);

            // When: each owner ends its own session.
            for (id, bearer) in ids.iter().zip([ALICE_TOKEN, BOB_TOKEN, SERVICE_TOKEN]) {
                let (status, _, _) = raw(&app, Method::DELETE, Some(bearer), Some(id)).await;
                assert_eq!(status, StatusCode::ACCEPTED);
            }

            // Then: nothing is left behind.
            assert_eq!(sessions.owner_count().await, 0);
        }

        #[tokio::test]
        async fn a_record_outliving_its_session_is_reclaimed_at_the_next_initialize() {
            // Given: two sessions ended the way rmcp ends them when nobody is
            // watching — what the idle timeout, an init timeout and a worker error
            // all funnel through, and the one path our `DELETE` hook cannot see.
            let api = MockServer::start().await;
            let (app, sessions) = app_parts(&api.uri(), DEFAULT_MAX_SESSIONS);
            let first = open_session(&app, ALICE_TOKEN).await;
            let second = open_session(&app, BOB_TOKEN).await;
            for id in [&first, &second] {
                sessions
                    .manager()
                    .close_session(&SessionId::from(id.as_str()))
                    .await
                    .expect("rmcp closes a session it holds");
            }
            assert_eq!(
                sessions.owner_count().await,
                2,
                "nothing told us those sessions ended"
            );

            // When: any new session is opened.
            open_session(&app, SERVICE_TOKEN).await;

            // Then: the stale records are gone and only the live one remains, so
            // the table cannot outgrow rmcp's.
            assert_eq!(sessions.owner_count().await, 1);
        }

        #[tokio::test]
        async fn a_stale_record_answers_its_own_owner_as_an_unknown_session() {
            // Given: alice's session, ended out from under her.
            let api = MockServer::start().await;
            let (app, sessions) = app_parts(&api.uri(), DEFAULT_MAX_SESSIONS);
            let alice = open_session(&app, ALICE_TOKEN).await;
            sessions
                .manager()
                .close_session(&SessionId::from(alice.as_str()))
                .await
                .expect("rmcp closes a session it holds");

            // When / Then: she gets what a stranger gets, because the session is
            // genuinely gone — a stale record can only ever refuse an id rmcp would
            // refuse anyway.
            let mine = raw(&app, Method::GET, Some(ALICE_TOKEN), Some(&alice)).await;
            let theirs = raw(&app, Method::GET, Some(BOB_TOKEN), Some(UNKNOWN_SESSION)).await;
            assert_eq!(mine.0, StatusCode::NOT_FOUND);
            assert_eq!(mine.2, theirs.2);
        }

        /// The owner kinds `every_pair_of_distinct_principals_is_refused` covers,
        /// one per `Principal` variant the server can tell apart.
        ///
        /// Exhaustive on purpose, and the reason that test may claim no pair was
        /// left out: the pairs are enumerated by hand from an array, so without
        /// this a new principal kind would widen the gap in silence. Adding one
        /// fails to compile here first, and the fix is to add it to `owners` too.
        const OWNER_KINDS: fn(&Principal) -> &'static str = |principal| match principal {
            Principal::Anyone => "anonymous",
            Principal::SignedIn(Identity::ServiceToken) => "service",
            Principal::SignedIn(Identity::User(_)) => "alice and bob",
        };

        /// Every ordered pair of distinct owners the server can tell apart — two
        /// users, the service token and the anonymous caller — on every method.
        /// The tests above each pin one pair or one property; this one is what
        /// says no pair was left out, and the tripwire below is what keeps that
        /// true as `Principal` grows.
        #[tokio::test]
        async fn every_pair_of_distinct_principals_is_refused() {
            // Given: one session per owner, on a deployment that admits the
            // anonymous caller (D59) beside bearers and the service token.
            let api = MockServer::start().await;
            let app = anonymous_app(&api.uri());
            // One owner per `Principal` kind (see `OWNER_KINDS`), plus a second
            // `User` for the user-to-user pair.
            let owners: [(&str, Option<&str>); 4] = [
                ("alice", Some(ALICE_TOKEN)),
                ("bob", Some(BOB_TOKEN)),
                ("service", Some(SERVICE_TOKEN)),
                ("anonymous", None),
            ];
            // And every kind it names really is in that array — the compile-time
            // half proves the match is exhaustive, this proves the array kept up.
            for kind in [
                Principal::Anyone,
                Principal::SignedIn(Identity::ServiceToken),
                Principal::SignedIn(Identity::User(notedthat_core::UserIdentity {
                    subject: "alice".to_string(),
                    groups: std::collections::BTreeSet::new(),
                })),
            ] {
                let label = OWNER_KINDS(&kind);
                assert!(
                    owners.iter().any(|(name, _)| label.contains(name)),
                    "no owner in the array stands for {label}"
                );
            }
            let mut sessions = Vec::new();
            for (name, bearer) in owners {
                let id = match bearer {
                    Some(token) => open_session(&app, token).await,
                    None => open_anonymous_session(&app).await,
                };
                sessions.push((name, bearer, id));
            }

            // When / Then: every other owner presents it. `POST` and `GET` are
            // refused as an unknown id is; `DELETE` is accepted and does nothing,
            // as rmcp answers a `DELETE` for any id.
            for (opener, _, id) in &sessions {
                for (presenter, bearer, _) in &sessions {
                    if opener == presenter {
                        continue;
                    }
                    for (method, expected) in [
                        (Method::POST, StatusCode::NOT_FOUND),
                        (Method::GET, StatusCode::NOT_FOUND),
                        (Method::DELETE, StatusCode::ACCEPTED),
                    ] {
                        let (status, _, _) = raw(&app, method.clone(), *bearer, Some(id)).await;
                        assert_eq!(
                            status, expected,
                            "{presenter} presenting {opener}'s session on {method}"
                        );
                    }
                }
            }

            // And: every owner still has its session, so none of those `DELETE`s
            // ended one.
            for (opener, bearer, id) in &sessions {
                let (status, _, _) = raw(&app, Method::GET, *bearer, Some(id)).await;
                assert_eq!(status, StatusCode::OK, "{opener} lost its own session");
            }
        }

        async fn open_anonymous_session(app: &Router) -> String {
            let (response, message) = post_anonymous_initialize(app).await;
            assert_eq!(response.status(), StatusCode::OK, "{message:?}");
            response
                .headers()
                .get(SESSION_HEADER)
                .and_then(|v| v.to_str().ok())
                .expect("an anonymous session id")
                .to_owned()
        }

        /// `initialize` with no credential at all — an anonymous caller must not
        /// send an empty bearer and be refused for the wrong reason.
        async fn post_anonymous_initialize(
            app: &Router,
        ) -> (axum::response::Response, Option<serde_json::Value>) {
            let request = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "127.0.0.1")
                .header("accept", "application/json, text/event-stream")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "jsonrpc": "2.0", "id": 0, "method": "initialize",
                        "params": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "clientInfo": { "name": "test", "version": "0" }
                        },
                    })
                    .to_string(),
                ))
                .expect("request");
            let response = app.clone().oneshot(request).await.expect("response");
            let (parts, body) = response.into_parts();
            let bytes = axum::body::to_bytes(body, 64 * 1024).await.expect("body");
            let mut parser = crate::sse::SseParser::new();
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
    }
}

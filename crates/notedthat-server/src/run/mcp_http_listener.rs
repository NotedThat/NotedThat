use super::mcp_http::{build_router, internal_http_api_url, tool_call_client};
use crate::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
use anyhow::Context as _;
use axum::{Router, routing::get};
use notedthat_core::{KbSlug, TenantSlug};
use notedthat_storage_s3::S3Config;
use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
    sync::Notify,
};
use tokio_util::sync::CancellationToken;

const SHUTDOWN_BOUND: Duration = Duration::from_secs(15);

fn test_config() -> Config {
    let mut kbs = BTreeMap::new();
    kbs.insert(
        "notes".to_string(),
        KbSlug::try_new("notes").expect("test KB slug is valid"),
    );
    Config {
        api_token: "test-token".to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr: "127.0.0.1:0".parse().expect("test HTTP addr is valid"),
        metrics_listen_addr: None,
        storage: crate::config::StorageConfig::S3(S3Config {
            endpoint_url: Some("http://127.0.0.1:8333".to_string()),
            region: "us-east-1".to_string(),
            access_key_id: "any".to_string(),
            secret_access_key: "any".to_string(),
            force_path_style: true,
            reconcile_on_startup: true,
            allow_unenforced_conditional_writes: false,
        }),
        events: crate::config::EventsConfig::None,
        log_format: LogFormat::Pretty,
        qdrant: ServerQdrantConfig {
            url: "http://127.0.0.1:6334".to_string(),
            api_key: None,
            timeout_ms: 30_000,
            connect_timeout_ms: 10_000,
        },
        embedder: EmbedderConfig {
            endpoint_url: "http://127.0.0.1:9999".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dimensions: 3,
            batch_size: 32,
            timeout_ms: 30_000,
            max_retries: 3,
            max_input_tokens: 8192,
        },
        webdav_username: "webdav-user".to_string(),
        webdav_password: "webdav-pass".to_string(),
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "::1".to_string(),
        ],
        mcp_anonymous: crate::config::McpAnonymous::Auto,
        max_patchable_size: 100 * 1024 * 1024,
        mcp_max_read_bytes: 16 * 1024 * 1024,
        mcp_max_sessions: notedthat_mcp::DEFAULT_MAX_SESSIONS,
        request_bounds: crate::config::RequestBoundsConfig::default(),
        ready_probe_interval_ms: 5_000,
        staging: notedthat_core::StagingConfig::default(),
        oidc: None,
    }
}

#[tokio::test]
async fn invalid_staging_directory_fails_before_infrastructure_setup() {
    // Given: a config that is broken in TWO ways at once — a missing staging
    // directory and a Qdrant URL that cannot be parsed. Only the second one is
    // reachable by `backends_from_config`, so whichever error comes back names
    // the stage that ran first. G11 wants the staging error: it is the one an
    // operator can act on without reading the source.
    let missing_directory = std::env::temp_dir().join(format!(
        "notedthat-staging-config-missing-{}",
        std::process::id()
    ));
    let mut config = test_config();
    config.staging = notedthat_core::StagingConfig::new(missing_directory);
    config.qdrant.url = String::new();

    // Guard the premise: the Qdrant URL really is bad enough to fail on its own.
    assert!(
        super::backends_from_config(&config, None, None).is_err(),
        "test premise: an empty Qdrant URL must fail backend construction, \
         otherwise this test cannot distinguish the two orderings"
    );

    // When: the server is started.
    let Err(error) = super::run(config).await else {
        panic!("missing staging directory must fail startup");
    };

    // Then: staging was checked first.
    assert!(
        error
            .to_string()
            .contains("failed to validate NOTEDTHAT_UPLOAD_TMP_DIR"),
        "staging must be validated before backends are built, got: {error:#}"
    );
}

#[test]
fn internal_http_api_url_uses_actual_bound_socket() {
    let wildcard_v4 = SocketAddr::from(([0, 0, 0, 0], 49_123));
    let wildcard_v6 = SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 49_124));
    let concrete = SocketAddr::from(([192, 0, 2, 10], 49_125));

    assert_eq!(internal_http_api_url(wildcard_v4), "http://127.0.0.1:49123");
    assert_eq!(internal_http_api_url(wildcard_v6), "http://[::1]:49124");
    assert_eq!(internal_http_api_url(concrete), "http://192.0.2.10:49125");
}

/// The tool-call client's deadline follows `NOTEDTHAT_REQUEST_TIMEOUT_MS`.
///
/// The E2E case (`a_stalled_tool_call_is_answered_by_the_servers_504`) cannot
/// see this: it turns the server timeout *down*, and a client left on its
/// built-in default would outlast that one too and still pass. What it cannot
/// survive is the timeout being raised — an operator giving a slow embedder
/// 90 s would find tool calls still cut off at the fixed default — so the
/// check is that two configurations derive two different deadlines, each
/// outlasting its own.
#[test]
fn the_tool_call_clients_deadline_follows_the_configured_request_timeout() {
    let derived = |request_timeout| {
        let mut config = test_config();
        config.request_bounds.request_timeout = request_timeout;
        tool_call_client(&config, "http://127.0.0.1:49126")
            .expect("the tool-call client builds")
            .api_timeout()
    };

    let long = Duration::from_secs(90);
    let short = Duration::from_secs(5);
    assert!(
        derived(long) > long,
        "a 90 s request timeout must leave the client waiting past the server's 504"
    );
    assert!(derived(short) > short);
    assert!(
        derived(long) > derived(short),
        "the deadline is fixed, not derived from the configured request timeout"
    );
}

/// `initialize` a session and hand back the `Mcp-Session-Id` header the
/// stateful transport issues (`None` while the transport is stateless), so
/// the hand-written request below can carry it.
async fn initialize_mcp(addr: SocketAddr) -> Option<String> {
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .bearer_auth("test-token")
        .header("accept", "application/json, text/event-stream")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "listener-test", "version": "1" }
            }
        }))
        .send()
        .await
        .expect("MCP initialize request should succeed");
    assert!(response.status().is_success());
    response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn one_listener_closes_active_mcp_tool_call_during_shutdown() {
    let config = test_config();
    let api_started = Arc::new(Notify::new());
    let backend_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("controlled API backend should bind");
    let backend_addr = backend_listener
        .local_addr()
        .expect("controlled API backend exposes its address");
    let backend_started = Arc::clone(&api_started);
    let backend = tokio::spawn(async move {
        axum::serve(
            backend_listener,
            Router::new().route(
                "/api/v1/knowledgebases",
                get(move || {
                    let request_started = Arc::clone(&backend_started);
                    async move {
                        request_started.notify_one();
                        std::future::pending::<axum::Json<serde_json::Value>>().await
                    }
                }),
            ),
        )
        .await
        .context("controlled API backend failed")
    });
    let listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .expect("unified listener should bind");
    let addr = listener
        .local_addr()
        .expect("unified listener exposes its address");
    let shutdown = CancellationToken::new();
    let graceful_shutdown = shutdown.clone();
    let mcp_shutdown = shutdown.child_token();
    let app = Router::new()
        .route("/api/v1/probe", get(|| async { "api" }))
        .route("/webdav", get(|| async { "webdav" }))
        .merge(
            build_router(
                &config,
                std::sync::Arc::new(notedthat_core::Authenticator::new(config.api_token.clone())),
                &std::collections::BTreeMap::new(),
                &internal_http_api_url(backend_addr),
                mcp_shutdown,
                false,
                &notedthat_api_http::bounds::RequestBounds::unbounded(),
            )
            .expect("MCP router should build"),
        );
    // The accept loop the product listener actually runs (D71), not
    // `axum::serve` — otherwise this test stopped covering the shutdown
    // sequence the moment `run.rs` moved off it, and `serve.rs`'s graceful
    // drain would have no test at all.
    let server = tokio::spawn(async move {
        crate::run::serve::serve(listener, app, Duration::from_secs(30), graceful_shutdown)
            .await
            .context("unified listener failed")
    });

    let client = reqwest::Client::new();
    for (path, expected) in [("/api/v1/probe", "api"), ("/webdav", "webdav")] {
        let response = client
            .get(format!("http://{addr}{path}"))
            .send()
            .await
            .expect("surface request should succeed");
        assert_eq!(response.text().await.expect("response has body"), expected);
    }
    let session = initialize_mcp(addr)
        .await
        .map_or(String::new(), |id| format!("Mcp-Session-Id: {id}\r\n"));

    let mut mcp_connection = TcpStream::connect(addr)
        .await
        .expect("active MCP connection should connect");
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "list_knowledgebases", "arguments": {} }
    })
    .to_string();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer test-token\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\n{session}Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
        body.len()
    );
    mcp_connection
        .write_all(request.as_bytes())
        .await
        .expect("MCP tool request should be written");
    tokio::time::timeout(SHUTDOWN_BOUND, api_started.notified())
        .await
        .expect("MCP tool call should reach the blocked API backend");

    shutdown.cancel();
    tokio::time::timeout(SHUTDOWN_BOUND, server)
        .await
        .expect("unified listener should quiesce within 15 seconds")
        .expect("server task should join")
        .expect("unified listener should stop cleanly");
    let mut response = [0_u8; 1024];
    tokio::time::timeout(SHUTDOWN_BOUND, async {
        loop {
            let bytes_read = mcp_connection
                .read(&mut response)
                .await
                .expect("MCP connection read should resolve");
            if bytes_read == 0 {
                break;
            }
        }
    })
    .await
    .expect("active MCP connection should reach EOF within 15 seconds");
    backend.abort();
}

/// Only the legs that answer once draw a permit: `GET /mcp` is the session's
/// notification stream and is meant to stay open for hours (D71).
#[tokio::test]
async fn only_the_mcp_request_legs_are_bounded() {
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request};
    use tower::ServiceExt as _;

    let config = test_config();
    // No permits at all, so every bounded leg is refused before it runs.
    let bounds = notedthat_api_http::bounds::RequestBounds::new(
        Duration::from_secs(60),
        Duration::from_secs(60),
        Arc::new(tokio::sync::Semaphore::new(0)),
    );
    let app = build_router(
        &config,
        Arc::new(notedthat_core::Authenticator::new(config.api_token.clone())),
        &BTreeMap::new(),
        "http://127.0.0.1:1",
        CancellationToken::new(),
        false,
        &bounds,
    )
    .expect("MCP router should build");

    let refused_by_the_cap = |method: Method, path: &'static str| {
        let app = app.clone();
        async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("host", "127.0.0.1")
                        .header("authorization", "Bearer test-token")
                        .header("accept", "application/json, text/event-stream")
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .expect("test request is valid"),
                )
                .await
                .expect("router is infallible");
            let body = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            String::from_utf8_lossy(&body).contains("limit of requests in flight")
        }
    };

    assert!(refused_by_the_cap(Method::POST, "/mcp").await);
    assert!(refused_by_the_cap(Method::DELETE, "/mcp").await);
    assert!(refused_by_the_cap(Method::GET, "/sse").await);
    assert!(!refused_by_the_cap(Method::GET, "/mcp").await);
}

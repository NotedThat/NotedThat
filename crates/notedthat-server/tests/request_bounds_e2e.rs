//! E2E: the product listener's request bounds (D70) — a real server on
//! in-process backends, with its timeouts turned down far enough to cross.
//! No Docker, and not `#[ignore]`d.
//!
//! ```sh
//! cargo test -p notedthat-server --locked --test request_bounds_e2e
//! ```
#![allow(missing_docs)]

#[path = "support/patch_env.rs"]
mod patch_env;
#[path = "support/sse.rs"]
mod sse;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use notedthat_events::MemoryPublisher;
use notedthat_indexer::testing::StubEmbedder;
use notedthat_indexer::{Embedder, EmbedderError};
use notedthat_mcp::testing::McpSession;
use patch_env::{API_TOKEN, PatchServer, in_memory_backends};
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use sse::{Subscription, change_events};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::Notify;

const MAX_PATCHABLE: u64 = 10 * 1024 * 1024;
const WAIT: Duration = Duration::from_secs(5);
/// Longer than any header-read timeout a test here sets.
const HEAD_WAIT: Duration = Duration::from_secs(10);

/// An embedder whose query call never answers, and says when one started.
///
/// Stands in for the case the bound exists for: a search stuck on its
/// embedding call while the caller waits.
struct StalledEmbedder {
    inner: StubEmbedder,
    started: Arc<Notify>,
}

#[async_trait]
impl Embedder for StalledEmbedder {
    async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedderError> {
        self.started.notify_one();
        std::future::pending().await
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    fn max_input_tokens(&self) -> usize {
        self.inner.max_input_tokens()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
}

fn bearer(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request.header("Authorization", format!("Bearer {API_TOKEN}"))
}

/// Open a connection, send half a request head, and wait for the server to
/// close it. Panics if it has not within [`HEAD_WAIT`].
async fn send_half_a_head(base_url: &str) {
    let addr = base_url.strip_prefix("http://").expect("an http base URL");
    let mut slow = tokio::net::TcpStream::connect(addr).await.expect("connect");
    slow.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n")
        .await
        .expect("a partial head");
    // There is no request to answer yet, so the server answers nothing: the
    // read ends at the close, cleanly or with a reset.
    let mut buffer = Vec::new();
    let read = tokio::time::timeout(HEAD_WAIT, slow.read_to_end(&mut buffer))
        .await
        .expect("the server should close a connection that never finishes its head");
    if read.is_ok() {
        assert!(buffer.is_empty(), "{}", String::from_utf8_lossy(&buffer));
    }
}

/// `GET /api/v1/knowledgebases`: the cheapest bounded route there is.
async fn list_kbs(server: &PatchServer) -> reqwest::Response {
    bearer(
        server
            .client
            .get(format!("{}/api/v1/knowledgebases", server.base_url)),
    )
    .send()
    .await
    .expect("list")
}

fn basic_auth() -> String {
    let encoded =
        base64::engine::general_purpose::STANDARD.encode("e2e-webdav-user:e2e-webdav-pass");
    format!("Basic {encoded}")
}

/// Neither stream may be cut by the request timeout: both must outlive it
/// several times over and still deliver.
#[tokio::test]
async fn the_events_stream_and_the_mcp_notification_leg_outlive_the_timeout() {
    // Given: a request timeout of a third of a second.
    let timeout = Duration::from_millis(300);
    let backends = notedthat_server::run::Backends {
        events: Some(Arc::new(MemoryPublisher::new(256))),
        ..in_memory_backends()
    };
    let server = PatchServer::start_with_config(MAX_PATCHABLE, backends, |config| {
        config.request_bounds.request_timeout = timeout;
    })
    .await;
    server.put_text("held/a.md", "first").await;

    // And an open events subscription and an MCP notification leg,
    // subscribed to the same key.
    let response = bearer(server.client.get(format!(
        "{}/api/v1/knowledgebases/{}/events",
        server.base_url, server.kb
    )))
    .header("Accept", "text/event-stream")
    .send()
    .await
    .expect("subscribe");
    assert_eq!(response.status(), StatusCode::OK);
    let mut events = Subscription::open(response);
    let mcp = McpSession::connect(&server.base_url, API_TOKEN);
    let mut notifications = mcp.notifications().await;
    let uri = format!("notedthat://{}/held/a.md", server.kb);
    let answer = mcp
        .request(1, "resources/subscribe", &serde_json::json!({ "uri": uri }))
        .await;
    assert!(answer.get("result").is_some(), "{answer}");

    // When: both are held open for well past the timeout, and then the key
    // is written.
    tokio::time::sleep(timeout * 4).await;
    server.put_text("held/a.md", "second").await;

    // Then: both streams are still there to deliver it.
    let delivered = events.events_where(1, WAIT, change_events).await;
    assert_eq!(delivered[0].key(), "held/a.md");
    let notified = notifications.next(WAIT).await;
    assert_eq!(notified["method"], "notifications/resources/updated");
    assert_eq!(notified["params"]["uri"], uri, "{notified}");
}

/// One stuck search holds the only permit until its timeout answers it; while
/// it does, every other bounded surface is refused — and the probes and both
/// streams, which are routed outside the bound, are not.
#[tokio::test]
async fn a_stuck_search_holds_the_cap_until_its_timeout_answers_504() {
    // Given: one permit, a timeout long enough for every check below to run
    // inside it on a slow runner, and an embedder that never answers — plus
    // the metrics listener, to see the refusals counted.
    let started = Arc::new(Notify::new());
    let backends = notedthat_server::run::Backends {
        embedder: Arc::new(StalledEmbedder {
            inner: StubEmbedder::new(4),
            started: Arc::clone(&started),
        }),
        events: Some(Arc::new(MemoryPublisher::new(256))),
        ..in_memory_backends()
    };
    let metrics_addr = notedthat_api_http::testing::reserve_addr();
    let server = PatchServer::start_with_config(MAX_PATCHABLE, backends, |config| {
        config.request_bounds.request_timeout = Duration::from_secs(3);
        config.request_bounds.max_requests_in_flight = 1;
        // Longer than any pause below, so no pooled connection idles out
        // mid-test; short enough for the half-sent head to cross it.
        config.request_bounds.header_read_timeout = Duration::from_secs(5);
        config.metrics_listen_addr = Some(metrics_addr);
    })
    .await;

    // An MCP session opened beforehand: `initialize` is a POST, which the
    // full listener would refuse like any other.
    let mcp = McpSession::connect(&server.base_url, API_TOKEN);
    let listed = mcp.request(1, "tools/list", &serde_json::json!({})).await;
    assert!(listed.get("result").is_some(), "{listed}");

    // When: a search is stuck on its embedding call.
    let search = {
        let request = bearer(server.client.post(format!(
            "{}/api/v1/knowledgebases/{}/search",
            server.base_url, server.kb
        )))
        .json(&serde_json::json!({ "query": "anything" }));
        tokio::spawn(request.send())
    };
    tokio::time::timeout(WAIT, started.notified())
        .await
        .expect("the search should reach the embedder");

    // Then: the API is refused in the capacity shape …
    let refused = list_kbs(&server).await;
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        refused
            .headers()
            .get(RETRY_AFTER)
            .map(reqwest::header::HeaderValue::as_bytes),
        Some(&b"5"[..])
    );
    let body: serde_json::Value = refused.json().await.expect("JSON refusal");
    assert_eq!(body["error"], "backend_unavailable", "{body}");
    assert!(
        body["request_id"].as_str().is_some_and(|id| !id.is_empty()),
        "{body}"
    );

    // … and so is WebDAV, which shares the listener's permits …
    let propfind = server
        .client
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").expect("PROPFIND is a method"),
            format!("{}/webdav/{}/", server.base_url, server.kb),
        )
        .header("Authorization", basic_auth())
        .header("Depth", "1")
        .send()
        .await
        .expect("PROPFIND");
    assert_eq!(propfind.status(), StatusCode::SERVICE_UNAVAILABLE);

    // … while both streams still open: a full listener does not stop anyone
    // from listening …
    let events = bearer(server.client.get(format!(
        "{}/api/v1/knowledgebases/{}/events",
        server.base_url, server.kb
    )))
    .header("Accept", "text/event-stream")
    .send()
    .await
    .expect("subscribe");
    assert_eq!(events.status(), StatusCode::OK);
    let _notifications = mcp.notifications().await;

    // … and the probes still answer an orchestrator.
    for probe in ["healthz", "readyz"] {
        let response = server
            .client
            .get(format!("{}/{probe}", server.base_url))
            .send()
            .await
            .expect("probe");
        assert_eq!(response.status(), StatusCode::OK, "/{probe}");
    }

    // And the search itself is answered 504 once its timeout passes.
    let timed_out = search
        .await
        .expect("join")
        .expect("the search should be answered, not dropped");
    assert_eq!(timed_out.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(timed_out.headers().get(RETRY_AFTER).is_none());
    let body: serde_json::Value = timed_out.json().await.expect("JSON refusal");
    assert_eq!(body["error"], "request_timeout", "{body}");

    // Which gave its permit back.
    let listed = list_kbs(&server).await;
    assert_eq!(listed.status(), StatusCode::OK);

    // A connection that never finishes its head is closed, and counted.
    send_half_a_head(&server.base_url).await;

    assert_refusals_counted(metrics_addr).await;
}

/// Every refusal the stuck-search test provoked is in the exposition, by
/// route and reason, and so is the connection closed for its half-sent head.
async fn assert_refusals_counted(metrics_addr: std::net::SocketAddr) {
    let exposition = reqwest::get(format!("http://{metrics_addr}/metrics"))
        .await
        .expect("scrape")
        .text()
        .await
        .expect("exposition");
    for expected in [
        r#"notedthat_http_requests_refused_total{surface="api",route="/api/v1/knowledgebases",reason="in_flight"} 1"#,
        r#"notedthat_http_requests_refused_total{surface="webdav",route="/webdav/{*path}",reason="in_flight"} 1"#,
        r#"notedthat_http_requests_refused_total{surface="api",route="/api/v1/knowledgebases/{kb_slug}/search",reason="timeout"} 1"#,
    ] {
        assert!(
            exposition.lines().any(|line| line == expected),
            "missing {expected} in:\n{exposition}"
        );
    }
    // Another test in this binary may have closed one too: the recorder is
    // the process's.
    let closed: f64 = exposition
        .lines()
        .find_map(|line| line.strip_prefix("notedthat_http_header_read_timeouts_total "))
        .expect("the header-read timeout counter")
        .parse()
        .expect("a count");
    assert!(closed >= 1.0, "{closed}");
}

/// A client that opens a connection and never finishes its request head is
/// closed, instead of holding a task for as long as it likes.
#[tokio::test]
async fn a_connection_that_never_finishes_its_request_head_is_closed() {
    let server = PatchServer::start_with_config(MAX_PATCHABLE, in_memory_backends(), |config| {
        config.request_bounds.header_read_timeout = Duration::from_millis(500);
    })
    .await;

    send_half_a_head(&server.base_url).await;

    // While a client that sends its head promptly is served as ever.
    let response = server
        .client
        .get(format!("{}/healthz", server.base_url))
        .send()
        .await
        .expect("healthz");
    assert_eq!(response.status(), StatusCode::OK);
}

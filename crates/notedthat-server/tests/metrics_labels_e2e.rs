//! E2E: a scraped exposition names no object key, no principal, no credential
//! and no subscriber's filter — asserted against sentinels this test put into
//! the running deployment, not against a pattern (D69).
//!
//! Every sentinel below is unique to this file. If one appears in the rendered
//! text, the only thing that could have put it there is a label. The test runs
//! in a binary of its own so the registry it scrapes holds its own series and
//! nothing else's.
//!
//! ```sh
//! cargo test -p notedthat-server --locked --test metrics_labels_e2e
//! ```

#[path = "support/metrics_env.rs"]
mod metrics_env;

use async_trait::async_trait;
use metrics_env::{LEAK_PASSWORD, LEAK_PRINCIPAL, LEAK_TOKEN, MetricsServer, series_lines};
use notedthat_indexer::embedder::{Embedder, EmbedderError};
use notedthat_indexer::testing::StubEmbedder;
use reqwest::StatusCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

/// A key no other test uses, written and left in place.
const CANARY_KEY: &str = "leak/canary-object-3f9c1d.md";
/// A key written and then deleted, so a tombstone passes through the indexer.
const CANARY_TOMBSTONE: &str = "leak/canary-tombstone-3f9c1d.md";
/// A key whose indexing is made to fail.
const CANARY_FAILED: &str = "leak/canary-failed-3f9c1d.md";
/// The search text, which is also the canary object's content, so it is a hit.
const CANARY_QUERY: &str = "canaryquery3f9c1d";
/// A subscriber's filter, the closest thing a subscriber has to an identity.
const CANARY_PREFIX: &str = "leak/canary-prefix-3f9c1d/";

/// Everything that must never appear in an exposition, with what it is, so a
/// failure says which class of identifier leaked.
fn sentinels() -> Vec<(&'static str, String)> {
    vec![
        ("an object key", CANARY_KEY.to_string()),
        ("a deleted object's key", CANARY_TOMBSTONE.to_string()),
        ("a failed object's key", CANARY_FAILED.to_string()),
        ("a search query", CANARY_QUERY.to_string()),
        ("the service token", LEAK_TOKEN.to_string()),
        ("a principal", LEAK_PRINCIPAL.to_string()),
        ("a credential", LEAK_PASSWORD.to_string()),
        ("a subscriber's filter", CANARY_PREFIX.to_string()),
    ]
}

/// An embedder the test drives: it can be made to fail the next call, and to
/// stall every call until released — which is how the indexing queue is filled
/// without putting a knob on the queue for a test's sake.
struct ScriptedEmbedder {
    inner: StubEmbedder,
    fail_next: AtomicBool,
    blocked: AtomicBool,
    release: Notify,
}

impl ScriptedEmbedder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: StubEmbedder::new(4),
            fail_next: AtomicBool::new(false),
            blocked: AtomicBool::new(false),
            release: Notify::new(),
        })
    }

    fn fail_next(&self) {
        self.fail_next.store(true, Ordering::SeqCst);
    }

    fn block(&self) {
        self.blocked.store(true, Ordering::SeqCst);
    }

    fn unblock(&self) {
        self.blocked.store(false, Ordering::SeqCst);
        self.release.notify_waiters();
    }
}

#[async_trait]
impl Embedder for ScriptedEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedderError> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(EmbedderError::Transport("connection refused".into()));
        }
        while self.blocked.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        self.inner.embed(texts).await
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

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn the_exposition_names_no_key_principal_credential_or_subscriber_filter() {
    let embedder = ScriptedEmbedder::new();
    let publisher = Arc::new(notedthat_events::MemoryPublisher::new(4096));
    let backends = notedthat_server::run::Backends {
        embedder: embedder.clone(),
        events: Some(publisher),
        ..metrics_env::in_memory_backends()
    };
    let server = MetricsServer::start_with(backends, true).await;

    // Writes, including one under the subscriber's prefix and one deleted.
    let created = server
        .put_text(CANARY_KEY, &format!("# canary\n\n{CANARY_QUERY}\n"))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    server.put_text(CANARY_TOMBSTONE, "# tombstone").await;
    assert_eq!(
        server.delete(CANARY_TOMBSTONE).await.status(),
        StatusCode::NO_CONTENT
    );

    // A WebDAV request, so a Basic principal reaches the server.
    let propfind = server
        .client
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").expect("PROPFIND is a method"),
            format!("{}/webdav/{}/", server.base_url, server.kb),
        )
        .basic_auth(LEAK_PRINCIPAL, Some(LEAK_PASSWORD))
        .header("Depth", "1")
        .send()
        .await
        .expect("PROPFIND should answer");
    assert!(
        propfind.status().is_success() || propfind.status().is_client_error(),
        "the WebDAV surface answered: {}",
        propfind.status()
    );

    // An SSE subscriber, carrying a prefix filter — the closest thing a
    // subscriber has to an identity.
    let events_url = format!(
        "{}/api/v1/knowledgebases/{}/events?prefix={CANARY_PREFIX}",
        server.base_url, server.kb
    );
    let subscription = server
        .client
        .get(&events_url)
        .header("Authorization", format!("Bearer {LEAK_TOKEN}"))
        .send()
        .await
        .expect("the events route should answer");
    assert_eq!(subscription.status(), StatusCode::OK);

    // A search, which must actually return the canary rather than nothing.
    wait_for_indexed(&server).await;
    let hits: serde_json::Value = server
        .search(CANARY_QUERY)
        .await
        .json()
        .await
        .expect("search answers JSON");
    assert!(
        hits["hits"].as_array().is_some_and(|h| !h.is_empty()),
        "the canary should be findable, or the search path was never exercised: {hits}"
    );

    // An indexing failure, deterministically.
    embedder.fail_next();
    server.put_text(CANARY_FAILED, "# fails to index").await;
    wait_for_failure(&server).await;

    // A queue-full refusal. The worker handles one event at a time, so a
    // blocked embedder stalls the drain completely; the capacity is read from
    // the server rather than hard-coded, so this keeps working if it changes.
    let capacity = server.index_health().await["queue"]["capacity"]
        .as_u64()
        .expect("the index view reports the queue's capacity");
    embedder.block();
    let mut refused = false;
    for i in 0..capacity + 64 {
        let response = server.put_text(&format!("flood/{i}.md"), "x").await;
        if response.status() == StatusCode::SERVICE_UNAVAILABLE {
            refused = true;
            break;
        }
    }
    assert!(
        refused,
        "the bounded index queue should refuse a write once full"
    );
    // Released before the server drops, so the drain is never what ends the test.
    embedder.unblock();
    drop(subscription);

    let body = server.scrape().await;

    // The run above exercised writes, a deletion, a search, an index failure, a
    // subscriber and a queue-full refusal, so every family the catalogue
    // promises has had something to record. A family missing here is an
    // instrumentation site that was never reached, which no absence check
    // would notice.
    for metric in [
        "notedthat_http_requests_total",
        "notedthat_http_request_duration_seconds",
        "notedthat_http_requests_in_flight",
        "notedthat_search_requests_total",
        "notedthat_search_duration_seconds",
        "notedthat_search_hits",
        "notedthat_embedding_requests_total",
        "notedthat_embedding_duration_seconds",
        "notedthat_embedding_errors_total",
        "notedthat_index_queue_depth",
        "notedthat_index_queue_capacity",
        "notedthat_index_events_enqueued_total",
        "notedthat_index_events_refused_total",
        "notedthat_index_events_completed_total",
        "notedthat_index_worker_alive",
        "notedthat_vector_store_operations_total",
        "notedthat_vector_store_operation_duration_seconds",
        "notedthat_events_published_total",
        "notedthat_events_subscribers",
        "notedthat_storage_operations_total",
        "notedthat_storage_operation_duration_seconds",
        "notedthat_build_info",
    ] {
        assert!(
            body.contains(metric),
            "{metric} is in the catalogue but nothing recorded it:\n{body}"
        );
    }

    // Positive first: an empty exposition would satisfy every absence below.
    assert!(
        body.contains(&format!("kb=\"{}\"", server.kb)),
        "the kb label must be present, or this test proves nothing:\n{body}"
    );
    let series = series_lines(&body);
    assert!(
        series.len() > 20,
        "expected a populated exposition, got {}",
        series.len()
    );

    for (what, value) in sentinels() {
        assert!(
            !body.contains(&value),
            "{what} leaked into the exposition as {value:?}"
        );
    }

    // Sentinels catch the leaks we thought of. This catches a class of the ones
    // we did not: a request id, an encoded path or any other unbounded value
    // shows up as an implausibly long label or as a series explosion.
    for line in &series {
        for value in label_values(line) {
            assert!(
                value.len() <= 64,
                "a label value of {} characters is not from a closed set: {line}",
                value.len()
            );
        }
    }
    assert!(
        series.len() < 2000,
        "{} series suggests an unbounded label",
        series.len()
    );
}

/// The label values in one exposition line.
fn label_values(line: &str) -> Vec<&str> {
    let Some(start) = line.find('{') else {
        return Vec::new();
    };
    let Some(end) = line[start..].find('}') else {
        return Vec::new();
    };
    line[start + 1..start + end]
        .split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(_, value)| value.trim().trim_matches('"'))
        .collect()
}

async fn wait_for_indexed(server: &MetricsServer) {
    for _ in 0..200 {
        if server.index_health().await["pending"] == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the indexer did not drain");
}

async fn wait_for_failure(server: &MetricsServer) {
    for _ in 0..200 {
        let health = server.index_health().await;
        if health["last_failure"].is_object() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the indexing failure was never recorded");
}

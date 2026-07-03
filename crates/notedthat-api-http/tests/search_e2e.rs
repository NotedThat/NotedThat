//! End-to-end integration tests for the search API.
//!
//! Uses a Qdrant testcontainer, an in-process wiremock embedder, and the shared
//! [`InMemoryStorage`] so the HTTP router and the [`IndexerWorker`] operate on
//! the same backing store without requiring a real S3 / `SeaweedFS` instance.
//!
//! Run with:
//!   cargo test -p notedthat-api-http --test `search_e2e` -- --ignored --nocapture
#![allow(missing_docs)]

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    response::Response,
};
use notedthat_api_http::{
    router::build_router,
    state::AppState,
    testing::{InMemoryStorage, NoopSearcher},
};
use notedthat_core::{KbSlug, Storage};
use notedthat_indexer::{
    Embedder, IndexerWorker, OpenAiCompatibleConfig, OpenAiCompatibleEmbedder, QdrantClient,
    QdrantConfig, QdrantProvisioner, Searcher,
};
use qdrant_client::qdrant::{Condition, Filter, ScrollPoints};
use testcontainers::{
    GenericImage,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio_util::sync::CancellationToken;
use tower::util::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

// ─── Constants ──────────────────────────────────────────────────────────────

const TOKEN: &str = "e2e-test-token";
/// KB declared in the simple (no-Qdrant) router used for HTTP-level error tests.
const KB: &str = "notes";

// ─── Test serialization ──────────────────────────────────────────────────────

/// Serialize all Qdrant-backed tests to avoid testcontainer resource exhaustion
/// (same pattern as `notedthat-indexer` `worker_integration` tests).
static E2E_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn e2e_guard() -> tokio::sync::MutexGuard<'static, ()> {
    E2E_MUTEX.lock().await
}

// ─── Qdrant container helper ─────────────────────────────────────────────────

async fn start_qdrant() -> (impl std::any::Any, String) {
    let container = GenericImage::new("qdrant/qdrant", "v1.15.4")
        .with_exposed_port(6334_u16.tcp())
        .with_wait_for(WaitFor::seconds(5))
        .start()
        .await
        .expect("failed to start qdrant/qdrant:v1.15.4 — is Docker running?");
    let port = container
        .get_host_port_ipv4(6334_u16)
        .await
        .expect("failed to get Qdrant gRPC port");
    (container, format!("http://127.0.0.1:{port}"))
}

// ─── Wiremock embedder helper ────────────────────────────────────────────────

/// Build an OpenAI-compatible embeddings response with `count` identical 4-dim unit vectors.
fn embedding_response(dim: usize, count: usize) -> serde_json::Value {
    let data: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            let v: Vec<f32> = (0..dim)
                .map(|j| if j == i % dim { 1.0_f32 } else { 0.0_f32 })
                .collect();
            serde_json::json!({"index": i, "embedding": v, "object": "embedding"})
        })
        .collect();
    serde_json::json!({"object": "list", "data": data})
}

// ─── Qdrant polling helpers ──────────────────────────────────────────────────

/// Poll Qdrant until at least `expected_count` points exist in `collection`.
///
/// Returns `Err` when `timeout` elapses without the expected number of points.
/// Uses a 500 ms polling interval; never sleeps without bound.
async fn wait_for_index(
    qdrant: &qdrant_client::Qdrant,
    collection: &str,
    expected_count: usize,
    timeout: Duration,
) -> Result<(), String> {
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() > timeout {
            return Err(format!(
                "timed out after {timeout:?} waiting for {expected_count} points in {collection}"
            ));
        }
        match qdrant
            .scroll(ScrollPoints {
                collection_name: collection.to_string(),
                limit: Some(1000),
                with_payload: Some(false.into()),
                with_vectors: Some(false.into()),
                ..Default::default()
            })
            .await
        {
            Ok(r) if r.result.len() >= expected_count => return Ok(()),
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Poll Qdrant until all points for `object_key` have been removed from `collection`.
///
/// Returns `Err` when `timeout` elapses without the key disappearing.
async fn wait_for_tombstone(
    qdrant: &qdrant_client::Qdrant,
    collection: &str,
    object_key: &str,
    timeout: Duration,
) -> Result<(), String> {
    let start = std::time::Instant::now();
    let filter = Filter::must([Condition::matches("object_key", object_key.to_string())]);
    loop {
        if start.elapsed() > timeout {
            return Err(format!(
                "timed out after {timeout:?} waiting for tombstone of '{object_key}' in {collection}"
            ));
        }
        match qdrant
            .scroll(ScrollPoints {
                collection_name: collection.to_string(),
                filter: Some(filter.clone()),
                limit: Some(1000),
                with_payload: Some(false.into()),
                with_vectors: Some(false.into()),
                ..Default::default()
            })
            .await
        {
            Ok(r) if r.result.is_empty() => return Ok(()),
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ─── Full E2E environment ────────────────────────────────────────────────────

/// All state needed for a full E2E test with real Qdrant + wiremock embedder.
///
/// The Qdrant container is returned separately (see `setup_full_e2e`) so that
/// it is the *last* thing dropped, ensuring the worker has fully stopped before
/// the container is torn down.
struct FullE2eEnv {
    router: axum::Router,
    qdrant_raw: qdrant_client::Qdrant,
    collection: String,
    shutdown: CancellationToken,
    worker_handle: tokio::task::JoinHandle<()>,
    /// Kept alive so the wiremock server answers embedding requests.
    _mock_server: MockServer,
}

impl FullE2eEnv {
    /// Cancel the indexer worker, drain pending events, then drop all shared state.
    ///
    /// Call this at the end of every full-E2E test so the worker exits cleanly
    /// before the Qdrant container (returned from `setup_full_e2e`) is dropped.
    async fn join(self) {
        self.shutdown.cancel();
        // Give the drain loop up to 10 s to flush remaining events.
        let _ = tokio::time::timeout(Duration::from_secs(10), self.worker_handle).await;
        // `self._mock_server` drops here, then the environment is fully torn down.
    }
}

/// Construct a full E2E environment for `kb`.
///
/// Returns `(container, env)`.  The **caller must keep `container` alive**
/// until after `env.join().await` has returned; only then should `container`
/// be dropped (which stops the Qdrant testcontainer).
async fn setup_full_e2e(kb: &str) -> (impl std::any::Any, FullE2eEnv) {
    let (container, qdrant_url) = start_qdrant().await;

    let kb_slug = KbSlug::try_new(kb).expect("valid kb slug for e2e test");
    let collection = format!("kb_{kb}_v1");

    let qdrant_cfg = QdrantConfig {
        url: qdrant_url.clone(),
        api_key: None,
    };
    let qdrant_client = Arc::new(QdrantClient::new(&qdrant_cfg).expect("qdrant client creation"));
    let provisioner =
        QdrantProvisioner::new(QdrantClient::new(&qdrant_cfg).expect("provisioner qdrant client"));
    provisioner
        .ensure_collection(&kb_slug, 4)
        .await
        .expect("ensure_collection failed");

    // Wiremock: return a single 4-dim embedding for every POST /v1/embeddings.
    // Test documents must produce exactly one chunk so that count == 1.
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder: Arc<dyn Embedder> = Arc::new(
        OpenAiCompatibleEmbedder::new(OpenAiCompatibleConfig {
            endpoint_url: mock_server.uri(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dim: 4,
            max_input_tokens: 8192,
            timeout: Duration::from_secs(10),
            max_retries: 3,
        })
        .expect("embedder construction"),
    );

    let storage = Arc::new(InMemoryStorage::default());
    let (indexer_tx, indexer_rx) = tokio::sync::mpsc::channel(1024);
    let shutdown = CancellationToken::new();

    // Worker uses the same InMemoryStorage that the HTTP router stores objects in.
    let worker = IndexerWorker::new(
        Arc::clone(&storage) as Arc<dyn Storage>,
        Arc::clone(&embedder),
        Arc::clone(&qdrant_client),
        indexer_rx,
        shutdown.clone(),
        32,
    );
    let worker_handle = tokio::spawn(worker.run());

    // HybridSearcher shares the same Qdrant client + embedder as the worker
    // to avoid model/endpoint drift (§6.4, D18).
    let searcher: Arc<dyn Searcher> = Arc::new(notedthat_indexer::searcher::HybridSearcher::new(
        Arc::clone(&qdrant_client),
        Arc::clone(&embedder),
    ));

    let mut kbs = BTreeMap::new();
    kbs.insert(kb.to_string(), kb_slug);

    let state = AppState {
        storage: Arc::clone(&storage) as Arc<dyn Storage>,
        declared_kbs: Arc::new(kbs),
        bearer_token: Arc::new(TOKEN.to_string()),
        max_body_size: 16 * 1024 * 1024,
        indexer_tx,
        searcher,
    };

    let router = build_router(state);
    let qdrant_raw = qdrant_client::Qdrant::from_url(&qdrant_url)
        .build()
        .expect("raw qdrant client for polling");

    (
        container,
        FullE2eEnv {
            router,
            qdrant_raw,
            collection,
            shutdown,
            worker_handle,
            _mock_server: mock_server,
        },
    )
}

// ─── Simple (no-Qdrant) router ───────────────────────────────────────────────

/// Build a lightweight router with `InMemoryStorage` + `NoopSearcher` for HTTP-layer
/// tests that do not require a real search backend.
fn simple_router_for(kb: &str) -> axum::Router {
    let storage = Arc::new(InMemoryStorage::default());
    let mut kbs = BTreeMap::new();
    kbs.insert(kb.to_string(), KbSlug::try_new(kb).unwrap());
    let (indexer_tx, _rx) = tokio::sync::mpsc::channel(1024);
    let state = AppState {
        storage: storage as Arc<dyn Storage>,
        declared_kbs: Arc::new(kbs),
        bearer_token: Arc::new(TOKEN.to_string()),
        max_body_size: 16 * 1024 * 1024,
        indexer_tx,
        searcher: Arc::new(NoopSearcher),
    };
    build_router(state)
}

// ─── Response helpers ─────────────────────────────────────────────────────────

async fn response_json(resp: Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), 128 * 1024)
        .await
        .expect("failed to read response body");
    serde_json::from_slice(&bytes).expect("response body must be valid JSON")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// PUT a document via the HTTP API, wait for the indexer to write it to Qdrant,
/// then POST /search and assert at least one hit is returned.
#[tokio::test]
#[ignore = "requires docker + seaweedfs + qdrant"]
async fn e2e_put_then_search_finds_hit() {
    let _guard = e2e_guard().await;
    let (_container, env) = setup_full_e2e("notes-put-search").await;
    let kb = "notes-put-search";

    // PUT a single-heading document (1 chunk) so the mock returns the right
    // number of embeddings.
    let put = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{kb}/getting-started.md"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "text/markdown")
                .body(Body::from(
                    "# Getting Started\n\nThis guide explains how to get started quickly.\n",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::CREATED, "PUT must return 201");

    wait_for_index(&env.qdrant_raw, &env.collection, 1, Duration::from_secs(10))
        .await
        .expect("document was not indexed within 10 s");

    let search = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/knowledgebases/{kb}/search"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"getting started guide"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(search.status(), StatusCode::OK);
    let json = response_json(search).await;
    let hits = json["hits"].as_array().expect("hits must be an array");
    assert!(
        !hits.is_empty(),
        "expected ≥1 search hit after indexing, got 0"
    );

    env.join().await;
}

/// A `limit` value of 999 (above the internal 50-cap) must be silently clamped
/// and return 200 with ≤50 hits rather than an error.
#[tokio::test]
#[ignore = "requires docker + seaweedfs + qdrant"]
async fn e2e_search_limit_clamped() {
    let _guard = e2e_guard().await;
    let (_container, env) = setup_full_e2e("notes-limit").await;
    let kb = "notes-limit";

    let put = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{kb}/doc.md"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "text/markdown")
                .body(Body::from(
                    "# Limit Test\n\nSome content for the limit-clamping test.\n",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::CREATED);

    wait_for_index(&env.qdrant_raw, &env.collection, 1, Duration::from_secs(10))
        .await
        .expect("indexing timed out");

    let search = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/knowledgebases/{kb}/search"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"content","limit":999}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        search.status(),
        StatusCode::OK,
        "limit=999 must not be rejected as an error"
    );
    let json = response_json(search).await;
    let hits = json["hits"].as_array().expect("hits must be an array");
    assert!(
        hits.len() <= 50,
        "hits must be clamped to ≤50, got {}",
        hits.len()
    );

    env.join().await;
}

/// POST /search with `heading_path_prefix` must return only hits whose heading
/// path starts with the requested segments.
#[tokio::test]
#[ignore = "requires docker + seaweedfs + qdrant"]
async fn e2e_filter_by_heading_path_prefix() {
    let _guard = e2e_guard().await;
    let (_container, env) = setup_full_e2e("notes-heading").await;
    let kb = "notes-heading";

    // Single H1 → one chunk with heading_path = ["Installation"]
    let put = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{kb}/install.md"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "text/markdown")
                .body(Body::from(
                    "# Installation\n\nRun the installer to set up the software.\n",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::CREATED);

    wait_for_index(&env.qdrant_raw, &env.collection, 1, Duration::from_secs(10))
        .await
        .expect("indexing timed out");

    let search = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/knowledgebases/{kb}/search"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"query":"installer software","filter":{"heading_path_prefix":["Installation"]}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(search.status(), StatusCode::OK);
    let json = response_json(search).await;
    let hits = json["hits"].as_array().expect("hits must be an array");
    assert!(
        !hits.is_empty(),
        "expected ≥1 hit matching heading_path_prefix=[\"Installation\"]"
    );
    for hit in hits {
        let hp = hit["heading_path"]
            .as_array()
            .expect("heading_path must be an array");
        assert_eq!(
            hp.first().and_then(|v| v.as_str()),
            Some("Installation"),
            "first heading path element must be 'Installation', got {hp:?}"
        );
    }

    env.join().await;
}

/// DELETE a document, wait for its Qdrant tombstone to propagate, then verify
/// the document no longer appears in search results.
#[tokio::test]
#[ignore = "requires docker + seaweedfs + qdrant"]
async fn e2e_delete_removes_from_search() {
    let _guard = e2e_guard().await;
    let (_container, env) = setup_full_e2e("notes-delete").await;
    let kb = "notes-delete";
    let unique_term = "xqz9uniqueterm2025nt";

    let put = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{kb}/to-delete.md"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "text/markdown")
                .body(Body::from(format!(
                    "# Delete Test\n\nDocument with unique term: {unique_term}.\n"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::CREATED);

    wait_for_index(&env.qdrant_raw, &env.collection, 1, Duration::from_secs(10))
        .await
        .expect("indexing timed out");

    let del = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/knowledgebases/{kb}/to-delete.md"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        del.status(),
        StatusCode::NO_CONTENT,
        "DELETE must return 204"
    );

    wait_for_tombstone(
        &env.qdrant_raw,
        &env.collection,
        "to-delete.md",
        Duration::from_secs(10),
    )
    .await
    .expect("tombstone was not applied within 10 s");

    let search = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/knowledgebases/{kb}/search"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"query":"{unique_term}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(search.status(), StatusCode::OK);
    let json = response_json(search).await;
    let hits = json["hits"].as_array().expect("hits must be an array");
    assert!(
        hits.is_empty(),
        "deleted document must not appear in search results, got {} hit(s)",
        hits.len()
    );

    env.join().await;
}

/// A POST /search request without an `Authorization` header must return 401.
#[tokio::test]
#[ignore = "requires docker + seaweedfs + qdrant"]
async fn e2e_401_on_missing_bearer() {
    let router = simple_router_for(KB);
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/knowledgebases/{KB}/search"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// A POST /search request against an undeclared knowledge base must return 404
/// with `error="not_found"`.
#[tokio::test]
#[ignore = "requires docker + seaweedfs + qdrant"]
async fn e2e_404_on_unknown_kb() {
    let router = simple_router_for(KB);
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/knowledgebases/unknown-kb-xyz/search")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let json = response_json(resp).await;
    assert_eq!(
        json["error"].as_str(),
        Some("not_found"),
        "error field must be 'not_found', got: {json}"
    );
}

/// A POST /search request whose body exceeds 64 KiB must return 413 with
/// `error="payload_too_large"`.
#[tokio::test]
#[ignore = "requires docker + seaweedfs + qdrant"]
async fn e2e_413_on_body_too_large() {
    let router = simple_router_for(KB);
    // 70,000 bytes > SEARCH_BODY_MAX_BYTES (64 KiB = 65,536)
    let big_body = "x".repeat(70_000);
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/knowledgebases/{KB}/search"))
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(big_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let json = response_json(resp).await;
    assert_eq!(
        json["error"].as_str(),
        Some("payload_too_large"),
        "error field must be 'payload_too_large', got: {json}"
    );
}

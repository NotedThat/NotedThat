//! End-to-end integration tests for the search API.
//!
//! Uses the in-process [`InMemoryVectorStore`], a wiremock embedder, and the
//! shared [`InMemoryStorage`], so the HTTP router and the [`IndexerWorker`]
//! operate on the same backing stores with no container in sight. The subject
//! is the route-to-index-to-search round trip, not either backend's wire
//! protocol.
//!
//! Run with:
//!   cargo test -p notedthat-api-http --test `search_e2e`
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
use notedthat_indexer::testing::InMemoryVectorStore;
use notedthat_indexer::{
    Embedder, IndexerWorker, OpenAiCompatibleConfig, OpenAiCompatibleEmbedder, QdrantProvisioner,
    Searcher,
};
use tokio_util::sync::CancellationToken;
use tower::util::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

/// How long to wait for an asynchronous index event to land in the vector store.
///
/// Generous on purpose. Nothing here waits on a container any more, but indexing
/// is still asynchronous — the worker re-reads the object, calls the wiremock
/// embedder and upserts — and a loaded CI runner can be slow enough that a tight
/// budget turns into an intermittent failure rather than a real signal.
const INDEX_READY_TIMEOUT: Duration = Duration::from_secs(60);

// ─── Constants ──────────────────────────────────────────────────────────────

const TOKEN: &str = "e2e-test-token";
/// KB declared in the simple (no-Qdrant) router used for HTTP-level error tests.
const KB: &str = "notes";

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

// ─── Index polling helpers ───────────────────────────────────────────────────

/// Poll until at least `expected_count` points exist for `kb`.
///
/// Indexing is asynchronous — the route returns before the worker has written
/// the point — so these tests still have to wait, container or not. The waits
/// are just far shorter now.
async fn wait_for_index(
    store: &InMemoryVectorStore,
    kb: &KbSlug,
    expected_count: usize,
    timeout: Duration,
) -> Result<(), String> {
    let start = std::time::Instant::now();
    loop {
        if store.point_count(kb).await.unwrap_or_default() >= expected_count {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "timed out after {timeout:?} waiting for {expected_count} points in {}",
                kb.as_str()
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Poll until every point for `object_key` has been removed from `kb`.
async fn wait_for_tombstone(
    store: &InMemoryVectorStore,
    kb: &KbSlug,
    object_key: &str,
    timeout: Duration,
) -> Result<(), String> {
    let start = std::time::Instant::now();
    loop {
        if store.scroll_object(kb, object_key, false).await.is_empty() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(format!(
                "timed out after {timeout:?} waiting for tombstone of '{object_key}' in {}",
                kb.as_str()
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ─── Full E2E environment ────────────────────────────────────────────────────

/// All state needed for a full E2E test: router, indexer worker, and the
/// in-memory backends they share.
struct FullE2eEnv {
    router: axum::Router,
    store: InMemoryVectorStore,
    kb: KbSlug,
    shutdown: CancellationToken,
    worker_handle: tokio::task::JoinHandle<()>,
    /// Kept alive so the wiremock server answers embedding requests.
    _mock_server: MockServer,
}

impl FullE2eEnv {
    /// Cancel the indexer worker, drain pending events, then drop all shared state.
    ///
    /// Call this at the end of every full-E2E test so the worker exits cleanly.
    ///
    /// The router is dropped *first*, on purpose. It owns the `AppState` that
    /// holds the only remaining `indexer_tx`, and while a sender is alive the
    /// worker's receive loop has no reason to return — so cancelling without
    /// dropping it left every test waiting out the full timeout below.
    async fn join(self) {
        let Self {
            router,
            shutdown,
            worker_handle,
            _mock_server: mock_server,
            ..
        } = self;
        drop(router);
        shutdown.cancel();
        // Give the drain loop up to 10 s to flush remaining events.
        let _ = tokio::time::timeout(Duration::from_secs(10), worker_handle).await;
        // The wiremock server must outlive the drain, so it is dropped last.
        drop(mock_server);
    }
}

/// Construct a full E2E environment for `kb`.
async fn setup_full_e2e(kb: &str) -> FullE2eEnv {
    let kb_slug = KbSlug::try_new(kb).expect("valid kb slug for e2e test");

    let store = InMemoryVectorStore::new();
    let provisioner = QdrantProvisioner::new(Arc::new(store.clone()));
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
        Arc::new(store.clone()),
        indexer_rx,
        shutdown.clone(),
        32,
    );
    let worker_handle = tokio::spawn(worker.run());

    // HybridSearcher shares the same vector store + embedder as the worker
    // to avoid model/endpoint drift (§6.4, D18).
    let searcher: Arc<dyn Searcher> = Arc::new(notedthat_indexer::searcher::HybridSearcher::new(
        Arc::new(store.clone()),
        Arc::clone(&embedder),
    ));

    let mut kbs = BTreeMap::new();
    kbs.insert(kb.to_string(), kb_slug.clone());

    let state = AppState {
        storage: Arc::clone(&storage) as Arc<dyn Storage>,
        access_policies: Arc::new(notedthat_core::signed_in_policies(&kbs)),
        declared_kbs: Arc::new(kbs),
        bearer_token: Arc::new(TOKEN.to_string()),
        max_body_size: 16 * 1024 * 1024,
        max_patchable_size: 16 * 1024 * 1024,
        indexer_tx,
        searcher,
    };

    let router = build_router(state);

    FullE2eEnv {
        router,
        store,
        kb: kb_slug,
        shutdown,
        worker_handle,
        _mock_server: mock_server,
    }
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
        access_policies: Arc::new(notedthat_core::signed_in_policies(&kbs)),
        declared_kbs: Arc::new(kbs),
        bearer_token: Arc::new(TOKEN.to_string()),
        max_body_size: 16 * 1024 * 1024,
        max_patchable_size: 16 * 1024 * 1024,
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
async fn e2e_put_then_search_finds_hit() {
    let env = setup_full_e2e("notes-put-search").await;
    let kb = "notes-put-search";

    // PUT a single-heading document (1 chunk) so the mock returns the right
    // number of embeddings.
    let put = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/knowledgebases/{kb}/getting-started.md"))
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

    wait_for_index(&env.store, &env.kb, 1, INDEX_READY_TIMEOUT)
        .await
        .expect("document was not indexed within 10 s");

    let search = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/knowledgebases/{kb}/search"))
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
async fn e2e_search_limit_clamped() {
    let env = setup_full_e2e("notes-limit").await;
    let kb = "notes-limit";

    let put = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/knowledgebases/{kb}/doc.md"))
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

    wait_for_index(&env.store, &env.kb, 1, INDEX_READY_TIMEOUT)
        .await
        .expect("indexing timed out");

    let search = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/knowledgebases/{kb}/search"))
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
async fn e2e_filter_by_heading_path_prefix() {
    let env = setup_full_e2e("notes-heading").await;
    let kb = "notes-heading";

    // Single H1 → one chunk with heading_path = ["Installation"]
    let put = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/knowledgebases/{kb}/install.md"))
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

    wait_for_index(&env.store, &env.kb, 1, INDEX_READY_TIMEOUT)
        .await
        .expect("indexing timed out");

    let search = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/knowledgebases/{kb}/search"))
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
async fn e2e_delete_removes_from_search() {
    let env = setup_full_e2e("notes-delete").await;
    let kb = "notes-delete";
    let unique_term = "xqz9uniqueterm2025nt";

    let put = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/v1/knowledgebases/{kb}/to-delete.md"))
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

    wait_for_index(&env.store, &env.kb, 1, INDEX_READY_TIMEOUT)
        .await
        .expect("indexing timed out");

    let del = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/knowledgebases/{kb}/to-delete.md"))
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

    wait_for_tombstone(&env.store, &env.kb, "to-delete.md", INDEX_READY_TIMEOUT)
        .await
        .expect("tombstone was not applied within 10 s");

    let search = env
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/knowledgebases/{kb}/search"))
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
async fn e2e_401_on_missing_bearer() {
    let router = simple_router_for(KB);
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/knowledgebases/{KB}/search"))
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
async fn e2e_404_on_unknown_kb() {
    let router = simple_router_for(KB);
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/knowledgebases/unknown-kb-xyz/search")
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
async fn e2e_413_on_body_too_large() {
    let router = simple_router_for(KB);
    // 70,000 bytes > SEARCH_BODY_MAX_BYTES (64 KiB = 65,536)
    let big_body = "x".repeat(70_000);
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/knowledgebases/{KB}/search"))
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

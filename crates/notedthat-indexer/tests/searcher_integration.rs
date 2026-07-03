//! Integration tests for `HybridSearcher`.
//!
//! Each test requires a running Qdrant container (Docker) and is marked `#[ignore]`.
//! Run with: `cargo test -p notedthat-indexer --test searcher_integration -- --ignored --nocapture`

#![allow(missing_docs)]

use notedthat_core::{
    KbSlug,
    search::{SearchError, SearchFilter, SearchRequest},
};
use notedthat_indexer::{
    Embedder, OpenAiCompatibleConfig, OpenAiCompatibleEmbedder, QdrantClient, QdrantConfig,
    QdrantProvisioner, Searcher, searcher::HybridSearcher,
};
use std::{sync::Arc, time::Duration};
use testcontainers::{
    GenericImage,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

static INTEGRATION_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn start_qdrant() -> (impl std::any::Any, String) {
    let container = GenericImage::new("qdrant/qdrant", "v1.15.4")
        .with_exposed_port(6334_u16.tcp())
        .with_wait_for(WaitFor::seconds(5))
        .start()
        .await
        .expect("failed to start Qdrant testcontainer — is Docker running?");
    let port = container
        .get_host_port_ipv4(6334_u16)
        .await
        .expect("failed to get Qdrant gRPC port");
    (container, format!("http://127.0.0.1:{port}"))
}

fn embedding_response(dim: usize, count: usize) -> serde_json::Value {
    let data: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            let v: Vec<f32> = (0..dim)
                .map(|j| if j == i % dim { 1.0 } else { 0.0 })
                .collect();
            serde_json::json!({ "index": i, "embedding": v, "object": "embedding" })
        })
        .collect();
    serde_json::json!({ "object": "list", "data": data })
}

fn make_embedder(server_uri: &str, dim: usize) -> Arc<dyn Embedder> {
    Arc::new(
        OpenAiCompatibleEmbedder::new(OpenAiCompatibleConfig {
            endpoint_url: server_uri.to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dim,
            max_input_tokens: 8192,
            timeout: Duration::from_secs(10),
            max_retries: 3,
        })
        .expect("embedder construction failed"),
    )
}

fn make_qdrant(url: &str) -> (Arc<QdrantClient>, QdrantProvisioner) {
    let cfg = QdrantConfig {
        url: url.to_string(),
        api_key: None,
    };
    let client = Arc::new(QdrantClient::new(&cfg).unwrap());
    let provisioner = QdrantProvisioner::new(QdrantClient::new(&cfg).unwrap());
    (client, provisioner)
}

fn raw_client(url: &str) -> qdrant_client::Qdrant {
    qdrant_client::Qdrant::from_url(url)
        .build()
        .expect("raw qdrant client build failed")
}

fn kb() -> KbSlug {
    KbSlug::try_new("test-kb").unwrap()
}

fn coll(kb: &KbSlug) -> String {
    format!("kb_{}_v1", kb.as_str())
}

/// Directly upsert a point into Qdrant for test isolation.
/// Writes BOTH `dense` (fixed 4-dim vector) AND `sparse_bm25` (Document inference) vectors.
/// This mirrors the T1 fix where M4 only wrote `dense`.
async fn raw_upsert_point(
    qdrant: &qdrant_client::Qdrant,
    collection: &str,
    id: u64,
    text: &str,
    object_key: &str,
    mime: &str,
    heading_path: Vec<String>,
    mtime: i64,
) {
    use qdrant_client::qdrant::{Document, PointStruct, UpsertPointsBuilder, Vector};
    use std::collections::HashMap;

    let mut payload = HashMap::<String, qdrant_client::qdrant::Value>::new();
    payload.insert("object_key".to_string(), object_key.to_string().into());
    payload.insert("chunk_index".to_string(), 0_i64.into());
    payload.insert("byte_start".to_string(), 0_i64.into());
    payload.insert("byte_end".to_string(), (text.len() as i64).into());
    payload.insert("etag".to_string(), "deadbeef".to_string().into());
    payload.insert("content_hash".to_string(), "deadbeef".to_string().into());
    payload.insert("mtime".to_string(), mtime.into());
    payload.insert("mime".to_string(), mime.to_string().into());
    payload.insert("heading_path".to_string(), heading_path.into());
    payload.insert("tags".to_string(), Vec::<String>::new().into());
    payload.insert("text".to_string(), text.to_string().into());

    let vectors = HashMap::from([
        (
            "dense".to_string(),
            Vector::from(vec![1.0_f32, 0.0, 0.0, 0.0]),
        ),
        (
            "sparse_bm25".to_string(),
            Vector::from(Document::new(text.to_string(), "qdrant/bm25")),
        ),
    ]);

    let point = PointStruct::new(id, vectors, payload);
    qdrant
        .upsert_points(UpsertPointsBuilder::new(collection, vec![point]).wait(true))
        .await
        .expect("raw_upsert_point failed");
}

#[tokio::test]
#[ignore = "requires qdrant testcontainer + docker"]
async fn search_returns_upserted_chunks() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (container, url) = start_qdrant().await;
    let (qdrant_client, provisioner) = make_qdrant(&url);
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    let raw = raw_client(&url);
    let collection = coll(&kb);
    raw_upsert_point(&raw, &collection, 1, "the quick brown fox", "fox.md", "text/markdown", vec![], 1000).await;
    raw_upsert_point(&raw, &collection, 2, "lazy dog sits still", "dog.md", "text/markdown", vec![], 2000).await;
    raw_upsert_point(&raw, &collection, 3, "hello world greeting", "hello.md", "text/markdown", vec![], 3000).await;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::clone(&qdrant_client), embedder);

    let request = SearchRequest {
        query: "quick".to_string(),
        filter: None,
        limit: None,
    }
    .validate()
    .unwrap();

    let response = searcher.search(&kb, request).await.expect("search failed");
    let hits = &response.hits;

    assert!(!hits.is_empty(), "expected at least one hit, got 0");
    assert!(
        !hits[0].object_key.as_str().is_empty(),
        "expected non-empty object_key"
    );
    assert!(hits[0].score > 0.0, "expected score > 0.0, got {}", hits[0].score);

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant testcontainer + docker"]
async fn filter_by_mime_excludes_non_matching() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (container, url) = start_qdrant().await;
    let (qdrant_client, provisioner) = make_qdrant(&url);
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    let raw = raw_client(&url);
    let collection = coll(&kb);
    raw_upsert_point(&raw, &collection, 1, "markdown document content", "md-file.md", "text/markdown", vec![], 1000).await;
    raw_upsert_point(&raw, &collection, 2, "plain text document content", "txt-file.md", "text/plain", vec![], 2000).await;
    raw_upsert_point(&raw, &collection, 3, "pdf document content binary", "pdf-file.md", "application/pdf", vec![], 3000).await;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::clone(&qdrant_client), embedder);

    let request = SearchRequest {
        query: "document content".to_string(),
        filter: Some(SearchFilter {
            mime: Some("text/markdown".to_string()),
            ..Default::default()
        }),
        limit: Some(10),
    }
    .validate()
    .unwrap();

    let response = searcher.search(&kb, request).await.expect("search failed");

    for hit in &response.hits {
        assert_eq!(
            hit.object_key.as_str(),
            "md-file.md",
            "expected only text/markdown hit (md-file.md), got {}",
            hit.object_key.as_str()
        );
    }

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant testcontainer + docker"]
async fn filter_by_heading_path_prefix() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (container, url) = start_qdrant().await;
    let (qdrant_client, provisioner) = make_qdrant(&url);
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    let raw = raw_client(&url);
    let collection = coll(&kb);
    raw_upsert_point(&raw, &collection, 1, "section A overview", "a.md", "text/markdown", vec!["A".to_string()], 1000).await;
    raw_upsert_point(&raw, &collection, 2, "section A sub B details", "ab.md", "text/markdown", vec!["A".to_string(), "B".to_string()], 2000).await;
    raw_upsert_point(&raw, &collection, 3, "section C unrelated", "c.md", "text/markdown", vec!["C".to_string()], 3000).await;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::clone(&qdrant_client), embedder);

    let request = SearchRequest {
        query: "section".to_string(),
        filter: Some(SearchFilter {
            heading_path_prefix: vec!["A".to_string()],
            ..Default::default()
        }),
        limit: Some(10),
    }
    .validate()
    .unwrap();

    let response = searcher.search(&kb, request).await.expect("search failed");

    for hit in &response.hits {
        assert!(
            !hit.heading_path.is_empty() && hit.heading_path[0] == "A",
            "expected heading_path[0]=='A', got {:?} for key {}",
            hit.heading_path,
            hit.object_key.as_str()
        );
    }

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant testcontainer + docker"]
async fn filter_by_updated_after() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (container, url) = start_qdrant().await;
    let (qdrant_client, provisioner) = make_qdrant(&url);
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    let raw = raw_client(&url);
    let collection = coll(&kb);
    raw_upsert_point(&raw, &collection, 1, "old document content here", "old.md", "text/markdown", vec![], 1000).await;
    raw_upsert_point(&raw, &collection, 2, "mid document content here", "mid.md", "text/markdown", vec![], 2000).await;
    raw_upsert_point(&raw, &collection, 3, "new document content here", "new.md", "text/markdown", vec![], 3000).await;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::clone(&qdrant_client), embedder);

    let request = SearchRequest {
        query: "document content".to_string(),
        filter: Some(SearchFilter {
            updated_after: Some(2000),
            ..Default::default()
        }),
        limit: Some(10),
    }
    .validate()
    .unwrap();

    let response = searcher.search(&kb, request).await.expect("search failed");

    for hit in &response.hits {
        assert_ne!(
            hit.object_key.as_str(),
            "old.md",
            "old.md (mtime=1000) should be excluded by updated_after=2000"
        );
    }

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant testcontainer + docker"]
async fn empty_collection_returns_empty_hits() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (container, url) = start_qdrant().await;
    let (qdrant_client, provisioner) = make_qdrant(&url);
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::clone(&qdrant_client), embedder);

    let request = SearchRequest {
        query: "anything".to_string(),
        filter: None,
        limit: None,
    }
    .validate()
    .unwrap();

    let response = searcher
        .search(&kb, request)
        .await
        .expect("empty collection search should return Ok, not Err");

    assert!(
        response.hits.is_empty(),
        "expected 0 hits on empty collection, got {}",
        response.hits.len()
    );

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant testcontainer + docker"]
async fn missing_collection_returns_unknown_kb() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (container, url) = start_qdrant().await;
    let (qdrant_client, _provisioner) = make_qdrant(&url);
    let kb = kb();
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::clone(&qdrant_client), embedder);

    let request = SearchRequest {
        query: "anything".to_string(),
        filter: None,
        limit: None,
    }
    .validate()
    .unwrap();

    let err = searcher
        .search(&kb, request)
        .await
        .expect_err("expected Err for missing collection");

    assert!(
        matches!(err, SearchError::UnknownKb { .. }),
        "expected SearchError::UnknownKb, got {err:?}"
    );

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant testcontainer + docker"]
async fn preview_truncates_multi_byte() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (container, url) = start_qdrant().await;
    let (qdrant_client, provisioner) = make_qdrant(&url);
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    let raw = raw_client(&url);
    let collection = coll(&kb);
    let long_text = "日本語".repeat(300);
    raw_upsert_point(&raw, &collection, 1, &long_text, "long.md", "text/markdown", vec![], 1000).await;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::clone(&qdrant_client), embedder);

    let request = SearchRequest {
        query: "日本語".to_string(),
        filter: None,
        limit: None,
    }
    .validate()
    .unwrap();

    let response = searcher.search(&kb, request).await.expect("search failed");

    assert!(!response.hits.is_empty(), "expected at least one hit");
    let preview_len = response.hits[0].preview.chars().count();
    assert_eq!(
        preview_len, 500,
        "expected preview truncated to exactly 500 chars, got {preview_len}"
    );

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant testcontainer + docker"]
async fn limit_capped_by_request() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (container, url) = start_qdrant().await;
    let (qdrant_client, provisioner) = make_qdrant(&url);
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    let raw = raw_client(&url);
    let collection = coll(&kb);
    for i in 1_u64..=10 {
        raw_upsert_point(
            &raw,
            &collection,
            i,
            &format!("document entry number {i}"),
            &format!("doc{i}.md"),
            "text/markdown",
            vec![],
            i as i64 * 1000,
        )
        .await;
    }

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::clone(&qdrant_client), embedder);

    let request = SearchRequest {
        query: "document".to_string(),
        filter: None,
        limit: Some(3),
    }
    .validate()
    .unwrap();

    let response = searcher.search(&kb, request).await.expect("search failed");
    assert!(
        response.hits.len() <= 3,
        "expected ≤3 hits for limit=3, got {}",
        response.hits.len()
    );

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant testcontainer + docker"]
async fn object_key_prefix_post_filter() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (container, url) = start_qdrant().await;
    let (qdrant_client, provisioner) = make_qdrant(&url);
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    let raw = raw_client(&url);
    let collection = coll(&kb);
    let entries = [
        (1_u64, "docs/a.md"),
        (2, "docs/b.md"),
        (3, "docs/c.md"),
        (4, "notes/x.md"),
        (5, "notes/y.md"),
        (6, "notes/z.md"),
    ];
    for (id, key) in &entries {
        raw_upsert_point(
            &raw,
            &collection,
            *id,
            &format!("content in file {key}"),
            key,
            "text/markdown",
            vec![],
            *id as i64 * 1000,
        )
        .await;
    }

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::clone(&qdrant_client), embedder);

    let request = SearchRequest {
        query: "content file".to_string(),
        filter: Some(SearchFilter {
            object_key_prefix: Some("docs/".to_string()),
            ..Default::default()
        }),
        limit: Some(10),
    }
    .validate()
    .unwrap();

    let response = searcher.search(&kb, request).await.expect("search failed");

    assert!(!response.hits.is_empty(), "expected at least one docs/ hit");
    for hit in &response.hits {
        assert!(
            hit.object_key.as_str().starts_with("docs/"),
            "expected object_key to start with 'docs/', got {}",
            hit.object_key.as_str()
        );
    }

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant testcontainer + docker"]
async fn sparse_only_query_finds_bm25_match() {
    let _guard = INTEGRATION_MUTEX.lock().await;
    let (container, url) = start_qdrant().await;
    let (qdrant_client, provisioner) = make_qdrant(&url);
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    let raw = raw_client(&url);
    let collection = coll(&kb);
    raw_upsert_point(
        &raw,
        &collection,
        1,
        "the anaconda coils around its prey",
        "anaconda.md",
        "text/markdown",
        vec![],
        1000,
    )
    .await;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::clone(&qdrant_client), embedder);

    let request = SearchRequest {
        query: "anaconda".to_string(),
        filter: None,
        limit: None,
    }
    .validate()
    .unwrap();

    let response = searcher.search(&kb, request).await.expect("search failed");
    assert!(
        !response.hits.is_empty(),
        "expected ≥1 hit for 'anaconda' — sparse_bm25 must be populated at upsert (T1 fix)"
    );
    assert_eq!(
        response.hits[0].object_key.as_str(),
        "anaconda.md",
        "expected 'anaconda.md' as top hit"
    );

    drop(container);
}

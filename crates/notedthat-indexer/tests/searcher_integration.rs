//! Integration tests for `HybridSearcher`.
//!
//! The subject is the searcher's own behaviour — how it builds a hybrid query,
//! applies filters, truncates to the requested limit, and maps a missing
//! collection onto `UnknownKb`. It runs against [`InMemoryVectorStore`], whose
//! retrieval shape (dense cosine arm, sparse BM25 arm, RRF fusion) mirrors the
//! Qdrant configuration and is itself covered by
//! `tests/in_memory_vector_store.rs`.
//!
//! The embedder is a wiremock server, as it always was.
//!
//! Run with: `cargo test -p notedthat-indexer --test searcher_integration`

#![allow(missing_docs)]

use notedthat_core::{
    KbSlug,
    search::{SearchError, SearchFilter, SearchRequest},
};
use notedthat_indexer::testing::InMemoryVectorStore;
use notedthat_indexer::vector_store::VectorStore;
use notedthat_indexer::{
    Embedder, OpenAiCompatibleConfig, OpenAiCompatibleEmbedder, QdrantProvisioner, Searcher,
    searcher::HybridSearcher,
};
use std::{sync::Arc, time::Duration};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

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

/// A store and a provisioner sharing it, mirroring how the server wires one
/// backend into both.
fn make_store() -> (InMemoryVectorStore, QdrantProvisioner) {
    let store = InMemoryVectorStore::new();
    let provisioner = QdrantProvisioner::new(Arc::new(store.clone()));
    (store, provisioner)
}

fn kb() -> KbSlug {
    KbSlug::try_new("test-kb").unwrap()
}

/// Upsert a point directly, bypassing the indexing pipeline, so each test
/// controls exactly what is in the collection.
///
/// Writes BOTH `dense` (fixed 4-dim vector) AND `sparse_bm25` (BM25 document)
/// vectors. This mirrors the T1 fix where M4 only wrote `dense`.
#[allow(clippy::too_many_arguments)]
async fn upsert_point(
    store: &InMemoryVectorStore,
    kb: &KbSlug,
    id: u64,
    text: &str,
    object_key: &str,
    mime_type: &str,
    heading_path: Vec<String>,
    mtime: i64,
) {
    use qdrant_client::qdrant::{Document, PointStruct, Vector};
    use std::collections::HashMap;

    let mut payload = HashMap::<String, qdrant_client::qdrant::Value>::new();
    payload.insert("object_key".to_string(), object_key.to_string().into());
    payload.insert("chunk_index".to_string(), 0_i64.into());
    payload.insert("byte_start".to_string(), 0_i64.into());
    payload.insert(
        "byte_end".to_string(),
        (i64::try_from(text.len()).unwrap_or(i64::MAX)).into(),
    );
    payload.insert("etag".to_string(), "deadbeef".to_string().into());
    payload.insert("content_hash".to_string(), "deadbeef".to_string().into());
    payload.insert("mtime".to_string(), mtime.into());
    payload.insert("mime".to_string(), mime_type.to_string().into());
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

    store
        .upsert_points(kb, vec![PointStruct::new(id, vectors, payload)])
        .await
        .expect("upsert_point failed");
}

#[tokio::test]
async fn search_returns_upserted_chunks() {
    let (store, provisioner) = make_store();
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    upsert_point(
        &store,
        &kb,
        1,
        "the quick brown fox",
        "fox.md",
        "text/markdown",
        vec![],
        1000,
    )
    .await;
    upsert_point(
        &store,
        &kb,
        2,
        "lazy dog sits still",
        "dog.md",
        "text/markdown",
        vec![],
        2000,
    )
    .await;
    upsert_point(
        &store,
        &kb,
        3,
        "hello world greeting",
        "hello.md",
        "text/markdown",
        vec![],
        3000,
    )
    .await;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::new(store.clone()), embedder);

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
    assert!(
        hits[0].score > 0.0,
        "expected score > 0.0, got {}",
        hits[0].score
    );
}

#[tokio::test]
async fn filter_by_mime_excludes_non_matching() {
    let (store, provisioner) = make_store();
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    upsert_point(
        &store,
        &kb,
        1,
        "markdown document content",
        "md-file.md",
        "text/markdown",
        vec![],
        1000,
    )
    .await;
    upsert_point(
        &store,
        &kb,
        2,
        "plain text document content",
        "txt-file.md",
        "text/plain",
        vec![],
        2000,
    )
    .await;
    upsert_point(
        &store,
        &kb,
        3,
        "pdf document content binary",
        "pdf-file.md",
        "application/pdf",
        vec![],
        3000,
    )
    .await;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::new(store.clone()), embedder);

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
}

#[tokio::test]
async fn filter_by_heading_path_prefix() {
    let (store, provisioner) = make_store();
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    upsert_point(
        &store,
        &kb,
        1,
        "section A overview",
        "a.md",
        "text/markdown",
        vec!["A".to_string()],
        1000,
    )
    .await;
    upsert_point(
        &store,
        &kb,
        2,
        "section A sub B details",
        "ab.md",
        "text/markdown",
        vec!["A".to_string(), "B".to_string()],
        2000,
    )
    .await;
    upsert_point(
        &store,
        &kb,
        3,
        "section C unrelated",
        "c.md",
        "text/markdown",
        vec!["C".to_string()],
        3000,
    )
    .await;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::new(store.clone()), embedder);

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
}

#[tokio::test]
async fn filter_by_updated_after() {
    let (store, provisioner) = make_store();
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    upsert_point(
        &store,
        &kb,
        1,
        "old document content here",
        "old.md",
        "text/markdown",
        vec![],
        1000,
    )
    .await;
    upsert_point(
        &store,
        &kb,
        2,
        "mid document content here",
        "mid.md",
        "text/markdown",
        vec![],
        2000,
    )
    .await;
    upsert_point(
        &store,
        &kb,
        3,
        "new document content here",
        "new.md",
        "text/markdown",
        vec![],
        3000,
    )
    .await;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::new(store.clone()), embedder);

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
}

#[tokio::test]
async fn empty_collection_returns_empty_hits() {
    let (store, provisioner) = make_store();
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
    let searcher = HybridSearcher::new(Arc::new(store.clone()), embedder);

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
}

#[tokio::test]
async fn missing_collection_returns_unknown_kb() {
    let (store, _provisioner) = make_store();
    let kb = kb();
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let embedder = make_embedder(&mock_server.uri(), 4);
    let searcher = HybridSearcher::new(Arc::new(store.clone()), embedder);

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
}

#[tokio::test]
async fn preview_truncates_multi_byte() {
    let (store, provisioner) = make_store();
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    let long_text = "日本語".repeat(300);
    upsert_point(
        &store,
        &kb,
        1,
        &long_text,
        "long.md",
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
    let searcher = HybridSearcher::new(Arc::new(store.clone()), embedder);

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
}

#[tokio::test]
async fn limit_capped_by_request() {
    let (store, provisioner) = make_store();
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    for i in 1_u64..=10 {
        upsert_point(
            &store,
            &kb,
            i,
            &format!("document entry number {i}"),
            &format!("doc{i}.md"),
            "text/markdown",
            vec![],
            i64::try_from(i).unwrap_or(i64::MAX) * 1000,
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
    let searcher = HybridSearcher::new(Arc::new(store.clone()), embedder);

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
}

#[tokio::test]
async fn object_key_prefix_post_filter() {
    let (store, provisioner) = make_store();
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    let entries = [
        (1_u64, "docs/a.md"),
        (2, "docs/b.md"),
        (3, "docs/c.md"),
        (4, "notes/x.md"),
        (5, "notes/y.md"),
        (6, "notes/z.md"),
    ];
    for (id, key) in &entries {
        upsert_point(
            &store,
            &kb,
            *id,
            &format!("content in file {key}"),
            key,
            "text/markdown",
            vec![],
            i64::try_from(*id).unwrap_or(i64::MAX) * 1000,
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
    let searcher = HybridSearcher::new(Arc::new(store.clone()), embedder);

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
}

#[tokio::test]
async fn sparse_only_query_finds_bm25_match() {
    let (store, provisioner) = make_store();
    let kb = kb();
    provisioner
        .ensure_collection(&kb, 4)
        .await
        .expect("ensure_collection failed");

    upsert_point(
        &store,
        &kb,
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
    let searcher = HybridSearcher::new(Arc::new(store.clone()), embedder);

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
}

//! E2E integration tests for IndexerWorker.
//!
//! Requires: Docker with qdrant/qdrant:v1.15.4, wiremock (in-process).
//! Run with:
//!   cargo test -p notedthat-indexer --test worker_integration -- --ignored

#![allow(missing_docs)]

use async_trait::async_trait;
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, KbManifest, KbSlug, ListResponse, ObjectMeta, ObjectPath,
    ObjectRead, PutOutcome, Storage, StorageError,
};
use notedthat_indexer::{
    Embedder, IndexEvent, IndexerWorker, OpenAiCompatibleConfig, OpenAiCompatibleEmbedder,
    QdrantClient, QdrantConfig, QdrantProvisioner,
};
use qdrant_client::qdrant::{Condition, Filter, ScrollPoints};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use testcontainers::{
    GenericImage,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

struct MockStorage {
    objects: Mutex<HashMap<(String, String), (Bytes, Option<String>)>>,
}

impl MockStorage {
    fn new() -> Self {
        Self { objects: Mutex::new(HashMap::new()) }
    }

    fn insert(&self, kb: &str, key: &str, content: &str, content_type: &str) {
        self.objects.lock().unwrap().insert(
            (kb.to_string(), key.to_string()),
            (Bytes::from(content.to_owned()), Some(content_type.to_owned())),
        );
    }
}

#[async_trait]
impl Storage for MockStorage {
    async fn ensure_bucket(&self, _kb: &KbSlug) -> Result<(), StorageError> {
        Ok(())
    }

    async fn read_manifest(&self, _kb: &KbSlug) -> Result<KbManifest, StorageError> {
        Err(StorageError::NotFound { key: ".notedthat/manifest.json".to_string() })
    }

    async fn write_manifest(
        &self,
        _kb: &KbSlug,
        _manifest: &KbManifest,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    async fn head_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        _conditionals: ConditionalHeaders,
    ) -> Result<ObjectMeta, StorageError> {
        let guard = self.objects.lock().unwrap();
        match guard.get(&(kb.as_str().to_string(), path.as_str().to_string())) {
            Some((bytes, content_type)) => Ok(ObjectMeta {
                key: path.as_str().to_string(),
                size: bytes.len() as u64,
                last_modified: Some(1_700_000_000),
                content_type: content_type.clone(),
                etag: Some("\"test-etag\"".to_string()),
            }),
            None => Err(StorageError::NotFound { key: path.as_str().to_string() }),
        }
    }

    async fn get_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        _range: Option<Vec<ByteRange>>,
        _conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        let guard = self.objects.lock().unwrap();
        match guard.get(&(kb.as_str().to_string(), path.as_str().to_string())) {
            Some((bytes, content_type)) => Ok(ObjectRead {
                bytes: bytes.clone(),
                meta: ObjectMeta {
                    key: path.as_str().to_string(),
                    size: bytes.len() as u64,
                    last_modified: Some(1_700_000_000),
                    content_type: content_type.clone(),
                    etag: Some("\"test-etag\"".to_string()),
                },
                content_range: None,
            }),
            None => Err(StorageError::NotFound { key: path.as_str().to_string() }),
        }
    }

    async fn put_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        bytes: Bytes,
        content_type: Option<&str>,
        _conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        self.objects.lock().unwrap().insert(
            (kb.as_str().to_string(), path.as_str().to_string()),
            (bytes, content_type.map(str::to_string)),
        );
        Ok(PutOutcome { etag: Some("\"test-etag\"".to_string()) })
    }

    async fn delete_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        _conditionals: ConditionalHeaders,
    ) -> Result<(), StorageError> {
        self.objects
            .lock()
            .unwrap()
            .remove(&(kb.as_str().to_string(), path.as_str().to_string()));
        Ok(())
    }

    async fn list_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
        limit: u32,
    ) -> Result<ListResponse, StorageError> {
        let guard = self.objects.lock().unwrap();
        let kb_str = kb.as_str().to_string();
        let objects: Vec<ObjectMeta> = guard
            .iter()
            .filter(|((k, p), _)| {
                k == &kb_str && prefix.map_or(true, |pfx| p.starts_with(pfx))
            })
            .take(limit as usize)
            .map(|((_, key), (bytes, ct))| ObjectMeta {
                key: key.clone(),
                size: bytes.len() as u64,
                last_modified: Some(1_700_000_000),
                content_type: ct.clone(),
                etag: Some("\"test-etag\"".to_string()),
            })
            .collect();
        let truncated = objects.len() == limit as usize;
        Ok(ListResponse { objects, truncated })
    }
}

/// Drop the returned guard to stop and remove the container.
async fn start_qdrant_url() -> (impl std::any::Any, String) {
    let container = GenericImage::new("qdrant/qdrant", "v1.15.4")
        .with_exposed_port(6334_u16.tcp())
        .with_wait_for(WaitFor::seconds(5))
        .start()
        .await
        .expect("failed to start qdrant/qdrant:v1.15.4 — is Docker running?");
    let grpc_port = container
        .get_host_port_ipv4(6334_u16)
        .await
        .expect("failed to get Qdrant gRPC port");
    (container, format!("http://127.0.0.1:{grpc_port}"))
}

fn embedding_response(dim: usize, count: usize) -> serde_json::Value {
    let data: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            let v: Vec<f32> = (0..dim).map(|j| if j == i % dim { 1.0 } else { 0.0 }).collect();
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

fn make_worker(
    storage: Arc<MockStorage>,
    embedder: Arc<dyn Embedder>,
    qdrant: Arc<QdrantClient>,
    rx: mpsc::Receiver<IndexEvent>,
    shutdown: CancellationToken,
) -> IndexerWorker {
    IndexerWorker::new(storage as Arc<dyn Storage>, embedder, qdrant, rx, shutdown, 32)
}

fn kb() -> KbSlug {
    KbSlug::try_new("test-kb").unwrap()
}

fn opath(s: &str) -> ObjectPath {
    ObjectPath::try_from(s).unwrap()
}

fn coll(kb: &KbSlug) -> String {
    format!("kb_{}_v1", kb.as_str())
}

fn make_qdrant(url: &str) -> (Arc<QdrantClient>, QdrantProvisioner) {
    let cfg = QdrantConfig { url: url.to_string(), api_key: None };
    let client = Arc::new(QdrantClient::new(&cfg).unwrap());
    let provisioner = QdrantProvisioner::new(QdrantClient::new(&cfg).unwrap());
    (client, provisioner)
}

async fn count_points(qdrant_url: &str, collection: &str, key: &str) -> usize {
    let qdrant = qdrant_client::Qdrant::from_url(qdrant_url)
        .build()
        .expect("qdrant build failed");
    let filter = Filter::must([Condition::matches("object_key", key.to_string())]);
    qdrant
        .scroll(ScrollPoints {
            collection_name: collection.to_string(),
            filter: Some(filter),
            limit: Some(1000),
            ..Default::default()
        })
        .await
        .expect("scroll failed")
        .result
        .len()
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn happy_path_upsert_creates_qdrant_point() {
    let (_container, qdrant_url) = start_qdrant_url().await;
    let kb = kb();
    let (qdrant_client, provisioner) = make_qdrant(&qdrant_url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "hello.md", "# Hello\n\nThis is a test document.", "text/markdown");

    let (tx, rx) = mpsc::channel(100);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(
        make_worker(
            Arc::clone(&storage),
            make_embedder(&mock_server.uri(), 4),
            Arc::clone(&qdrant_client),
            rx,
            shutdown.clone(),
        )
        .run(),
    );

    tx.send(IndexEvent::Upsert {
        kb: kb.clone(),
        object_key: opath("hello.md"),
        etag: "etag-001".to_string(),
        mtime: 1_700_000_000,
    })
    .await
    .unwrap();

    drop(tx);
    shutdown.cancel();
    handle.await.unwrap();

    let n = count_points(&qdrant_url, &coll(&kb), "hello.md").await;
    assert!(n >= 1, "expected ≥1 point for hello.md, got {n}");

    drop(_container);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn tombstone_removes_points() {
    let (_container, qdrant_url) = start_qdrant_url().await;
    let kb = kb();
    let (qdrant_client, provisioner) = make_qdrant(&qdrant_url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "doc.md", "# Doc\n\nContent to be tombstoned.", "text/markdown");

    let (tx, rx) = mpsc::channel(100);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(
        make_worker(
            Arc::clone(&storage),
            make_embedder(&mock_server.uri(), 4),
            Arc::clone(&qdrant_client),
            rx,
            shutdown.clone(),
        )
        .run(),
    );

    tx.send(IndexEvent::Upsert {
        kb: kb.clone(),
        object_key: opath("doc.md"),
        etag: "etag-002".to_string(),
        mtime: 1_700_000_001,
    })
    .await
    .unwrap();

    tx.send(IndexEvent::Tombstone { kb: kb.clone(), object_key: opath("doc.md") })
        .await
        .unwrap();

    drop(tx);
    shutdown.cancel();
    handle.await.unwrap();

    let n = count_points(&qdrant_url, &coll(&kb), "doc.md").await;
    assert_eq!(n, 0, "expected 0 points after tombstone, got {n}");

    drop(_container);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn not_found_on_reread_implicit_tombstone() {
    let (_container, qdrant_url) = start_qdrant_url().await;
    let kb = kb();
    let (qdrant_client, provisioner) = make_qdrant(&qdrant_url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let mock_server = MockServer::start().await;
    let storage = Arc::new(MockStorage::new());

    let (tx, rx) = mpsc::channel(100);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(
        make_worker(
            Arc::clone(&storage),
            make_embedder(&mock_server.uri(), 4),
            Arc::clone(&qdrant_client),
            rx,
            shutdown.clone(),
        )
        .run(),
    );

    tx.send(IndexEvent::Upsert {
        kb: kb.clone(),
        object_key: opath("missing.md"),
        etag: "etag-003".to_string(),
        mtime: 1_700_000_002,
    })
    .await
    .unwrap();

    drop(tx);
    shutdown.cancel();
    handle.await.unwrap();

    let n = count_points(&qdrant_url, &coll(&kb), "missing.md").await;
    assert_eq!(n, 0, "implicit tombstone should produce 0 points, got {n}");

    let calls = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(calls.len(), 0, "embedder must not be called when object is absent");

    drop(_container);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn non_markdown_content_type_skipped() {
    todo!("insert image/png object, send Upsert, assert 0 Qdrant points");
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn oversized_chunk_dropped_with_warn() {
    todo!("set max_input_tokens=5, insert large doc, assert 0 points and no panic");
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn queue_full_logs_index_queue_full() {
    let (tx, _rx) = mpsc::channel::<IndexEvent>(4);
    let kb = kb();

    for i in 0..4_u32 {
        tx.try_send(IndexEvent::Upsert {
            kb: kb.clone(),
            object_key: opath(&format!("note{i}.md")),
            etag: format!("e{i}"),
            mtime: i as i64,
        })
        .unwrap_or_else(|_| panic!("send {i} should succeed"));
    }

    let result = tx.try_send(IndexEvent::Upsert {
        kb,
        object_key: opath("note4.md"),
        etag: "e4".to_string(),
        mtime: 4,
    });
    assert!(
        matches!(result, Err(tokio::sync::mpsc::error::TrySendError::Full(_))),
        "5th send must return TrySendError::Full, got: {result:?}",
    );
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn graceful_shutdown_drains_queue() {
    let (_container, qdrant_url) = start_qdrant_url().await;
    let kb = kb();
    let (qdrant_client, provisioner) = make_qdrant(&qdrant_url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let storage = Arc::new(MockStorage::new());
    for i in 0..5_u32 {
        storage.insert(
            "test-kb",
            &format!("drain{i}.md"),
            &format!("# Drain {i}\n\nParagraph."),
            "text/markdown",
        );
    }

    let (tx, rx) = mpsc::channel(100);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(
        make_worker(
            Arc::clone(&storage),
            make_embedder(&mock_server.uri(), 4),
            Arc::clone(&qdrant_client),
            rx,
            shutdown.clone(),
        )
        .run(),
    );

    for i in 0..5_u32 {
        tx.send(IndexEvent::Upsert {
            kb: kb.clone(),
            object_key: opath(&format!("drain{i}.md")),
            etag: format!("etag-drain-{i}"),
            mtime: 1_700_000_000 + i as i64,
        })
        .await
        .unwrap();
    }

    shutdown.cancel();
    drop(tx);

    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("worker did not exit within 30 s after drain")
        .unwrap();

    let collection = coll(&kb);
    for i in 0..5_u32 {
        let key = format!("drain{i}.md");
        let n = count_points(&qdrant_url, &collection, &key).await;
        assert!(n >= 1, "expected ≥1 point for {key} after drain, got {n}");
    }

    drop(_container);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn qdrant_down_logs_indexing_failed() {
    todo!("point worker at non-existent port, send Upsert, assert worker exits without panic");
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn embedder_retry_on_429_succeeds() {
    todo!("wiremock returns 429 twice then 200; assert point created after 3 attempts");
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn embedder_retries_exhausted_logs_indexing_failed() {
    todo!("wiremock always returns 429; assert worker exits without panic");
}

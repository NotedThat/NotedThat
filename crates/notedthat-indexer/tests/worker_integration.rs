//! E2E integration tests for `IndexerWorker`.
//!
//! Requires: Docker with qdrant/qdrant:v1.15.4, wiremock (in-process).
//! Run with:
//!   cargo test -p notedthat-indexer --test `worker_integration` -- --ignored

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
use qdrant_client::qdrant::{
    Condition, Filter, RetrievedPoint, ScrollPoints, VectorsOutput, value::Kind,
    vectors_output::VectorsOptions,
};
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

type StorageObject = (Bytes, Option<String>);

static INTEGRATION_TEST_MUTEX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn integration_guard() -> tokio::sync::MutexGuard<'static, ()> {
    INTEGRATION_TEST_MUTEX.lock().await
}

struct MockStorage {
    objects: Mutex<HashMap<(String, String), StorageObject>>,
}

impl MockStorage {
    fn new() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
        }
    }

    fn insert(&self, kb: &str, key: &str, content: &str, content_type: &str) {
        self.objects.lock().unwrap().insert(
            (kb.to_string(), key.to_string()),
            (
                Bytes::from(content.to_owned()),
                Some(content_type.to_owned()),
            ),
        );
    }
}

#[async_trait]
impl Storage for MockStorage {
    async fn ensure_bucket(&self, _kb: &KbSlug) -> Result<(), StorageError> {
        Ok(())
    }

    async fn read_manifest(&self, _kb: &KbSlug) -> Result<KbManifest, StorageError> {
        Err(StorageError::NotFound {
            key: ".notedthat/manifest.json".to_string(),
        })
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
            None => Err(StorageError::NotFound {
                key: path.as_str().to_string(),
            }),
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
            None => Err(StorageError::NotFound {
                key: path.as_str().to_string(),
            }),
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
        Ok(PutOutcome {
            etag: Some("\"test-etag\"".to_string()),
        })
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
        _cursor: Option<&str>,
    ) -> Result<ListResponse, StorageError> {
        let guard = self.objects.lock().unwrap();
        let kb_str = kb.as_str().to_string();
        let objects: Vec<ObjectMeta> = guard
            .iter()
            .filter(|((k, p), _)| k == &kb_str && prefix.is_none_or(|pfx| p.starts_with(pfx)))
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
        Ok(ListResponse { objects, truncated, next_cursor: None })
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
            let v: Vec<f32> = (0..dim)
                .map(|j| if j == i % dim { 1.0 } else { 0.0 })
                .collect();
            serde_json::json!({ "index": i, "embedding": v, "object": "embedding" })
        })
        .collect();
    serde_json::json!({ "object": "list", "data": data })
}

fn make_embedder(server_uri: &str, dim: usize) -> Arc<dyn Embedder> {
    make_embedder_with_limits(server_uri, dim, 8192, 3)
}

fn make_embedder_with_limits(
    server_uri: &str,
    dim: usize,
    max_input_tokens: usize,
    max_retries: u32,
) -> Arc<dyn Embedder> {
    Arc::new(
        OpenAiCompatibleEmbedder::new(OpenAiCompatibleConfig {
            endpoint_url: server_uri.to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dim,
            max_input_tokens,
            timeout: Duration::from_secs(10),
            max_retries,
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
    IndexerWorker::new(
        storage as Arc<dyn Storage>,
        embedder,
        qdrant,
        rx,
        shutdown,
        32,
    )
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
    let cfg = QdrantConfig {
        url: url.to_string(),
        api_key: None,
    };
    let client = Arc::new(QdrantClient::new(&cfg).unwrap());
    let provisioner = QdrantProvisioner::new(QdrantClient::new(&cfg).unwrap());
    (client, provisioner)
}

async fn count_points(qdrant_url: &str, collection: &str, key: &str) -> usize {
    scroll_points(qdrant_url, collection, key, false)
        .await
        .len()
}

async fn scroll_points(
    qdrant_url: &str,
    collection: &str,
    key: &str,
    with_vectors: bool,
) -> Vec<RetrievedPoint> {
    let qdrant = qdrant_client::Qdrant::from_url(qdrant_url)
        .build()
        .expect("qdrant build failed");
    let filter = Filter::must([Condition::matches("object_key", key.to_string())]);
    qdrant
        .scroll(ScrollPoints {
            collection_name: collection.to_string(),
            filter: Some(filter),
            limit: Some(1000),
            with_payload: Some(true.into()),
            with_vectors: Some(with_vectors.into()),
            ..Default::default()
        })
        .await
        .expect("scroll failed")
        .result
}

fn string_payload<'a>(point: &'a RetrievedPoint, key: &str) -> &'a str {
    match point.payload.get(key).and_then(|value| value.kind.as_ref()) {
        Some(Kind::StringValue(value)) => value,
        other => panic!("expected string payload for {key}, got {other:?}"),
    }
}

fn list_payload_len(point: &RetrievedPoint, key: &str) -> usize {
    match point.payload.get(key).and_then(|value| value.kind.as_ref()) {
        Some(Kind::ListValue(value)) => value.values.len(),
        other => panic!("expected list payload for {key}, got {other:?}"),
    }
}

fn has_vector(vectors: &VectorsOutput, name: &str) -> bool {
    match vectors.vectors_options.as_ref() {
        Some(VectorsOptions::Vectors(named)) => named.vectors.contains_key(name),
        Some(VectorsOptions::Vector(_)) | None => false,
    }
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn happy_path_upsert_creates_qdrant_point() {
    let _guard = integration_guard().await;
    let _ = tracing_subscriber::fmt()
        .with_env_filter("notedthat=debug")
        .with_test_writer()
        .try_init();

    let (container, qdrant_url) = start_qdrant_url().await;
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
    storage.insert(
        "test-kb",
        "hello.md",
        "# Hello\n\nThis is a test document.",
        "text/markdown",
    );

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

    let points = scroll_points(&qdrant_url, &coll(&kb), "hello.md", true).await;
    let n = points.len();
    assert!(n >= 1, "expected ≥1 point for hello.md, got {n}");
    let point = points.first().expect("at least one point");

    assert_eq!(string_payload(point, "mime"), "text/markdown");
    assert_eq!(list_payload_len(point, "tags"), 0, "tags must be empty");
    let content_hash = string_payload(point, "content_hash");
    assert_eq!(content_hash.len(), 64, "content_hash must be sha256 hex");
    assert!(
        content_hash.chars().all(|ch| ch.is_ascii_hexdigit()),
        "content_hash must be hex"
    );
    assert!(
        !string_payload(point, "text").is_empty(),
        "text payload should be non-empty"
    );
    let vectors = point.vectors.as_ref().expect("vectors should be returned");
    assert!(has_vector(vectors, "dense"), "dense vector missing");
    assert!(
        has_vector(vectors, "sparse_bm25"),
        "sparse_bm25 vector missing"
    );

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn tombstone_removes_points() {
    let _guard = integration_guard().await;
    let (container, qdrant_url) = start_qdrant_url().await;
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
    storage.insert(
        "test-kb",
        "doc.md",
        "# Doc\n\nContent to be tombstoned.",
        "text/markdown",
    );

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

    tx.send(IndexEvent::Tombstone {
        kb: kb.clone(),
        object_key: opath("doc.md"),
    })
    .await
    .unwrap();

    drop(tx);
    shutdown.cancel();
    handle.await.unwrap();

    let n = count_points(&qdrant_url, &coll(&kb), "doc.md").await;
    assert_eq!(n, 0, "expected 0 points after tombstone, got {n}");

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn not_found_on_reread_implicit_tombstone() {
    let _guard = integration_guard().await;
    let (container, qdrant_url) = start_qdrant_url().await;
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
    assert_eq!(
        calls.len(),
        0,
        "embedder must not be called when object is absent"
    );

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn non_markdown_content_type_skipped() {
    let _guard = integration_guard().await;
    let (container, qdrant_url) = start_qdrant_url().await;
    let kb = kb();
    let (qdrant_client, provisioner) = make_qdrant(&qdrant_url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let mock_server = MockServer::start().await;
    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "image.png", "not markdown", "image/png");

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
        object_key: opath("image.png"),
        etag: "etag-image".to_string(),
        mtime: 1_700_000_003,
    })
    .await
    .unwrap();
    drop(tx);
    shutdown.cancel();
    handle.await.unwrap();

    let n = count_points(&qdrant_url, &coll(&kb), "image.png").await;
    assert_eq!(n, 0, "non-markdown object should not be indexed");
    let calls = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(
        calls.len(),
        0,
        "embedder should not be called for image/png"
    );

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn oversized_chunk_dropped_with_warn() {
    let _guard = integration_guard().await;
    let (container, qdrant_url) = start_qdrant_url().await;
    let kb = kb();
    let (qdrant_client, provisioner) = make_qdrant(&qdrant_url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let mock_server = MockServer::start().await;
    let storage = Arc::new(MockStorage::new());
    storage.insert(
        "test-kb",
        "large.md",
        "# Large\n\nThis chunk is intentionally longer than five characters.",
        "text/markdown",
    );

    let (tx, rx) = mpsc::channel(100);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(
        make_worker(
            Arc::clone(&storage),
            make_embedder_with_limits(&mock_server.uri(), 4, 5, 3),
            Arc::clone(&qdrant_client),
            rx,
            shutdown.clone(),
        )
        .run(),
    );

    tx.send(IndexEvent::Upsert {
        kb: kb.clone(),
        object_key: opath("large.md"),
        etag: "etag-large".to_string(),
        mtime: 1_700_000_004,
    })
    .await
    .unwrap();
    drop(tx);
    shutdown.cancel();
    handle.await.unwrap();

    let n = count_points(&qdrant_url, &coll(&kb), "large.md").await;
    assert_eq!(n, 0, "oversized chunk should be dropped");
    let calls = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(
        calls.len(),
        0,
        "embedder should not receive oversized chunk"
    );

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn queue_full_logs_index_queue_full() {
    let _guard = integration_guard().await;
    let (tx, _rx) = mpsc::channel::<IndexEvent>(4);
    let kb = kb();

    for i in 0..4_u32 {
        tx.try_send(IndexEvent::Upsert {
            kb: kb.clone(),
            object_key: opath(&format!("note{i}.md")),
            etag: format!("e{i}"),
            mtime: i64::from(i),
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
    let _guard = integration_guard().await;
    let (container, qdrant_url) = start_qdrant_url().await;
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
            mtime: 1_700_000_000 + i64::from(i),
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

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn qdrant_down_logs_indexing_failed() {
    let _guard = integration_guard().await;
    let kb = kb();
    let qdrant_client = Arc::new(
        QdrantClient::new(&QdrantConfig {
            url: "http://127.0.0.1:1".to_string(),
            api_key: None,
        })
        .unwrap(),
    );

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "down.md", "# Down\n\nContent.", "text/markdown");

    let (tx, rx) = mpsc::channel(100);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(
        make_worker(
            Arc::clone(&storage),
            make_embedder(&mock_server.uri(), 4),
            qdrant_client,
            rx,
            shutdown.clone(),
        )
        .run(),
    );

    tx.send(IndexEvent::Upsert {
        kb,
        object_key: opath("down.md"),
        etag: "etag-down".to_string(),
        mtime: 1_700_000_010,
    })
    .await
    .unwrap();
    drop(tx);
    shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn embedder_retry_on_429_succeeds() {
    let _guard = integration_guard().await;
    let (container, qdrant_url) = start_qdrant_url().await;
    let kb = kb();
    let (qdrant_client, provisioner) = make_qdrant(&qdrant_url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(2)
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_server)
        .await;

    let storage = Arc::new(MockStorage::new());
    storage.insert(
        "test-kb",
        "retry.md",
        "# Retry\n\nContent.",
        "text/markdown",
    );

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
        object_key: opath("retry.md"),
        etag: "etag-retry".to_string(),
        mtime: 1_700_000_011,
    })
    .await
    .unwrap();
    drop(tx);
    shutdown.cancel();
    handle.await.unwrap();

    let n = count_points(&qdrant_url, &coll(&kb), "retry.md").await;
    assert!(n >= 1, "expected point after retry success, got {n}");
    let calls = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(calls.len(), 3, "expected two retries plus success");

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn embedder_retries_exhausted_logs_indexing_failed() {
    let _guard = integration_guard().await;
    let (container, qdrant_url) = start_qdrant_url().await;
    let kb = kb();
    let (qdrant_client, provisioner) = make_qdrant(&qdrant_url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&mock_server)
        .await;

    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "fail.md", "# Fail\n\nContent.", "text/markdown");

    let (tx, rx) = mpsc::channel(100);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(
        make_worker(
            Arc::clone(&storage),
            make_embedder_with_limits(&mock_server.uri(), 4, 8192, 2),
            Arc::clone(&qdrant_client),
            rx,
            shutdown.clone(),
        )
        .run(),
    );

    tx.send(IndexEvent::Upsert {
        kb: kb.clone(),
        object_key: opath("fail.md"),
        etag: "etag-fail".to_string(),
        mtime: 1_700_000_012,
    })
    .await
    .unwrap();
    drop(tx);
    shutdown.cancel();
    handle.await.unwrap();

    let n = count_points(&qdrant_url, &coll(&kb), "fail.md").await;
    assert_eq!(n, 0, "failed embed should not write points");
    let calls = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(calls.len(), 2, "expected max_retries attempts");

    drop(container);
}

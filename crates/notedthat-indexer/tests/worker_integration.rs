//! E2E integration tests for `IndexerWorker`.
//!
//! Requires: Docker with qdrant/qdrant:v1.15.4, wiremock (in-process).
//! Run with:
//!   cargo test -p notedthat-indexer --test `worker_integration` -- --ignored

#![allow(missing_docs)]

mod support;
use support::start_qdrant;

use async_trait::async_trait;
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, CopyObjectOptions, KbManifest, KbSlug, ListResponse, ObjectMeta,
    ObjectPath, ObjectRead, ObjectStream, PutOutcome, StagedBody, Storage, StorageError,
};
use notedthat_indexer::chunker::stream_chunks;
use notedthat_indexer::{
    Embedder, EmbedderError, IndexEvent, IndexerWorker, OpenAiCompatibleConfig,
    OpenAiCompatibleEmbedder, QdrantClient, QdrantConfig, QdrantProvisioner,
};
use qdrant_client::qdrant::{
    Condition, Filter, RetrievedPoint, ScrollPoints, VectorsOutput, value::Kind,
    vectors_output::VectorsOptions,
};
use std::{
    collections::HashMap,
    fmt::Write as _,
    io::Cursor,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{io::AsyncReadExt, sync::mpsc};
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
    stream_calls: AtomicUsize,
    fail_stream_precondition: AtomicBool,
    fail_stream_midway: AtomicBool,
}

impl MockStorage {
    fn new() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
            stream_calls: AtomicUsize::new(0),
            fail_stream_precondition: AtomicBool::new(false),
            fail_stream_midway: AtomicBool::new(false),
        }
    }

    fn insert(&self, kb: &str, key: &str, content: &str, content_type: &str) {
        self.insert_bytes(kb, key, Bytes::from(content.to_owned()), content_type);
    }

    fn insert_bytes(&self, kb: &str, key: &str, content: Bytes, content_type: &str) {
        self.objects.lock().unwrap().insert(
            (kb.to_string(), key.to_string()),
            (content, Some(content_type.to_owned())),
        );
    }

    fn stream_calls(&self) -> usize {
        self.stream_calls.load(Ordering::SeqCst)
    }

    fn fail_next_stream_precondition(&self) {
        self.fail_stream_precondition.store(true, Ordering::SeqCst);
    }

    fn fail_next_stream_midway(&self) {
        self.fail_stream_midway.store(true, Ordering::SeqCst);
    }
}

struct ScriptedEmbedder {
    calls: AtomicUsize,
    batch_sizes: Mutex<Vec<usize>>,
    fail_call: Option<usize>,
    wrong_dimension_call: Option<usize>,
}

impl ScriptedEmbedder {
    fn new(fail_call: Option<usize>, wrong_dimension_call: Option<usize>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            batch_sizes: Mutex::new(Vec::new()),
            fail_call,
            wrong_dimension_call,
        }
    }

    fn batch_sizes(&self) -> Vec<usize> {
        self.batch_sizes.lock().unwrap().clone()
    }
}

#[async_trait]
impl Embedder for ScriptedEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.batch_sizes.lock().unwrap().push(texts.len());
        if self.fail_call == Some(call) {
            return Err(EmbedderError::Transport(
                "injected batch failure".to_owned(),
            ));
        }
        let dimension = if self.wrong_dimension_call == Some(call) {
            3
        } else {
            4
        };
        Ok(vec![vec![1.0; dimension]; texts.len()])
    }

    fn dim(&self) -> usize {
        4
    }

    fn max_input_tokens(&self) -> usize {
        40
    }

    fn model_id(&self) -> &'static str {
        "scripted-test"
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

    async fn get_object_stream(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        _range: Option<Vec<ByteRange>>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectStream, StorageError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_stream_precondition.swap(false, Ordering::SeqCst) {
            return Err(StorageError::PreconditionFailed);
        }
        let (bytes, content_type) = self
            .objects
            .lock()
            .unwrap()
            .get(&(kb.as_str().to_string(), path.as_str().to_string()))
            .cloned()
            .ok_or_else(|| StorageError::NotFound {
                key: path.as_str().to_string(),
            })?;
        let etag = "\"test-etag\"".to_string();
        if conditionals.if_match.as_deref() != Some(etag.as_str()) {
            return Err(StorageError::PreconditionFailed);
        }
        let mut chunks = bytes
            .chunks(3)
            .map(Bytes::copy_from_slice)
            .map(Ok)
            .collect::<Vec<Result<Bytes, StorageError>>>();
        if self.fail_stream_midway.swap(false, Ordering::SeqCst) {
            chunks.truncate(1);
            chunks.push(Err(StorageError::BackendUnavailable {
                message: "injected stream failure".to_owned(),
            }));
        }
        Ok(ObjectStream {
            chunks: Box::pin(futures::stream::iter(chunks)),
            meta: ObjectMeta {
                key: path.as_str().to_string(),
                size: bytes.len() as u64,
                last_modified: Some(1_700_000_000),
                content_type,
                etag: Some(etag),
            },
            content_range: None,
        })
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

    async fn put_staged_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        body: StagedBody,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        let mut reader = body.open().await.map_err(|source| StorageError::Other {
            source: Box::new(source),
        })?;
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .await
            .map_err(|source| StorageError::Other {
                source: Box::new(source),
            })?;
        self.put_object(kb, path, Bytes::from(bytes), content_type, conditionals)
            .await
    }

    async fn copy_object(
        &self,
        kb: &KbSlug,
        source: &ObjectPath,
        destination: &ObjectPath,
        options: CopyObjectOptions,
    ) -> Result<PutOutcome, StorageError> {
        let mut objects = self.objects.lock().unwrap();
        let source_key = (kb.as_str().to_owned(), source.as_str().to_owned());
        let destination_key = (kb.as_str().to_owned(), destination.as_str().to_owned());
        let (bytes, source_type) =
            objects
                .get(&source_key)
                .cloned()
                .ok_or_else(|| StorageError::NotFound {
                    key: source.as_str().to_owned(),
                })?;
        if options
            .source_if_match
            .as_deref()
            .is_some_and(|etag| etag != "\"test-etag\"")
            || (options.destination_if_none_match.as_deref() == Some("*")
                && objects.contains_key(&destination_key))
        {
            return Err(StorageError::PreconditionFailed);
        }
        objects.insert(
            destination_key,
            (bytes, options.content_type.or(source_type)),
        );
        Ok(PutOutcome {
            etag: Some("\"test-etag\"".to_owned()),
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
        Ok(ListResponse {
            objects,
            truncated,
            next_cursor: None,
        })
    }
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
    make_worker_with_batch(storage, embedder, qdrant, rx, shutdown, 32)
}

fn make_worker_with_batch(
    storage: Arc<MockStorage>,
    embedder: Arc<dyn Embedder>,
    qdrant: Arc<QdrantClient>,
    rx: mpsc::Receiver<IndexEvent>,
    shutdown: CancellationToken,
    batch_size: usize,
) -> IndexerWorker {
    IndexerWorker::new(
        storage as Arc<dyn Storage>,
        embedder,
        qdrant,
        rx,
        shutdown,
        batch_size,
    )
}

async fn index_once(
    storage: Arc<MockStorage>,
    embedder: Arc<dyn Embedder>,
    qdrant: Arc<QdrantClient>,
    kb: &KbSlug,
    key: &str,
    batch_size: usize,
) {
    let (tx, rx) = mpsc::channel(1);
    tx.send(IndexEvent::Upsert {
        kb: kb.clone(),
        object_key: opath(key),
        etag: "advisory-event-etag".to_owned(),
        mtime: 0,
    })
    .await
    .unwrap();
    drop(tx);
    make_worker_with_batch(
        storage,
        embedder,
        qdrant,
        rx,
        CancellationToken::new(),
        batch_size,
    )
    .run()
    .await;
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
        ..Default::default()
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
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10))
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

fn string_list_payload<'a>(point: &'a RetrievedPoint, key: &str) -> Vec<&'a str> {
    match point.payload.get(key).and_then(|value| value.kind.as_ref()) {
        Some(Kind::ListValue(value)) => value
            .values
            .iter()
            .map(|value| match value.kind.as_ref() {
                Some(Kind::StringValue(value)) => value.as_str(),
                other => panic!("expected string in {key}, got {other:?}"),
            })
            .collect(),
        other => panic!("expected list payload for {key}, got {other:?}"),
    }
}

fn integer_payload(point: &RetrievedPoint, key: &str) -> i64 {
    match point.payload.get(key).and_then(|value| value.kind.as_ref()) {
        Some(Kind::IntegerValue(value)) => *value,
        other => panic!("expected integer payload for {key}, got {other:?}"),
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

    let (container, qdrant_url) = start_qdrant().await;
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
    let (container, qdrant_url) = start_qdrant().await;
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
    let (container, qdrant_url) = start_qdrant().await;
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
    let (container, qdrant_url) = start_qdrant().await;
    let kb = kb();
    let (qdrant_client, provisioner) = make_qdrant(&qdrant_url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .expect(1)
        .mount(&mock_server)
        .await;
    let storage = Arc::new(MockStorage::new());
    storage.insert(
        "test-kb",
        "image.png",
        "old searchable text",
        "text/markdown",
    );

    let (seed_tx, seed_rx) = mpsc::channel(1);
    seed_tx
        .send(IndexEvent::Upsert {
            kb: kb.clone(),
            object_key: opath("image.png"),
            etag: "etag-old".to_string(),
            mtime: 1_700_000_002,
        })
        .await
        .unwrap();
    drop(seed_tx);
    make_worker(
        Arc::clone(&storage),
        make_embedder(&mock_server.uri(), 4),
        Arc::clone(&qdrant_client),
        seed_rx,
        CancellationToken::new(),
    )
    .run()
    .await;
    assert_eq!(count_points(&qdrant_url, &coll(&kb), "image.png").await, 1);

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
    assert_eq!(
        storage.stream_calls(),
        1,
        "non-indexable replacement must be deleted after HEAD without a body stream"
    );
    let calls = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(
        calls.len(),
        1,
        "embedder should be called only for the original Markdown"
    );

    drop(container);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn shrinking_replacement_removes_stale_okf_points() {
    let _guard = integration_guard().await;
    let (_container, url) = start_qdrant().await;
    let kb = kb();
    let (qdrant, provisioner) = make_qdrant(&url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    let replacements = [
        ("large ".repeat(30), "old-tag"),
        ("short".to_owned(), "current-tag"),
    ];
    let mut previous_count = None;
    for (body, tag) in replacements {
        let expected_points = stream_chunks(Cursor::new(body.as_bytes()), 8, 0)
            .expect("bounded chunks")
            .count();
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
            .expect(u64::try_from(expected_points).expect("small fixture"))
            .mount(&mock)
            .await;
        let raw = format!("---\ntype: Metric\ntags: [{tag}]\n---\n{body}");
        storage.insert("test-kb", "metric.md", &raw, "text/markdown");
        let (tx, rx) = mpsc::channel(1);
        tx.send(IndexEvent::Upsert {
            kb: kb.clone(),
            object_key: opath("metric.md"),
            etag: "test".to_owned(),
            mtime: 0,
        })
        .await
        .unwrap();
        drop(tx);
        make_worker_with_batch(
            Arc::clone(&storage),
            make_embedder_with_limits(&mock.uri(), 4, 32, 1),
            Arc::clone(&qdrant),
            rx,
            CancellationToken::new(),
            1,
        )
        .run()
        .await;
        let points = scroll_points(&url, &coll(&kb), "metric.md", false).await;
        assert_eq!(
            points.len(),
            expected_points,
            "replacement must remove points beyond its new contiguous chunk count"
        );
        assert!(
            points
                .iter()
                .all(|point| string_list_payload(point, "tags") == [tag]),
            "every surviving point must carry only current OKF metadata"
        );
        for point in &points {
            let start = usize::try_from(integer_payload(point, "byte_start"))
                .expect("nonnegative fixture offset");
            let end = usize::try_from(integer_payload(point, "byte_end"))
                .expect("nonnegative fixture offset");
            assert_eq!(&raw[start..end], string_payload(point, "text"));
            assert!(body.contains(string_payload(point, "text")));
        }
        if let Some(previous_count) = previous_count {
            assert!(expected_points < previous_count);
        }
        previous_count = Some(expected_points);
    }

    storage.insert("test-kb", "metric.md", "", "text/markdown");
    let empty_mock = MockServer::start().await;
    let (tx, rx) = mpsc::channel(1);
    tx.send(IndexEvent::Upsert {
        kb: kb.clone(),
        object_key: opath("metric.md"),
        etag: "empty".to_owned(),
        mtime: 0,
    })
    .await
    .unwrap();
    drop(tx);
    make_worker(
        Arc::clone(&storage),
        make_embedder(&empty_mock.uri(), 4),
        Arc::clone(&qdrant),
        rx,
        CancellationToken::new(),
    )
    .run()
    .await;
    assert_eq!(count_points(&url, &coll(&kb), "metric.md").await, 0);
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn oversized_chunk_is_split_without_dropping_content() {
    let _guard = integration_guard().await;
    let (container, qdrant_url) = start_qdrant().await;
    let kb = kb();
    let (qdrant_client, provisioner) = make_qdrant(&qdrant_url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let mock_server = MockServer::start().await;
    let source = "# Large\n\nThis chunk is intentionally longer than five characters.";
    let expected_chunks = stream_chunks(Cursor::new(source.as_bytes()), 5, 0)
        .expect("bounded chunks")
        .count();
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(embedding_response(4, expected_chunks)),
        )
        .expect(1)
        .mount(&mock_server)
        .await;
    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "large.md", source, "text/markdown");

    let (tx, rx) = mpsc::channel(100);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(
        make_worker(
            Arc::clone(&storage),
            make_embedder_with_limits(&mock_server.uri(), 4, 20, 3),
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

    let points = scroll_points(&qdrant_url, &coll(&kb), "large.md", false).await;
    assert_eq!(points.len(), expected_chunks);
    let mut ranges = points
        .iter()
        .map(|point| {
            (
                integer_payload(point, "chunk_index"),
                integer_payload(point, "byte_start"),
                integer_payload(point, "byte_end"),
                string_payload(point, "text"),
            )
        })
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| range.0);
    for (expected_index, (index, start, end, text)) in ranges.into_iter().enumerate() {
        assert_eq!(index, i64::try_from(expected_index).expect("small fixture"));
        let start = usize::try_from(start).expect("nonnegative offset");
        let end = usize::try_from(end).expect("nonnegative offset");
        assert_eq!(&source[start..end], text);
        assert!(text.chars().count() <= 5);
    }
    let calls = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(calls.len(), 1, "one bounded batch should be embedded");

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
    let (container, qdrant_url) = start_qdrant().await;
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
            ..Default::default()
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
    let (container, qdrant_url) = start_qdrant().await;
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
    let (container, qdrant_url) = start_qdrant().await;
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

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn failed_embed_and_upsert_batches_preserve_stale_points_until_repair() {
    let _guard = integration_guard().await;
    let (_container, url) = start_qdrant().await;
    let kb = kb();
    let (qdrant, provisioner) = make_qdrant(&url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    let old = (0..8).fold(String::new(), |mut output, index| {
        write!(output, "# Old {index}\nold-{index}\n").expect("write fixture");
        output
    });
    storage.insert("test-kb", "repair.md", &old, "text/markdown");
    index_once(
        Arc::clone(&storage),
        Arc::new(ScriptedEmbedder::new(None, None)),
        Arc::clone(&qdrant),
        &kb,
        "repair.md",
        2,
    )
    .await;
    let old_count = count_points(&url, &coll(&kb), "repair.md").await;
    assert!(old_count > 2, "fixture must span more than one batch");

    let replacement = (0..6).fold(String::new(), |mut output, index| {
        write!(output, "# New {index}\nnew-{index}\n").expect("write fixture");
        output
    });
    storage.insert("test-kb", "repair.md", &replacement, "text/markdown");
    let embed_failure = Arc::new(ScriptedEmbedder::new(Some(2), None));
    index_once(
        Arc::clone(&storage),
        Arc::clone(&embed_failure) as Arc<dyn Embedder>,
        Arc::clone(&qdrant),
        &kb,
        "repair.md",
        2,
    )
    .await;
    assert_eq!(
        count_points(&url, &coll(&kb), "repair.md").await,
        old_count,
        "embed failure must not run final stale cleanup"
    );
    assert_eq!(embed_failure.batch_sizes(), [2, 2]);

    index_once(
        Arc::clone(&storage),
        Arc::new(ScriptedEmbedder::new(None, None)),
        Arc::clone(&qdrant),
        &kb,
        "repair.md",
        2,
    )
    .await;
    let repaired = scroll_points(&url, &coll(&kb), "repair.md", false).await;
    assert!(
        repaired
            .iter()
            .all(|point| !string_payload(point, "text").contains("old-"))
    );

    let newest = replacement.replace("new-", "newest-");
    storage.insert("test-kb", "repair.md", &newest, "text/markdown");
    let upsert_failure = Arc::new(ScriptedEmbedder::new(None, Some(2)));
    index_once(
        Arc::clone(&storage),
        Arc::clone(&upsert_failure) as Arc<dyn Embedder>,
        Arc::clone(&qdrant),
        &kb,
        "repair.md",
        2,
    )
    .await;
    assert_eq!(upsert_failure.batch_sizes(), [2, 2]);
    assert_eq!(
        count_points(&url, &coll(&kb), "repair.md").await,
        repaired.len(),
        "upsert failure must not run final stale cleanup"
    );

    index_once(
        Arc::clone(&storage),
        Arc::new(ScriptedEmbedder::new(None, None)),
        Arc::clone(&qdrant),
        &kb,
        "repair.md",
        2,
    )
    .await;
    let repaired = scroll_points(&url, &coll(&kb), "repair.md", false).await;
    assert!(
        repaired
            .iter()
            .all(|point| !string_payload(point, "text").contains("new-")
                || string_payload(point, "text").contains("newest-"))
    );
}

#[tokio::test]
#[ignore = "requires qdrant/qdrant:v1.15.4 testcontainer"]
async fn unstable_or_invalid_stream_preserves_last_complete_index_until_repair() {
    let _guard = integration_guard().await;
    let (_container, url) = start_qdrant().await;
    let kb = kb();
    let (qdrant, provisioner) = make_qdrant(&url);
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "stable.md", "old stable", "text/markdown");
    index_once(
        Arc::clone(&storage),
        Arc::new(ScriptedEmbedder::new(None, None)),
        Arc::clone(&qdrant),
        &kb,
        "stable.md",
        2,
    )
    .await;

    storage.insert("test-kb", "stable.md", "new stable", "text/markdown");
    let unused_embedder = Arc::new(ScriptedEmbedder::new(None, None));
    storage.fail_next_stream_precondition();
    index_once(
        Arc::clone(&storage),
        Arc::clone(&unused_embedder) as Arc<dyn Embedder>,
        Arc::clone(&qdrant),
        &kb,
        "stable.md",
        2,
    )
    .await;
    storage.fail_next_stream_midway();
    index_once(
        Arc::clone(&storage),
        Arc::clone(&unused_embedder) as Arc<dyn Embedder>,
        Arc::clone(&qdrant),
        &kb,
        "stable.md",
        2,
    )
    .await;
    storage.insert_bytes(
        "test-kb",
        "stable.md",
        Bytes::from_static(b"new\xffstable"),
        "text/markdown",
    );
    index_once(
        Arc::clone(&storage),
        Arc::clone(&unused_embedder) as Arc<dyn Embedder>,
        Arc::clone(&qdrant),
        &kb,
        "stable.md",
        2,
    )
    .await;
    assert!(unused_embedder.batch_sizes().is_empty());
    let preserved = scroll_points(&url, &coll(&kb), "stable.md", false).await;
    assert_eq!(preserved.len(), 1);
    assert_eq!(string_payload(&preserved[0], "text"), "old stable");

    storage.insert("test-kb", "stable.md", "new stable", "text/markdown");
    index_once(
        Arc::clone(&storage),
        Arc::new(ScriptedEmbedder::new(None, None)),
        Arc::clone(&qdrant),
        &kb,
        "stable.md",
        2,
    )
    .await;
    let repaired = scroll_points(&url, &coll(&kb), "stable.md", false).await;
    assert_eq!(repaired.len(), 1);
    assert_eq!(string_payload(&repaired[0], "text"), "new stable");
}

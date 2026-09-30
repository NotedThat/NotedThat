//! E2E integration tests for `IndexerWorker`.
//!
//! The subject is the worker's pipeline — chunking, batching, payload
//! construction, tombstones, obsolete-chunk cleanup and drain-on-shutdown — so
//! it runs against [`InMemoryVectorStore`] and an in-process wiremock embedder,
//! with no container. The store reports what was written in the same
//! `RetrievedPoint` shape a Qdrant `scroll` returns, so every payload and vector
//! assertion below is unchanged.
//!
//! Run with:
//!   cargo test -p notedthat-indexer --test `worker_integration`

#![allow(missing_docs)]

use async_trait::async_trait;
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, CopyObjectOptions, KbManifest, KbSlug, ListResponse, ObjectMeta,
    ObjectPath, ObjectRead, ObjectStream, PutOutcome, StagedBody, Storage, StorageError,
};
use notedthat_indexer::chunker::stream_chunks;
use notedthat_indexer::testing::InMemoryVectorStore;
use notedthat_indexer::vector_store::{
    HybridQuery, IndexedObject, PayloadFieldKind, PointSelector, VectorStore, VectorStoreError,
};
use notedthat_indexer::{
    Embedder, EmbedderError, IndexEvent, IndexHealth, IndexState, IndexerWorker,
    OpenAiCompatibleConfig, OpenAiCompatibleEmbedder, QdrantProvisioner, RefreshOrigin,
    index_queue,
};
use qdrant_client::qdrant::{
    RetrievedPoint, VectorsOutput, point_id::PointIdOptions, value::Kind,
    vectors_output::VectorsOptions,
};
use std::{
    collections::HashMap,
    fmt::Write as _,
    io::Cursor,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::{Semaphore, mpsc},
};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

type StorageObject = (Bytes, Option<String>);

struct MockStorage {
    objects: Mutex<HashMap<(String, String), StorageObject>>,
    stream_calls: AtomicUsize,
    fail_next_head: AtomicBool,
    fail_stream_precondition: AtomicBool,
    fail_stream_midway: AtomicBool,
}

impl MockStorage {
    fn new() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
            stream_calls: AtomicUsize::new(0),
            fail_next_head: AtomicBool::new(false),
            fail_stream_precondition: AtomicBool::new(false),
            fail_stream_midway: AtomicBool::new(false),
        }
    }

    /// An `ETag` derived from the bytes, the way a real backend produces one.
    ///
    /// A constant would make every object look unchanged forever, which is precisely
    /// the condition `Skip::IfUnchanged` turns on — so the stub has to vary it.
    fn etag_for(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("\"{:x}\"", Sha256::digest(bytes))
    }

    fn insert(&self, kb: &str, key: &str, content: &str, content_type: &str) {
        self.insert_bytes(kb, key, Bytes::from(content.to_owned()), content_type);
    }

    fn remove(&self, kb: &str, key: &str) {
        self.objects
            .lock()
            .unwrap()
            .remove(&(kb.to_string(), key.to_string()));
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

    /// The next `HEAD` fails as an unreachable backend would, before the
    /// worker learns anything about the object.
    fn fail_next_head(&self) {
        self.fail_next_head.store(true, Ordering::SeqCst);
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

    /// How many embedding requests were made — the number this feature exists to keep down.
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
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

struct BlockingEmbedder {
    started: Arc<Semaphore>,
    release: Arc<Semaphore>,
    /// Embed calls currently inside `embed`, and the most there ever were.
    running: AtomicUsize,
    peak: AtomicUsize,
}

impl BlockingEmbedder {
    fn new() -> Self {
        Self {
            started: Arc::new(Semaphore::new(0)),
            release: Arc::new(Semaphore::new(0)),
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    /// Wait until `n` more embed calls have begun.
    async fn wait_started(&self, n: u32) {
        tokio::time::timeout(Duration::from_secs(10), self.started.acquire_many(n))
            .await
            .expect("embed calls did not start in time")
            .unwrap()
            .forget();
    }

    /// True when another embed call begins within a short grace period.
    async fn another_starts(&self) -> bool {
        tokio::time::timeout(Duration::from_millis(100), self.started.acquire())
            .await
            .is_ok_and(|permit| {
                permit.unwrap().forget();
                true
            })
    }

    /// Let every current and future embed call through.
    fn release_all(&self) {
        self.release.add_permits(Semaphore::MAX_PERMITS / 2);
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Embedder for BlockingEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedderError> {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        self.started.add_permits(1);
        // Consumed, not returned: each release permit lets exactly one call
        // through, so a test can open one slot at a time.
        self.release
            .acquire()
            .await
            .expect("test semaphore stays open")
            .forget();
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(vec![vec![1.0; 4]; texts.len()])
    }

    fn dim(&self) -> usize {
        4
    }

    fn max_input_tokens(&self) -> usize {
        40
    }

    fn model_id(&self) -> &'static str {
        "blocking-test"
    }
}

#[async_trait]
impl Storage for MockStorage {
    async fn probe(&self, _kb: &KbSlug) -> Result<(), StorageError> {
        Ok(())
    }

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
        if self.fail_next_head.swap(false, Ordering::SeqCst) {
            return Err(StorageError::BackendUnavailable {
                message: "injected head failure".to_owned(),
            });
        }
        let guard = self.objects.lock().unwrap();
        match guard.get(&(kb.as_str().to_string(), path.as_str().to_string())) {
            Some((bytes, content_type)) => Ok(ObjectMeta {
                key: path.as_str().to_string(),
                size: bytes.len() as u64,
                last_modified: Some(1_700_000_000),
                content_type: content_type.clone(),
                etag: Some(Self::etag_for(bytes)),
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
        _range: Option<ByteRange>,
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
                    etag: Some(Self::etag_for(bytes)),
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
        _range: Option<ByteRange>,
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
        let etag = Self::etag_for(&bytes);
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
        let replaced = self.objects.lock().unwrap().insert(
            (kb.as_str().to_string(), path.as_str().to_string()),
            (bytes, content_type.map(str::to_string)),
        );
        Ok(PutOutcome {
            etag: Some("\"test-etag\"".to_string()),
            created: replaced.is_none(),
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
            .is_some_and(|etag| etag != Self::etag_for(&bytes))
            || (options.destination_if_none_match.as_deref() == Some("*")
                && objects.contains_key(&destination_key))
        {
            return Err(StorageError::PreconditionFailed);
        }
        let etag = Self::etag_for(&bytes);
        let replaced = objects.insert(
            destination_key,
            (bytes, options.content_type.or(source_type)),
        );
        Ok(PutOutcome {
            etag: Some(etag),
            created: replaced.is_none(),
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
                etag: Some(Self::etag_for(bytes)),
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
    store: Arc<dyn VectorStore>,
    rx: mpsc::Receiver<IndexEvent>,
    shutdown: CancellationToken,
) -> IndexerWorker {
    make_worker_with_batch(storage, embedder, store, rx, shutdown, 32)
}

fn make_worker_with_batch(
    storage: Arc<MockStorage>,
    embedder: Arc<dyn Embedder>,
    store: Arc<dyn VectorStore>,
    rx: mpsc::Receiver<IndexEvent>,
    shutdown: CancellationToken,
    batch_size: usize,
) -> IndexerWorker {
    IndexerWorker::new(
        storage as Arc<dyn Storage>,
        embedder,
        store,
        rx,
        shutdown,
        batch_size,
    )
}

/// A worker over the bounded production queue, running up to `concurrency`
/// files at once, with its sender.
fn make_queue_worker(
    storage: Arc<MockStorage>,
    embedder: Arc<dyn Embedder>,
    store: Arc<dyn VectorStore>,
    shutdown: CancellationToken,
    capacity: usize,
    concurrency: usize,
) -> (notedthat_indexer::IndexQueueSender, IndexerWorker) {
    let (tx, rx) = index_queue(capacity);
    let worker = IndexerWorker::new_queue(
        storage as Arc<dyn Storage>,
        embedder,
        store,
        rx,
        shutdown,
        32,
        NonZeroUsize::new(concurrency).expect("tests use a positive concurrency"),
    );
    (tx, worker)
}

fn upsert(kb: &KbSlug, key: &str) -> IndexEvent {
    IndexEvent::Upsert {
        kb: kb.clone(),
        object_key: opath(key),
        etag: format!("etag-{key}"),
        mtime: 0,
    }
}

async fn index_once(
    storage: Arc<MockStorage>,
    embedder: Arc<dyn Embedder>,
    store: Arc<dyn VectorStore>,
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
        store,
        rx,
        CancellationToken::new(),
        batch_size,
    )
    .run()
    .await;
}

/// Drive one `Refresh` through a worker, the way the filesystem watcher does.
async fn refresh_once(
    storage: Arc<MockStorage>,
    embedder: Arc<dyn Embedder>,
    store: Arc<dyn VectorStore>,
    kb: &KbSlug,
    key: &str,
) {
    let (tx, rx) = mpsc::channel(1);
    tx.send(IndexEvent::Refresh {
        kb: kb.clone(),
        object_key: opath(key),
        origin: RefreshOrigin::Watch,
    })
    .await
    .unwrap();
    drop(tx);
    make_worker_with_batch(storage, embedder, store, rx, CancellationToken::new(), 32)
        .run()
        .await;
}

fn kb() -> KbSlug {
    KbSlug::try_new("test-kb").unwrap()
}

fn opath(s: &str) -> ObjectPath {
    ObjectPath::try_from(s).unwrap()
}

/// A store and a provisioner sharing it, mirroring how the server wires one
/// backend into both.
fn make_store() -> (InMemoryVectorStore, QdrantProvisioner) {
    let store = InMemoryVectorStore::new();
    let provisioner = QdrantProvisioner::new(Arc::new(store.clone()));
    (store, provisioner)
}

async fn count_points(store: &InMemoryVectorStore, kb: &KbSlug, key: &str) -> usize {
    scroll_points(store, kb, key, false).await.len()
}

async fn scroll_points(
    store: &InMemoryVectorStore,
    kb: &KbSlug,
    key: &str,
    with_vectors: bool,
) -> Vec<RetrievedPoint> {
    store.scroll_object(kb, key, with_vectors).await
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

fn numeric_id(point: &RetrievedPoint) -> u64 {
    match point
        .id
        .as_ref()
        .and_then(|id| id.point_id_options.as_ref())
    {
        Some(PointIdOptions::Num(id)) => *id,
        other => panic!("expected a numeric point id, got {other:?}"),
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
async fn happy_path_upsert_creates_qdrant_point() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("notedthat=debug")
        .with_test_writer()
        .try_init();

    let kb = kb();
    let (store, provisioner) = make_store();
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
            Arc::new(store.clone()),
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

    let points = scroll_points(&store, &kb, "hello.md", true).await;
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
}

#[tokio::test]
async fn tombstone_removes_points() {
    let kb = kb();
    let (store, provisioner) = make_store();
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
            Arc::new(store.clone()),
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

    let n = count_points(&store, &kb, "doc.md").await;
    assert_eq!(n, 0, "expected 0 points after tombstone, got {n}");
}

#[tokio::test]
async fn not_found_on_reread_implicit_tombstone() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let mock_server = MockServer::start().await;
    let storage = Arc::new(MockStorage::new());

    let (tx, rx) = mpsc::channel(100);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(
        make_worker(
            Arc::clone(&storage),
            make_embedder(&mock_server.uri(), 4),
            Arc::new(store.clone()),
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

    let n = count_points(&store, &kb, "missing.md").await;
    assert_eq!(n, 0, "implicit tombstone should produce 0 points, got {n}");

    let calls = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(
        calls.len(),
        0,
        "embedder must not be called when object is absent"
    );
}

/// A `Content-Type` with parameters or odd casing is indexed as its media type,
/// so the `mime` filter a caller naturally writes finds it (#286).
#[tokio::test]
async fn parameterised_content_type_is_found_by_its_media_type() {
    let kb = kb();
    let (store, provisioner) = make_store();
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
        "Text/Markdown; charset=utf-8",
    );
    index_once(
        storage,
        make_embedder(&mock_server.uri(), 4),
        Arc::new(store.clone()),
        &kb,
        "hello.md",
        16,
    )
    .await;

    let points = scroll_points(&store, &kb, "hello.md", false).await;
    assert!(!points.is_empty(), "hello.md was not indexed");
    for point in &points {
        assert_eq!(string_payload(point, "mime"), "text/markdown");
    }

    let hits = store
        .hybrid_search(
            &kb,
            HybridQuery {
                text: "test document".to_string(),
                dense: vec![1.0; 4],
                filter: Some(notedthat_core::search::SearchFilter {
                    mime: Some("text/markdown".to_string()),
                    ..Default::default()
                }),
                prefetch_limit: 10,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert!(!hits.is_empty(), "mime filter text/markdown found nothing");
}

#[tokio::test]
async fn non_markdown_content_type_skipped() {
    let kb = kb();
    let (store, provisioner) = make_store();
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
        Arc::new(store.clone()),
        seed_rx,
        CancellationToken::new(),
    )
    .run()
    .await;
    assert_eq!(count_points(&store, &kb, "image.png").await, 1);

    storage.insert("test-kb", "image.png", "not markdown", "image/png");

    let (tx, rx) = mpsc::channel(100);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(
        make_worker(
            Arc::clone(&storage),
            make_embedder(&mock_server.uri(), 4),
            Arc::new(store.clone()),
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

    let n = count_points(&store, &kb, "image.png").await;
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
}

#[tokio::test]
async fn shrinking_replacement_removes_stale_okf_points() {
    let kb = kb();
    let (store, provisioner) = make_store();
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
            Arc::new(store.clone()),
            rx,
            CancellationToken::new(),
            1,
        )
        .run()
        .await;
        let points = scroll_points(&store, &kb, "metric.md", false).await;
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
        Arc::new(store.clone()),
        rx,
        CancellationToken::new(),
    )
    .run()
    .await;
    assert_eq!(count_points(&store, &kb, "metric.md").await, 0);
}

#[tokio::test]
async fn oversized_chunk_is_split_without_dropping_content() {
    let kb = kb();
    let (store, provisioner) = make_store();
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
            Arc::new(store.clone()),
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

    let points = scroll_points(&store, &kb, "large.md", false).await;
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
}

#[tokio::test]
async fn queue_full_logs_index_queue_full() {
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
async fn bounded_queue_keeps_active_handlers_within_its_limit() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "one.md", "# One", "text/markdown");
    storage.insert("test-kb", "two.md", "# Two", "text/markdown");
    let embedder = Arc::new(BlockingEmbedder::new());
    let (tx, worker) = make_queue_worker(
        storage,
        Arc::clone(&embedder) as Arc<dyn Embedder>,
        Arc::new(store),
        CancellationToken::new(),
        2,
        2,
    );
    let handle = tokio::spawn(worker.run());

    for key in ["one.md", "two.md"] {
        tx.send(upsert(&kb, key)).await.unwrap();
    }
    embedder.wait_started(2).await;
    assert_eq!(tx.depth(), 2, "active handlers retain their queue permits");
    assert!(matches!(
        tx.try_send(upsert(&kb, "three.md")),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_))
    ));

    embedder.release_all();
    drop(tx);
    handle.await.unwrap();
}

/// Queue `files` distinct files on a worker limited to `limit`, and check that
/// exactly `limit` run at once: no more start while they are held, a finished
/// one frees exactly one slot, and every file is indexed in the end.
///
/// The queue is far larger than the limit, so what holds the rest back is the
/// concurrency bound, not a full queue.
async fn assert_runs_at_most(limit: usize, files: usize) {
    assert!(files > limit, "the check needs files left waiting");
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    let keys: Vec<String> = (0..files).map(|i| format!("file{i}.md")).collect();
    for key in &keys {
        storage.insert("test-kb", key, &format!("# {key}"), "text/markdown");
    }
    let embedder = Arc::new(BlockingEmbedder::new());
    let (tx, worker) = make_queue_worker(
        storage,
        Arc::clone(&embedder) as Arc<dyn Embedder>,
        Arc::new(store.clone()),
        CancellationToken::new(),
        64,
        limit,
    );
    let handle = tokio::spawn(worker.run());

    for key in &keys {
        tx.send(upsert(&kb, key)).await.unwrap();
    }
    embedder.wait_started(u32::try_from(limit).unwrap()).await;
    assert!(
        !embedder.another_starts().await,
        "no file beyond the limit of {limit} may start while {limit} are running"
    );
    assert_eq!(tx.depth(), files, "every file was accepted and is waiting");

    embedder.release.add_permits(1);
    embedder.wait_started(1).await;
    assert!(
        !embedder.another_starts().await,
        "one finished file frees exactly one slot"
    );

    embedder.release_all();
    drop(tx);
    handle.await.unwrap();
    assert_eq!(embedder.peak(), limit, "files running at once");
    for key in &keys {
        assert!(
            count_points(&store, &kb, key).await > 0,
            "{key} was indexed"
        );
    }
}

#[tokio::test]
async fn concurrency_limit_caps_running_files() {
    assert_runs_at_most(2, 5).await;
}

#[tokio::test]
async fn concurrency_of_one_indexes_files_serially() {
    assert_runs_at_most(1, 3).await;
}

#[tokio::test]
async fn concurrency_of_eight_indexes_eight_files_at_once() {
    assert_runs_at_most(8, 10).await;
}

/// Run an upsert of `a.md` that is held mid-embedding, queue `delete` for the
/// same file behind it and an upsert of `b.md` beside it, then let everything
/// finish. The delete must wait for the upsert, or the upsert's chunks would
/// land after it and outlive the file; the unrelated file must not wait.
async fn assert_delete_waits_for_update(delete: IndexEvent, remove_object: bool) {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "a.md", "# A\n\nalpha", "text/markdown");
    storage.insert("test-kb", "b.md", "# B\n\nbeta", "text/markdown");
    let embedder = Arc::new(BlockingEmbedder::new());
    let (tx, worker) = make_queue_worker(
        Arc::clone(&storage),
        Arc::clone(&embedder) as Arc<dyn Embedder>,
        Arc::new(store.clone()),
        CancellationToken::new(),
        16,
        8,
    );
    let handle = tokio::spawn(worker.run());

    tx.send(upsert(&kb, "a.md")).await.unwrap();
    embedder.wait_started(1).await;
    if remove_object {
        storage.remove("test-kb", "a.md");
    }
    tx.send(delete).await.unwrap();
    tx.send(upsert(&kb, "b.md")).await.unwrap();
    // b.md embeds while a.md is still held: different files overlap.
    embedder.wait_started(1).await;

    embedder.release_all();
    drop(tx);
    handle.await.unwrap();
    assert_eq!(
        count_points(&store, &kb, "a.md").await,
        0,
        "the delete ran after the update, so no chunks survive it"
    );
    assert!(count_points(&store, &kb, "b.md").await > 0);
}

#[tokio::test]
async fn a_tombstone_waits_for_an_update_of_the_same_file() {
    let delete = IndexEvent::Tombstone {
        kb: kb(),
        object_key: opath("a.md"),
    };
    assert_delete_waits_for_update(delete, false).await;
}

#[tokio::test]
async fn a_refresh_of_a_deleted_file_waits_for_its_update() {
    let delete = IndexEvent::Refresh {
        kb: kb(),
        object_key: opath("a.md"),
        origin: RefreshOrigin::Watch,
    };
    assert_delete_waits_for_update(delete, true).await;
}

#[tokio::test]
async fn updates_of_the_same_file_run_in_order_and_keep_the_last_content() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "serial.md", "# First\n\nalpha", "text/markdown");
    let embedder = Arc::new(BlockingEmbedder::new());
    let (tx, worker) = make_queue_worker(
        Arc::clone(&storage),
        Arc::clone(&embedder) as Arc<dyn Embedder>,
        Arc::new(store.clone()),
        CancellationToken::new(),
        16,
        8,
    );
    let handle = tokio::spawn(worker.run());

    tx.send(upsert(&kb, "serial.md")).await.unwrap();
    embedder.wait_started(1).await;
    storage.insert("test-kb", "serial.md", "# Second\n\nbeta", "text/markdown");
    tx.send(upsert(&kb, "serial.md")).await.unwrap();
    assert!(
        !embedder.another_starts().await,
        "a second event for the active path must not start"
    );

    embedder.release_all();
    drop(tx);
    handle.await.unwrap();
    let points = scroll_points(&store, &kb, "serial.md", false).await;
    assert!(!points.is_empty());
    for point in &points {
        let text = string_payload(point, "text");
        assert!(
            !text.contains("alpha"),
            "stale first version survived: {text}"
        );
    }
    assert!(
        points
            .iter()
            .any(|point| string_payload(point, "text").contains("beta")),
        "the second version is indexed"
    );
    assert_eq!(embedder.peak(), 1, "the two updates never overlapped");
}

#[tokio::test]
async fn graceful_shutdown_drains_lanes_and_running_files_with_concurrency() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    let keys = ["d0.md", "d1.md", "d2.md", "d3.md"];
    for key in keys {
        storage.insert("test-kb", key, &format!("# {key}"), "text/markdown");
    }
    let embedder = Arc::new(BlockingEmbedder::new());
    let health = Arc::new(IndexHealth::new());
    let shutdown = CancellationToken::new();
    let (tx, worker) = make_queue_worker(
        storage,
        Arc::clone(&embedder) as Arc<dyn Embedder>,
        Arc::new(store.clone()),
        shutdown.clone(),
        16,
        2,
    );
    let handle = tokio::spawn(worker.with_health(Arc::clone(&health)).run());

    // Four files plus a second event for d0.md, which has to wait in its lane.
    for key in keys.iter().chain(["d0.md"].iter()) {
        health.enqueued("test-kb");
        tx.send(upsert(&kb, key)).await.unwrap();
    }
    embedder.wait_started(2).await;

    // Shut down with two files running and three events still queued; the
    // sender stays open, so only the drain can finish them.
    shutdown.cancel();
    embedder.release_all();
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("worker did not exit within 30 s after drain")
        .unwrap();

    assert_eq!(
        embedder.started.available_permits(),
        3,
        "all five events ran, including both for d0.md"
    );
    assert!(embedder.peak() <= 2, "the drain kept to the limit");
    assert_eq!(tx.depth(), 0, "every queue permit was returned");
    for key in keys {
        assert!(
            count_points(&store, &kb, key).await > 0,
            "{key} was indexed"
        );
    }
    let snapshot = health.snapshot("test-kb");
    assert_eq!(snapshot.pending, 0, "the drain finished what was queued");
    assert!(!snapshot.worker_alive);
}

#[tokio::test]
async fn graceful_shutdown_drains_queue() {
    let kb = kb();
    let (store, provisioner) = make_store();
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
            Arc::new(store.clone()),
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

    for i in 0..5_u32 {
        let key = format!("drain{i}.md");
        let n = count_points(&store, &kb, &key).await;
        assert!(n >= 1, "expected ≥1 point for {key} after drain, got {n}");
    }
}

/// A write that the backend rejects must be logged and swallowed, not panic the
/// worker or stall the queue.
///
/// This used to point a real client at a dead port. The equivalent here is a
/// store with no collection provisioned, so every write fails — same contract,
/// no socket.
#[tokio::test]
async fn vector_store_failure_logs_indexing_failed() {
    let kb = kb();
    let store: Arc<dyn VectorStore> = Arc::new(InMemoryVectorStore::new());

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
            store,
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
async fn embedder_retry_on_429_succeeds() {
    let kb = kb();
    let (store, provisioner) = make_store();
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
            Arc::new(store.clone()),
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

    let n = count_points(&store, &kb, "retry.md").await;
    assert!(n >= 1, "expected point after retry success, got {n}");
    let calls = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(calls.len(), 3, "expected two retries plus success");
}

#[tokio::test]
async fn embedder_retries_exhausted_logs_indexing_failed() {
    let kb = kb();
    let (store, provisioner) = make_store();
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
            Arc::new(store.clone()),
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

    let n = count_points(&store, &kb, "fail.md").await;
    assert_eq!(n, 0, "failed embed should not write points");
    let calls = mock_server.received_requests().await.unwrap_or_default();
    assert_eq!(calls.len(), 2, "expected max_retries attempts");
}

#[tokio::test]
async fn failed_embed_and_upsert_batches_preserve_stale_points_until_repair() {
    let kb = kb();
    let (store, provisioner) = make_store();
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
        Arc::new(store.clone()),
        &kb,
        "repair.md",
        2,
    )
    .await;
    let old_count = count_points(&store, &kb, "repair.md").await;
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
        Arc::new(store.clone()),
        &kb,
        "repair.md",
        2,
    )
    .await;
    assert_eq!(
        count_points(&store, &kb, "repair.md").await,
        old_count,
        "embed failure must not run final stale cleanup"
    );
    assert_eq!(embed_failure.batch_sizes(), [2, 2]);

    index_once(
        Arc::clone(&storage),
        Arc::new(ScriptedEmbedder::new(None, None)),
        Arc::new(store.clone()),
        &kb,
        "repair.md",
        2,
    )
    .await;
    let repaired = scroll_points(&store, &kb, "repair.md", false).await;
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
        Arc::new(store.clone()),
        &kb,
        "repair.md",
        2,
    )
    .await;
    assert_eq!(upsert_failure.batch_sizes(), [2, 2]);
    assert_eq!(
        count_points(&store, &kb, "repair.md").await,
        repaired.len(),
        "upsert failure must not run final stale cleanup"
    );

    index_once(
        Arc::clone(&storage),
        Arc::new(ScriptedEmbedder::new(None, None)),
        Arc::new(store.clone()),
        &kb,
        "repair.md",
        2,
    )
    .await;
    let repaired = scroll_points(&store, &kb, "repair.md", false).await;
    assert!(
        repaired
            .iter()
            .all(|point| !string_payload(point, "text").contains("new-")
                || string_payload(point, "text").contains("newest-"))
    );
}

/// A re-index that failed part-way leaves some chunks on the new `ETag` and the rest on
/// the old one. A later `Refresh` must see that as "not indexed" whichever chunk the store
/// happens to read first — point ids are hashes, so that chunk is effectively random
/// (issue #276). Keys are tried until both orders have been exercised: one whose
/// lowest-id point is a rewritten chunk, and one whose lowest-id point is a stale one.
#[tokio::test]
async fn refresh_repairs_a_half_written_object_whichever_chunk_sorts_first() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    let version = |word: &str| {
        (0..8).fold(String::new(), |mut output, index| {
            write!(output, "# {word} {index}\n{word}-{index}\n").expect("write fixture");
            output
        })
    };

    let mut lowest_rewritten = false;
    let mut lowest_stale = false;
    for attempt in 0..64 {
        if lowest_rewritten && lowest_stale {
            break;
        }
        let key = format!("mix-{attempt}.md");
        storage.insert("test-kb", &key, &version("old"), "text/markdown");
        index_once(
            Arc::clone(&storage),
            Arc::new(ScriptedEmbedder::new(None, None)),
            Arc::new(store.clone()),
            &kb,
            &key,
            2,
        )
        .await;

        storage.insert("test-kb", &key, &version("new"), "text/markdown");
        index_once(
            Arc::clone(&storage),
            Arc::new(ScriptedEmbedder::new(Some(2), None)),
            Arc::new(store.clone()),
            &kb,
            &key,
            2,
        )
        .await;
        let half_written = scroll_points(&store, &kb, &key, false).await;
        let new_etag = string_payload(&half_written[0], "etag").to_owned();
        assert!(
            half_written
                .iter()
                .any(|point| string_payload(point, "etag") != new_etag),
            "{key}: the failed batch must leave stale chunks behind"
        );
        let lowest = half_written
            .iter()
            .min_by_key(|point| numeric_id(point))
            .expect("points");
        if string_payload(lowest, "etag") == new_etag {
            lowest_rewritten = true;
        } else {
            lowest_stale = true;
        }

        let repair = Arc::new(ScriptedEmbedder::new(None, None));
        refresh_once(
            Arc::clone(&storage),
            Arc::clone(&repair) as Arc<dyn Embedder>,
            Arc::new(store.clone()),
            &kb,
            &key,
        )
        .await;
        assert!(
            repair.calls() > 0,
            "{key}: refresh skipped a half-written object"
        );
        let repaired = scroll_points(&store, &kb, &key, false).await;
        assert!(
            repaired
                .iter()
                .all(|point| !string_payload(point, "text").contains("old-")),
            "{key}: stale chunks survived the refresh"
        );
        assert!(
            repaired
                .iter()
                .all(|point| string_payload(point, "etag") == new_etag),
            "{key}: chunks still disagree on their ETag"
        );
    }
    assert!(
        lowest_rewritten && lowest_stale,
        "no key put a rewritten and a stale chunk first; widen the search"
    );
}

#[tokio::test]
async fn unstable_or_invalid_stream_preserves_last_complete_index_until_repair() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "stable.md", "old stable", "text/markdown");
    index_once(
        Arc::clone(&storage),
        Arc::new(ScriptedEmbedder::new(None, None)),
        Arc::new(store.clone()),
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
        Arc::new(store.clone()),
        &kb,
        "stable.md",
        2,
    )
    .await;
    storage.fail_next_stream_midway();
    index_once(
        Arc::clone(&storage),
        Arc::clone(&unused_embedder) as Arc<dyn Embedder>,
        Arc::new(store.clone()),
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
        Arc::new(store.clone()),
        &kb,
        "stable.md",
        2,
    )
    .await;
    assert!(unused_embedder.batch_sizes().is_empty());
    let preserved = scroll_points(&store, &kb, "stable.md", false).await;
    assert_eq!(preserved.len(), 1);
    assert_eq!(string_payload(&preserved[0], "text"), "old stable");

    storage.insert("test-kb", "stable.md", "new stable", "text/markdown");
    index_once(
        Arc::clone(&storage),
        Arc::new(ScriptedEmbedder::new(None, None)),
        Arc::new(store.clone()),
        &kb,
        "stable.md",
        2,
    )
    .await;
    let repaired = scroll_points(&store, &kb, "stable.md", false).await;
    assert_eq!(repaired.len(), 1);
    assert_eq!(string_payload(&repaired[0], "text"), "new stable");
}

// ─── Refresh: re-derive from disk, and skip when nothing changed ────────────

/// The test that protects the embedding bill.
///
/// A reconciliation pass re-examines every object in a knowledge base, and a filesystem
/// watcher sees the server's own writes echoed back. Both would be unaffordable if
/// re-examining unchanged bytes cost an embedding request, so assert on the request count
/// rather than on the points, which would look identical either way.
#[tokio::test]
async fn a_refresh_of_unchanged_content_does_not_re_embed() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let storage = Arc::new(MockStorage::new());
    storage.insert(
        kb.as_str(),
        "note.md",
        "# Title\n\nBody text.",
        "text/markdown",
    );
    let embedder = Arc::new(ScriptedEmbedder::new(None, None));
    let store_arc: Arc<dyn VectorStore> = Arc::new(store.clone());

    refresh_once(
        storage.clone(),
        embedder.clone(),
        store_arc.clone(),
        &kb,
        "note.md",
    )
    .await;
    let after_first = embedder.calls();
    assert!(after_first > 0, "the first refresh must index the object");
    assert!(count_points(&store, &kb, "note.md").await > 0);

    refresh_once(storage, embedder.clone(), store_arc, &kb, "note.md").await;

    assert_eq!(
        embedder.calls(),
        after_first,
        "re-examining unchanged content must not reach the embedder"
    );
}

#[tokio::test]
async fn a_refresh_of_changed_content_re_embeds() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let storage = Arc::new(MockStorage::new());
    storage.insert(kb.as_str(), "note.md", "first body", "text/markdown");
    let embedder = Arc::new(ScriptedEmbedder::new(None, None));
    let store_arc: Arc<dyn VectorStore> = Arc::new(store.clone());

    refresh_once(
        storage.clone(),
        embedder.clone(),
        store_arc.clone(),
        &kb,
        "note.md",
    )
    .await;
    let after_first = embedder.calls();

    storage.insert(kb.as_str(), "note.md", "second body", "text/markdown");
    refresh_once(storage, embedder.clone(), store_arc, &kb, "note.md").await;

    assert!(
        embedder.calls() > after_first,
        "changed content must be re-embedded"
    );
    // Joined, because the stub embedder's small input limit splits the body across chunks.
    let indexed: String = scroll_points(&store, &kb, "note.md", false)
        .await
        .iter()
        .map(|point| string_payload(point, "text").to_owned())
        .collect();
    assert!(
        indexed.contains("second body"),
        "the index must hold the new content, got {indexed:?}"
    );
}

/// A deletion reaches the worker as a `Refresh`, never as a tombstone — that is what stops
/// a delayed event from outracing a re-create. The conversion happens here.
#[tokio::test]
async fn a_refresh_of_a_deleted_object_tombstones_it() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let storage = Arc::new(MockStorage::new());
    storage.insert(kb.as_str(), "note.md", "body", "text/markdown");
    let embedder = Arc::new(ScriptedEmbedder::new(None, None));
    let store_arc: Arc<dyn VectorStore> = Arc::new(store.clone());

    refresh_once(
        storage.clone(),
        embedder.clone(),
        store_arc.clone(),
        &kb,
        "note.md",
    )
    .await;
    assert!(count_points(&store, &kb, "note.md").await > 0);

    storage.remove(kb.as_str(), "note.md");
    refresh_once(storage, embedder, store_arc, &kb, "note.md").await;

    assert_eq!(count_points(&store, &kb, "note.md").await, 0);
}

/// Re-writing an object is the only reindex mechanism v1 offers (D42). Skipping unchanged
/// content on the write path would quietly take it away, so `Upsert` must never skip.
#[tokio::test]
async fn a_write_upsert_re_embeds_even_when_unchanged() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let storage = Arc::new(MockStorage::new());
    storage.insert(kb.as_str(), "note.md", "body", "text/markdown");
    let embedder = Arc::new(ScriptedEmbedder::new(None, None));
    let store_arc: Arc<dyn VectorStore> = Arc::new(store.clone());

    index_once(
        storage.clone(),
        embedder.clone(),
        store_arc.clone(),
        &kb,
        "note.md",
        32,
    )
    .await;
    let after_first = embedder.calls();

    index_once(storage, embedder.clone(), store_arc, &kb, "note.md", 32).await;

    assert!(
        embedder.calls() > after_first,
        "a write must re-index even when the bytes are identical"
    );
}

/// When the lookup itself fails we index rather than skip. A redundant re-index costs an
/// embedding request; a wrong skip leaves a document wrong in search with nothing left to
/// notice.
#[tokio::test]
async fn a_failed_indexed_etag_lookup_indexes_anyway() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();

    let storage = Arc::new(MockStorage::new());
    storage.insert(kb.as_str(), "note.md", "body", "text/markdown");
    let embedder = Arc::new(ScriptedEmbedder::new(None, None));
    let failing: Arc<dyn VectorStore> = Arc::new(FailingEtagLookup {
        inner: store.clone(),
    });

    refresh_once(
        storage.clone(),
        embedder.clone(),
        failing.clone(),
        &kb,
        "note.md",
    )
    .await;
    let after_first = embedder.calls();

    refresh_once(storage, embedder.clone(), failing, &kb, "note.md").await;

    assert!(
        embedder.calls() > after_first,
        "an unreadable indexed ETag must not be read as 'unchanged'"
    );
}

/// A store that behaves normally except that it cannot answer "what is indexed?".
struct FailingEtagLookup {
    inner: InMemoryVectorStore,
}

#[async_trait]
impl VectorStore for FailingEtagLookup {
    async fn probe(&self) -> Result<(), VectorStoreError> {
        Ok(())
    }

    async fn collection_exists(&self, kb: &KbSlug) -> Result<bool, VectorStoreError> {
        self.inner.collection_exists(kb).await
    }
    async fn create_collection(&self, kb: &KbSlug, dense_dim: u64) -> Result<(), VectorStoreError> {
        self.inner.create_collection(kb, dense_dim).await
    }
    async fn create_payload_index(
        &self,
        kb: &KbSlug,
        field: &str,
        kind: PayloadFieldKind,
    ) -> Result<(), VectorStoreError> {
        self.inner.create_payload_index(kb, field, kind).await
    }
    async fn upsert_points(
        &self,
        kb: &KbSlug,
        points: Vec<qdrant_client::qdrant::PointStruct>,
    ) -> Result<(), VectorStoreError> {
        self.inner.upsert_points(kb, points).await
    }
    async fn delete_points(
        &self,
        kb: &KbSlug,
        selector: PointSelector,
    ) -> Result<(), VectorStoreError> {
        self.inner.delete_points(kb, selector).await
    }
    async fn indexed_etag(
        &self,
        _kb: &KbSlug,
        _object_key: &str,
    ) -> Result<Option<String>, VectorStoreError> {
        Err(VectorStoreError::backend("injected lookup failure"))
    }
    async fn indexed_objects(
        &self,
        _kb: &KbSlug,
        _prefix: Option<&str>,
    ) -> Result<Vec<IndexedObject>, VectorStoreError> {
        Err(VectorStoreError::backend("injected lookup failure"))
    }
    async fn hybrid_search(
        &self,
        kb: &KbSlug,
        query: HybridQuery,
    ) -> Result<Vec<qdrant_client::qdrant::ScoredPoint>, VectorStoreError> {
        self.inner.hybrid_search(kb, query).await
    }
}

// ─── Detected changes are announced, once ──────────────────────────────────

mod announcing {
    use super::*;
    use futures::StreamExt;
    use notedthat_core::{EventId, EventPublisher, EventSource, ObjectEvent, ObjectEventKind};
    use notedthat_events::MemoryPublisher;
    use std::time::Duration;

    /// Run one worker over `events`, with `publisher` as its event log, until the
    /// channel closes.
    async fn drive(
        storage: Arc<MockStorage>,
        store: Arc<dyn VectorStore>,
        publisher: Arc<MemoryPublisher>,
        events: Vec<IndexEvent>,
    ) {
        let embedder: Arc<dyn Embedder> = Arc::new(ScriptedEmbedder::new(None, None));
        drive_with(
            storage,
            embedder,
            store,
            publisher,
            Arc::new(IndexHealth::new()),
            events,
        )
        .await;
    }

    /// [`drive`] with the embedder and health record chosen by the test, for
    /// the outcomes that need a failing embedder or a look at `/index`'s view.
    async fn drive_with(
        storage: Arc<MockStorage>,
        embedder: Arc<dyn Embedder>,
        store: Arc<dyn VectorStore>,
        publisher: Arc<MemoryPublisher>,
        health: Arc<IndexHealth>,
        events: Vec<IndexEvent>,
    ) {
        let (tx, rx) = mpsc::channel(events.len().max(1));
        for event in events {
            tx.send(event).await.unwrap();
        }
        drop(tx);
        make_worker_with_batch(storage, embedder, store, rx, CancellationToken::new(), 32)
            .with_event_publisher(Some(publisher as Arc<dyn EventPublisher>))
            .with_health(health)
            .run()
            .await;
    }

    fn indexed_events(events: &[ObjectEvent]) -> Vec<&ObjectEvent> {
        events
            .iter()
            .filter(|event| matches!(event.kind, ObjectEventKind::Indexed { .. }))
            .collect()
    }

    /// Everything the log holds for the test knowledge base.
    async fn announced(publisher: &MemoryPublisher) -> Vec<ObjectEvent> {
        let mut stream = publisher.subscribe(&kb(), Some(EventId(0))).await.unwrap();
        let mut out = Vec::new();
        while let Ok(Some(item)) =
            tokio::time::timeout(Duration::from_millis(100), stream.next()).await
        {
            out.push(item.unwrap().1);
        }
        out
    }

    fn refresh(key: &str, origin: RefreshOrigin) -> IndexEvent {
        IndexEvent::Refresh {
            kb: kb(),
            object_key: opath(key),
            origin,
        }
    }

    fn upsert(key: &str, etag: &str) -> IndexEvent {
        IndexEvent::Upsert {
            kb: kb(),
            object_key: opath(key),
            etag: etag.to_owned(),
            mtime: 0,
        }
    }

    #[tokio::test]
    async fn a_detected_mp3_is_announced_even_though_it_is_never_indexed() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        storage.insert_bytes(
            kb().as_str(),
            "memo.mp3",
            Bytes::from_static(b"ID3\x03"),
            "audio/mpeg",
        );
        let publisher = Arc::new(MemoryPublisher::new(16));

        drive(
            storage.clone(),
            Arc::new(store.clone()),
            publisher.clone(),
            vec![refresh("memo.mp3", RefreshOrigin::Watch)],
        )
        .await;

        let events = announced(&publisher).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].object_key.as_str(), "memo.mp3");
        assert_eq!(events[0].source, EventSource::FsWatch);
        assert_eq!(
            events[0].kind,
            ObjectEventKind::Written {
                etag: MockStorage::etag_for(b"ID3\x03"),
                size: 4,
                mime: "audio/mpeg".into(),
                mtime: 1_700_000_000,
            }
        );
        assert_eq!(count_points(&store, &kb(), "memo.mp3").await, 0);
    }

    #[tokio::test]
    async fn a_detected_deletion_is_announced_as_deleted_with_its_origin() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        let publisher = Arc::new(MemoryPublisher::new(16));

        drive(
            storage.clone(),
            Arc::new(store.clone()),
            publisher.clone(),
            vec![refresh("gone.md", RefreshOrigin::Reconcile)],
        )
        .await;

        let events = announced(&publisher).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, ObjectEventKind::Deleted);
        assert_eq!(events[0].source, EventSource::Reconcile);
    }

    /// The write path announced the write before enqueueing it; the worker's
    /// part is the verdict (D65): the version it indexed, and how many points
    /// now stand for it.
    #[tokio::test]
    async fn a_write_upsert_is_not_announced_but_its_outcome_is() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        storage.insert(kb().as_str(), "note.md", "body", "text/markdown");
        let publisher = Arc::new(MemoryPublisher::new(16));

        drive(
            storage.clone(),
            Arc::new(store.clone()),
            publisher.clone(),
            vec![upsert("note.md", "\"advisory\"")],
        )
        .await;

        let points = count_points(&store, &kb(), "note.md").await;
        assert!(points > 0, "still indexed");
        let events = announced(&publisher).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].object_key.as_str(), "note.md");
        assert_eq!(events[0].source, EventSource::Indexer);
        assert_eq!(
            events[0].kind,
            ObjectEventKind::Indexed {
                etag: MockStorage::etag_for(b"body"),
                mime: "text/markdown".into(),
                chunks: u32::try_from(points).unwrap(),
            },
            "the stamp is the one HEAD reported, not the advisory one enqueued"
        );
        assert!(events[0].occurred_at.ends_with('Z'));
    }

    /// An upsert names a key, not a version: it indexes whatever is current
    /// when the worker reaches it. Two writes in quick succession therefore
    /// yield verdicts that both carry the second version's stamp — the first
    /// write's version is never announced as indexed, because it never was.
    /// A subscriber waiting for its own `etag` must accept a later one as
    /// superseding it, which is what the API.md recipe says.
    #[tokio::test]
    async fn back_to_back_writes_announce_only_the_latest_etag_as_indexed() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        // By the time the worker takes the first upsert, the second write has
        // already replaced the bytes.
        storage.insert(kb().as_str(), "note.md", "second version", "text/markdown");
        let publisher = Arc::new(MemoryPublisher::new(16));
        let first = MockStorage::etag_for(b"first version");
        let second = MockStorage::etag_for(b"second version");

        drive(
            storage.clone(),
            Arc::new(store.clone()),
            publisher.clone(),
            vec![upsert("note.md", &first), upsert("note.md", &second)],
        )
        .await;

        let events = announced(&publisher).await;
        assert_eq!(events.len(), 2, "one verdict per upsert: {events:?}");
        for event in &events {
            assert_eq!(event.kind.name(), "object.indexed");
            assert_eq!(
                event.kind.etag(),
                Some(second.as_str()),
                "every verdict names the version that is in the index, never the superseded one"
            );
        }
        assert!(
            events.iter().all(|e| e.kind.etag() != Some(first.as_str())),
            "nothing is ever published for the first write's version"
        );
    }

    /// A change the watcher detected is announced and then indexed, and the
    /// two events name the same version.
    #[tokio::test]
    async fn a_refresh_that_re_embeds_publishes_written_then_indexed_with_the_same_etag() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        storage.insert(kb().as_str(), "note.md", "body", "text/markdown");
        let publisher = Arc::new(MemoryPublisher::new(16));

        drive(
            storage.clone(),
            Arc::new(store.clone()),
            publisher.clone(),
            vec![refresh("note.md", RefreshOrigin::Watch)],
        )
        .await;

        let events = announced(&publisher).await;
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].kind.name(), "object.written");
        assert_eq!(events[0].source, EventSource::FsWatch);
        assert_eq!(events[1].kind.name(), "object.indexed");
        assert_eq!(events[1].source, EventSource::Indexer);
        assert_eq!(events[0].kind.etag(), events[1].kind.etag());
        assert_eq!(events[1].kind.mime(), Some("text/markdown"));
    }

    /// The D50 skip changes nothing in the index, so it says nothing.
    #[tokio::test]
    async fn a_refresh_skipped_for_an_unchanged_etag_publishes_nothing() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        storage.insert(kb().as_str(), "note.md", "body", "text/markdown");
        let publisher = Arc::new(MemoryPublisher::new(16));
        let etag = MockStorage::etag_for(b"body");

        drive(
            storage.clone(),
            Arc::new(store.clone()),
            publisher.clone(),
            vec![
                upsert("note.md", &etag),
                refresh("note.md", RefreshOrigin::Watch),
                refresh("note.md", RefreshOrigin::Reconcile),
            ],
        )
        .await;

        let events = announced(&publisher).await;
        assert_eq!(
            events.len(),
            1,
            "one indexed for the write, nothing for the two skips: {events:?}"
        );
        assert_eq!(events[0].kind.name(), "object.indexed");
    }

    /// The index never gained these, so there is no `object.indexed` to send:
    /// `object.written` already described the bytes, and `/index` still
    /// advances.
    #[tokio::test]
    async fn an_upsert_the_pipeline_tombstones_publishes_nothing() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        storage.insert(kb().as_str(), "empty.md", "", "text/markdown");
        storage.insert_bytes(
            kb().as_str(),
            "pic.png",
            Bytes::from_static(b"\x89PNG"),
            "image/png",
        );
        let publisher = Arc::new(MemoryPublisher::new(16));

        drive(
            storage.clone(),
            Arc::new(store.clone()),
            publisher.clone(),
            vec![
                upsert("empty.md", "\"e\""),
                upsert("pic.png", "\"p\""),
                upsert("missing.md", "\"m\""),
            ],
        )
        .await;

        let events = announced(&publisher).await;
        assert!(events.is_empty(), "{events:?}");
    }

    /// `object.deleted` already said what happened to the key; there is no
    /// `object.unindexed`.
    #[tokio::test]
    async fn a_tombstone_publishes_nothing() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        storage.insert(kb().as_str(), "note.md", "body", "text/markdown");
        let publisher = Arc::new(MemoryPublisher::new(16));

        drive(
            storage.clone(),
            Arc::new(store.clone()),
            publisher.clone(),
            vec![
                upsert("note.md", "\"n\""),
                IndexEvent::Tombstone {
                    kb: kb(),
                    object_key: opath("note.md"),
                },
            ],
        )
        .await;

        let events = announced(&publisher).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind.name(), "object.indexed");
        assert_eq!(count_points(&store, &kb(), "note.md").await, 0);
    }

    /// A subscriber waiting on `object.indexed` would otherwise wait forever.
    /// The failure names the version it was working on and carries the very
    /// summary `/index` reports.
    #[tokio::test]
    async fn a_failed_upsert_publishes_index_failed_and_no_indexed() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        storage.insert(kb().as_str(), "broken.md", "body", "text/markdown");
        let publisher = Arc::new(MemoryPublisher::new(16));
        let health = Arc::new(IndexHealth::new());
        let embedder: Arc<dyn Embedder> = Arc::new(ScriptedEmbedder::new(Some(1), None));

        drive_with(
            storage.clone(),
            embedder,
            Arc::new(store.clone()),
            publisher.clone(),
            health.clone(),
            vec![upsert("broken.md", "\"b\"")],
        )
        .await;

        let events = announced(&publisher).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(indexed_events(&events).is_empty());
        assert_eq!(events[0].object_key.as_str(), "broken.md");
        assert_eq!(events[0].source, EventSource::Indexer);
        let ObjectEventKind::IndexFailed {
            etag,
            mime,
            summary,
        } = &events[0].kind
        else {
            panic!("expected object.index_failed, got {:?}", events[0].kind);
        };
        assert_eq!(
            etag.as_deref(),
            Some(MockStorage::etag_for(b"body").as_str())
        );
        assert_eq!(mime.as_deref(), Some("text/markdown"));
        let summary = summary.as_deref().expect("the log holds the summary");
        assert!(
            summary.starts_with("embedder.embed failed"),
            "the pipeline's own error: {summary}"
        );
        assert!(summary.chars().count() <= 201, "bounded");
        let recorded = health
            .snapshot(kb().as_str())
            .last_failure
            .expect("the health record saw it too");
        assert_eq!(summary, recorded.summary, "stream and /index agree");
    }

    #[tokio::test]
    async fn a_retry_after_a_failure_publishes_indexed() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        storage.insert(kb().as_str(), "flaky.md", "body", "text/markdown");
        let publisher = Arc::new(MemoryPublisher::new(16));
        let embedder: Arc<dyn Embedder> = Arc::new(ScriptedEmbedder::new(Some(1), None));

        drive_with(
            storage.clone(),
            embedder,
            Arc::new(store.clone()),
            publisher.clone(),
            Arc::new(IndexHealth::new()),
            vec![upsert("flaky.md", "\"f\""), upsert("flaky.md", "\"f\"")],
        )
        .await;

        let events = announced(&publisher).await;
        let names: Vec<&str> = events.iter().map(|event| event.kind.name()).collect();
        assert_eq!(
            names,
            ["object.index_failed", "object.indexed"],
            "{events:?}"
        );
        assert_eq!(events[0].kind.etag(), events[1].kind.etag());
        assert!(count_points(&store, &kb(), "flaky.md").await > 0);
    }

    /// When storage itself is unreachable the worker knows only the key.
    #[tokio::test]
    async fn a_failure_before_head_carries_no_stamp() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        storage.insert(kb().as_str(), "note.md", "body", "text/markdown");
        storage.fail_next_head();
        let publisher = Arc::new(MemoryPublisher::new(16));

        drive(
            storage.clone(),
            Arc::new(store.clone()),
            publisher.clone(),
            vec![upsert("note.md", "\"n\"")],
        )
        .await;

        let events = announced(&publisher).await;
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(
            events[0].kind,
            ObjectEventKind::IndexFailed {
                etag: None,
                mime: None,
                summary: Some(
                    "storage.head_object failed: backend unavailable: injected head failure".into()
                ),
            }
        );
    }

    /// The `fs` watcher reports the server's own write back to it (D50). The write
    /// path already announced it; the echo must not.
    #[tokio::test]
    async fn the_watchers_echo_of_an_upsert_is_not_announced_again() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        storage.insert_bytes(
            kb().as_str(),
            "memo.mp3",
            Bytes::from_static(b"ID3\x03"),
            "audio/mpeg",
        );
        let publisher = Arc::new(MemoryPublisher::new(16));
        let etag = MockStorage::etag_for(b"ID3\x03");

        drive(
            storage.clone(),
            Arc::new(store.clone()),
            publisher.clone(),
            vec![
                upsert("memo.mp3", &etag),
                refresh("memo.mp3", RefreshOrigin::Watch),
                refresh("memo.mp3", RefreshOrigin::Reconcile),
            ],
        )
        .await;

        assert!(
            announced(&publisher).await.is_empty(),
            "the echo carries the stamp the write already announced"
        );

        // A genuinely new stamp is news again.
        storage.insert_bytes(
            kb().as_str(),
            "memo.mp3",
            Bytes::from_static(b"ID3\x04"),
            "audio/mpeg",
        );
        drive(
            storage.clone(),
            Arc::new(store.clone()),
            publisher.clone(),
            vec![refresh("memo.mp3", RefreshOrigin::Watch)],
        )
        .await;
        let events = announced(&publisher).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, EventSource::FsWatch);
    }

    #[tokio::test]
    async fn a_deletion_the_server_made_is_not_announced_again_by_its_echo() {
        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        let publisher = Arc::new(MemoryPublisher::new(16));

        drive(
            storage.clone(),
            Arc::new(store.clone()),
            publisher.clone(),
            vec![
                IndexEvent::Tombstone {
                    kb: kb(),
                    object_key: opath("gone.md"),
                },
                refresh("gone.md", RefreshOrigin::Watch),
            ],
        )
        .await;

        assert!(announced(&publisher).await.is_empty());
    }

    #[tokio::test]
    async fn a_refused_publish_does_not_stop_indexing() {
        struct Refusing;

        #[async_trait]
        impl EventPublisher for Refusing {
            async fn publish(
                &self,
                _event: ObjectEvent,
            ) -> Result<EventId, notedthat_core::PublishError> {
                Err(notedthat_core::PublishError::Unavailable {
                    message: "down".into(),
                })
            }

            async fn subscribe(
                &self,
                _kb: &KbSlug,
                _after: Option<EventId>,
            ) -> Result<notedthat_core::EventStream, notedthat_core::SubscribeError> {
                unimplemented!()
            }

            fn ready(&self) -> bool {
                false
            }

            fn backend_name(&self) -> &'static str {
                "refusing"
            }
        }

        let (store, provisioner) = make_store();
        provisioner.ensure_collection(&kb(), 4).await.unwrap();
        let storage = Arc::new(MockStorage::new());
        storage.insert(kb().as_str(), "note.md", "body", "text/markdown");
        storage.insert(kb().as_str(), "other.md", "more", "text/markdown");
        let (tx, rx) = mpsc::channel(2);
        // One announcement and two outcomes refused; neither path may fail
        // the indexing itself.
        tx.send(refresh("note.md", RefreshOrigin::Watch))
            .await
            .unwrap();
        tx.send(upsert("other.md", "\"o\"")).await.unwrap();
        drop(tx);
        let embedder: Arc<dyn Embedder> = Arc::new(ScriptedEmbedder::new(None, None));
        make_worker_with_batch(
            storage,
            embedder,
            Arc::new(store.clone()),
            rx,
            CancellationToken::new(),
            32,
        )
        .with_event_publisher(Some(Arc::new(Refusing)))
        .run()
        .await;

        assert!(count_points(&store, &kb(), "note.md").await > 0);
        assert!(count_points(&store, &kb(), "other.md").await > 0);
    }
}

// ─── Index health (#97) ─────────────────────────────────────────────────────

/// Drive one event through a worker that records on `health`, the way the
/// server wires it, and return once the worker has drained and stopped.
async fn run_one_with_health(
    storage: Arc<MockStorage>,
    embedder: Arc<dyn Embedder>,
    store: Arc<dyn VectorStore>,
    event: IndexEvent,
    health: Arc<IndexHealth>,
) {
    let (tx, rx) = mpsc::channel(1);
    tx.send(event).await.unwrap();
    health.enqueued("test-kb");
    drop(tx);
    make_worker_with_batch(storage, embedder, store, rx, CancellationToken::new(), 32)
        .with_health(health)
        .run()
        .await;
}

#[tokio::test]
async fn the_worker_records_a_success_and_its_own_exit_on_the_health_record() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "ok.md", "# Fine\n\nindexed", "text/markdown");
    let health = Arc::new(IndexHealth::new());

    // `run_one_with_health` records the enqueue itself; the worker's own
    // completion is what brings `pending` back to zero.
    run_one_with_health(
        Arc::clone(&storage),
        Arc::new(ScriptedEmbedder::new(None, None)),
        Arc::new(store.clone()),
        IndexEvent::Upsert {
            kb: kb.clone(),
            object_key: opath("ok.md"),
            etag: "etag-ok".into(),
            mtime: 0,
        },
        Arc::clone(&health),
    )
    .await;

    let snapshot = health.snapshot("test-kb");
    assert_eq!(snapshot.pending, 0, "the worker finished what was queued");
    assert!(snapshot.last_indexed_at.is_some());
    assert_eq!(snapshot.last_failure, None);
    // The channel closed, so the loop ended: nothing drains the queue any more,
    // and the record says so rather than staying healthy forever.
    assert!(!snapshot.worker_alive);
    assert_eq!(snapshot.state, IndexState::Failed);
}

#[tokio::test]
async fn the_worker_records_a_failure_with_the_key_and_the_pipelines_error() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    storage.insert(
        "test-kb",
        "broken.md",
        "# Broken\n\nwill not embed",
        "text/markdown",
    );
    let health = Arc::new(IndexHealth::new());

    run_one_with_health(
        Arc::clone(&storage),
        Arc::new(ScriptedEmbedder::new(Some(1), None)),
        Arc::new(store.clone()),
        IndexEvent::Upsert {
            kb: kb.clone(),
            object_key: opath("broken.md"),
            etag: "etag-broken".into(),
            mtime: 0,
        },
        Arc::clone(&health),
    )
    .await;

    let snapshot = health.snapshot("test-kb");
    assert_eq!(snapshot.pending, 0);
    assert_eq!(snapshot.last_indexed_at, None);
    let failure = snapshot.last_failure.expect("the failure is on record");
    assert_eq!(failure.object_key, "broken.md");
    assert!(
        failure.summary.contains("embed"),
        "the summary is the pipeline's own error: {}",
        failure.summary
    );
    assert_eq!(snapshot.state, IndexState::Failed);
}

#[tokio::test]
async fn a_refresh_that_finds_nothing_to_do_still_counts_as_a_success() {
    let kb = kb();
    let (store, provisioner) = make_store();
    provisioner.ensure_collection(&kb, 4).await.unwrap();
    let storage = Arc::new(MockStorage::new());
    storage.insert("test-kb", "same.md", "# Same\n\nunchanged", "text/markdown");
    let embedder: Arc<dyn Embedder> = Arc::new(ScriptedEmbedder::new(None, None));
    index_once(
        Arc::clone(&storage),
        Arc::clone(&embedder),
        Arc::new(store.clone()),
        &kb,
        "same.md",
        32,
    )
    .await;

    let health = Arc::new(IndexHealth::new());
    run_one_with_health(
        Arc::clone(&storage),
        embedder,
        Arc::new(store.clone()),
        IndexEvent::Refresh {
            kb: kb.clone(),
            object_key: opath("same.md"),
            origin: RefreshOrigin::Watch,
        },
        Arc::clone(&health),
    )
    .await;

    let snapshot = health.snapshot("test-kb");
    assert!(
        snapshot.last_indexed_at.is_some(),
        "a skipped refresh confirmed the index is current, which is a success"
    );
    assert_eq!(snapshot.last_failure, None);
}

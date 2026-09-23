//! Metered views of the backends, wrapped once where they are assembled (D68).
//!
//! Each of these forwards every call to an inner `Arc<dyn …>` and times it. The
//! alternative — recording inside `S3Storage`, `FsStorage` and the Qdrant client
//! — was not taken, for four reasons:
//!
//! 1. One implementation covers `s3`, `fs` *and* the in-process doubles, so the
//!    E2E suites exercise the same instrumentation production runs on.
//! 2. The `op` label set is the trait's method list, so it is closed by the
//!    compiler. An operation cannot be added without a trait method, and one
//!    cannot be left uninstrumented without the impl failing to build.
//! 3. The adapter crates stay free of an observability dependency, which is
//!    what D28's layering is for.
//! 4. `phase` is only decidable here: `OpenAiCompatibleEmbedder::embed` cannot
//!    know whether the indexer or the search route is calling it.
//!
//! Timing at this seam measures what the caller waited for, including any retry
//! the adapter made internally, which is the number an operator wants.
//!
//! # One path these do not see
//!
//! The `fs` watcher bridge builds its own `FsStorage` for the reconciliation
//! walk, so that walk's reads are not in `notedthat_storage_*`. That is
//! deliberate — a pass has its own `notedthat_reconcile_*` family — but it is
//! why storage call counts do not add up to every read the process makes.

use async_trait::async_trait;
use bytes::Bytes;
use notedthat_core::events::{EventId, PublishError, SubscribeError};
use notedthat_core::metrics::{label, name, outcome, storage_error_kind, storage_outcome};
use notedthat_core::{
    ByteRange, ConditionalHeaders, CopyObjectOptions, EventPublisher, EventStream, KbManifest,
    KbSlug, ListResponse, ObjectEvent, ObjectMeta, ObjectPath, ObjectRead, ObjectStream,
    PutOutcome, StagedBody, Storage, StorageError,
};
use notedthat_indexer::embedder::{Embedder, EmbedderError};
use notedthat_indexer::{
    HybridQuery, IndexedObject, PayloadFieldKind, PointSelector, PointStruct, ScoredPoint,
    VectorStore, VectorStoreError,
};
use std::sync::Arc;
use std::time::Instant;

/// Time a storage call and record its duration, its outcome and, when it
/// failed, the kind of failure.
fn record_storage<T>(
    backend: &'static str,
    op: &'static str,
    started: Instant,
    result: &Result<T, StorageError>,
) {
    metrics::histogram!(name::STORAGE_DURATION, label::BACKEND => backend, label::OP => op)
        .record(started.elapsed().as_secs_f64());
    let outcome = storage_outcome(result);
    metrics::counter!(
        name::STORAGE_OPERATIONS,
        label::BACKEND => backend,
        label::OP => op,
        label::OUTCOME => outcome,
    )
    .increment(1);
    if let Err(error) = result
        && let Some(kind) = storage_error_kind(error)
    {
        metrics::counter!(
            name::STORAGE_ERRORS,
            label::BACKEND => backend,
            label::OP => op,
            label::ERROR_KIND => kind,
        )
        .increment(1);
    }
}

/// A [`Storage`] that counts and times every call.
pub(crate) struct MeteredStorage {
    inner: Arc<dyn Storage>,
    backend: &'static str,
}

impl MeteredStorage {
    /// Wrap `inner`, attributing its calls to `backend`.
    pub(crate) fn new(inner: Arc<dyn Storage>, backend: &'static str) -> Self {
        Self { inner, backend }
    }
}

macro_rules! metered_storage {
    ($self:ident, $op:literal, $call:expr) => {{
        let started = Instant::now();
        let result = $call.await;
        record_storage($self.backend, $op, started, &result);
        result
    }};
}

#[async_trait]
impl Storage for MeteredStorage {
    async fn ensure_bucket(&self, kb: &KbSlug) -> Result<(), StorageError> {
        metered_storage!(self, "ensure_bucket", self.inner.ensure_bucket(kb))
    }

    async fn probe(&self, kb: &KbSlug) -> Result<(), StorageError> {
        metered_storage!(self, "probe", self.inner.probe(kb))
    }

    async fn read_manifest(&self, kb: &KbSlug) -> Result<KbManifest, StorageError> {
        metered_storage!(self, "read_manifest", self.inner.read_manifest(kb))
    }

    async fn write_manifest(&self, kb: &KbSlug, manifest: &KbManifest) -> Result<(), StorageError> {
        metered_storage!(
            self,
            "write_manifest",
            self.inner.write_manifest(kb, manifest)
        )
    }

    async fn head_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectMeta, StorageError> {
        metered_storage!(
            self,
            "head_object",
            self.inner.head_object(kb, path, conditionals)
        )
    }

    async fn get_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<ByteRange>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        metered_storage!(
            self,
            "get_object",
            self.inner.get_object(kb, path, range, conditionals)
        )
    }

    async fn get_object_stream(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<ByteRange>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectStream, StorageError> {
        // Times the call that opens the stream, not the transfer: the body is
        // read by whoever holds it, long after this returns.
        metered_storage!(
            self,
            "get_object_stream",
            self.inner.get_object_stream(kb, path, range, conditionals)
        )
    }

    async fn put_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        bytes: Bytes,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        metered_storage!(
            self,
            "put_object",
            self.inner
                .put_object(kb, path, bytes, content_type, conditionals)
        )
    }

    async fn put_staged_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        body: StagedBody,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        metered_storage!(
            self,
            "put_staged_object",
            self.inner
                .put_staged_object(kb, path, body, content_type, conditionals)
        )
    }

    async fn copy_object(
        &self,
        kb: &KbSlug,
        source: &ObjectPath,
        destination: &ObjectPath,
        options: CopyObjectOptions,
    ) -> Result<PutOutcome, StorageError> {
        metered_storage!(
            self,
            "copy_object",
            self.inner.copy_object(kb, source, destination, options)
        )
    }

    async fn delete_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<(), StorageError> {
        metered_storage!(
            self,
            "delete_object",
            self.inner.delete_object(kb, path, conditionals)
        )
    }

    async fn list_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<ListResponse, StorageError> {
        metered_storage!(
            self,
            "list_objects",
            self.inner.list_objects(kb, prefix, limit, cursor)
        )
    }
}

/// The `error_kind` for a vector-store failure: the variant's name, never the
/// collection name or the backend's message.
fn vector_store_error_kind(error: &VectorStoreError) -> &'static str {
    match error {
        VectorStoreError::CollectionNotFound { .. } => "collection_not_found",
        VectorStoreError::Backend { .. } => "backend",
    }
}

fn record_vector_store<T>(
    op: &'static str,
    started: Instant,
    result: &Result<T, VectorStoreError>,
) {
    metrics::histogram!(name::VECTOR_STORE_DURATION, label::OP => op)
        .record(started.elapsed().as_secs_f64());
    metrics::counter!(
        name::VECTOR_STORE_OPERATIONS,
        label::OP => op,
        label::OUTCOME => if result.is_ok() { outcome::OK } else { outcome::ERROR },
    )
    .increment(1);
    if let Err(error) = result {
        metrics::counter!(
            name::VECTOR_STORE_ERRORS,
            label::OP => op,
            label::ERROR_KIND => vector_store_error_kind(error),
        )
        .increment(1);
    }
}

/// A [`VectorStore`] that counts and times every call.
pub(crate) struct MeteredVectorStore {
    inner: Arc<dyn VectorStore>,
}

impl MeteredVectorStore {
    /// Wrap `inner`.
    pub(crate) fn new(inner: Arc<dyn VectorStore>) -> Self {
        Self { inner }
    }
}

macro_rules! metered_vector_store {
    ($op:literal, $call:expr) => {{
        let started = Instant::now();
        let result = $call.await;
        record_vector_store($op, started, &result);
        result
    }};
}

#[async_trait]
impl VectorStore for MeteredVectorStore {
    async fn probe(&self) -> Result<(), VectorStoreError> {
        metered_vector_store!("probe", self.inner.probe())
    }

    async fn collection_exists(&self, kb: &KbSlug) -> Result<bool, VectorStoreError> {
        metered_vector_store!("collection_exists", self.inner.collection_exists(kb))
    }

    async fn create_collection(&self, kb: &KbSlug, dense_dim: u64) -> Result<(), VectorStoreError> {
        metered_vector_store!(
            "create_collection",
            self.inner.create_collection(kb, dense_dim)
        )
    }

    async fn create_payload_index(
        &self,
        kb: &KbSlug,
        field: &str,
        kind: PayloadFieldKind,
    ) -> Result<(), VectorStoreError> {
        metered_vector_store!(
            "create_payload_index",
            self.inner.create_payload_index(kb, field, kind)
        )
    }

    async fn upsert_points(
        &self,
        kb: &KbSlug,
        points: Vec<PointStruct>,
    ) -> Result<(), VectorStoreError> {
        metered_vector_store!("upsert_points", self.inner.upsert_points(kb, points))
    }

    async fn delete_points(
        &self,
        kb: &KbSlug,
        selector: PointSelector,
    ) -> Result<(), VectorStoreError> {
        metered_vector_store!("delete_points", self.inner.delete_points(kb, selector))
    }

    async fn indexed_etag(
        &self,
        kb: &KbSlug,
        object_key: &str,
    ) -> Result<Option<String>, VectorStoreError> {
        metered_vector_store!("indexed_etag", self.inner.indexed_etag(kb, object_key))
    }

    async fn indexed_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
    ) -> Result<Vec<IndexedObject>, VectorStoreError> {
        metered_vector_store!("indexed_objects", self.inner.indexed_objects(kb, prefix))
    }

    async fn hybrid_search(
        &self,
        kb: &KbSlug,
        query: HybridQuery,
    ) -> Result<Vec<ScoredPoint>, VectorStoreError> {
        metered_vector_store!("hybrid_search", self.inner.hybrid_search(kb, query))
    }
}

/// The `error_kind` for an embedding failure: the variant's name alone. The
/// variants carry a response body and an endpoint, neither of which is a label.
fn embedder_error_kind(error: &EmbedderError) -> &'static str {
    match error {
        EmbedderError::Transport(_) => "transport",
        EmbedderError::Http { .. } => "http",
        EmbedderError::Malformed(_) => "malformed",
        EmbedderError::CountMismatch { .. } => "count_mismatch",
        EmbedderError::DimensionMismatch { .. } => "dimension_mismatch",
        EmbedderError::IndexOutOfRange { .. } => "index_out_of_range",
        EmbedderError::DuplicateIndex { .. } => "duplicate_index",
        EmbedderError::RetriesExhausted { .. } => "retries_exhausted",
    }
}

/// An [`Embedder`] that counts and times every call, attributed to one phase.
///
/// Both phases wrap the *same* inner embedder — one endpoint, one client, one
/// connection pool — and differ only in the label they record under. That is
/// why the phase is decided by wiring rather than threaded as an argument
/// through the pipeline and the searcher.
pub(crate) struct MeteredEmbedder {
    inner: Arc<dyn Embedder>,
    phase: &'static str,
}

impl MeteredEmbedder {
    /// Wrap `inner`, attributing its calls to `phase`.
    pub(crate) fn new(inner: Arc<dyn Embedder>, phase: &'static str) -> Self {
        Self { inner, phase }
    }
}

#[async_trait]
impl Embedder for MeteredEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedderError> {
        let started = Instant::now();
        #[allow(clippy::cast_precision_loss)]
        metrics::histogram!(name::EMBEDDING_TEXTS, label::PHASE => self.phase)
            .record(texts.len() as f64);
        let result = self.inner.embed(texts).await;
        metrics::histogram!(name::EMBEDDING_DURATION, label::PHASE => self.phase)
            .record(started.elapsed().as_secs_f64());
        metrics::counter!(
            name::EMBEDDING_REQUESTS,
            label::PHASE => self.phase,
            label::OUTCOME => if result.is_ok() { outcome::OK } else { outcome::ERROR },
        )
        .increment(1);
        if let Err(error) = &result {
            metrics::counter!(
                name::EMBEDDING_ERRORS,
                label::PHASE => self.phase,
                label::ERROR_KIND => embedder_error_kind(error),
            )
            .increment(1);
        }
        result
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

/// An [`EventPublisher`] that counts what reached the log and what it refused.
///
/// One wrapper covers both publishers — the write paths and the indexer
/// worker's outcome events — and `kind` already says which produced an event,
/// so no `source` label is needed.
pub(crate) struct MeteredEventPublisher {
    inner: Arc<dyn EventPublisher>,
}

impl MeteredEventPublisher {
    /// Wrap `inner`.
    pub(crate) fn new(inner: Arc<dyn EventPublisher>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl EventPublisher for MeteredEventPublisher {
    async fn publish(&self, event: ObjectEvent) -> Result<EventId, PublishError> {
        // Read before the move: the event is consumed by the call.
        let kb = event.kb.as_str().to_string();
        let kind = event.kind.name();
        let result = self.inner.publish(event).await;
        let metric = if result.is_ok() {
            name::EVENTS_PUBLISHED
        } else {
            name::EVENTS_PUBLISH_FAILED
        };
        metrics::counter!(metric, label::KB => kb, label::KIND => kind).increment(1);
        result
    }

    async fn subscribe(
        &self,
        kb: &KbSlug,
        after: Option<EventId>,
    ) -> Result<EventStream, SubscribeError> {
        self.inner.subscribe(kb, after).await
    }

    fn ready(&self) -> bool {
        self.inner.ready()
    }

    fn backend_name(&self) -> &'static str {
        self.inner.backend_name()
    }
}

/// The backends, each wrapped in its metered view (D68).
///
/// The embedder appears twice on purpose: both fields hold the *same* inner
/// embedder and differ only in the `phase` they record under.
pub(crate) struct MeteredBackends {
    /// Storage, attributed to the selected backend.
    pub(crate) storage: Arc<dyn Storage>,
    /// The vector store.
    pub(crate) store: Arc<dyn VectorStore>,
    /// The embedder as the indexer sees it.
    pub(crate) embed_index: Arc<dyn Embedder>,
    /// The embedder as the search route sees it.
    pub(crate) embed_query: Arc<dyn Embedder>,
    /// The event log, when one is configured.
    pub(crate) events: Option<Arc<dyn EventPublisher>>,
}

/// Wrap every backend once, before anything clones them.
///
/// Everything downstream — the API, `WebDAV`, the worker, provisioning, the
/// readiness poller, the reconciler — then holds an instrumented backend
/// without knowing it, and neither storage adapter nor the Qdrant client
/// carries an observability dependency.
pub(crate) fn meter(
    storage: &crate::config::StorageConfig,
    backends: super::backends::Backends,
) -> MeteredBackends {
    // The `backend` label, decided here so the one place that knows which
    // adapter is behind the trait object is the one that wraps it.
    let storage_backend = match storage {
        crate::config::StorageConfig::S3(_) => "s3",
        crate::config::StorageConfig::Fs(_) => "fs",
    };
    let super::backends::Backends {
        storage,
        store,
        embedder,
        events,
    } = backends;

    MeteredBackends {
        storage: Arc::new(MeteredStorage::new(storage, storage_backend)),
        store: Arc::new(MeteredVectorStore::new(store)),
        embed_index: Arc::new(MeteredEmbedder::new(
            embedder.clone(),
            notedthat_core::metrics::phase::INDEX,
        )),
        embed_query: Arc::new(MeteredEmbedder::new(
            embedder,
            notedthat_core::metrics::phase::QUERY,
        )),
        events: events.map(|publisher| Arc::new(MeteredEventPublisher::new(publisher)) as Arc<_>),
    }
}

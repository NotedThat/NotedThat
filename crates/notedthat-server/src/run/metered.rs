//! Metered views of the backends, wrapped once where they are assembled (D69).
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

/// One storage call, recorded in `Drop` so that a cancelled one is counted.
///
/// Recording after the await loses every call whose future the caller drops —
/// an abandoned `GET` of a large object, a search the client gave up on — and
/// loses them selectively: the calls most likely to be abandoned are the slow
/// ones, so `notedthat_storage_duration_seconds` goes quiet exactly when the
/// backend is degrading. A guard makes the count unconditional and turns the
/// gap into a measurement, `outcome="cancelled"`, which is the outcome the
/// call keeps if nothing sets another. Same reasoning as `InFlight` in
/// [`notedthat_api_http::metrics`], which already had to solve this for the
/// in-flight gauge.
struct StorageCall {
    backend: &'static str,
    op: &'static str,
    started: Instant,
    outcome: &'static str,
    error_kind: Option<&'static str>,
}

impl StorageCall {
    fn begin(backend: &'static str, op: &'static str) -> Self {
        Self {
            backend,
            op,
            started: Instant::now(),
            outcome: outcome::CANCELLED,
            error_kind: None,
        }
    }

    /// Replace the `cancelled` the guard started with by what actually
    /// happened. Not called when the future is dropped, which is the point.
    fn finish<T>(&mut self, result: &Result<T, StorageError>) {
        self.outcome = storage_outcome(result);
        self.error_kind = result.as_ref().err().and_then(storage_error_kind);
    }
}

impl Drop for StorageCall {
    fn drop(&mut self) {
        metrics::histogram!(
            name::STORAGE_DURATION,
            label::BACKEND => self.backend,
            label::OP => self.op,
        )
        .record(self.started.elapsed().as_secs_f64());
        metrics::counter!(
            name::STORAGE_OPERATIONS,
            label::BACKEND => self.backend,
            label::OP => self.op,
            label::OUTCOME => self.outcome,
        )
        .increment(1);
        if let Some(kind) = self.error_kind {
            metrics::counter!(
                name::STORAGE_ERRORS,
                label::BACKEND => self.backend,
                label::OP => self.op,
                label::ERROR_KIND => kind,
            )
            .increment(1);
        }
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
        let mut call = StorageCall::begin($self.backend, $op);
        let result = $call.await;
        call.finish(&result);
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

/// The `error_kind` for a vector-store failure, or `None` when it is control
/// flow. The variant's name only — never the collection name or the backend's
/// message.
fn vector_store_error_kind(error: &VectorStoreError) -> Option<&'static str> {
    match error {
        VectorStoreError::CollectionNotFound { .. } => None,
        VectorStoreError::Backend { .. } => Some("backend"),
    }
}

/// The `outcome` for a vector-store call.
///
/// `CollectionNotFound` is control flow here, exactly as `NotFound` is for
/// storage: both reconcilers treat it as "no collection yet, skip the pass" and
/// search maps it to a client error, so counting it as a failure would inflate
/// the series an operator alerts on with a state the code handles by design.
fn vector_store_outcome<T>(result: &Result<T, VectorStoreError>) -> &'static str {
    match result {
        Ok(_) => outcome::OK,
        Err(VectorStoreError::CollectionNotFound { .. }) => outcome::NOT_FOUND,
        Err(VectorStoreError::Backend { .. }) => outcome::UNAVAILABLE,
    }
}

/// One vector-store call, recorded in `Drop` for the reason [`StorageCall`]
/// gives: a dropped future would otherwise record nothing at all.
struct VectorStoreCall {
    op: &'static str,
    started: Instant,
    outcome: &'static str,
    error_kind: Option<&'static str>,
}

impl VectorStoreCall {
    fn begin(op: &'static str) -> Self {
        Self {
            op,
            started: Instant::now(),
            outcome: outcome::CANCELLED,
            error_kind: None,
        }
    }

    fn finish<T>(&mut self, result: &Result<T, VectorStoreError>) {
        self.outcome = vector_store_outcome(result);
        self.error_kind = result.as_ref().err().and_then(vector_store_error_kind);
    }
}

impl Drop for VectorStoreCall {
    fn drop(&mut self) {
        metrics::histogram!(name::VECTOR_STORE_DURATION, label::OP => self.op)
            .record(self.started.elapsed().as_secs_f64());
        metrics::counter!(
            name::VECTOR_STORE_OPERATIONS,
            label::OP => self.op,
            label::OUTCOME => self.outcome,
        )
        .increment(1);
        if let Some(kind) = self.error_kind {
            metrics::counter!(
                name::VECTOR_STORE_ERRORS,
                label::OP => self.op,
                label::ERROR_KIND => kind,
            )
            .increment(1);
        }
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
        let mut call = VectorStoreCall::begin($op);
        let result = $call.await;
        call.finish(&result);
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

/// One embedding call, recorded in `Drop` for the reason [`StorageCall`] gives.
///
/// `notedthat_embedding_texts` matters most here. It was recorded *before* the
/// await while everything else was recorded after, so a dropped future moved
/// one family and not the other and the two disagreed by construction —
/// `rate(embedding_texts_count)` above `rate(embedding_requests_total)`, with
/// the gap unattributable. Recording both from `Drop` means they always move
/// together, whatever becomes of the future.
struct EmbeddingCall {
    phase: &'static str,
    texts: usize,
    started: Instant,
    outcome: &'static str,
    error_kind: Option<&'static str>,
}

impl EmbeddingCall {
    fn begin(phase: &'static str, texts: usize) -> Self {
        Self {
            phase,
            texts,
            started: Instant::now(),
            outcome: outcome::CANCELLED,
            error_kind: None,
        }
    }

    fn finish<T>(&mut self, result: &Result<T, EmbedderError>) {
        self.outcome = if result.is_ok() {
            outcome::OK
        } else {
            outcome::ERROR
        };
        self.error_kind = result.as_ref().err().map(embedder_error_kind);
    }
}

impl Drop for EmbeddingCall {
    #[allow(clippy::cast_precision_loss)]
    fn drop(&mut self) {
        metrics::histogram!(name::EMBEDDING_TEXTS, label::PHASE => self.phase)
            .record(self.texts as f64);
        metrics::histogram!(name::EMBEDDING_DURATION, label::PHASE => self.phase)
            .record(self.started.elapsed().as_secs_f64());
        metrics::counter!(
            name::EMBEDDING_REQUESTS,
            label::PHASE => self.phase,
            label::OUTCOME => self.outcome,
        )
        .increment(1);
        if let Some(kind) = self.error_kind {
            metrics::counter!(
                name::EMBEDDING_ERRORS,
                label::PHASE => self.phase,
                label::ERROR_KIND => kind,
            )
            .increment(1);
        }
    }
}

#[async_trait]
impl Embedder for MeteredEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedderError> {
        let mut call = EmbeddingCall::begin(self.phase, texts.len());
        let result = self.inner.embed(texts).await;
        call.finish(&result);
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

/// The backends, each wrapped in its metered view (D69).
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

#[cfg(test)]
mod tests {
    use super::{MeteredEmbedder, StorageCall, VectorStoreCall};
    use async_trait::async_trait;
    use metrics_util::debugging::{DebuggingRecorder, Snapshotter};
    use notedthat_core::metrics::outcome;
    use notedthat_indexer::embedder::{Embedder, EmbedderError};
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};

    /// An embedder whose call never answers, so the only way out of it is for
    /// the caller to give up — which is the case under test.
    struct NeverAnswers;

    #[async_trait]
    impl Embedder for NeverAnswers {
        async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedderError> {
            std::future::pending().await
        }
        fn dim(&self) -> usize {
            1
        }
        fn max_input_tokens(&self) -> usize {
            1
        }
        fn model_id(&self) -> &'static str {
            "never"
        }
    }

    /// Every `(name, labels)` the closure recorded, rendered as flat strings so
    /// a test can look for one without depending on the snapshot's shape.
    fn recorded(f: impl FnOnce()) -> Vec<String> {
        let recorder = DebuggingRecorder::new();
        let snapshotter: Snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, f);
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(key, _, _, _)| {
                let key = key.key();
                let labels = key
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{}{{{}}}", key.name(), labels)
            })
            .collect()
    }

    #[test]
    fn a_dropped_embedding_call_is_counted_as_cancelled() {
        // Given: an embedder that never answers, wrapped in the meter.
        let series = recorded(|| {
            let embedder = MeteredEmbedder::new(Arc::new(NeverAnswers), "query");
            let texts = vec!["anything".to_string()];
            let mut call = Box::pin(embedder.embed(&texts));
            let mut cx = Context::from_waker(Waker::noop());

            // When: the caller polls once and then gives up, as hyper does when
            // a client disconnects mid-search.
            assert!(matches!(call.as_mut().poll(&mut cx), Poll::Pending));
            drop(call);
        });

        // Then: the call is counted, with the outcome that says what happened —
        // not lost, which is what recording after the await did.
        assert!(
            series
                .iter()
                .any(|s| s.contains("notedthat_embedding_requests_total")
                    && s.contains(&format!("outcome={}", outcome::CANCELLED))),
            "a dropped embedding call must be counted as cancelled, got {series:?}"
        );
        // And: `texts` moved with it. These two disagreeing by construction —
        // `texts` recorded before the await, everything else after — was the
        // defect; an unattributable gap between the two families.
        assert!(
            series
                .iter()
                .any(|s| s.contains("notedthat_embedding_texts")),
            "texts must be recorded with the request it belongs to, got {series:?}"
        );
        assert!(
            series
                .iter()
                .any(|s| s.contains("notedthat_embedding_duration_seconds")),
            "a cancelled call still took time, got {series:?}"
        );
    }

    #[test]
    fn a_guard_that_is_told_what_happened_records_that_instead() {
        // The other half: `cancelled` is only what a call keeps when nothing
        // sets an outcome, so a completed call must not be counted as one.
        let series = recorded(|| {
            let mut call = StorageCall::begin("fs", "probe");
            call.finish::<()>(&Ok(()));
        });
        assert!(
            series
                .iter()
                .any(|s| s.contains("notedthat_storage_operations_total")
                    && s.contains(&format!("outcome={}", outcome::OK))),
            "a finished call records its real outcome, got {series:?}"
        );

        let series = recorded(|| {
            let call = VectorStoreCall::begin("hybrid_search");
            drop(call);
        });
        assert!(
            series
                .iter()
                .any(|s| s.contains("notedthat_vector_store_operations_total")
                    && s.contains(&format!("outcome={}", outcome::CANCELLED))),
            "an abandoned vector-store call is cancelled, got {series:?}"
        );
    }
}

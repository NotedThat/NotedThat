//! The seam between `NotedThat` and its vector-search backend.
//!
//! # Why this exists
//!
//! Provisioning, indexing and search all used to hold a `QdrantClient` and call
//! `client.inner()` to reach `qdrant_client::Qdrant` directly. That is fine for
//! production — there is exactly one backend — but it made every test that
//! touches indexing or search require a Qdrant container, because there was no
//! type to substitute. The integration job spent roughly ten minutes per run
//! starting containers, one pair per test, and that cost sat directly in front
//! of the release pipeline.
//!
//! [`VectorStore`] is the substitutable boundary. `QdrantClient` implements it
//! for production; an in-memory implementation backs the tests.
//!
//! # Where the boundary sits
//!
//! Operations are expressed in `NotedThat` terms — a [`KbSlug`] rather than a
//! collection name, a [`PointSelector`] rather than a Qdrant `Filter`, a
//! [`SearchFilter`] rather than translated `Condition`s — so an implementation
//! never has to interpret Qdrant query builders.
//!
//! Point payloads and scored results deliberately stay as `qdrant_client`'s
//! `PointStruct` and `ScoredPoint`. Those two are plain protobuf DTOs, and
//! keeping them means the payload codecs on either side — `worker::points`
//! building points and `searcher::hybrid::point_to_hit` reading them — and the
//! unit tests covering them are untouched by this seam. Moving them to a
//! neutral type would have rewritten well-tested code for no gain in
//! substitutability: an in-memory backend can construct a `ScoredPoint` as
//! easily as anything else.

use async_trait::async_trait;
use notedthat_core::KbSlug;
use notedthat_core::search::SearchFilter;
use qdrant_client::qdrant::{PointStruct, ScoredPoint};

/// Payload field types the search surface indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadFieldKind {
    /// Exact-match string field.
    Keyword,
    /// Numeric field supporting range conditions.
    Integer,
}

/// Which points a delete applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointSelector {
    /// Every chunk belonging to one object, used for tombstones.
    Object {
        /// Object key whose chunks are removed.
        object_key: String,
    },
    /// Chunks of one object at or above `from_chunk_index`.
    ///
    /// Used after a re-index that produced fewer chunks than the previous pass,
    /// to drop the chunks that no longer have a source.
    ObjectChunksFrom {
        /// Object key whose trailing chunks are removed.
        object_key: String,
        /// First chunk index to remove, inclusive.
        from_chunk_index: u32,
    },
}

/// One object a collection holds chunks for, and the `ETag` those chunks were built from.
///
/// Reported by [`VectorStore::indexed_objects`] so a caller can compare what is indexed
/// against what is in storage without reading a single chunk's text or vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedObject {
    /// Object key the chunks belong to.
    pub object_key: String,
    /// `ETag` recorded on those chunks when they were written.
    pub etag: String,
}

/// One hybrid query: dense nearest-neighbour and sparse BM25 prefetches, fused.
#[derive(Debug, Clone)]
pub struct HybridQuery {
    /// Raw query text, handed to the backend's BM25 encoder.
    pub text: String,
    /// Dense query vector from the embedder.
    pub dense: Vec<f32>,
    /// Structured filter to apply, if the request carried one.
    pub filter: Option<SearchFilter>,
    /// How many candidates each prefetch arm retrieves before fusion.
    pub prefetch_limit: u64,
    /// How many fused results to return.
    pub limit: u64,
}

/// Errors surfaced by a vector-store backend.
#[derive(Debug, thiserror::Error)]
pub enum VectorStoreError {
    /// The knowledge base has no collection in the backend.
    #[error("no collection for knowledge base '{kb}'")]
    CollectionNotFound {
        /// Knowledge-base slug that has no collection.
        kb: String,
    },
    /// Any other backend failure.
    #[error("vector store backend error: {message}")]
    Backend {
        /// Message reported by the backend.
        message: String,
    },
}

impl VectorStoreError {
    /// Build a [`VectorStoreError::Backend`] from anything printable.
    pub fn backend(message: impl std::fmt::Display) -> Self {
        Self::Backend {
            message: message.to_string(),
        }
    }
}

/// Vector-search backend used for provisioning, indexing and querying.
///
/// Implementations must be safe to share across tasks: the indexer worker, the
/// HTTP search route and startup provisioning all hold the same instance.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Whether a collection already exists for `kb`.
    async fn collection_exists(&self, kb: &KbSlug) -> Result<bool, VectorStoreError>;

    /// Create the collection for `kb` with a `dense_dim`-wide dense vector and
    /// an IDF-modified sparse vector.
    async fn create_collection(&self, kb: &KbSlug, dense_dim: u64) -> Result<(), VectorStoreError>;

    /// Ensure a payload index exists on `field`.
    ///
    /// Called on every startup, including for collections that already exist,
    /// so that a release adding an index backfills it on an upgraded server.
    async fn create_payload_index(
        &self,
        kb: &KbSlug,
        field: &str,
        kind: PayloadFieldKind,
    ) -> Result<(), VectorStoreError>;

    /// Insert or replace `points` in `kb`'s collection, waiting for the write.
    async fn upsert_points(
        &self,
        kb: &KbSlug,
        points: Vec<PointStruct>,
    ) -> Result<(), VectorStoreError>;

    /// Delete the points `selector` matches, waiting for the write.
    async fn delete_points(
        &self,
        kb: &KbSlug,
        selector: PointSelector,
    ) -> Result<(), VectorStoreError>;

    /// The `ETag` recorded on `object_key`'s chunks, or `None` when it has none.
    ///
    /// Answers "is what I have on disk already indexed?" for one object. That question is
    /// what keeps a re-examined but unchanged object from being embedded again — see
    /// `IndexEvent::Refresh`.
    ///
    /// # Errors
    ///
    /// A knowledge base with no collection is [`VectorStoreError::CollectionNotFound`],
    /// not an empty answer. "Nothing is indexed" would send every object in the tree to
    /// be embedded and then fail every write, since the collection they would be written
    /// to is the one that is missing.
    async fn indexed_etag(
        &self,
        kb: &KbSlug,
        object_key: &str,
    ) -> Result<Option<String>, VectorStoreError>;

    /// Every object the collection holds chunks for, optionally narrowed to `prefix`.
    ///
    /// The same question as [`VectorStore::indexed_etag`], asked of a whole knowledge base
    /// in one round trip — which is what makes reconciliation cost one call rather than
    /// one per object. It also reports keys that storage no longer has, the only way to
    /// discover an object deleted while nothing was watching.
    ///
    /// Reads payloads, never vectors or chunk text. Implementations may apply `prefix`
    /// client-side; `qdrant-client` 1.15 has no keyword prefix matcher (see issue #68).
    ///
    /// # Contract
    ///
    /// Returned in ascending `object_key` order, byte-lexicographic. Reconciliation walks
    /// this against a sorted directory walk with two cursors and no lookup table, so an
    /// implementation that returns them unordered does not merely cost extra work: the
    /// merge reports keys as changed that are not, reports live keys as orphaned, and
    /// skips real differences entirely — objects left wrong in search with nothing to
    /// notice. The conformance suite pins this for both implementations.
    ///
    /// # Errors
    ///
    /// As for [`VectorStore::indexed_etag`], a knowledge base with no collection is
    /// [`VectorStoreError::CollectionNotFound`] rather than an empty result.
    async fn indexed_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
    ) -> Result<Vec<IndexedObject>, VectorStoreError>;

    /// Run a hybrid query and return fused, payload-carrying results.
    async fn hybrid_search(
        &self,
        kb: &KbSlug,
        query: HybridQuery,
    ) -> Result<Vec<ScoredPoint>, VectorStoreError>;
}

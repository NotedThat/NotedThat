//! Thin wrapper around the `qdrant-client` crate.
//!
//! Owns the client construction, config parsing, and error mapping. The
//! provisioner (see `provisioner.rs`) and worker (see `worker.rs`) call into
//! this module — they do not import `qdrant_client` directly.
//!
//! Per §6.11 dep graph: `notedthat-indexer` is the only crate that
//! depends on `qdrant-client`.

use crate::vector_store::{
    HybridQuery, PayloadFieldKind, PointSelector, VectorStore, VectorStoreError,
};
use crate::worker::collection_name;
use async_trait::async_trait;
use notedthat_core::KbSlug;
use qdrant_client::Qdrant;
use qdrant_client::qdrant::{
    Condition, CreateCollectionBuilder, CreateFieldIndexCollectionBuilder, DeletePointsBuilder,
    Distance, Document, FieldType, Filter, Fusion, Modifier, PointStruct, PrefetchQueryBuilder,
    Query, QueryPointsBuilder, Range, ScoredPoint, SparseVectorParamsBuilder,
    SparseVectorsConfigBuilder, UpsertPointsBuilder, VectorParamsBuilder, VectorsConfigBuilder,
};
use std::sync::Arc;
use std::time::Duration;

/// Default per-RPC timeout.
///
/// `qdrant-client` defaults to **5 seconds for every RPC**, and nothing here
/// used to override it. That is too tight for this workload: a full embedding
/// batch upserted with `wait(true)`, against a collection that is still building
/// payload indexes, on a busy host, can exceed it. When it does, the failure
/// surfaces as an opaque `Cancelled: Timeout expired` that reads like a Qdrant
/// fault rather than a deadline, and the document silently goes unindexed.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default connection-establishment timeout.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Qdrant client configuration, parsed from `NOTEDTHAT_QDRANT_*` env vars.
#[derive(Debug, Clone)]
pub struct QdrantConfig {
    /// Qdrant server URL (e.g., <http://127.0.0.1:6334>).
    pub url: String,
    /// Optional API key for authentication.
    pub api_key: Option<String>,
    /// Per-RPC timeout. See [`DEFAULT_TIMEOUT`] for why this is set explicitly.
    pub timeout: Duration,
    /// Connection-establishment timeout.
    pub connect_timeout: Duration,
}

impl Default for QdrantConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:6334".to_string(),
            api_key: None,
            timeout: DEFAULT_TIMEOUT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }
}

/// Errors from the Qdrant wrapper.
#[derive(Debug, thiserror::Error)]
pub enum QdrantWrapperError {
    /// Failed to build the Qdrant client.
    #[error("qdrant client build failed: {0}")]
    ClientBuild(String),
    /// Qdrant operation failed.
    #[error("qdrant operation failed: {0}")]
    Operation(String),
}

/// Thin wrapper: constructs a `Qdrant` client from config.
#[derive(Clone)]
pub struct QdrantClient {
    inner: Arc<Qdrant>,
}

impl QdrantClient {
    /// Build a `QdrantClient` from the provided config.
    ///
    /// Construction is cheap — no network connection is made until the first RPC call.
    pub fn new(config: &QdrantConfig) -> Result<Self, QdrantWrapperError> {
        let builder = Qdrant::from_url(&config.url);
        let builder = if let Some(key) = &config.api_key {
            builder.api_key(key.clone())
        } else {
            builder
        };
        let client = builder
            .timeout(config.timeout)
            .connect_timeout(config.connect_timeout)
            .build()
            .map_err(|e| QdrantWrapperError::ClientBuild(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(client),
        })
    }

    /// Access the underlying `qdrant_client::Qdrant` for advanced operations.
    ///
    /// Kept `pub(crate)` so provisioner + worker can call directly, but the API
    /// does not leak out of this crate.
    pub(crate) fn inner(&self) -> &Qdrant {
        &self.inner
    }
}

/// Translate a [`PointSelector`] into the Qdrant filter that selects those points.
fn selector_filter(selector: &PointSelector) -> Filter {
    match selector {
        PointSelector::Object { object_key } => {
            Filter::must([Condition::matches("object_key", object_key.clone())])
        }
        PointSelector::ObjectChunksFrom {
            object_key,
            from_chunk_index,
        } => Filter::must([
            Condition::matches("object_key", object_key.clone()),
            Condition::range(
                "chunk_index",
                Range {
                    gte: Some(f64::from(*from_chunk_index)),
                    ..Range::default()
                },
            ),
        ]),
    }
}

/// Classify a Qdrant transport error, separating "no such collection" from the rest.
///
/// Qdrant reports a missing collection as an ordinary status error, so the only
/// signal is the message text.
fn classify(kb: &KbSlug, err: &qdrant_client::QdrantError) -> VectorStoreError {
    let message = err.to_string();
    let lower = message.to_ascii_lowercase();
    if lower.contains("not found")
        || lower.contains("doesn't exist")
        || lower.contains("does not exist")
    {
        VectorStoreError::CollectionNotFound {
            kb: kb.as_str().to_string(),
        }
    } else {
        VectorStoreError::Backend { message }
    }
}

#[async_trait]
impl VectorStore for QdrantClient {
    async fn collection_exists(&self, kb: &KbSlug) -> Result<bool, VectorStoreError> {
        self.inner()
            .collection_exists(collection_name(kb))
            .await
            .map_err(|err| classify(kb, &err))
    }

    async fn create_collection(&self, kb: &KbSlug, dense_dim: u64) -> Result<(), VectorStoreError> {
        let mut vectors_config = VectorsConfigBuilder::default();
        vectors_config.add_named_vector_params(
            "dense",
            VectorParamsBuilder::new(dense_dim, Distance::Cosine),
        );

        let mut sparse_vectors_config = SparseVectorsConfigBuilder::default();
        sparse_vectors_config.add_named_vector_params(
            "sparse_bm25",
            SparseVectorParamsBuilder::default().modifier(Modifier::Idf),
        );

        self.inner()
            .create_collection(
                CreateCollectionBuilder::new(collection_name(kb))
                    .vectors_config(vectors_config)
                    .sparse_vectors_config(sparse_vectors_config),
            )
            .await
            .map(|_| ())
            .map_err(|err| classify(kb, &err))
    }

    async fn create_payload_index(
        &self,
        kb: &KbSlug,
        field: &str,
        kind: PayloadFieldKind,
    ) -> Result<(), VectorStoreError> {
        let field_type = match kind {
            PayloadFieldKind::Keyword => FieldType::Keyword,
            PayloadFieldKind::Integer => FieldType::Integer,
        };
        self.inner()
            .create_field_index(CreateFieldIndexCollectionBuilder::new(
                collection_name(kb),
                field,
                field_type,
            ))
            .await
            .map(|_| ())
            .map_err(|err| classify(kb, &err))
    }

    async fn upsert_points(
        &self,
        kb: &KbSlug,
        points: Vec<PointStruct>,
    ) -> Result<(), VectorStoreError> {
        self.inner()
            .upsert_points(UpsertPointsBuilder::new(collection_name(kb), points).wait(true))
            .await
            .map(|_| ())
            .map_err(|err| classify(kb, &err))
    }

    async fn delete_points(
        &self,
        kb: &KbSlug,
        selector: PointSelector,
    ) -> Result<(), VectorStoreError> {
        self.inner()
            .delete_points(
                DeletePointsBuilder::new(collection_name(kb))
                    .points(selector_filter(&selector))
                    .wait(true),
            )
            .await
            .map(|_| ())
            .map_err(|err| classify(kb, &err))
    }

    async fn hybrid_search(
        &self,
        kb: &KbSlug,
        query: HybridQuery,
    ) -> Result<Vec<ScoredPoint>, VectorStoreError> {
        // Only the natively expressible half of the filter goes to Qdrant. The
        // remainder (currently `object_key_prefix`, which qdrant-client 1.15
        // cannot express as a keyword-index condition) is applied by the caller
        // to the returned hits, which is why it over-fetches.
        let native = query
            .filter
            .as_ref()
            .map(crate::searcher::filter::translate_filter)
            .and_then(|translated| translated.qdrant);

        let mut builder = QueryPointsBuilder::new(collection_name(kb))
            .add_prefetch(
                PrefetchQueryBuilder::default()
                    .query(Query::new_nearest(query.dense))
                    .using("dense")
                    .limit(query.prefetch_limit),
            )
            .add_prefetch(
                PrefetchQueryBuilder::default()
                    .query(Query::new_nearest(Document::new(query.text, "qdrant/bm25")))
                    .using("sparse_bm25")
                    .limit(query.prefetch_limit),
            )
            .query(Query::new_fusion(Fusion::Rrf))
            .limit(query.limit)
            .with_payload(true)
            .with_vectors(false);

        if let Some(filter) = native {
            builder = builder.filter(filter);
        }

        self.inner()
            .query(builder)
            .await
            .map(|response| response.result)
            .map_err(|err| classify(kb, &err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_clone_debug() {
        let c = QdrantConfig {
            url: "http://localhost:6334".into(),
            api_key: None,
            ..Default::default()
        };
        let _c2 = c.clone();
        let _s = format!("{c:?}");
    }

    #[test]
    fn new_without_api_key() {
        let config = QdrantConfig {
            url: "http://localhost:6334".into(),
            api_key: None,
            ..Default::default()
        };
        // Construction should succeed (no network until first RPC)
        let result = QdrantClient::new(&config);
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
    }

    #[test]
    fn new_with_api_key() {
        let config = QdrantConfig {
            url: "http://localhost:6334".into(),
            api_key: Some("test-key".into()),
            ..Default::default()
        };
        let result = QdrantClient::new(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn client_is_clone() {
        let config = QdrantConfig {
            url: "http://localhost:6334".into(),
            api_key: None,
            ..Default::default()
        };
        let client = QdrantClient::new(&config).unwrap();
        let _cloned = client.clone();
    }
}

//! Hybrid searcher combining dense (cosine) + sparse (BM25) prefetches
//! with Qdrant's server-side RRF fusion.

use crate::embedder::Embedder;
use crate::qdrant::QdrantClient;
use crate::worker::collection_name;
use async_trait::async_trait;
use notedthat_core::KbSlug;
use notedthat_core::search::{ObjectKey, SearchError, SearchHit, SearchResponse, ValidatedRequest};
use qdrant_client::qdrant::{
    Document, Fusion, PrefetchQueryBuilder, Query, QueryPointsBuilder, ScoredPoint,
};
use std::sync::Arc;

const POST_FILTER_OVER_FETCH_MULTIPLIER: u64 = 10;
const POST_FILTER_OVER_FETCH_CAP: u64 = 500;

/// Hybrid searcher that combines dense (cosine) + sparse (BM25) prefetches
/// with Qdrant's server-side RRF fusion.
///
/// The same `QdrantClient` and `Embedder` instances used by `IndexerWorker`
/// are shared here — using separate instances would risk vector space mismatch
/// (§6.4, D18).
#[allow(dead_code)]
pub struct HybridSearcher {
    qdrant: Arc<QdrantClient>,
    embedder: Arc<dyn Embedder>,
}

impl HybridSearcher {
    /// Create a new `HybridSearcher`.
    ///
    /// Must receive the SAME `qdrant` and `embedder` instances used by the
    /// `IndexerWorker` — different instances risk model or endpoint drift.
    pub fn new(qdrant: Arc<QdrantClient>, embedder: Arc<dyn Embedder>) -> Self {
        Self { qdrant, embedder }
    }

    /// Returns the Qdrant collection name for the given knowledge base.
    #[allow(dead_code)]
    pub(crate) fn collection_for(kb: &KbSlug) -> String {
        collection_name(kb)
    }
}

#[async_trait]
impl super::Searcher for HybridSearcher {
    #[tracing::instrument(
        skip(self),
        fields(
            kb = %kb,
            query_len = request.query.len(),
            limit = request.limit,
            qdrant_filter_present = tracing::field::Empty,
            post_filter_present = tracing::field::Empty,
        )
    )]
    async fn search(
        &self,
        kb: &KbSlug,
        request: ValidatedRequest,
    ) -> Result<SearchResponse, SearchError> {
        let collection = collection_name(kb);
        let query_text = request.query.clone();

        let mut embeddings = self
            .embedder
            .embed(std::slice::from_ref(&query_text))
            .await
            .map_err(search_error_from_embedder)?;
        if embeddings.len() != 1 {
            return Err(SearchError::internal(
                "embedder returned wrong number of vectors",
            ));
        }
        let Some(dense_vec) = embeddings.pop() else {
            return Err(SearchError::internal(
                "embedder returned wrong number of vectors",
            ));
        };

        let translated = request
            .filter
            .as_ref()
            .map(super::filter::translate_filter)
            .unwrap_or_default();
        tracing::Span::current()
            .record("qdrant_filter_present", translated.qdrant.is_some())
            .record("post_filter_present", !translated.post.is_empty());

        let prefetch_limit: u64 = if translated.qdrant.is_some() {
            100 // §9.6: bump prefetch limit under selective filters.
        } else {
            20
        };

        let outer_limit = if translated.post.is_empty() {
            u64::from(request.limit)
        } else {
            (u64::from(request.limit) * POST_FILTER_OVER_FETCH_MULTIPLIER)
                .min(POST_FILTER_OVER_FETCH_CAP)
        };

        let mut query_builder = QueryPointsBuilder::new(collection.clone())
            .add_prefetch(
                PrefetchQueryBuilder::default()
                    .query(Query::new_nearest(dense_vec))
                    .using("dense")
                    .limit(prefetch_limit),
            )
            .add_prefetch(
                PrefetchQueryBuilder::default()
                    .query(Query::new_nearest(Document::new(query_text, "qdrant/bm25")))
                    .using("sparse_bm25")
                    .limit(prefetch_limit),
            )
            .query(Query::new_fusion(Fusion::Rrf))
            .limit(outer_limit)
            .with_payload(true)
            .with_vectors(false);

        if let Some(filter) = translated.qdrant {
            query_builder = query_builder.filter(filter);
        }

        let response = self
            .qdrant
            .inner()
            .query(query_builder)
            .await
            .map_err(|err| search_error_from_qdrant(&collection, err))?;

        let mut hits: Vec<SearchHit> = response
            .result
            .into_iter()
            .map(point_to_hit)
            .collect::<Result<Vec<_>, _>>()?;

        hits.retain(|hit| translated.post.matches(hit.object_key.as_str()));
        hits.truncate(request.limit as usize);

        Ok(SearchResponse::new(hits))
    }
}

fn point_to_hit(point: ScoredPoint) -> Result<SearchHit, SearchError> {
    use qdrant_client::qdrant::value::Kind;

    let payload = point.payload;

    let object_key_str = payload
        .get("object_key")
        .and_then(|value| match &value.kind {
            Some(Kind::StringValue(value)) => Some(value.clone()),
            _ => None,
        })
        .ok_or_else(|| SearchError::internal("hit missing object_key"))?;
    let object_key = ObjectKey::try_new(object_key_str)
        .map_err(|err| SearchError::internal(format!("invalid object_key: {err}")))?;

    let byte_start = payload
        .get("byte_start")
        .and_then(|value| match &value.kind {
            Some(Kind::IntegerValue(value)) => u64::try_from(*value).ok(),
            _ => None,
        })
        .unwrap_or(0);
    let byte_end = payload
        .get("byte_end")
        .and_then(|value| match &value.kind {
            Some(Kind::IntegerValue(value)) => u64::try_from(*value).ok(),
            _ => None,
        })
        .unwrap_or(0);

    let heading_path = payload
        .get("heading_path")
        .and_then(|value| match &value.kind {
            Some(Kind::ListValue(list)) => Some(
                list.values
                    .iter()
                    .filter_map(|value| match &value.kind {
                        Some(Kind::StringValue(value)) => Some(value.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default();

    let text = payload
        .get("text")
        .and_then(|value| match &value.kind {
            Some(Kind::StringValue(value)) => Some(value.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    let preview = super::preview::truncate_preview(text, super::preview::PREVIEW_MAX_CHARS);
    let okf = payload
        .get("okf")
        .map(|value| {
            serde_json::from_value(value.clone().into_json())
                .map_err(|err| SearchError::internal(format!("invalid OKF metadata: {err}")))
        })
        .transpose()?;

    Ok(SearchHit {
        object_key,
        byte_start,
        byte_end,
        heading_path,
        score: point.score,
        preview,
        okf,
    })
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn search_error_from_qdrant(
    collection: &str,
    err: qdrant_client::QdrantError,
) -> SearchError {
    let message = err.to_string();
    let lower = message.to_ascii_lowercase();
    if lower.contains("not found")
        || lower.contains("doesn't exist")
        || lower.contains("does not exist")
    {
        let slug = collection
            .trim_start_matches("kb_")
            .trim_end_matches("_v1")
            .to_string();
        SearchError::UnknownKb { slug }
    } else {
        SearchError::BackendUnavailable { message }
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn search_error_from_embedder(err: crate::embedder::EmbedderError) -> SearchError {
    SearchError::BackendUnavailable {
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests;

//! Hybrid searcher combining dense (cosine) + sparse (BM25) prefetches
//! with Qdrant's server-side RRF fusion.

use crate::embedder::Embedder;
use crate::vector_store::{HybridQuery, VectorStore, VectorStoreError};
use crate::worker::collection_name;
use async_trait::async_trait;
use notedthat_core::KbSlug;
use notedthat_core::search::{ObjectKey, SearchError, SearchHit, SearchResponse, ValidatedRequest};
use qdrant_client::qdrant::ScoredPoint;
use std::sync::Arc;

/// Each prefetch arm's depth when nothing narrows the candidates.
const PREFETCH_FLOOR: u64 = 20;
/// Each arm's depth under a Qdrant-native payload filter, which Qdrant applies
/// after its own top-k and would otherwise starve fusion (§8.6).
const FILTERED_PREFETCH_FLOOR: u64 = 100;
/// Over-fetch factor when a key filter is applied client-side, after fusion:
/// the request's `object_key_prefix`, or the caller's grant.
const POST_FILTER_OVER_FETCH_MULTIPLIER: u64 = 10;
/// Cap on each arm's depth; the fused window is at most twice this.
const PREFETCH_CAP: u64 = 250;

/// How deep the backend looks for one request (D56).
///
/// Fusion can only rank what the two prefetch arms returned, so the arm depth
/// is what decides how many candidates exist — a fused `limit` above twice the
/// arm depth is inert, which is how #68 happened. Both numbers are derived
/// here from one computation so they cannot drift apart again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FetchWindow {
    /// Candidates each prefetch arm contributes to fusion.
    prefetch_limit: u64,
    /// Fused points asked of the backend: always `2 * prefetch_limit`, the
    /// whole fused set (the union of both arms), so the backend never
    /// truncates and therefore never resolves a tie — the searcher's own
    /// order does (#128).
    limit: u64,
}

fn fetch_window(limit: u32, native_filter: bool, post_filter: bool) -> FetchWindow {
    let wanted = if post_filter {
        u64::from(limit) * POST_FILTER_OVER_FETCH_MULTIPLIER
    } else {
        u64::from(limit)
    };
    let floor = if native_filter {
        FILTERED_PREFETCH_FLOOR
    } else {
        PREFETCH_FLOOR
    };
    let prefetch_limit = wanted.max(floor).min(PREFETCH_CAP);
    FetchWindow {
        prefetch_limit,
        limit: prefetch_limit * 2,
    }
}

/// The documented order of hits (D56): RRF score descending, then
/// `object_key` byte-lexicographic ascending, then `byte_start` ascending.
///
/// `(object_key, byte_start)` is unique per chunk, so the order is total and
/// one query against one index yields byte-identical hits between calls,
/// whatever order the backend handed equal scores back in (#128).
fn sort_hits(hits: &mut [SearchHit]) {
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.object_key.as_str().cmp(b.object_key.as_str()))
            .then_with(|| a.byte_start.cmp(&b.byte_start))
    });
}

/// Hybrid searcher that combines dense (cosine) + sparse (BM25) prefetches
/// with Qdrant's server-side RRF fusion.
///
/// The same `QdrantClient` and `Embedder` instances used by `IndexerWorker`
/// are shared here — using separate instances would risk vector space mismatch
/// (§6.4, D18).
#[allow(dead_code)]
pub struct HybridSearcher {
    store: Arc<dyn VectorStore>,
    embedder: Arc<dyn Embedder>,
}

impl HybridSearcher {
    /// Create a new `HybridSearcher`.
    ///
    /// Must receive the SAME `store` and `embedder` instances used by the
    /// `IndexerWorker` — different instances risk model or endpoint drift.
    pub fn new(store: Arc<dyn VectorStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self { store, embedder }
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
        skip(self, key_filter),
        fields(
            kb = %kb,
            query_len = request.query.len(),
            limit = request.limit,
            qdrant_filter_present = tracing::field::Empty,
            post_filter_present = tracing::field::Empty,
            key_filter_present = key_filter.is_some(),
            prefetch_limit = tracing::field::Empty,
            fetch_limit = tracing::field::Empty,
        )
    )]
    async fn search(
        &self,
        kb: &KbSlug,
        request: ValidatedRequest,
        key_filter: Option<super::KeyPredicate<'_>>,
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
        let window = fetch_window(
            request.limit,
            translated.qdrant.is_some(),
            !translated.post.is_empty() || key_filter.is_some(),
        );
        tracing::Span::current()
            .record("qdrant_filter_present", translated.qdrant.is_some())
            .record("post_filter_present", !translated.post.is_empty())
            .record("prefetch_limit", window.prefetch_limit)
            .record("fetch_limit", window.limit);

        let points = self
            .store
            .hybrid_search(
                kb,
                HybridQuery {
                    text: query_text,
                    dense: dense_vec,
                    filter: request.filter.clone(),
                    prefetch_limit: window.prefetch_limit,
                    limit: window.limit,
                },
            )
            .await
            .map_err(|err| search_error_from_store(&collection, err))?;

        let mut hits: Vec<SearchHit> = points
            .into_iter()
            .map(point_to_hit)
            .collect::<Result<Vec<_>, _>>()?;

        // Both client-side key filters run over the whole fused window, and
        // the window is ordered before it is cut: the page is then the first
        // `limit` acceptable hits in the documented order, not whatever the
        // backend happened to return first.
        hits.retain(|hit| {
            let key = hit.object_key.as_str();
            translated.post.matches(key) && key_filter.is_none_or(|allows| allows(key))
        });
        sort_hits(&mut hits);
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
pub(crate) fn search_error_from_store(collection: &str, err: VectorStoreError) -> SearchError {
    match err {
        VectorStoreError::CollectionNotFound { kb } => SearchError::UnknownKb { slug: kb },
        VectorStoreError::Backend { message } => {
            // A backend that reports a missing collection as a plain transport
            // error still has to be classified as an unknown KB rather than an
            // outage, so the message is inspected as a fallback.
            let lower = message.to_ascii_lowercase();
            if lower.contains("not found")
                || lower.contains("doesn't exist")
                || lower.contains("does not exist")
            {
                SearchError::UnknownKb {
                    slug: collection
                        .trim_start_matches("kb_")
                        .trim_end_matches("_v1")
                        .to_string(),
                }
            } else {
                SearchError::BackendUnavailable { message }
            }
        }
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

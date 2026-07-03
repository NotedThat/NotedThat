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

    Ok(SearchHit {
        object_key,
        byte_start,
        byte_end,
        heading_path,
        score: point.score,
        preview,
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
mod tests {
    use super::*;
    use notedthat_core::search::SearchError;
    use qdrant_client::qdrant::{ScoredPoint, Value, value::Kind};
    use std::collections::HashMap;

    #[test]
    fn hybrid_searcher_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<HybridSearcher>();
    }

    #[test]
    fn collection_for_format() {
        use notedthat_core::KbSlug;
        let slug = KbSlug::try_new("notes").unwrap();
        assert_eq!(HybridSearcher::collection_for(&slug), "kb_notes_v1");
    }

    #[test]
    fn search_error_from_qdrant_classifies_not_found_as_unknown_kb() {
        let err = search_error_from_qdrant(
            "kb_my-notes_v1",
            qdrant_client::QdrantError::ConversionError("collection not found".into()),
        );

        assert!(matches!(
            err,
            SearchError::UnknownKb { slug } if slug == "my-notes"
        ));
    }

    #[test]
    fn search_error_from_qdrant_classifies_other_errors_as_backend_unavailable() {
        let err = search_error_from_qdrant(
            "kb_notes_v1",
            qdrant_client::QdrantError::ConversionError("transport closed".into()),
        );

        assert!(matches!(err, SearchError::BackendUnavailable { .. }));
    }

    #[test]
    fn search_error_from_embedder_classifies_backend_unavailable() {
        let err = search_error_from_embedder(crate::embedder::EmbedderError::Transport(
            "connection refused".into(),
        ));

        assert!(matches!(err, SearchError::BackendUnavailable { .. }));
    }

    #[test]
    fn point_to_hit_extracts_payload_fields() {
        let mut payload = HashMap::new();
        payload.insert("object_key".to_string(), "docs/a.md".to_string().into());
        payload.insert("byte_start".to_string(), 5_i64.into());
        payload.insert("byte_end".to_string(), 42_i64.into());
        payload.insert("heading_path".to_string(), vec!["A", "B"].into());
        payload.insert("text".to_string(), "hello world".to_string().into());

        let hit = point_to_hit(scored_point(payload, 0.5)).expect("valid hit");

        assert_eq!(hit.object_key.as_str(), "docs/a.md");
        assert_eq!(hit.byte_start, 5);
        assert_eq!(hit.byte_end, 42);
        assert_eq!(hit.heading_path, vec!["A", "B"]);
        assert!((hit.score - 0.5).abs() < f32::EPSILON);
        assert_eq!(hit.preview, "hello world");
    }

    #[test]
    fn point_to_hit_missing_text_yields_empty_preview() {
        let mut payload = HashMap::new();
        payload.insert("object_key".to_string(), "docs/a.md".to_string().into());
        payload.insert("byte_start".to_string(), 0_i64.into());
        payload.insert("byte_end".to_string(), 100_i64.into());

        let hit = point_to_hit(scored_point(payload, 0.5)).expect("valid hit");

        assert!(hit.preview.is_empty());
        assert_eq!(hit.object_key.as_str(), "docs/a.md");
        assert!((hit.score - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn point_to_hit_missing_object_key_returns_error() {
        let err = point_to_hit(scored_point(HashMap::new(), 0.5)).expect_err("missing object key");

        assert!(matches!(err, SearchError::Internal { .. }));
    }

    #[test]
    fn point_to_hit_invalid_object_key_returns_error() {
        let mut payload = HashMap::new();
        payload.insert("object_key".to_string(), "/docs/a.md".to_string().into());

        let err = point_to_hit(scored_point(payload, 0.5)).expect_err("invalid object key");

        assert!(matches!(err, SearchError::Internal { .. }));
    }

    #[test]
    fn point_to_hit_missing_heading_path_yields_empty_vec() {
        let mut payload = HashMap::new();
        payload.insert("object_key".to_string(), "docs/b.md".to_string().into());

        let hit = point_to_hit(scored_point(payload, 0.1)).expect("valid hit");

        assert!(hit.heading_path.is_empty());
    }

    #[test]
    fn point_to_hit_negative_offsets_default_to_zero() {
        let mut payload = HashMap::new();
        payload.insert("object_key".to_string(), "docs/b.md".to_string().into());
        payload.insert("byte_start".to_string(), (-1_i64).into());
        payload.insert("byte_end".to_string(), (-2_i64).into());

        let hit = point_to_hit(scored_point(payload, 0.1)).expect("valid hit");

        assert_eq!(hit.byte_start, 0);
        assert_eq!(hit.byte_end, 0);
    }

    #[test]
    fn preview_truncated_to_500_chars() {
        let long_text = "日本語".repeat(300);
        let mut payload = HashMap::new();
        payload.insert("object_key".to_string(), "docs/c.md".to_string().into());
        payload.insert("text".to_string(), long_text.into());

        let hit = point_to_hit(scored_point(payload, 0.2)).expect("valid hit");

        assert_eq!(hit.preview.chars().count(), 500);
    }

    #[test]
    fn point_to_hit_ignores_non_string_heading_values() {
        let mut payload = HashMap::new();
        payload.insert("object_key".to_string(), "docs/d.md".to_string().into());
        payload.insert(
            "heading_path".to_string(),
            Value {
                kind: Some(Kind::ListValue(qdrant_client::qdrant::ListValue {
                    values: vec!["A".to_string().into(), 1_i64.into(), "B".to_string().into()],
                })),
            },
        );

        let hit = point_to_hit(scored_point(payload, 0.3)).expect("valid hit");

        assert_eq!(hit.heading_path, vec!["A", "B"]);
    }

    fn scored_point(payload: HashMap<String, Value>, score: f32) -> ScoredPoint {
        ScoredPoint {
            id: None,
            payload,
            score,
            version: 0,
            vectors: None,
            shard_key: None,
            order_value: None,
        }
    }
}

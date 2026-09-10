//! In-memory [`VectorStore`] for tests.
//!
//! # What this is for
//!
//! Every test that touched indexing or search used to start its own Qdrant
//! container. This implements the same [`VectorStore`] contract in-process so
//! those tests keep their assertions without the container.
//!
//! # How faithful it is
//!
//! Faithful enough that ranking assertions still mean something, and no more.
//! The retrieval shape mirrors the Qdrant configuration in
//! [`crate::qdrant`]: a dense cosine arm, a sparse BM25 arm with IDF, and
//! Reciprocal Rank Fusion over the two, with the same prefetch and outer limits
//! the caller passes. Scores are therefore comparable in *order* to a real
//! backend, but not in absolute value — Qdrant's BM25 tokenizer and IDF
//! bookkeeping are its own, and this makes no attempt to reproduce them
//! bit-for-bit.
//!
//! What it does not model: sharding, quantization, index build latency (payload
//! indexes are recorded and instantly available, where Qdrant builds them in the
//! background), consistency levels, and payload-index-dependent query planning.
//! A test that needs any of those needs a real Qdrant.

use crate::vector_store::{
    HybridQuery, IndexedObject, PayloadFieldKind, PointSelector, VectorStore, VectorStoreError,
};
use async_trait::async_trait;
use notedthat_core::KbSlug;
use notedthat_core::search::SearchFilter;
use qdrant_client::qdrant::{
    NamedVectorsOutput, PointId, PointStruct, RetrievedPoint, ScoredPoint, Value, VectorOutput,
    VectorsOutput, value::Kind, vector, vectors_output::VectorsOptions,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Rank constant for Reciprocal Rank Fusion, matching Qdrant's default.
const RRF_K: f32 = 60.0;

/// BM25 term-frequency saturation.
const BM25_K1: f64 = 1.2;

/// BM25 length normalisation.
const BM25_B: f64 = 0.75;

/// One indexed point.
#[derive(Debug, Clone)]
struct StoredPoint {
    dense: Vec<f32>,
    /// Text handed to the sparse arm, recovered from the point's BM25 document.
    text: String,
    /// Names of the vectors the point was written with.
    ///
    /// Recorded because "were both the dense and the sparse vector written?" is
    /// a real regression this suite guards — an earlier release wrote only
    /// `dense`, which silently disabled the BM25 arm.
    vector_names: BTreeSet<String>,
    payload: HashMap<String, Value>,
}

/// One knowledge base's collection.
#[derive(Debug, Default)]
struct Collection {
    dense_dim: u64,
    payload_indexes: BTreeSet<String>,
    points: BTreeMap<u64, StoredPoint>,
}

/// In-memory vector store implementing the production [`VectorStore`] contract.
///
/// Cloning shares the same underlying state, so a clone handed to an indexer
/// worker and one handed to a searcher observe each other's writes — matching
/// how a single Qdrant instance is shared in production.
#[derive(Debug, Default, Clone)]
pub struct InMemoryVectorStore {
    inner: Arc<RwLock<HashMap<String, Collection>>>,
}

impl InMemoryVectorStore {
    /// Create an empty store with no collections.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of points currently stored for `kb`, or `None` if it has no collection.
    pub async fn point_count(&self, kb: &KbSlug) -> Option<usize> {
        let state = self.inner.read().await;
        state.get(kb.as_str()).map(|c| c.points.len())
    }

    /// Payload values stored under `field`, one entry per point, in point-id order.
    ///
    /// Lets a test assert on what was indexed without a `scroll` round trip.
    pub async fn payload_values(&self, kb: &KbSlug, field: &str) -> Vec<Value> {
        let state = self.inner.read().await;
        state
            .get(kb.as_str())
            .into_iter()
            .flat_map(|c| c.points.values())
            .filter_map(|point| point.payload.get(field).cloned())
            .collect()
    }

    /// Distinct `object_key` payload values currently indexed for `kb`.
    pub async fn indexed_object_keys(&self, kb: &KbSlug) -> BTreeSet<String> {
        self.payload_values(kb, "object_key")
            .await
            .iter()
            .filter_map(as_string)
            .collect()
    }

    /// Points for one object, in chunk order, shaped like a Qdrant `scroll` result.
    ///
    /// Returning [`RetrievedPoint`] rather than a bespoke view means the payload
    /// and vector assertions written against a real Qdrant keep working
    /// unchanged. `with_vectors` mirrors the scroll flag: the names written are
    /// reported, with the dense data attached; sparse vectors are reported by
    /// name only, since the backend — not the caller — computes them.
    pub async fn scroll_object(
        &self,
        kb: &KbSlug,
        object_key: &str,
        with_vectors: bool,
    ) -> Vec<RetrievedPoint> {
        let state = self.inner.read().await;
        let Some(collection) = state.get(kb.as_str()) else {
            return Vec::new();
        };

        let mut matching: Vec<(i64, u64, &StoredPoint)> = collection
            .points
            .iter()
            .filter(|(_, point)| {
                point
                    .payload
                    .get("object_key")
                    .and_then(as_string)
                    .as_deref()
                    == Some(object_key)
            })
            .map(|(id, point)| {
                let chunk_index = point.payload.get("chunk_index").and_then(as_integer);
                (chunk_index.unwrap_or_default(), *id, point)
            })
            .collect();
        matching.sort_by_key(|(chunk_index, id, _)| (*chunk_index, *id));

        matching
            .into_iter()
            .map(|(_, id, point)| RetrievedPoint {
                id: Some(PointId::from(id)),
                payload: point.payload.clone(),
                vectors: with_vectors.then(|| VectorsOutput {
                    vectors_options: Some(VectorsOptions::Vectors(NamedVectorsOutput {
                        vectors: point
                            .vector_names
                            .iter()
                            .map(|name| {
                                let data = if name == "dense" {
                                    point.dense.clone()
                                } else {
                                    Vec::new()
                                };
                                (
                                    name.clone(),
                                    VectorOutput {
                                        data,
                                        ..VectorOutput::default()
                                    },
                                )
                            })
                            .collect(),
                    })),
                }),
                ..RetrievedPoint::default()
            })
            .collect()
    }

    /// Payload index field names ensured for `kb`.
    pub async fn payload_indexes(&self, kb: &KbSlug) -> BTreeSet<String> {
        let state = self.inner.read().await;
        state
            .get(kb.as_str())
            .map(|c| c.payload_indexes.clone())
            .unwrap_or_default()
    }

    /// Dense vector width the collection was created with.
    pub async fn dense_dim(&self, kb: &KbSlug) -> Option<u64> {
        let state = self.inner.read().await;
        state.get(kb.as_str()).map(|c| c.dense_dim)
    }
}

/// Resolve a dotted payload path, walking nested structs.
///
/// Qdrant treats `okf.type` as a path into a nested payload object, not as a
/// key containing a dot — the indexer writes `okf` as a struct, and the search
/// filter addresses `okf.type`. Looking it up flatly silently matched nothing.
fn payload_path<'a>(payload: &'a HashMap<String, Value>, path: &str) -> Option<&'a Value> {
    let mut segments = path.split('.');
    let mut current = payload.get(segments.next()?)?;
    for segment in segments {
        match &current.kind {
            Some(Kind::StructValue(nested)) => current = nested.fields.get(segment)?,
            _ => return None,
        }
    }
    Some(current)
}

/// Read a payload value as a string, if it is one.
fn as_string(value: &Value) -> Option<String> {
    match &value.kind {
        Some(Kind::StringValue(text)) => Some(text.clone()),
        _ => None,
    }
}

/// Read a payload value as an integer, if it is one.
fn as_integer(value: &Value) -> Option<i64> {
    match &value.kind {
        Some(Kind::IntegerValue(number)) => Some(*number),
        _ => None,
    }
}

/// Read a payload value as a list of strings, if it is one.
fn as_string_list(value: &Value) -> Vec<String> {
    match &value.kind {
        Some(Kind::ListValue(list)) => list.values.iter().filter_map(as_string).collect(),
        _ => Vec::new(),
    }
}

/// Split text into lowercase alphanumeric terms.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Cosine similarity, returning 0.0 for a zero-length or mismatched vector.
fn cosine(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() {
        return 0.0;
    }
    let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
    let left_norm: f32 = left.iter().map(|a| a * a).sum::<f32>().sqrt();
    let right_norm: f32 = right.iter().map(|b| b * b).sum::<f32>().sqrt();
    if left_norm == 0.0 || right_norm == 0.0 {
        0.0
    } else {
        dot / (left_norm * right_norm)
    }
}

/// Evaluate a [`SearchFilter`] against one point's payload.
///
/// Unlike the Qdrant path — which expresses what it can as native conditions and
/// leaves `object_key_prefix` to a client-side post-filter — this evaluates every
/// field, including the prefix. Applying the caller's post-filter afterwards is
/// then a no-op rather than a correction.
fn matches_filter(payload: &HashMap<String, Value>, filter: &SearchFilter) -> bool {
    let string_field = |field: &str| payload_path(payload, field).and_then(as_string);

    if let Some(prefix) = &filter.object_key_prefix
        && !string_field("object_key").is_some_and(|key| key.starts_with(prefix))
    {
        return false;
    }
    if let Some(mime) = &filter.mime
        && string_field("mime").as_ref() != Some(mime)
    {
        return false;
    }
    if let Some(concept_type) = &filter.concept_type
        && string_field("okf.type").as_ref() != Some(concept_type)
    {
        return false;
    }
    if !filter.heading_path_prefix.is_empty() {
        let heading_path = payload
            .get("heading_path")
            .map(as_string_list)
            .unwrap_or_default();
        if heading_path.len() < filter.heading_path_prefix.len()
            || heading_path[..filter.heading_path_prefix.len()] != filter.heading_path_prefix[..]
        {
            return false;
        }
    }
    if filter.updated_after.is_some() || filter.updated_before.is_some() {
        let Some(mtime) = payload_path(payload, "mtime").and_then(as_integer) else {
            return false;
        };
        if filter.updated_after.is_some_and(|after| mtime < after) {
            return false;
        }
        if filter.updated_before.is_some_and(|before| mtime > before) {
            return false;
        }
    }
    if !filter.tags.is_empty() {
        let tags = payload_path(payload, "tags")
            .map(as_string_list)
            .unwrap_or_default();
        if !filter.tags.iter().any(|wanted| tags.contains(wanted)) {
            return false;
        }
    }
    true
}

/// Rank candidates by descending score and return their ids.
///
/// Generic over the score type so each arm can keep its natural precision —
/// `f32` for cosine, `f64` for BM25 — without a narrowing cast that only
/// ordering depends on.
fn ranked_ids<S: PartialOrd>(mut scored: Vec<(u64, S)>, limit: usize) -> Vec<u64> {
    // Ties break on point id so ordering is deterministic across runs.
    scored.sort_by(|(left_id, left), (right_id, right)| {
        right
            .partial_cmp(left)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left_id.cmp(right_id))
    });
    scored.into_iter().take(limit).map(|(id, _)| id).collect()
}

/// BM25 scores for `query` over `candidates`, using the whole candidate set as
/// the corpus for document frequency.
///
/// Arithmetic is `f64` because term counts convert into it losslessly. Only the
/// ordering of these scores is used — the score a `ScoredPoint` carries comes
/// from the fusion step, not from either arm.
fn bm25_scores(query: &str, candidates: &[(u64, &StoredPoint)]) -> Vec<(u64, f64)> {
    /// Convert a count to `f64` without a lossy cast.
    fn count(value: usize) -> f64 {
        f64::from(u32::try_from(value).unwrap_or(u32::MAX))
    }

    let query_terms = tokenize(query);
    if query_terms.is_empty() || candidates.is_empty() {
        return Vec::new();
    }

    let documents: Vec<(u64, Vec<String>)> = candidates
        .iter()
        .map(|(id, point)| (*id, tokenize(&point.text)))
        .collect();
    let total_documents = count(documents.len());
    let average_length = documents
        .iter()
        .map(|(_, terms)| count(terms.len()))
        .sum::<f64>()
        / total_documents;

    let mut scored = Vec::with_capacity(documents.len());
    for (id, terms) in &documents {
        let length = count(terms.len());
        let mut score = 0.0_f64;
        for term in &query_terms {
            let frequency = count(terms.iter().filter(|candidate| *candidate == term).count());
            if frequency == 0.0 {
                continue;
            }
            let document_frequency = count(
                documents
                    .iter()
                    .filter(|(_, other)| other.contains(term))
                    .count(),
            );
            // Same IDF form Qdrant's Idf modifier uses.
            let idf = ((total_documents - document_frequency + 0.5) / (document_frequency + 0.5)
                + 1.0)
                .ln();
            let denominator =
                frequency + BM25_K1 * (1.0 - BM25_B + BM25_B * length / average_length);
            score += idf * (frequency * (BM25_K1 + 1.0)) / denominator;
        }
        if score > 0.0 {
            scored.push((*id, score));
        }
    }
    scored
}

#[async_trait]
impl VectorStore for InMemoryVectorStore {
    async fn collection_exists(&self, kb: &KbSlug) -> Result<bool, VectorStoreError> {
        let state = self.inner.read().await;
        Ok(state.contains_key(kb.as_str()))
    }

    async fn create_collection(&self, kb: &KbSlug, dense_dim: u64) -> Result<(), VectorStoreError> {
        let mut state = self.inner.write().await;
        state
            .entry(kb.as_str().to_string())
            .or_insert_with(Collection::default)
            .dense_dim = dense_dim;
        Ok(())
    }

    async fn create_payload_index(
        &self,
        kb: &KbSlug,
        field: &str,
        _kind: PayloadFieldKind,
    ) -> Result<(), VectorStoreError> {
        let mut state = self.inner.write().await;
        let collection =
            state
                .get_mut(kb.as_str())
                .ok_or_else(|| VectorStoreError::CollectionNotFound {
                    kb: kb.as_str().to_string(),
                })?;
        collection.payload_indexes.insert(field.to_string());
        Ok(())
    }

    async fn upsert_points(
        &self,
        kb: &KbSlug,
        points: Vec<PointStruct>,
    ) -> Result<(), VectorStoreError> {
        let mut state = self.inner.write().await;
        let collection =
            state
                .get_mut(kb.as_str())
                .ok_or_else(|| VectorStoreError::CollectionNotFound {
                    kb: kb.as_str().to_string(),
                })?;

        for point in points {
            let Some(qdrant_client::qdrant::point_id::PointIdOptions::Num(id)) =
                point.id.and_then(|id| id.point_id_options)
            else {
                return Err(VectorStoreError::backend(
                    "in-memory store requires numeric point ids",
                ));
            };

            let mut dense = Vec::new();
            let mut text = String::new();
            let mut vector_names = BTreeSet::new();
            if let Some(qdrant_client::qdrant::vectors::VectorsOptions::Vectors(named)) =
                point.vectors.and_then(|vectors| vectors.vectors_options)
            {
                for (name, value) in named.vectors {
                    vector_names.insert(name.clone());
                    match value.vector {
                        Some(vector::Vector::Dense(dense_vector)) if name == "dense" => {
                            dense = dense_vector.data;
                        }
                        Some(vector::Vector::Document(document)) => text = document.text,
                        _ => {}
                    }
                }
            }

            // Reject a dense vector whose width does not match the collection,
            // as Qdrant does. The indexer relies on this: a batch embedded at
            // the wrong dimension must fail the write rather than silently
            // corrupt the collection, leaving the previous points in place for
            // a later repair pass.
            if vector_names.contains("dense") {
                let width = u64::try_from(dense.len()).unwrap_or(u64::MAX);
                if width != collection.dense_dim {
                    return Err(VectorStoreError::backend(format!(
                        "dense vector width {width} does not match collection width {}",
                        collection.dense_dim
                    )));
                }
            }

            collection.points.insert(
                id,
                StoredPoint {
                    dense,
                    text,
                    vector_names,
                    payload: point.payload,
                },
            );
        }
        Ok(())
    }

    async fn delete_points(
        &self,
        kb: &KbSlug,
        selector: PointSelector,
    ) -> Result<(), VectorStoreError> {
        let mut state = self.inner.write().await;
        let collection =
            state
                .get_mut(kb.as_str())
                .ok_or_else(|| VectorStoreError::CollectionNotFound {
                    kb: kb.as_str().to_string(),
                })?;

        collection.points.retain(|_, point| {
            let key = point.payload.get("object_key").and_then(as_string);
            match &selector {
                PointSelector::Object { object_key } => key.as_ref() != Some(object_key),
                PointSelector::ObjectChunksFrom {
                    object_key,
                    from_chunk_index,
                } => {
                    if key.as_ref() != Some(object_key) {
                        return true;
                    }
                    let chunk_index = point.payload.get("chunk_index").and_then(as_integer);
                    chunk_index.is_none_or(|index| index < i64::from(*from_chunk_index))
                }
            }
        });
        Ok(())
    }

    async fn indexed_etag(
        &self,
        kb: &KbSlug,
        object_key: &str,
    ) -> Result<Option<String>, VectorStoreError> {
        let state = self.inner.read().await;
        Ok(state
            .get(kb.as_str())
            .into_iter()
            .flat_map(|collection| collection.points.values())
            .find(|point| {
                point
                    .payload
                    .get("object_key")
                    .and_then(as_string)
                    .as_deref()
                    == Some(object_key)
            })
            .and_then(|point| point.payload.get("etag").and_then(as_string)))
    }

    async fn indexed_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
    ) -> Result<Vec<IndexedObject>, VectorStoreError> {
        let state = self.inner.read().await;
        let mut by_key: BTreeMap<String, String> = BTreeMap::new();
        for point in state
            .get(kb.as_str())
            .into_iter()
            .flat_map(|collection| collection.points.values())
        {
            let Some(object_key) = point.payload.get("object_key").and_then(as_string) else {
                continue;
            };
            if prefix.is_some_and(|prefix| !object_key.starts_with(prefix)) {
                continue;
            }
            by_key.entry(object_key).or_insert_with(|| {
                point
                    .payload
                    .get("etag")
                    .and_then(as_string)
                    .unwrap_or_default()
            });
        }
        Ok(by_key
            .into_iter()
            .map(|(object_key, etag)| IndexedObject { object_key, etag })
            .collect())
    }

    async fn hybrid_search(
        &self,
        kb: &KbSlug,
        query: HybridQuery,
    ) -> Result<Vec<ScoredPoint>, VectorStoreError> {
        let state = self.inner.read().await;
        let collection =
            state
                .get(kb.as_str())
                .ok_or_else(|| VectorStoreError::CollectionNotFound {
                    kb: kb.as_str().to_string(),
                })?;

        let candidates: Vec<(u64, &StoredPoint)> = collection
            .points
            .iter()
            .filter(|(_, point)| {
                query
                    .filter
                    .as_ref()
                    .is_none_or(|filter| matches_filter(&point.payload, filter))
            })
            .map(|(id, point)| (*id, point))
            .collect();

        let prefetch = usize::try_from(query.prefetch_limit).unwrap_or(usize::MAX);

        let dense_ranked = ranked_ids(
            candidates
                .iter()
                .map(|(id, point)| (*id, cosine(&query.dense, &point.dense)))
                .collect(),
            prefetch,
        );
        let sparse_ranked = ranked_ids(bm25_scores(&query.text, &candidates), prefetch);

        // Reciprocal Rank Fusion over the two arms, as Qdrant's Fusion::Rrf does.
        let mut fused: HashMap<u64, f32> = HashMap::new();
        for ranked in [&dense_ranked, &sparse_ranked] {
            for (rank, id) in ranked.iter().enumerate() {
                let rank = f32::from(u16::try_from(rank).unwrap_or(u16::MAX));
                *fused.entry(*id).or_insert(0.0) += 1.0 / (RRF_K + rank + 1.0);
            }
        }

        let limit = usize::try_from(query.limit).unwrap_or(usize::MAX);
        let winners = ranked_ids(
            fused.iter().map(|(id, score)| (*id, *score)).collect(),
            limit,
        );

        Ok(winners
            .into_iter()
            .filter_map(|id| {
                let point = collection.points.get(&id)?;
                Some(ScoredPoint {
                    id: Some(PointId::from(id)),
                    payload: point.payload.clone(),
                    score: fused.get(&id).copied().unwrap_or_default(),
                    ..ScoredPoint::default()
                })
            })
            .collect())
    }
}

/// Embedder producing deterministic vectors without an embedding endpoint.
///
/// Used by tests that need the indexing pipeline to *succeed* — the server
/// suites, which exercise routes end to end — rather than tests about embedding
/// behaviour itself, which script their own embedder.
///
/// Vectors are derived from a hash of the text, so they are stable across runs
/// and distinct per document, but they carry no semantic similarity: two texts
/// about the same subject are no closer than two unrelated ones. Lexical
/// matching in those suites therefore rests on the sparse BM25 arm, which does
/// read the text. A test asserting on dense semantic ranking needs a real
/// embedder, not this.
#[derive(Debug, Clone)]
pub struct StubEmbedder {
    dim: usize,
}

impl StubEmbedder {
    /// Build a stub producing `dim`-wide vectors.
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }

    /// Deterministic unit vector for `text`.
    fn vector_for(&self, text: &str) -> Vec<f32> {
        use sha2::{Digest, Sha256};

        let digest = Sha256::digest(text.as_bytes());
        let mut values: Vec<f32> = (0..self.dim)
            .map(|index| {
                let byte = digest[index % digest.len()];
                // Map a byte onto [-1, 1] so directions vary across documents.
                (f32::from(byte) - 127.5) / 127.5
            })
            .collect();

        let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut values {
                *value /= norm;
            }
        } else if let Some(first) = values.first_mut() {
            *first = 1.0;
        }
        values
    }
}

#[async_trait]
impl crate::embedder::Embedder for StubEmbedder {
    async fn embed(
        &self,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, crate::embedder::EmbedderError> {
        Ok(texts.iter().map(|text| self.vector_for(text)).collect())
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn max_input_tokens(&self) -> usize {
        8192
    }

    fn model_id(&self) -> &'static str {
        "stub-embedder"
    }
}

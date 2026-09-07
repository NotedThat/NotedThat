//! `IndexerWorker` — serial async task draining `IndexEvent`s into Qdrant.
//!
//! Behavior: one event at a time, batched embedding, drain on shutdown.

use crate::{chunker, embedder::Embedder, event::IndexEvent, qdrant::QdrantClient};
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, Storage, StorageError};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Maximum time to continue draining already queued events after cancellation.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Async indexing worker.
pub struct IndexerWorker {
    /// Object storage used to re-read source documents before indexing.
    pub storage: Arc<dyn Storage>,
    /// Embedding endpoint used to turn chunks into dense vectors.
    pub embedder: Arc<dyn Embedder>,
    /// Qdrant client wrapper used for point writes and deletes.
    pub qdrant: Arc<QdrantClient>,
    /// Event receiver drained by the worker loop.
    pub rx: mpsc::Receiver<IndexEvent>,
    /// Cancellation token that triggers graceful draining.
    pub shutdown: CancellationToken,
    /// Maximum number of chunks sent to the embedder per request.
    pub batch_size: usize,
    /// Sender for OKF index maintenance, when it is switched on (D48).
    ///
    /// `None` means the feature is off, which is the default. This worker is the
    /// producer because it is the one place that sees every write, whichever
    /// surface made it; it never performs the maintenance itself, so it cannot
    /// feed its own queue.
    pub okf_tx: Option<mpsc::Sender<crate::okf_event::OkfMaintenanceEvent>>,
}

impl IndexerWorker {
    /// Build a worker around shared dependencies and an event receiver.
    pub fn new(
        storage: Arc<dyn Storage>,
        embedder: Arc<dyn Embedder>,
        qdrant: Arc<QdrantClient>,
        rx: mpsc::Receiver<IndexEvent>,
        shutdown: CancellationToken,
        batch_size: usize,
    ) -> Self {
        Self {
            storage,
            embedder,
            qdrant,
            rx,
            shutdown,
            batch_size,
            okf_tx: None,
        }
    }

    /// Enable OKF index maintenance by attaching the maintenance queue.
    #[must_use]
    pub fn with_okf_maintenance(
        mut self,
        okf_tx: mpsc::Sender<crate::okf_event::OkfMaintenanceEvent>,
    ) -> Self {
        self.okf_tx = Some(okf_tx);
        self
    }

    /// Enqueue a maintenance event, dropping it if the queue is full.
    ///
    /// Deliberately unlike the indexing queue's backpressure-to-503: a listing
    /// that did not get updated must never fail or retry a user's write.
    fn enqueue_maintenance(&self, event: crate::okf_event::OkfMaintenanceEvent) {
        let Some(tx) = self.okf_tx.as_ref() else {
            return;
        };
        // Reserved files are never concepts, and never enqueued. This is the
        // first of the guards that stop maintenance from feeding itself.
        if notedthat_okf::is_reserved(event.object_key().as_str()) {
            return;
        }
        if let Err(err) = tx.try_send(event) {
            tracing::warn!(
                target: "notedthat::indexing",
                error = %err,
                "OKF_MAINTENANCE_QUEUE_FULL; dropping maintenance event"
            );
        }
    }

    /// Run until the channel closes or shutdown is requested.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                biased;

                () = self.shutdown.cancelled() => {
                    let drain = async {
                        while let Some(event) = self.rx.recv().await {
                            self.handle(event).await;
                        }
                    };

                    if tokio::time::timeout(DRAIN_TIMEOUT, drain).await.is_err() {
                        tracing::warn!(target: "notedthat::indexing", "indexer worker: drain timeout elapsed");
                    } else {
                        tracing::info!(target: "notedthat::indexing", "indexer worker: drained on shutdown");
                    }
                    break;
                }
                maybe_event = self.rx.recv() => {
                    if let Some(event) = maybe_event {
                        self.handle(event).await;
                    } else {
                        tracing::info!(target: "notedthat::indexing", "indexer worker: channel closed, exiting");
                        break;
                    }
                }
            }
        }
    }

    async fn handle(&self, event: IndexEvent) {
        let kb = event.kb().clone();
        let object_key = event.object_key().clone();
        let kind = event.kind();
        tracing::info!(
            target: "notedthat::indexing",
            kb = %kb.as_str(),
            path = %object_key.as_str(),
            kind,
            "processing index event"
        );

        let result = match event {
            IndexEvent::Upsert { kb, object_key, .. } => self.handle_upsert(kb, object_key).await,
            IndexEvent::Tombstone { kb, object_key } => self.handle_tombstone(kb, object_key).await,
        };

        if let Err(message) = result {
            tracing::error!(target: "notedthat::indexing", error = %message, "INDEXING_FAILED");
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_upsert(&self, kb: KbSlug, object_key: ObjectPath) -> Result<(), String> {
        let object_read = match self
            .storage
            .get_object(&kb, &object_key, None, ConditionalHeaders::default())
            .await
        {
            Ok(read) => read,
            Err(StorageError::NotFound { .. }) => {
                tracing::debug!(
                    target: "notedthat::indexing",
                    kb = %kb.as_str(),
                    path = %object_key.as_str(),
                    "object not found on re-read; treating as tombstone"
                );
                return self.handle_tombstone(kb, object_key).await;
            }
            Err(err) => return Err(format!("storage.get_object failed: {err}")),
        };

        let mime = object_read.meta.content_type.clone().unwrap_or_default();
        if !is_indexable(&mime) {
            tracing::debug!(
                target: "notedthat::indexing",
                kb = %kb.as_str(),
                path = %object_key.as_str(),
                mime,
                "skipping non-indexable content type"
            );
            return Ok(());
        }

        let content_hash = sha256_hex(&object_read.bytes);
        let text = match std::str::from_utf8(&object_read.bytes) {
            Ok(text) => text.to_string(),
            Err(err) => {
                tracing::warn!(
                    target: "notedthat::indexing",
                    kb = %kb.as_str(),
                    path = %object_key.as_str(),
                    error = %err,
                    "object is not valid UTF-8; skipping"
                );
                return Ok(());
            }
        };

        // OKF v0.2 (D48): a conformant document has its frontmatter indexed as its
        // own metadata point, and its body chunked from `body_start` onward with
        // offsets still absolute in the stored bytes. Everything else keeps D33's
        // fully-raw behaviour, byte for byte.
        let metadata =
            crate::metadata::MetadataExtractor::extract(&crate::metadata::OkfExtractor, &text);
        if metadata.is_none()
            && let Err(reason) = notedthat_okf::parse_document(&text)
            && !matches!(reason, notedthat_okf::OkfParseError::NoFrontmatter)
        {
            // Answers "why did my document lose its badge?" for an operator.
            tracing::debug!(
                target: "notedthat::indexing",
                kb = %kb.as_str(),
                path = %object_key.as_str(),
                reason = %reason,
                "frontmatter is not OKF-conformant; indexing raw (D33)"
            );
        }

        let body_start = metadata.as_ref().map_or(0, |m| m.body_start);
        let chunks = chunker::chunk_from(&text, body_start);

        let max_chars = self.embedder.max_input_tokens();
        let mut filtered = Vec::with_capacity(chunks.len());
        for (chunk_index, chunk) in chunks.iter().enumerate() {
            let char_count = chunk.text.chars().count();
            if char_count > max_chars {
                tracing::warn!(
                    target: "notedthat::indexing",
                    kb = %kb.as_str(),
                    path = %object_key.as_str(),
                    chunk_index,
                    char_count,
                    max_input_tokens = max_chars,
                    "dropping oversized chunk"
                );
                continue;
            }
            filtered.push((chunk_index, chunk));
        }

        // The metadata unit is embedded first, then the body chunks. An OKF concept
        // that is entirely frontmatter has no body chunks at all; without the
        // metadata unit here it would be indexed as nothing and become completely
        // unsearchable, which is why the emptiness gate is below this and not above.
        let mut unit_texts: Vec<String> = Vec::with_capacity(filtered.len() + 1);
        let metadata = metadata.filter(|md| {
            let char_count = md.indexed_text.chars().count();
            if char_count > max_chars {
                tracing::warn!(
                    target: "notedthat::indexing",
                    kb = %kb.as_str(),
                    path = %object_key.as_str(),
                    char_count,
                    max_input_tokens = max_chars,
                    "dropping oversized metadata unit"
                );
                return false;
            }
            true
        });
        if let Some(md) = &metadata {
            unit_texts.push(md.indexed_text.clone());
        }
        unit_texts.extend(filtered.iter().map(|(_, chunk)| chunk.text.clone()));

        if unit_texts.is_empty() {
            tracing::debug!(
                target: "notedthat::indexing",
                kb = %kb.as_str(),
                path = %object_key.as_str(),
                "nothing to index after chunking and size filtering"
            );
            return Ok(());
        }

        let mut all_embeddings = Vec::with_capacity(unit_texts.len());
        for batch in unit_texts.chunks(self.batch_size.max(1)) {
            let embeddings = self
                .embedder
                .embed(batch)
                .await
                .map_err(|err| format!("embedder.embed failed: {err}"))?;
            if embeddings.len() != batch.len() {
                return Err(format!(
                    "embedder returned {} embeddings for {} chunks",
                    embeddings.len(),
                    batch.len()
                ));
            }
            all_embeddings.extend(embeddings);
        }

        let context = PointContext {
            object_key: &object_key,
            etag: object_read.meta.etag.as_deref().unwrap_or(""),
            mtime: object_read.meta.last_modified.unwrap_or(0),
            mime: &mime,
            content_hash: &content_hash,
            okf_payload: metadata.as_ref().map_or(&[][..], |md| &md.payload),
        };

        let mut embeddings = all_embeddings.into_iter();
        let mut points = Vec::with_capacity(unit_texts.len());
        if let Some(md) = &metadata {
            let embedding = embeddings
                .next()
                .ok_or_else(|| "missing embedding for metadata unit".to_string())?;
            points.push(build_metadata_point(&context, md, &embedding));
        }
        let body_embeddings: Vec<Vec<f32>> = embeddings.collect();
        points.extend(build_points(&filtered, &body_embeddings, &context)?);

        self.qdrant
            .inner()
            .upsert_points(
                qdrant_client::qdrant::UpsertPointsBuilder::new(collection_name(&kb), points)
                    .wait(true),
            )
            .await
            .map_err(|err| format!("qdrant upsert failed: {err}"))?;

        // Only parse the concept again when maintenance is actually switched on,
        // so the default path pays nothing for a feature nobody enabled.
        let maintenance_concept = self
            .okf_tx
            .as_ref()
            .filter(|_| metadata.is_some())
            .and_then(|_| notedthat_okf::parse_document(&text).ok())
            .map(|(concept, _)| concept);

        if let Some(concept) = &maintenance_concept {
            self.enqueue_maintenance(crate::okf_event::OkfMaintenanceEvent::Upserted {
                kb: kb.clone(),
                object_key: object_key.clone(),
                concept_type: concept.concept_type.clone(),
                title: concept
                    .title
                    .clone()
                    .unwrap_or_else(|| humanise_stem(object_key.as_str())),
                description: concept.description.clone(),
            });
        }

        if metadata.is_none() {
            // A document that has stopped being OKF-conformant must not keep the
            // metadata point from when it was: point ids are deterministic, so an
            // upsert alone would leave it behind.
            self.delete_metadata_point(&kb, &object_key).await?;
        }

        tracing::info!(
            target: "notedthat::indexing",
            kb = %kb.as_str(),
            path = %object_key.as_str(),
            chunks = filtered.len(),
            okf = metadata.is_some(),
            "indexed"
        );
        Ok(())
    }

    /// Remove a document's metadata point, if one is present.
    async fn delete_metadata_point(
        &self,
        kb: &KbSlug,
        object_key: &ObjectPath,
    ) -> Result<(), String> {
        use qdrant_client::qdrant::{DeletePointsBuilder, PointsIdsList};

        self.qdrant
            .inner()
            .delete_points(
                DeletePointsBuilder::new(collection_name(kb)).points(PointsIdsList {
                    ids: vec![metadata_point_id(object_key).into()],
                }),
            )
            .await
            .map_err(|err| format!("qdrant delete_points failed: {err}"))?;
        Ok(())
    }

    async fn handle_tombstone(&self, kb: KbSlug, object_key: ObjectPath) -> Result<(), String> {
        use qdrant_client::qdrant::{Condition, DeletePointsBuilder, Filter};

        let filter = Filter::must([Condition::matches(
            "object_key",
            object_key.as_str().to_string(),
        )]);
        self.qdrant
            .inner()
            .delete_points(DeletePointsBuilder::new(collection_name(&kb)).points(filter))
            .await
            .map_err(|err| format!("qdrant delete_points failed: {err}"))?;

        self.enqueue_maintenance(crate::okf_event::OkfMaintenanceEvent::Deleted {
            kb: kb.clone(),
            object_key: object_key.clone(),
        });

        tracing::info!(
            target: "notedthat::indexing",
            kb = %kb.as_str(),
            path = %object_key.as_str(),
            "tombstoned"
        );
        Ok(())
    }
}

/// Everything a point needs beyond its own chunk and embedding.
///
/// Introduced because `build_points` had already reached clippy's
/// `too_many_arguments` threshold; a context struct is a better answer than an
/// `#[allow]`.
struct PointContext<'a> {
    object_key: &'a ObjectPath,
    etag: &'a str,
    mtime: i64,
    mime: &'a str,
    content_hash: &'a str,
    /// OKF payload entries copied onto every point of the document. Empty for a
    /// document without conformant frontmatter.
    okf_payload: &'a [(String, qdrant_client::qdrant::Value)],
}

impl PointContext<'_> {
    /// The payload fields every point of a document shares.
    fn common_payload(&self) -> std::collections::HashMap<String, qdrant_client::qdrant::Value> {
        use qdrant_client::qdrant::Value;
        use std::collections::HashMap;

        let mut payload = HashMap::<String, Value>::new();
        payload.insert(
            "object_key".to_string(),
            self.object_key.as_str().to_string().into(),
        );
        payload.insert("etag".to_string(), self.etag.to_string().into());
        payload.insert("mime".to_string(), self.mime.to_string().into());
        payload.insert("mtime".to_string(), self.mtime.into());
        payload.insert(
            "content_hash".to_string(),
            self.content_hash.to_string().into(),
        );
        for (key, value) in self.okf_payload {
            payload.insert(key.clone(), value.clone());
        }
        payload
    }
}

/// Build the metadata point for a document.
///
/// The payload `text` is the **stored** frontmatter bytes, so `preview` stays
/// honest and `raw[byte_start..byte_end] == text` holds here too. The dense vector
/// and the BM25 document are built from the rendered `indexed_text` instead —
/// this is the one deliberate divergence, and it is what makes metadata
/// independently searchable rather than merely filterable (D48).
fn build_metadata_point(
    context: &PointContext<'_>,
    metadata: &crate::metadata::DocumentMetadata,
    embedding: &[f32],
) -> qdrant_client::qdrant::PointStruct {
    use qdrant_client::qdrant::{Document, PointStruct, Value, Vector};
    use std::collections::HashMap;

    let mut payload = context.common_payload();
    payload.insert("chunk_index".to_string(), Value::from(-1_i64));
    payload.insert(
        "chunk_kind".to_string(),
        crate::metadata::CHUNK_KIND_METADATA.to_string().into(),
    );
    payload.insert(
        "byte_start".to_string(),
        i64::try_from(metadata.byte_start)
            .unwrap_or(i64::MAX)
            .into(),
    );
    payload.insert(
        "byte_end".to_string(),
        i64::try_from(metadata.byte_end).unwrap_or(i64::MAX).into(),
    );
    payload.insert("heading_path".to_string(), Value::from(Vec::<Value>::new()));
    payload.insert("text".to_string(), metadata.raw_text.clone().into());
    payload
        .entry("tags".to_string())
        .or_insert_with(|| Value::from(Vec::<Value>::new()));

    let vectors = HashMap::from([
        ("dense".to_string(), Vector::from(embedding.to_vec())),
        (
            "sparse_bm25".to_string(),
            Vector::from(Document::new(metadata.indexed_text.clone(), "qdrant/bm25")),
        ),
    ]);
    PointStruct::new(metadata_point_id(context.object_key), vectors, payload)
}

fn build_points(
    filtered: &[(usize, &chunker::Chunk)],
    embeddings: &[Vec<f32>],
    context: &PointContext<'_>,
) -> Result<Vec<qdrant_client::qdrant::PointStruct>, String> {
    use qdrant_client::qdrant::{Document, PointStruct, Value, Vector};
    use std::collections::HashMap;

    if filtered.len() != embeddings.len() {
        return Err(format!(
            "embedding count mismatch: chunks={} embeddings={}",
            filtered.len(),
            embeddings.len()
        ));
    }

    Ok(filtered
        .iter()
        .zip(embeddings.iter())
        .map(|((chunk_index, chunk), embedding)| {
            let mut payload = context.common_payload();
            payload.insert(
                "chunk_index".to_string(),
                i64::try_from(*chunk_index).unwrap_or(i64::MAX).into(),
            );
            payload.insert(
                "chunk_kind".to_string(),
                crate::metadata::CHUNK_KIND_BODY.to_string().into(),
            );
            payload.insert(
                "byte_start".to_string(),
                i64::try_from(chunk.byte_start).unwrap_or(i64::MAX).into(),
            );
            payload.insert(
                "byte_end".to_string(),
                i64::try_from(chunk.byte_end).unwrap_or(i64::MAX).into(),
            );
            payload.insert(
                "heading_path".to_string(),
                chunk.heading_path.clone().into(),
            );
            payload.insert("text".to_string(), chunk.text.clone().into());
            payload
                .entry("tags".to_string())
                .or_insert_with(|| Value::from(Vec::<Value>::new()));

            let vectors = HashMap::from([
                ("dense".to_string(), Vector::from(embedding.clone())),
                (
                    "sparse_bm25".to_string(),
                    Vector::from(Document::new(chunk.text.clone(), "qdrant/bm25")),
                ),
            ]);
            PointStruct::new(point_id(context.object_key, *chunk_index), vectors, payload)
        })
        .collect())
}

/// A file stem turned into a display title: `daily-revenue.md` → `Daily revenue`.
///
/// Used only when a concept states no `title`.
fn humanise_stem(key: &str) -> String {
    let name = key.rsplit('/').next().unwrap_or(key);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let spaced = stem.replace(['-', '_'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let hash = Sha256::digest(bytes);
    format!("{hash:x}")
}

fn point_id(object_key: &ObjectPath, chunk_index: usize) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let id = format!("{}/{}", object_key.as_str(), chunk_index);
    id.as_bytes().iter().fold(FNV_OFFSET, |hash, byte| {
        let hash = hash ^ u64::from(*byte);
        hash.wrapping_mul(FNV_PRIME)
    })
}

/// Point id for a document's metadata point.
///
/// Hashes `"<key>#metadata"`, which is disjoint from the `"<key>/<index>"` space
/// used by body chunks, so the two can never collide.
fn metadata_point_id(object_key: &ObjectPath) -> u64 {
    fnv1a(&format!("{}#metadata", object_key.as_str()))
}

fn fnv1a(id: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    id.as_bytes().iter().fold(FNV_OFFSET, |hash, byte| {
        let hash = hash ^ u64::from(*byte);
        hash.wrapping_mul(FNV_PRIME)
    })
}

pub(crate) fn collection_name(kb: &KbSlug) -> String {
    format!("kb_{}_v1", kb.as_str())
}

/// Check if the content type is indexable (markdown or plain text).
pub fn is_indexable(mime: &str) -> bool {
    let mime = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(mime.as_str(), "text/markdown" | "text/plain" | "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexable_text_markdown() {
        assert!(is_indexable("text/markdown"));
    }

    #[test]
    fn indexable_text_markdown_with_charset() {
        assert!(is_indexable("text/markdown; charset=utf-8"));
    }

    #[test]
    fn indexable_text_plain() {
        assert!(is_indexable("text/plain"));
    }

    #[test]
    fn indexable_text_plain_with_charset_and_spaces() {
        assert!(is_indexable(" text/plain ; charset=utf-8"));
    }

    #[test]
    fn indexable_empty_mime() {
        assert!(is_indexable(""));
    }

    #[test]
    fn indexable_case_insensitive() {
        assert!(is_indexable("TEXT/MARKDOWN"));
    }

    #[test]
    fn not_indexable_image_png() {
        assert!(!is_indexable("image/png"));
    }

    #[test]
    fn not_indexable_application_pdf() {
        assert!(!is_indexable("application/pdf"));
    }

    #[test]
    fn not_indexable_application_json() {
        assert!(!is_indexable("APPLICATION/JSON"));
    }

    #[test]
    fn point_id_is_stable_and_chunk_specific() {
        let path = ObjectPath::try_from("hello.md").expect("valid path");
        assert_eq!(point_id(&path, 0), point_id(&path, 0));
        assert_ne!(point_id(&path, 0), point_id(&path, 1));
    }

    // Compile-only: `IndexerWorker` must be spawnable.
    fn _spawn_bounds()
    where
        IndexerWorker: Send + 'static,
    {
    }
}

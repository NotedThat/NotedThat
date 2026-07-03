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

        let chunks = chunker::chunk(&text);
        if chunks.is_empty() {
            tracing::debug!(
                target: "notedthat::indexing",
                kb = %kb.as_str(),
                path = %object_key.as_str(),
                "no chunks produced; skipping"
            );
            return Ok(());
        }

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

        if filtered.is_empty() {
            tracing::debug!(
                target: "notedthat::indexing",
                kb = %kb.as_str(),
                path = %object_key.as_str(),
                "all chunks dropped after size filter"
            );
            return Ok(());
        }

        let mut all_embeddings = Vec::with_capacity(filtered.len());
        for batch in filtered.chunks(self.batch_size.max(1)) {
            let texts: Vec<String> = batch.iter().map(|(_, chunk)| chunk.text.clone()).collect();
            let embeddings = self
                .embedder
                .embed(&texts)
                .await
                .map_err(|err| format!("embedder.embed failed: {err}"))?;
            if embeddings.len() != texts.len() {
                return Err(format!(
                    "embedder returned {} embeddings for {} chunks",
                    embeddings.len(),
                    texts.len()
                ));
            }
            all_embeddings.extend(embeddings);
        }

        let points = build_points(
            &filtered,
            &all_embeddings,
            &object_key,
            object_read.meta.etag.as_deref().unwrap_or(""),
            object_read.meta.last_modified.unwrap_or(0),
            &mime,
            &content_hash,
        )?;

        self.qdrant
            .inner()
            .upsert_points(
                qdrant_client::qdrant::UpsertPointsBuilder::new(collection_name(&kb), points)
                    .wait(true),
            )
            .await
            .map_err(|err| format!("qdrant upsert failed: {err}"))?;

        tracing::info!(
            target: "notedthat::indexing",
            kb = %kb.as_str(),
            path = %object_key.as_str(),
            chunks = filtered.len(),
            "indexed"
        );
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

        tracing::info!(
            target: "notedthat::indexing",
            kb = %kb.as_str(),
            path = %object_key.as_str(),
            "tombstoned"
        );
        Ok(())
    }
}

fn build_points(
    filtered: &[(usize, &chunker::Chunk)],
    embeddings: &[Vec<f32>],
    object_key: &ObjectPath,
    etag: &str,
    mtime: i64,
    object_mime: &str,
    content_hash: &str,
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
            let mut payload = HashMap::<String, Value>::new();
            payload.insert(
                "object_key".to_string(),
                object_key.as_str().to_string().into(),
            );
            payload.insert(
                "chunk_index".to_string(),
                i64::try_from(*chunk_index).unwrap_or(i64::MAX).into(),
            );
            payload.insert(
                "byte_start".to_string(),
                i64::try_from(chunk.byte_start).unwrap_or(i64::MAX).into(),
            );
            payload.insert(
                "byte_end".to_string(),
                i64::try_from(chunk.byte_end).unwrap_or(i64::MAX).into(),
            );
            payload.insert("etag".to_string(), etag.to_string().into());
            payload.insert("mime".to_string(), object_mime.to_string().into());
            payload.insert("mtime".to_string(), mtime.into());
            payload.insert(
                "heading_path".to_string(),
                chunk.heading_path.clone().into(),
            );
            payload.insert("tags".to_string(), Value::from(Vec::<Value>::new()));
            payload.insert("content_hash".to_string(), content_hash.to_string().into());
            payload.insert("text".to_string(), chunk.text.clone().into());

            let vectors = HashMap::from([
                ("dense".to_string(), Vector::from(embedding.clone())),
                (
                    "sparse_bm25".to_string(),
                    Vector::from(Document::new(chunk.text.clone(), "qdrant/bm25")),
                ),
            ]);
            PointStruct::new(point_id(object_key, *chunk_index), vectors, payload)
        })
        .collect())
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

fn collection_name(kb: &KbSlug) -> String {
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

//! `IndexerWorker` — serial async task draining `IndexEvent`s into Qdrant.
//!
//! Behavior: one event at a time, batched embedding, drain on shutdown.

mod chunks;
mod pipeline;
mod points;
mod snapshot;

use crate::{
    embedder::Embedder,
    event::IndexEvent,
    vector_store::{PointSelector, VectorStore},
};
use notedthat_core::{KbSlug, ObjectPath, StagingConfig, Storage};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Maximum time to continue draining already queued events after cancellation.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether an upsert may be abandoned when the object's content is already indexed.
///
/// The distinction exists because the two producers want opposite things. A write through
/// `NotedThat` is a deliberate act and must always re-index — re-writing an object is the
/// only reindex mechanism v1 offers (D42), and taking that away would leave an operator
/// with no way to repair a bad index entry. A change merely *observed* on disk carries no
/// such intent, and observing the same unchanged bytes is exactly what a reconciliation
/// pass does thousands of times in a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    /// Always re-read, re-chunk and re-embed.
    Never,
    /// Do nothing when the indexed chunks already carry the object's current `ETag`.
    IfUnchanged,
}

/// Async indexing worker.
pub struct IndexerWorker {
    /// Object storage used to re-read source documents before indexing.
    pub storage: Arc<dyn Storage>,
    /// Embedding endpoint used to turn chunks into dense vectors.
    pub embedder: Arc<dyn Embedder>,
    /// Vector store used for point writes and deletes.
    pub store: Arc<dyn VectorStore>,
    /// Event receiver drained by the worker loop.
    pub rx: mpsc::Receiver<IndexEvent>,
    /// Cancellation token that triggers graceful draining.
    pub shutdown: CancellationToken,
    /// Maximum number of chunks sent to the embedder per request.
    pub batch_size: usize,
    /// Directory configuration for private index snapshots.
    pub staging: StagingConfig,
}

impl IndexerWorker {
    /// Build a worker around shared dependencies and an event receiver.
    pub fn new(
        storage: Arc<dyn Storage>,
        embedder: Arc<dyn Embedder>,
        store: Arc<dyn VectorStore>,
        rx: mpsc::Receiver<IndexEvent>,
        shutdown: CancellationToken,
        batch_size: usize,
    ) -> Self {
        Self {
            storage,
            embedder,
            store,
            rx,
            shutdown,
            batch_size,
            staging: StagingConfig::default(),
        }
    }

    /// Use the validated shared staging directory for index snapshots.
    #[must_use]
    pub fn with_staging_config(mut self, staging: StagingConfig) -> Self {
        self.staging = staging;
        self
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
            IndexEvent::Upsert { kb, object_key, .. } => {
                self.handle_upsert(kb, object_key, Skip::Never).await
            }
            IndexEvent::Refresh { kb, object_key } => {
                self.handle_upsert(kb, object_key, Skip::IfUnchanged).await
            }
            IndexEvent::Tombstone { kb, object_key } => self.handle_tombstone(kb, object_key).await,
        };

        if let Err(message) = result {
            tracing::error!(target: "notedthat::indexing", error = %message, "INDEXING_FAILED");
        }
    }

    async fn handle_tombstone(&self, kb: KbSlug, object_key: ObjectPath) -> Result<(), String> {
        self.store
            .delete_points(
                &kb,
                PointSelector::Object {
                    object_key: object_key.as_str().to_string(),
                },
            )
            .await
            .map_err(|err| format!("vector store delete failed: {err}"))?;

        tracing::info!(
            target: "notedthat::indexing",
            kb = %kb.as_str(),
            path = %object_key.as_str(),
            "tombstoned"
        );
        Ok(())
    }
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
    use super::points::point_id;
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

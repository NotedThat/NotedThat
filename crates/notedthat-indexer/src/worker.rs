//! `IndexerWorker` — serial async task draining `IndexEvent`s into Qdrant.
//!
//! Behavior: one event at a time, batched embedding, drain on shutdown.

mod chunks;
mod last_seen;
mod pipeline;
mod points;
mod snapshot;

use crate::{
    embedder::Embedder,
    event::IndexEvent,
    health::{IndexHealth, bound_summary},
    vector_store::{PointSelector, VectorStore},
};
use last_seen::LastSeen;
use notedthat_core::{EventPublisher, KbSlug, ObjectEvent, ObjectPath, StagingConfig, Storage};
use pipeline::{PipelineFailure, PipelineOutcome};
use std::sync::{Arc, Mutex};
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
    /// Where detected changes and indexing outcomes are published, when an
    /// events backend is configured.
    ///
    /// Only a `Refresh` announces a *change*: a write through `NotedThat` was
    /// announced by the write path before its `Upsert` was even enqueued. Every
    /// `Upsert` and `Refresh` then reports what became of it — `object.indexed`
    /// or `object.index_failed` (D64) — once the health record has been stamped.
    pub events: Option<Arc<dyn EventPublisher>>,
    /// The last stamp this worker saw for each key, so the `fs` watcher's echo
    /// of a write the server made itself is not announced a second time.
    last_seen: Mutex<LastSeen>,
    /// Where each event's outcome is recorded for the health view (#97).
    health: Arc<IndexHealth>,
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
            events: None,
            last_seen: Mutex::new(LastSeen::default()),
            health: Arc::new(IndexHealth::new()),
        }
    }

    /// Record outcomes on a health record shared with the surfaces that
    /// report it.
    #[must_use]
    pub fn with_health(mut self, health: Arc<IndexHealth>) -> Self {
        self.health = health;
        self
    }

    /// Announce changes this worker detects, and the outcome of every upsert
    /// or refresh, on the given event log.
    #[must_use]
    pub fn with_event_publisher(mut self, events: Option<Arc<dyn EventPublisher>>) -> Self {
        self.events = events;
        self
    }

    /// Use the validated shared staging directory for index snapshots.
    #[must_use]
    pub fn with_staging_config(mut self, staging: StagingConfig) -> Self {
        self.staging = staging;
        self
    }

    /// Run until the channel closes or shutdown is requested.
    pub async fn run(mut self) {
        self.run_loop().await;
        // Whatever ended the loop, nothing drains the queue from here on, and
        // the health view says so rather than reporting `indexing` forever.
        self.health.worker_stopped();
    }

    async fn run_loop(&mut self) {
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

        // A tombstone reports no outcome, success or failure (D64): there is
        // no `object.unindexed`, and `object.deleted` already said what
        // happened to the key.
        let reports_outcome = !matches!(event, IndexEvent::Tombstone { .. });
        let result = match event {
            IndexEvent::Upsert {
                kb,
                object_key,
                etag,
                ..
            } => {
                self.remember(&kb, &object_key, Some(etag));
                self.handle_upsert(kb, object_key, Skip::Never, None).await
            }
            IndexEvent::Refresh {
                kb,
                object_key,
                origin,
            } => {
                self.handle_upsert(kb, object_key, Skip::IfUnchanged, Some(origin.source()))
                    .await
            }
            IndexEvent::Tombstone { kb, object_key } => {
                self.remember(&kb, &object_key, None);
                self.handle_tombstone(kb, object_key)
                    .await
                    .map(|()| PipelineOutcome::Tombstoned)
                    .map_err(PipelineFailure::before_head)
            }
        };

        // The health record is stamped before the outcome is published, so a
        // subscriber who reads `object.indexed` and then asks `/index` never
        // finds the view behind the stream.
        match result {
            Ok(outcome) => {
                self.health.succeeded(kb.as_str());
                if let PipelineOutcome::Indexed { etag, mime, chunks } = outcome {
                    self.publish(ObjectEvent::indexed(kb, object_key, etag, mime, chunks))
                        .await;
                }
            }
            Err(failure) => {
                tracing::error!(
                    target: "notedthat::indexing",
                    kb = %kb.as_str(),
                    path = %object_key.as_str(),
                    error = %failure.message,
                    "INDEXING_FAILED"
                );
                self.health
                    .failed(kb.as_str(), object_key.as_str(), &failure.message);
                if reports_outcome {
                    self.publish(ObjectEvent::index_failed(
                        kb,
                        object_key,
                        failure.etag,
                        failure.mime,
                        bound_summary(&failure.message),
                    ))
                    .await;
                }
            }
        }
    }

    /// Record the stamp a key was last seen with: its `ETag`, or `None` once deleted.
    /// Kept only while there is a log to keep quiet.
    fn remember(&self, kb: &KbSlug, object_key: &ObjectPath, etag: Option<String>) {
        if self.events.is_none() {
            return;
        }
        self.last_seen
            .lock()
            .expect("last-seen mutex not poisoned")
            .record(kb, object_key, etag);
    }

    /// Whether `etag` differs from the stamp the key was last seen with — and so
    /// whether a detected change is news rather than the echo of the server's
    /// own write (D50) or of a change already announced.
    fn is_news(&self, kb: &KbSlug, object_key: &ObjectPath, etag: Option<&str>) -> bool {
        self.last_seen
            .lock()
            .expect("last-seen mutex not poisoned")
            .differs(kb, object_key, etag)
    }

    /// Announce a detected change, once: the echo of the server's own write, or
    /// of a change already announced, is kept quiet (D50). Outcome events do
    /// not come through here — they are not change announcements, and must not
    /// be deduplicated against the stamp the write already announced.
    pub(crate) async fn announce(&self, event: ObjectEvent) {
        if self.events.is_none() {
            return;
        }
        let etag = event.kind.etag();
        if !self.is_news(&event.kb, &event.object_key, etag) {
            return;
        }
        self.remember(&event.kb, &event.object_key, etag.map(str::to_owned));
        self.publish(event).await;
    }

    /// Publish on the log, if there is one. Failure is logged and indexing goes
    /// on: there is no caller here to hand a 503 to — a missed announcement is
    /// re-detected by the next pass, and a missed outcome is what `/index` is
    /// for.
    async fn publish(&self, event: ObjectEvent) {
        let Some(events) = &self.events else {
            return;
        };
        let (kb, path) = (event.kb.clone(), event.object_key.clone());
        if let Err(error) = events.publish(event).await {
            tracing::error!(
                target: "notedthat::events",
                kb = %kb.as_str(),
                path = %path.as_str(),
                %error,
                "EVENT_PUBLISH_FAILED"
            );
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

use super::chunks::{ChunkCursor, open_chunk_cursor, take_chunk_batch, validate_chunk_byte_bound};
use super::last_seen::LastSeen;
use super::points::build_points;
use super::snapshot::{SnapshotFacts, SnapshotObserver};
use super::{Skip, Told, content_type_from_key, is_indexable};
use crate::chunker;
use crate::embedder::{Embedder, EmbedderError};
use crate::health::IndexHealth;
use crate::vector_store::{PointSelector, VectorStore, VectorStoreError};
use futures::StreamExt;
use notedthat_core::{
    ConditionalHeaders, EventPublisher, KbSlug, ObjectEvent, ObjectMeta, ObjectPath, StageError,
    StagedBody, StagingConfig, Storage, StorageError, search::ConceptMetadata,
};
use std::io;
use std::sync::{Arc, Mutex};
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

const FRONTMATTER_PREFIX_LIMIT: usize = 16 * 1024 * 1024;
const MAX_UTF8_BYTES_PER_SCALAR: usize = 4;

struct PreparedSnapshot {
    body: Arc<StagedBody>,
    /// The `ETag` `HEAD` reported and the streamed body confirmed: the version
    /// the points are stamped with, and the one `object.indexed` names.
    etag: String,
    meta: ObjectMeta,
    facts: SnapshotFacts,
    metadata: Option<ConceptMetadata>,
    body_start: usize,
}

/// What an `Upsert` or `Refresh` did, for the outcome event (D65).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PipelineOutcome {
    /// `chunks` points now carry the bytes stamped `etag`.
    Indexed {
        etag: String,
        mime: String,
        chunks: u32,
    },
    /// The index already held this `ETag` (D50); nothing was touched.
    Skipped,
    /// Nothing to index — missing on re-read, non-indexable, empty or chunking to
    /// nothing — and any previous points are gone. Also what an explicit
    /// `Tombstone` maps to.
    Tombstoned,
}

/// Why a pipeline stage failed. Each message is single-line and is the text the
/// failure summary has always carried, so `/index` and `object.index_failed`
/// read the same while a caller can now tell the causes apart.
#[derive(Debug, thiserror::Error)]
pub(super) enum PipelineError {
    #[error("embedder input limit must be greater than zero")]
    ZeroInputLimit,
    #[error("chunk count exceeds supported numeric range")]
    ChunkCountRange,
    #[error("storage.head_object returned no ETag for stable snapshot")]
    MissingEtag,
    #[error("storage.get_object_stream failed: {0}")]
    Stream(#[source] StorageError),
    #[error("streamed snapshot ETag differs from preceding HEAD")]
    EtagChanged,
    #[error("staging index snapshot failed: {0}")]
    Staging(#[source] StageError),
    #[error("index snapshot observer lock poisoned")]
    ObserverPoisoned,
    #[error("snapshot inspection failed: {0}")]
    SnapshotInspection(#[source] io::Error),
    #[error("streaming chunk read failed: {0}")]
    ChunkRead(#[source] io::Error),
    #[error("chunk index overflow")]
    ChunkIndexOverflow,
    #[error(
        "chunk {index} is {bytes} UTF-8 bytes, exceeding conservative embedder bound {maximum}"
    )]
    ChunkTooLarge {
        index: usize,
        bytes: usize,
        maximum: usize,
    },
    #[error("chunk iterator task failed: {0}")]
    ChunkIteratorTask(#[source] JoinError),
    #[error("chunk iterator setup failed: {0}")]
    ChunkIteratorSetup(#[source] io::Error),
    #[error("chunk batch task failed: {0}")]
    ChunkBatchTask(#[source] JoinError),
    #[error("embedder.embed failed: {0}")]
    Embed(#[source] EmbedderError),
    #[error("embedder returned {returned} embeddings for {sent} chunks")]
    EmbeddingCount { returned: usize, sent: usize },
    #[error("embedding count mismatch: chunks={chunks} embeddings={embeddings}")]
    PointCount { chunks: usize, embeddings: usize },
    #[error("vector store upsert failed: {0}")]
    Upsert(#[source] VectorStoreError),
    #[error("obsolete chunk cleanup failed: {0}")]
    Cleanup(#[source] VectorStoreError),
    #[error("vector store delete failed: {0}")]
    Delete(#[source] VectorStoreError),
}

/// Why the pipeline stopped, with what `HEAD` had established by then.
#[derive(Debug)]
pub(super) struct PipelineFailure {
    pub(super) message: String,
    pub(super) etag: Option<String>,
    pub(super) mime: Option<String>,
    /// Whether `HEAD` had succeeded: the key was found live. A `Tombstone`
    /// reports a failure only then (D65).
    pub(super) after_head: bool,
}

impl PipelineFailure {
    /// A failure with no stamp: `HEAD` had not succeeded.
    pub(super) fn before_head(message: String) -> Self {
        Self {
            message,
            etag: None,
            mime: None,
            after_head: false,
        }
    }
}

/// Everything one file's indexing needs: read, chunk, embed, write, and
/// report one outcome. Cheap to clone, and every clone shares the same
/// last-seen record and health view.
#[derive(Clone)]
pub(super) struct Pipeline {
    /// Object storage used to re-read source documents before indexing.
    pub(super) storage: Arc<dyn Storage>,
    /// Embedding endpoint used to turn chunks into dense vectors.
    pub(super) embedder: Arc<dyn Embedder>,
    /// Vector store used for point writes and deletes.
    pub(super) store: Arc<dyn VectorStore>,
    /// Cancellation token that triggers graceful draining.
    pub(super) shutdown: CancellationToken,
    /// Maximum number of chunks sent to the embedder per request.
    pub(super) batch_size: usize,
    /// Directory configuration for private index snapshots.
    pub(super) staging: StagingConfig,
    /// Where detected changes and indexing outcomes are published, when an
    /// events backend is configured.
    ///
    /// Only a `Refresh` announces a *change*: a write through `NotedThat` was
    /// announced by the write path before its `Upsert` was even enqueued. Every
    /// `Upsert` and `Refresh` then reports what became of it — `object.indexed`
    /// or `object.index_failed` (D65) — once the health record has been stamped.
    pub(super) events: Option<Arc<dyn EventPublisher>>,
    /// The last stamp any handler saw for each key, so the `fs` watcher's echo
    /// of a write the server made itself is not announced a second time.
    pub(super) last_seen: Arc<Mutex<LastSeen>>,
    /// Where each event's outcome is recorded for the health view (#97).
    pub(super) health: Arc<IndexHealth>,
}

impl Pipeline {
    pub(super) fn new(
        storage: Arc<dyn Storage>,
        embedder: Arc<dyn Embedder>,
        store: Arc<dyn VectorStore>,
        shutdown: CancellationToken,
        batch_size: usize,
    ) -> Self {
        Self {
            storage,
            embedder,
            store,
            shutdown,
            batch_size,
            staging: StagingConfig::default(),
            events: None,
            last_seen: Arc::new(Mutex::new(LastSeen::default())),
            health: Arc::new(IndexHealth::new()),
        }
    }

    /// `told` is what the log already says about the key. A change this worker
    /// detected rather than was told about is announced from the `HEAD` result,
    /// before any indexability check, so an mp3 dropped into the tree is
    /// announced even though it is never indexed. The last-seen stamp records
    /// only the state the write path announced: the version a write's `HEAD`
    /// finds, or the absence a tombstone's confirms. Anything else `HEAD` finds
    /// is someone else's change, left for the watcher to announce.
    pub(super) async fn handle_upsert(
        &self,
        kb: KbSlug,
        object_key: ObjectPath,
        skip: Skip,
        told: Told,
    ) -> Result<PipelineOutcome, PipelineFailure> {
        // No object may live under a reserved key (#279), so none is indexed there.
        // Reconcile and the `fs` watcher still hand one over: this is how search
        // entries indexed for it before the key was reserved are removed.
        if object_key.is_reserved() {
            tracing::debug!(
                target: "notedthat::indexing",
                kb = %kb.as_str(),
                path = %object_key.as_str(),
                "reserved key; removing index entries"
            );
            return self
                .handle_tombstone(kb, object_key)
                .await
                .map(|()| PipelineOutcome::Tombstoned)
                .map_err(|err| PipelineFailure::before_head(err.to_string()));
        }
        let head = match self
            .storage
            .head_object(&kb, &object_key, ConditionalHeaders::default())
            .await
        {
            Ok(meta) => meta,
            // Only the object's own absence is a tombstone. `BucketNotFound` — the whole
            // knowledge base gone from storage — falls through to the failure arm below:
            // an index must not be emptied because its bucket is temporarily missing.
            Err(StorageError::NotFound { .. }) => {
                tracing::debug!(
                    target: "notedthat::indexing",
                    kb = %kb.as_str(),
                    path = %object_key.as_str(),
                    "object not found on re-read; treating as tombstone"
                );
                match told {
                    Told::Nothing(source) => {
                        self.announce(ObjectEvent::deleted(kb.clone(), object_key.clone(), source))
                            .await;
                    }
                    Told::Deleted => self.remember(&kb, &object_key, None),
                    Told::Written => {}
                }
                return self
                    .handle_tombstone(kb, object_key)
                    .await
                    .map(|()| PipelineOutcome::Tombstoned)
                    .map_err(|err| PipelineFailure::before_head(err.to_string()));
            }
            // A `Tombstone` is the write path saying the delete is committed, and
            // nothing repairs a failure here: it is terminal and silent, and the
            // watcher sees no new change. So when storage cannot answer, the
            // tombstone is taken at its word and the points go. Were the key
            // re-created meanwhile, that write has its own event to index it.
            // `BucketNotFound` still fails, as above.
            Err(err @ (StorageError::BackendUnavailable { .. } | StorageError::Other { .. }))
                if matches!(told, Told::Deleted) =>
            {
                tracing::warn!(
                    target: "notedthat::indexing",
                    kb = %kb.as_str(),
                    path = %object_key.as_str(),
                    error = %err,
                    "could not re-read a deleted key; removing its index entries"
                );
                return self
                    .handle_tombstone(kb, object_key)
                    .await
                    .map(|()| PipelineOutcome::Tombstoned)
                    .map_err(|err| PipelineFailure::before_head(err.to_string()));
            }
            Err(err) => {
                return Err(PipelineFailure::before_head(format!(
                    "storage.head_object failed: {err}"
                )));
            }
        };
        match told {
            Told::Nothing(source) => {
                self.announce(ObjectEvent::from_meta(
                    kb.clone(),
                    object_key.clone(),
                    &head,
                    source,
                ))
                .await;
            }
            Told::Written => self.remember(&kb, &object_key, head.etag.clone()),
            Told::Deleted => {}
        }

        // An object with no declared type — a file the fs backend could not
        // guess one for, or a PUT whose `Content-Type` names no media type (empty,
        // or only parameters such as `;charset=utf-8`) — is judged by its key, so
        // an attachment is not read as Markdown. The type resolved here is the one
        // the outcome event reports and the chunks carry (D65).
        let mime = match head.content_type.as_deref() {
            Some(declared) if !crate::mime::essence(declared).is_empty() => {
                declared.trim().to_owned()
            }
            _ => content_type_from_key(object_key.as_str())
                .unwrap_or_default()
                .to_owned(),
        };

        // From here on a failure knows which version it was working on.
        let failure_etag = head.etag.clone();
        let failure_mime = Some(mime.clone()).filter(|mime| !mime.is_empty());
        self.index_head(kb, object_key, head, mime, skip)
            .await
            .map_err(|err| PipelineFailure {
                message: err.to_string(),
                etag: failure_etag,
                mime: failure_mime,
                after_head: true,
            })
    }

    /// Index the object `head` describes as `mime`, or take it out of the index
    /// when it is not something the index holds.
    async fn index_head(
        &self,
        kb: KbSlug,
        object_key: ObjectPath,
        head: ObjectMeta,
        mime: String,
        skip: Skip,
    ) -> Result<PipelineOutcome, PipelineError> {
        if !is_indexable(&mime) {
            tracing::debug!(
                target: "notedthat::indexing",
                kb = %kb.as_str(),
                path = %object_key.as_str(),
                mime,
                "removing index entries for non-indexable content type"
            );
            self.handle_tombstone(kb, object_key).await?;
            return Ok(PipelineOutcome::Tombstoned);
        }
        if head.size == 0 {
            tracing::debug!(
                target: "notedthat::indexing",
                kb = %kb.as_str(),
                path = %object_key.as_str(),
                "empty object; removing previous index entries"
            );
            self.handle_tombstone(kb, object_key).await?;
            return Ok(PipelineOutcome::Tombstoned);
        }

        if skip == Skip::IfUnchanged && self.already_indexed(&kb, &object_key, &head).await {
            tracing::debug!(
                target: "notedthat::indexing",
                kb = %kb.as_str(),
                path = %object_key.as_str(),
                "unchanged since it was last indexed; skipping"
            );
            return Ok(PipelineOutcome::Skipped);
        }

        let max_input_tokens = self.embedder.max_input_tokens();
        if max_input_tokens == 0 {
            return Err(PipelineError::ZeroInputLimit);
        }
        let mut snapshot = self.stage_snapshot(&kb, &object_key, head).await?;
        // The chunks carry the type the event reports, so a `mime` search filter
        // finds what `object.indexed` said was indexed, key-derived or declared.
        snapshot.meta.content_type = Some(mime.clone());
        let max_chars =
            chunker::SOFT_CHAR_CAP.min((max_input_tokens / MAX_UTF8_BYTES_PER_SCALAR).max(1));
        let cursor =
            open_chunk_cursor(Arc::clone(&snapshot.body), snapshot.body_start, max_chars).await?;
        let new_count = self
            .upsert_batches(&kb, &object_key, &snapshot, cursor)
            .await?;
        // A body that chunks to nothing (whitespace, frontmatter only) is out of
        // the index like an empty object: no `object.indexed { chunks: 0 }`.
        // shortcut: with no point to carry its ETag, every Refresh still re-reads
        // it; record the ETag elsewhere if that read ever matters.
        if new_count == 0 {
            self.handle_tombstone(kb, object_key).await?;
            return Ok(PipelineOutcome::Tombstoned);
        }
        let chunks = u32::try_from(new_count).map_err(|_| PipelineError::ChunkCountRange)?;
        self.cleanup_obsolete(&kb, &object_key, chunks).await?;

        tracing::info!(
            target: "notedthat::indexing",
            kb = %kb.as_str(),
            path = %object_key.as_str(),
            chunks,
            "indexed"
        );
        Ok(PipelineOutcome::Indexed {
            etag: snapshot.etag,
            mime,
            chunks,
        })
    }

    /// Whether `head`'s `ETag` is already the one recorded on this object's chunks.
    ///
    /// Compared before staging, and against the `ETag` `head_object` reports rather than a
    /// hash of the bytes, because that is what makes a reconciliation pass cheap: on the
    /// filesystem backend a `HEAD` reads a stat and a sidecar, rehashing only when the
    /// recorded stamp no longer describes the file (see `notedthat_storage_fs::meta`). So
    /// an unchanged corpus is confirmed unchanged without reading one byte of content.
    ///
    /// An object a failed re-index left half-written answers "not indexed" too: the store
    /// only reports an `ETag` every chunk agrees on (issue #276).
    ///
    /// A backend failure answers "not indexed". Re-indexing something that did not need it
    /// costs an embedding request; skipping something that did leaves a document wrong in
    /// search until it is written again, and nothing would notice.
    async fn already_indexed(
        &self,
        kb: &KbSlug,
        object_key: &ObjectPath,
        head: &ObjectMeta,
    ) -> bool {
        let Some(head_etag) = head.etag.as_deref() else {
            return false;
        };
        match self.store.indexed_etag(kb, object_key.as_str()).await {
            Ok(indexed) => indexed.as_deref() == Some(head_etag),
            Err(error) => {
                tracing::warn!(
                    target: "notedthat::indexing",
                    kb = %kb.as_str(),
                    path = %object_key.as_str(),
                    %error,
                    "could not read the indexed ETag; indexing rather than risking a stale skip"
                );
                false
            }
        }
    }

    async fn stage_snapshot(
        &self,
        kb: &KbSlug,
        object_key: &ObjectPath,
        head: ObjectMeta,
    ) -> Result<PreparedSnapshot, PipelineError> {
        let head_etag = head.etag.clone().ok_or(PipelineError::MissingEtag)?;
        let object_stream = self
            .storage
            .get_object_stream(
                kb,
                object_key,
                None,
                ConditionalHeaders {
                    if_match: Some(head_etag.clone()),
                    ..ConditionalHeaders::default()
                },
            )
            .await
            .map_err(PipelineError::Stream)?;
        if object_stream.meta.etag.as_deref() != Some(head_etag.as_str()) {
            // Deliberately not retried: retrying would only race the same writer again,
            // from further behind. Where the filesystem watcher is running, the change
            // that lost us this race raises its own event and re-enqueues the key.
            //
            // That repair is configuration-dependent, and this error is terminal —
            // `Pipeline::handle` logs INDEXING_FAILED and drops it, with no backoff
            // and no dead-letter queue. On S3, or on the filesystem with
            // NOTEDTHAT_FS_WATCH=false, nothing re-enqueues it and the object stays stale
            // in the index until it is next written.
            return Err(PipelineError::EtagChanged);
        }
        let object_meta = object_stream.meta;
        let observer = Arc::new(Mutex::new(SnapshotObserver::new(FRONTMATTER_PREFIX_LIMIT)));
        let stream_observer = Arc::clone(&observer);
        let observed_chunks = object_stream.chunks.map(move |result| {
            let bytes = result?;
            stream_observer
                .lock()
                .map_err(|_| StorageError::BackendUnavailable {
                    message: "index snapshot observer lock poisoned".to_owned(),
                })?
                .observe(&bytes)
                .map_err(|source| StorageError::Other {
                    source: Box::new(source),
                })?;
            Ok::<_, StorageError>(bytes)
        });
        let staged = Arc::new(
            StagedBody::stage_stream_to_file(
                observed_chunks,
                Some(head.size),
                u64::MAX,
                &self.staging,
            )
            .await
            .map_err(PipelineError::Staging)?,
        );
        let facts = observer
            .lock()
            .map_err(|_| PipelineError::ObserverPoisoned)?
            .finish()
            .map_err(PipelineError::SnapshotInspection)?;
        let (metadata, body_start) = crate::okf::concept_prefix(object_key.as_str(), &facts.prefix)
            .map_or((None, 0), |(metadata, body_start)| {
                (Some(metadata), body_start)
            });
        Ok(PreparedSnapshot {
            body: staged,
            etag: head_etag,
            meta: object_meta,
            facts,
            metadata,
            body_start,
        })
    }

    async fn upsert_batches(
        &self,
        kb: &KbSlug,
        object_key: &ObjectPath,
        snapshot: &PreparedSnapshot,
        mut cursor: ChunkCursor,
    ) -> Result<usize, PipelineError> {
        let batch_size = self.batch_size.max(1);
        loop {
            let (next_cursor, batch) =
                tokio::task::spawn_blocking(move || take_chunk_batch(cursor, batch_size))
                    .await
                    .map_err(PipelineError::ChunkBatchTask)??;
            cursor = next_cursor;
            if batch.is_empty() {
                break;
            }
            validate_chunk_byte_bound(&batch, self.embedder.max_input_tokens())?;
            let texts: Vec<String> = batch.iter().map(|(_, chunk)| chunk.text.clone()).collect();
            let embeddings = self
                .embedder
                .embed(&texts)
                .await
                .map_err(PipelineError::Embed)?;
            if embeddings.len() != texts.len() {
                return Err(PipelineError::EmbeddingCount {
                    returned: embeddings.len(),
                    sent: texts.len(),
                });
            }
            let points = build_points(
                &batch,
                &embeddings,
                object_key,
                &snapshot.meta,
                &snapshot.facts.content_hash,
                snapshot.metadata.as_ref(),
            )?;
            self.store
                .upsert_points(kb, points)
                .await
                .map_err(PipelineError::Upsert)?;
        }
        Ok(cursor.next_index)
    }

    async fn cleanup_obsolete(
        &self,
        kb: &KbSlug,
        object_key: &ObjectPath,
        from_chunk_index: u32,
    ) -> Result<(), PipelineError> {
        self.store
            .delete_points(
                kb,
                PointSelector::ObjectChunksFrom {
                    object_key: object_key.as_str().to_owned(),
                    from_chunk_index,
                },
            )
            .await
            .map_err(PipelineError::Cleanup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_failure_reads_as_the_text_the_summary_has_always_carried() {
        // Given: one of each failure, the sources built with known messages.
        let io = || io::Error::other("disk");
        let storage = || StorageError::BackendUnavailable {
            message: "down".to_owned(),
        };
        let store = || VectorStoreError::Backend {
            message: "gone".to_owned(),
        };
        let cases = [
            (
                PipelineError::ZeroInputLimit,
                "embedder input limit must be greater than zero",
            ),
            (
                PipelineError::ChunkCountRange,
                "chunk count exceeds supported numeric range",
            ),
            (
                PipelineError::MissingEtag,
                "storage.head_object returned no ETag for stable snapshot",
            ),
            (
                PipelineError::Stream(storage()),
                "storage.get_object_stream failed: backend unavailable: down",
            ),
            (
                PipelineError::EtagChanged,
                "streamed snapshot ETag differs from preceding HEAD",
            ),
            (
                PipelineError::Staging(StageError::TooLarge { size: 2, limit: 1 }),
                "staging index snapshot failed: body is 2 bytes, exceeding limit 1",
            ),
            (
                PipelineError::ObserverPoisoned,
                "index snapshot observer lock poisoned",
            ),
            (
                PipelineError::SnapshotInspection(io()),
                "snapshot inspection failed: disk",
            ),
            (
                PipelineError::ChunkRead(io()),
                "streaming chunk read failed: disk",
            ),
            (PipelineError::ChunkIndexOverflow, "chunk index overflow"),
            (
                PipelineError::ChunkTooLarge {
                    index: 3,
                    bytes: 9,
                    maximum: 8,
                },
                "chunk 3 is 9 UTF-8 bytes, exceeding conservative embedder bound 8",
            ),
            (
                PipelineError::ChunkIteratorSetup(io()),
                "chunk iterator setup failed: disk",
            ),
            (
                PipelineError::Embed(EmbedderError::Transport("refused".to_owned())),
                "embedder.embed failed: embedder transport error: refused",
            ),
            (
                PipelineError::EmbeddingCount {
                    returned: 1,
                    sent: 2,
                },
                "embedder returned 1 embeddings for 2 chunks",
            ),
            (
                PipelineError::PointCount {
                    chunks: 2,
                    embeddings: 1,
                },
                "embedding count mismatch: chunks=2 embeddings=1",
            ),
            (
                PipelineError::Upsert(store()),
                "vector store upsert failed: vector store backend error: gone",
            ),
            (
                PipelineError::Cleanup(store()),
                "obsolete chunk cleanup failed: vector store backend error: gone",
            ),
            (
                PipelineError::Delete(store()),
                "vector store delete failed: vector store backend error: gone",
            ),
        ];

        // When / Then: each renders exactly as the old `format!` did.
        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
    }

    #[tokio::test]
    async fn a_failed_task_keeps_its_stage_prefix() {
        // Given: a `JoinError`, whose own text names a task id, so only the prefix is pinned.
        let cancelled = || async {
            let task = tokio::spawn(std::future::pending::<()>());
            task.abort();
            task.await.expect_err("an aborted task fails")
        };

        // When: it is wrapped by either task stage.
        let iterator = PipelineError::ChunkIteratorTask(cancelled().await).to_string();
        let batch = PipelineError::ChunkBatchTask(cancelled().await).to_string();

        // Then: each reads as the old `format!` did.
        assert!(
            iterator.starts_with("chunk iterator task failed: task "),
            "{iterator}"
        );
        assert!(
            batch.starts_with("chunk batch task failed: task "),
            "{batch}"
        );
    }
}

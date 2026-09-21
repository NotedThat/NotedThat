use super::chunks::{ChunkCursor, open_chunk_cursor, take_chunk_batch, validate_chunk_byte_bound};
use super::points::build_points;
use super::snapshot::{SnapshotFacts, SnapshotObserver};
use super::{IndexerWorker, Skip, is_indexable};
use crate::chunker;
use crate::vector_store::PointSelector;
use futures::StreamExt;
use notedthat_core::{
    ConditionalHeaders, EventSource, KbSlug, ObjectEvent, ObjectMeta, ObjectPath, StagedBody,
    StorageError, search::ConceptMetadata,
};
use std::sync::{Arc, Mutex};

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
    /// Nothing to index — missing on re-read, non-indexable or empty — and any
    /// previous points are gone. Also what an explicit `Tombstone` maps to.
    Tombstoned,
}

/// Why the pipeline stopped, with what `HEAD` had established by then.
#[derive(Debug)]
pub(super) struct PipelineFailure {
    pub(super) message: String,
    pub(super) etag: Option<String>,
    pub(super) mime: Option<String>,
}

impl PipelineFailure {
    /// A failure with no stamp: `HEAD` had not succeeded.
    pub(super) fn before_head(message: String) -> Self {
        Self {
            message,
            etag: None,
            mime: None,
        }
    }
}

impl IndexerWorker {
    /// `announce` names the source to publish a change event under, for
    /// changes this worker detected rather than was told about. It fires from
    /// the `HEAD` result, before any indexability check, so an mp3 dropped into
    /// the tree is announced even though it is never indexed.
    pub(super) async fn handle_upsert(
        &self,
        kb: KbSlug,
        object_key: ObjectPath,
        skip: Skip,
        announce: Option<EventSource>,
    ) -> Result<PipelineOutcome, PipelineFailure> {
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
                if let Some(source) = announce {
                    self.announce(ObjectEvent::deleted(kb.clone(), object_key.clone(), source))
                        .await;
                }
                return self
                    .handle_tombstone(kb, object_key)
                    .await
                    .map(|()| PipelineOutcome::Tombstoned)
                    .map_err(PipelineFailure::before_head);
            }
            Err(err) => {
                return Err(PipelineFailure::before_head(format!(
                    "storage.head_object failed: {err}"
                )));
            }
        };
        if let Some(source) = announce {
            self.announce(ObjectEvent::from_meta(
                kb.clone(),
                object_key.clone(),
                &head,
                source,
            ))
            .await;
        }

        // From here on a failure knows which version it was working on.
        let (etag, mime) = (head.etag.clone(), head.content_type.clone());
        self.index_head(kb, object_key, head, skip)
            .await
            .map_err(|message| PipelineFailure {
                message,
                etag,
                mime,
            })
    }

    /// Index the object `head` describes, or take it out of the index when it
    /// is not something the index holds.
    async fn index_head(
        &self,
        kb: KbSlug,
        object_key: ObjectPath,
        head: ObjectMeta,
        skip: Skip,
    ) -> Result<PipelineOutcome, String> {
        let mime = head.content_type.clone().unwrap_or_default();
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
            return Err("embedder input limit must be greater than zero".to_owned());
        }
        let snapshot = self.stage_snapshot(&kb, &object_key, head).await?;
        let max_chars =
            chunker::SOFT_CHAR_CAP.min((max_input_tokens / MAX_UTF8_BYTES_PER_SCALAR).max(1));
        let cursor =
            open_chunk_cursor(Arc::clone(&snapshot.body), snapshot.body_start, max_chars).await?;
        let new_count = self
            .upsert_batches(&kb, &object_key, &snapshot, cursor)
            .await?;
        let chunks = u32::try_from(new_count)
            .map_err(|_| "chunk count exceeds supported numeric range".to_owned())?;
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
    ) -> Result<PreparedSnapshot, String> {
        let head_etag = head
            .etag
            .clone()
            .ok_or_else(|| "storage.head_object returned no ETag for stable snapshot".to_owned())?;
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
            .map_err(|err| format!("storage.get_object_stream failed: {err}"))?;
        if object_stream.meta.etag.as_deref() != Some(head_etag.as_str()) {
            // Deliberately not retried: retrying would only race the same writer again,
            // from further behind. Where the filesystem watcher is running, the change
            // that lost us this race raises its own event and re-enqueues the key.
            //
            // That repair is configuration-dependent, and this error is terminal —
            // `process_event` logs INDEXING_FAILED and drops it, with no backoff and no
            // dead-letter queue. On S3, or on the filesystem with NOTEDTHAT_FS_WATCH=false,
            // nothing re-enqueues it and the object stays stale in the index until it is
            // next written.
            return Err("streamed snapshot ETag differs from preceding HEAD".to_owned());
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
            .map_err(|err| format!("staging index snapshot failed: {err}"))?,
        );
        let facts = observer
            .lock()
            .map_err(|_| "index snapshot observer lock poisoned".to_owned())?
            .finish()
            .map_err(|err| format!("snapshot inspection failed: {err}"))?;
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
    ) -> Result<usize, String> {
        let batch_size = self.batch_size.max(1);
        loop {
            let (next_cursor, batch) =
                tokio::task::spawn_blocking(move || take_chunk_batch(cursor, batch_size))
                    .await
                    .map_err(|err| format!("chunk batch task failed: {err}"))??;
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
                .map_err(|err| format!("embedder.embed failed: {err}"))?;
            if embeddings.len() != texts.len() {
                return Err(format!(
                    "embedder returned {} embeddings for {} chunks",
                    embeddings.len(),
                    texts.len()
                ));
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
                .map_err(|err| format!("vector store upsert failed: {err}"))?;
        }
        Ok(cursor.next_index)
    }

    async fn cleanup_obsolete(
        &self,
        kb: &KbSlug,
        object_key: &ObjectPath,
        from_chunk_index: u32,
    ) -> Result<(), String> {
        self.store
            .delete_points(
                kb,
                PointSelector::ObjectChunksFrom {
                    object_key: object_key.as_str().to_owned(),
                    from_chunk_index,
                },
            )
            .await
            .map_err(|err| format!("obsolete chunk cleanup failed: {err}"))
    }
}

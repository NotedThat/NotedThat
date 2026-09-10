use super::chunks::{ChunkCursor, open_chunk_cursor, take_chunk_batch, validate_chunk_byte_bound};
use super::points::build_points;
use super::snapshot::{SnapshotFacts, SnapshotObserver};
use super::{IndexerWorker, Skip, is_indexable};
use crate::chunker;
use crate::vector_store::PointSelector;
use futures::StreamExt;
use notedthat_core::{
    ConditionalHeaders, KbSlug, ObjectMeta, ObjectPath, StagedBody, StorageError,
    search::ConceptMetadata,
};
use std::sync::{Arc, Mutex};

const FRONTMATTER_PREFIX_LIMIT: usize = 16 * 1024 * 1024;
const MAX_UTF8_BYTES_PER_SCALAR: usize = 4;

struct PreparedSnapshot {
    body: Arc<StagedBody>,
    meta: ObjectMeta,
    facts: SnapshotFacts,
    metadata: Option<ConceptMetadata>,
    body_start: usize,
}

impl IndexerWorker {
    pub(super) async fn handle_upsert(
        &self,
        kb: KbSlug,
        object_key: ObjectPath,
        skip: Skip,
    ) -> Result<(), String> {
        let head = match self
            .storage
            .head_object(&kb, &object_key, ConditionalHeaders::default())
            .await
        {
            Ok(meta) => meta,
            Err(StorageError::NotFound { .. }) => {
                tracing::debug!(
                    target: "notedthat::indexing",
                    kb = %kb.as_str(),
                    path = %object_key.as_str(),
                    "object not found on re-read; treating as tombstone"
                );
                return self.handle_tombstone(kb, object_key).await;
            }
            Err(err) => return Err(format!("storage.head_object failed: {err}")),
        };

        let mime = head.content_type.clone().unwrap_or_default();
        if !is_indexable(&mime) {
            tracing::debug!(
                target: "notedthat::indexing",
                kb = %kb.as_str(),
                path = %object_key.as_str(),
                mime,
                "removing index entries for non-indexable content type"
            );
            return self.handle_tombstone(kb, object_key).await;
        }
        if head.size == 0 {
            tracing::debug!(
                target: "notedthat::indexing",
                kb = %kb.as_str(),
                path = %object_key.as_str(),
                "empty object; removing previous index entries"
            );
            return self.handle_tombstone(kb, object_key).await;
        }

        if skip == Skip::IfUnchanged && self.already_indexed(&kb, &object_key, &head).await {
            tracing::debug!(
                target: "notedthat::indexing",
                kb = %kb.as_str(),
                path = %object_key.as_str(),
                "unchanged since it was last indexed; skipping"
            );
            return Ok(());
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
        self.cleanup_obsolete(&kb, &object_key, new_count).await?;

        tracing::info!(
            target: "notedthat::indexing",
            kb = %kb.as_str(),
            path = %object_key.as_str(),
            chunks = new_count,
            "indexed"
        );
        Ok(())
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
        new_count: usize,
    ) -> Result<(), String> {
        let threshold = u32::try_from(new_count)
            .map_err(|_| "chunk count exceeds supported numeric range".to_owned())?;
        self.store
            .delete_points(
                kb,
                PointSelector::ObjectChunksFrom {
                    object_key: object_key.as_str().to_owned(),
                    from_chunk_index: threshold,
                },
            )
            .await
            .map_err(|err| format!("obsolete chunk cleanup failed: {err}"))
    }
}

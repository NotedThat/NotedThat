//! Shared commit operations for object writes and deletes.

use notedthat_core::{
    ConditionalHeaders, CopyObjectOptions, KbSlug, ObjectEvent, ObjectPath, PutOutcome, StagedBody,
    Storage, StorageError,
};
use notedthat_indexer::IndexEvent;
use tokio::sync::mpsc::error::TrySendError;

use crate::WriteError;
use crate::error::WriteEffect;
use crate::mime::sniff_content_type;
use crate::sinks::WriteSinks;

/// Maximum upload size accepted by shared write paths: 5 GiB.
pub const MAX_UPLOAD_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Validate an upload size against a byte limit.
pub fn check_size(size: u64, limit: u64) -> Result<(), WriteError> {
    if size > limit {
        Err(WriteError::TooLarge { size, limit })
    } else {
        Ok(())
    }
}

/// Store an object, enqueue a best-effort index upsert event and publish the
/// change.
pub async fn commit<B: Into<StagedBody>>(
    storage: &dyn Storage,
    sinks: &WriteSinks<'_>,
    kb: &KbSlug,
    path: &ObjectPath,
    body: B,
    caller_content_type: Option<&str>,
    conditionals: ConditionalHeaders,
) -> Result<PutOutcome, WriteError> {
    let body = body.into();
    let size = body.len();
    check_size(size, MAX_UPLOAD_BYTES)?;
    let _prefix = body.prefix(512).await.map_err(|source| {
        WriteError::Storage(StorageError::Other {
            source: Box::new(source),
        })
    })?;
    let mime = sniff_content_type(caller_content_type, path);
    let outcome = storage
        .put_staged_object(kb, path, body, Some(&mime), conditionals)
        .await?;

    after_write(sinks, kb, path, &outcome, size, &mime).await?;
    Ok(outcome)
}

/// Copy an object natively, enqueue its destination for indexing and publish
/// the change.
pub async fn commit_copy(
    storage: &dyn Storage,
    sinks: &WriteSinks<'_>,
    kb: &KbSlug,
    source: &ObjectPath,
    destination: &ObjectPath,
    options: CopyObjectOptions,
) -> Result<PutOutcome, WriteError> {
    let content_type = options.content_type.clone();
    let outcome = storage
        .copy_object(kb, source, destination, options)
        .await?;
    // A copy returns only the new ETag. The event wants the size too, and only
    // a subscriber cares, so the extra HEAD is paid only when one can exist.
    // The copy is already durable by now, so a HEAD that fails — a racing
    // delete, a transient error — degrades the stamp rather than the copy.
    let (size, mime) = if sinks.events.is_some() {
        match storage
            .head_object(kb, destination, ConditionalHeaders::default())
            .await
        {
            Ok(meta) => (meta.size, meta.content_type.or(content_type)),
            Err(error) => {
                tracing::warn!(
                    target: "notedthat::events",
                    kb = %kb, path = %destination, %error,
                    "could not HEAD the copy destination for its event; publishing without a size"
                );
                (0, content_type)
            }
        }
    } else {
        (0, content_type)
    };
    after_write(
        sinks,
        kb,
        destination,
        &outcome,
        size,
        mime.as_deref().unwrap_or_default(),
    )
    .await?;
    Ok(outcome)
}

/// Report a durable write: index first, then publish.
///
/// The order matters for D38's promise that a 503 makes the client the retry
/// mechanism. Either failure returns an error after the bytes are stored, and
/// a retried write re-runs both — an event may be published twice, never
/// zero times.
pub(crate) async fn after_write(
    sinks: &WriteSinks<'_>,
    kb: &KbSlug,
    path: &ObjectPath,
    outcome: &PutOutcome,
    size: u64,
    mime: &str,
) -> Result<(), WriteError> {
    let event = IndexEvent::Upsert {
        kb: kb.clone(),
        object_key: path.clone(),
        etag: outcome.etag.clone().unwrap_or_default(),
        mtime: current_unix_seconds(),
    };
    match sinks.indexer_tx.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(ev)) => {
            tracing::warn!(target: "notedthat::indexing", kb = %kb, path = %path, "INDEX_QUEUE_FULL");
            let _ = ev;
            return Err(WriteError::IndexerBackpressureUpsert);
        }
        Err(TrySendError::Closed(ev)) => {
            tracing::error!(target: "notedthat::indexing", kb = %kb, path = %path, "INDEX_QUEUE_CLOSED");
            let _ = ev;
            // Closed = indexer worker ended (shutdown OR panic). v1 preserves success-with-error-log
            // until post-v1 worker liveness detection is added.
        }
    }

    let Some(events) = sinks.events else {
        return Ok(());
    };
    let event = ObjectEvent::written(
        kb.clone(),
        path.clone(),
        outcome.etag.clone().unwrap_or_default(),
        size,
        mime.to_string(),
        current_unix_seconds(),
        sinks.source,
    );
    publish(events, event, WriteEffect::Stored).await
}

/// Delete an object idempotently, enqueue a best-effort tombstone event and
/// publish the change.
pub async fn commit_delete(
    storage: &dyn Storage,
    sinks: &WriteSinks<'_>,
    kb: &KbSlug,
    path: &ObjectPath,
    conditionals: ConditionalHeaders,
) -> Result<(), WriteError> {
    match storage.delete_object(kb, path, conditionals).await {
        Ok(()) | Err(StorageError::NotFound { .. }) => {}
        Err(e) => return Err(WriteError::Storage(e)),
    }

    let event = IndexEvent::Tombstone {
        kb: kb.clone(),
        object_key: path.clone(),
    };
    match sinks.indexer_tx.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(ev)) => {
            tracing::warn!(target: "notedthat::indexing", kb = %kb, path = %path, "INDEX_QUEUE_FULL");
            let _ = ev;
            return Err(WriteError::IndexerBackpressureTombstone);
        }
        // Closed means the indexer worker task ended via shutdown OR panic; v1 deliberately preserves success-with-error-log so writes are not blocked by a crashed worker, and post-v1 worker liveness detection will revisit this.
        Err(TrySendError::Closed(ev)) => {
            tracing::error!(target: "notedthat::indexing", kb = %kb, path = %path, "INDEX_QUEUE_CLOSED");
            let _ = ev;
        }
    }

    let Some(events) = sinks.events else {
        return Ok(());
    };
    let event = ObjectEvent::deleted(kb.clone(), path.clone(), sinks.source);
    publish(events, event, WriteEffect::Deleted).await
}

async fn publish(
    events: &dyn notedthat_core::EventPublisher,
    event: ObjectEvent,
    after: WriteEffect,
) -> Result<(), WriteError> {
    let (kb, path) = (event.kb.clone(), event.object_key.clone());
    match events.publish(event).await {
        Ok(_) => Ok(()),
        Err(error) => {
            tracing::error!(target: "notedthat::events", kb = %kb, path = %path, %error, "EVENT_PUBLISH_FAILED");
            Err(WriteError::EventPublishFailed { after })
        }
    }
}

pub(crate) fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteEffect;
    use async_trait::async_trait;
    use bytes::Bytes;
    use notedthat_core::{KbManifest, ListResponse, ObjectMeta, ObjectRead};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    #[derive(Default)]
    struct TestStorage {
        objects: Mutex<HashMap<String, String>>,
    }

    #[async_trait]
    impl Storage for TestStorage {
        async fn ensure_bucket(&self, _kb: &KbSlug) -> Result<(), StorageError> {
            unimplemented!()
        }

        async fn read_manifest(&self, _kb: &KbSlug) -> Result<KbManifest, StorageError> {
            unimplemented!()
        }

        async fn write_manifest(
            &self,
            _kb: &KbSlug,
            _manifest: &KbManifest,
        ) -> Result<(), StorageError> {
            unimplemented!()
        }

        async fn head_object(
            &self,
            kb: &KbSlug,
            path: &ObjectPath,
            _conditionals: ConditionalHeaders,
        ) -> Result<ObjectMeta, StorageError> {
            let key = format!("{}/{}", kb.as_str(), path.as_str());
            let objects = self.objects.lock().expect("mutex not poisoned");
            let etag = objects
                .get(&key)
                .ok_or_else(|| StorageError::NotFound { key: key.clone() })?;
            Ok(ObjectMeta {
                key: path.as_str().to_string(),
                size: 7,
                last_modified: Some(1_700_000_000),
                content_type: Some("text/markdown".into()),
                etag: Some(etag.clone()),
            })
        }

        async fn get_object(
            &self,
            _kb: &KbSlug,
            _path: &ObjectPath,
            _range: Option<Vec<notedthat_core::ByteRange>>,
            _conditionals: ConditionalHeaders,
        ) -> Result<ObjectRead, StorageError> {
            unimplemented!()
        }

        async fn get_object_stream(
            &self,
            _kb: &KbSlug,
            _path: &ObjectPath,
            _range: Option<Vec<notedthat_core::ByteRange>>,
            _conditionals: ConditionalHeaders,
        ) -> Result<notedthat_core::ObjectStream, StorageError> {
            unimplemented!()
        }

        async fn put_object(
            &self,
            kb: &KbSlug,
            path: &ObjectPath,
            _bytes: Bytes,
            _content_type: Option<&str>,
            conditionals: ConditionalHeaders,
        ) -> Result<PutOutcome, StorageError> {
            let key = format!("{}/{}", kb.as_str(), path.as_str());
            let mut objects = self.objects.lock().expect("mutex not poisoned");
            let existing = objects.get(&key);
            if let Some(if_match) = conditionals.if_match
                && existing.is_none_or(|etag| etag != &if_match)
            {
                return Err(StorageError::PreconditionFailed);
            }

            let etag = format!("\"etag-{}\"", objects.len() + 1);
            objects.insert(key, etag.clone());
            Ok(PutOutcome { etag: Some(etag) })
        }

        async fn put_staged_object(
            &self,
            kb: &KbSlug,
            path: &ObjectPath,
            body: StagedBody,
            content_type: Option<&str>,
            conditionals: ConditionalHeaders,
        ) -> Result<PutOutcome, StorageError> {
            let bytes =
                body.memory_bytes()
                    .cloned()
                    .ok_or_else(|| StorageError::BackendUnavailable {
                        message: "file staging is outside this commit unit test".into(),
                    })?;
            self.put_object(kb, path, bytes, content_type, conditionals)
                .await
        }

        async fn copy_object(
            &self,
            kb: &KbSlug,
            _source: &ObjectPath,
            destination: &ObjectPath,
            _options: CopyObjectOptions,
        ) -> Result<PutOutcome, StorageError> {
            let key = format!("{}/{}", kb.as_str(), destination.as_str());
            let mut objects = self.objects.lock().expect("mutex not poisoned");
            let etag = format!("\"etag-{}\"", objects.len() + 1);
            objects.insert(key, etag.clone());
            Ok(PutOutcome { etag: Some(etag) })
        }

        async fn delete_object(
            &self,
            kb: &KbSlug,
            path: &ObjectPath,
            _conditionals: ConditionalHeaders,
        ) -> Result<(), StorageError> {
            let key = format!("{}/{}", kb.as_str(), path.as_str());
            self.objects
                .lock()
                .expect("mutex not poisoned")
                .remove(&key);
            Ok(())
        }

        async fn list_objects(
            &self,
            _kb: &KbSlug,
            _prefix: Option<&str>,
            _limit: u32,
            _cursor: Option<&str>,
        ) -> Result<ListResponse, StorageError> {
            unimplemented!()
        }
    }

    fn kb() -> KbSlug {
        KbSlug::try_new("test-kb").expect("valid kb slug")
    }

    fn path() -> ObjectPath {
        ObjectPath::try_from_str("test.md").expect("valid path")
    }

    fn path_named(value: &str) -> ObjectPath {
        ObjectPath::try_from_str(value).expect("valid path")
    }

    #[tokio::test]
    async fn successful_put_enqueues_event() {
        let storage = TestStorage::default();
        let kb = kb();
        let path = path();
        let (indexer_tx, mut rx) = mpsc::channel(1024);

        let outcome = commit(
            &storage,
            &WriteSinks::indexer_only(&indexer_tx),
            &kb,
            &path,
            Bytes::from_static(b"# Test"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await;

        assert!(outcome.is_ok());
        assert!(outcome.unwrap().etag.is_some());

        let event = rx.recv().await.expect("event should be enqueued");
        assert_eq!(event.kb().as_str(), "test-kb");
        assert_eq!(event.object_key().as_str(), "test.md");
    }

    #[tokio::test]
    async fn successful_native_copy_enqueues_destination_event() {
        let storage = TestStorage::default();
        let kb = kb();
        let source = path_named("source.md");
        let destination = path_named("destination.md");
        let (indexer_tx, mut rx) = mpsc::channel(1);

        let outcome = commit_copy(
            &storage,
            &WriteSinks::indexer_only(&indexer_tx),
            &kb,
            &source,
            &destination,
            CopyObjectOptions::default(),
        )
        .await
        .expect("copy succeeds");

        assert!(outcome.etag.is_some());
        let event = rx.recv().await.expect("destination event");
        assert_eq!(event.object_key(), &destination);
    }

    #[tokio::test]
    async fn full_queue_returns_indexer_backpressure() {
        let storage = TestStorage::default();
        let kb = kb();
        let path = path();
        let (indexer_tx, _rx) = mpsc::channel(1);

        let dummy_event = IndexEvent::Upsert {
            kb: kb.clone(),
            object_key: path.clone(),
            etag: "dummy".to_string(),
            mtime: 0,
        };
        indexer_tx
            .try_send(dummy_event)
            .expect("first send should succeed");

        let outcome = commit(
            &storage,
            &WriteSinks::indexer_only(&indexer_tx),
            &kb,
            &path,
            Bytes::from_static(b"# Test"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await;

        let err = outcome.unwrap_err();
        assert!(
            matches!(err, WriteError::IndexerBackpressureUpsert),
            "expected IndexerBackpressureUpsert, got {err:?}"
        );
    }

    #[tokio::test]
    async fn burst_write_returns_backpressure_after_capacity() {
        let storage = Arc::new(TestStorage::default());
        let kb = kb();
        let path_a = path_named("a.md");
        let path_b = path_named("b.md");
        let path_c = path_named("c.md");
        let (indexer_tx, _rx) = mpsc::channel::<IndexEvent>(2);

        let first = commit(
            storage.as_ref(),
            &WriteSinks::indexer_only(&indexer_tx),
            &kb,
            &path_a,
            Bytes::from_static(b"# A"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await;
        let second = commit(
            storage.as_ref(),
            &WriteSinks::indexer_only(&indexer_tx),
            &kb,
            &path_b,
            Bytes::from_static(b"# B"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await;
        let third = commit(
            storage.as_ref(),
            &WriteSinks::indexer_only(&indexer_tx),
            &kb,
            &path_c,
            Bytes::from_static(b"# C"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await;

        assert!(first.is_ok(), "first write should fill queue slot one");
        assert!(second.is_ok(), "second write should fill queue slot two");
        let err = third.unwrap_err();
        assert!(
            matches!(err, WriteError::IndexerBackpressureUpsert),
            "expected IndexerBackpressureUpsert, got {err:?}"
        );
        assert!(
            storage
                .objects
                .lock()
                .expect("mutex not poisoned")
                .contains_key("test-kb/c.md"),
            "stored object should remain after enqueue backpressure"
        );
    }

    #[tokio::test]
    async fn commit_delete_full_queue_returns_backpressure() {
        let storage = TestStorage::default();
        let kb = kb();
        let path = path_named("delete.md");
        storage
            .put_object(
                &kb,
                &path,
                Bytes::from_static(b"# Delete"),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .expect("prepopulate object");
        let (indexer_tx, _rx) = mpsc::channel(1);
        let dummy_event = IndexEvent::Tombstone {
            kb: kb.clone(),
            object_key: path.clone(),
        };
        indexer_tx
            .try_send(dummy_event)
            .expect("first send should succeed");

        let outcome = commit_delete(
            &storage,
            &WriteSinks::indexer_only(&indexer_tx),
            &kb,
            &path,
            ConditionalHeaders::default(),
        )
        .await;

        let err = outcome.unwrap_err();
        assert!(
            matches!(err, WriteError::IndexerBackpressureTombstone),
            "expected IndexerBackpressureTombstone, got {err:?}"
        );
        assert!(
            !storage
                .objects
                .lock()
                .expect("mutex not poisoned")
                .contains_key("test-kb/delete.md"),
            "deleted object should remain deleted after enqueue backpressure"
        );
    }

    #[tokio::test]
    async fn closed_queue_returns_write_success() {
        let storage = TestStorage::default();
        let kb = kb();
        let path = path();
        let (indexer_tx, rx) = mpsc::channel(1024);

        drop(rx);

        let outcome = commit(
            &storage,
            &WriteSinks::indexer_only(&indexer_tx),
            &kb,
            &path,
            Bytes::from_static(b"# Test"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await;

        assert!(
            outcome.is_ok(),
            "write should succeed even if queue is closed"
        );
    }

    #[tokio::test]
    async fn put_failure_returns_error_no_event() {
        let storage = TestStorage::default();
        let kb = kb();
        let path = path();
        let (indexer_tx, mut rx) = mpsc::channel(1024);

        let conditionals = ConditionalHeaders {
            if_match: Some("\"wrong-etag\"".to_string()),
            ..ConditionalHeaders::default()
        };

        let outcome = commit(
            &storage,
            &WriteSinks::indexer_only(&indexer_tx),
            &kb,
            &path,
            Bytes::from_static(b"# Test"),
            Some("text/markdown"),
            conditionals,
        )
        .await;

        assert!(outcome.is_err(), "put should fail with precondition");
        assert!(
            rx.try_recv().is_err(),
            "no event should be enqueued on put failure"
        );
    }

    #[test]
    fn check_size_over_limit_returns_too_large() {
        let err = check_size(MAX_UPLOAD_BYTES + 1, MAX_UPLOAD_BYTES).expect_err("too large");
        assert!(matches!(
            err,
            WriteError::TooLarge {
                size,
                limit
            } if size == MAX_UPLOAD_BYTES + 1 && limit == MAX_UPLOAD_BYTES
        ));
    }

    #[test]
    fn check_size_at_limit_returns_ok() {
        assert!(check_size(MAX_UPLOAD_BYTES, MAX_UPLOAD_BYTES).is_ok());
    }

    #[test]
    fn check_size_below_limit_returns_ok() {
        assert!(check_size(1024, MAX_UPLOAD_BYTES).is_ok());
    }

    /// Records what was published, or refuses everything.
    struct RecordingPublisher {
        published: Mutex<Vec<ObjectEvent>>,
        refuse: bool,
    }

    impl RecordingPublisher {
        fn recording() -> Self {
            Self {
                published: Mutex::new(Vec::new()),
                refuse: false,
            }
        }

        fn refusing() -> Self {
            Self {
                published: Mutex::new(Vec::new()),
                refuse: true,
            }
        }

        fn events(&self) -> Vec<ObjectEvent> {
            self.published.lock().expect("mutex not poisoned").clone()
        }
    }

    #[async_trait]
    impl notedthat_core::EventPublisher for RecordingPublisher {
        async fn publish(
            &self,
            event: ObjectEvent,
        ) -> Result<notedthat_core::EventId, notedthat_core::PublishError> {
            if self.refuse {
                return Err(notedthat_core::PublishError::Unavailable {
                    message: "broker down".into(),
                });
            }
            let mut published = self.published.lock().expect("mutex not poisoned");
            published.push(event);
            Ok(notedthat_core::EventId(published.len() as u64))
        }

        async fn subscribe(
            &self,
            _kb: &KbSlug,
            _after: Option<notedthat_core::EventId>,
        ) -> Result<notedthat_core::EventStream, notedthat_core::SubscribeError> {
            unimplemented!()
        }

        fn ready(&self) -> bool {
            true
        }

        fn backend_name(&self) -> &'static str {
            "recording"
        }
    }

    fn sinks<'a>(
        indexer_tx: &'a mpsc::Sender<IndexEvent>,
        events: &'a RecordingPublisher,
        source: notedthat_core::EventSource,
    ) -> WriteSinks<'a> {
        WriteSinks::new(indexer_tx, Some(events), source)
    }

    #[tokio::test]
    async fn a_committed_write_is_published_with_its_stamp_and_source() {
        let storage = TestStorage::default();
        let events = RecordingPublisher::recording();
        let (indexer_tx, mut rx) = mpsc::channel(8);

        let outcome = commit(
            &storage,
            &sinks(&indexer_tx, &events, notedthat_core::EventSource::Webdav),
            &kb(),
            &path(),
            Bytes::from_static(b"# Test"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("write succeeds");

        rx.recv().await.expect("index event first");
        let published = events.events();
        assert_eq!(published.len(), 1);
        let event = &published[0];
        assert_eq!(event.kb.as_str(), "test-kb");
        assert_eq!(event.object_key.as_str(), "test.md");
        assert_eq!(event.source, notedthat_core::EventSource::Webdav);
        assert_eq!(
            event.kind,
            notedthat_core::ObjectEventKind::Written {
                etag: outcome.etag.expect("etag"),
                size: 6,
                mime: "text/markdown".into(),
                mtime: match &event.kind {
                    notedthat_core::ObjectEventKind::Written { mtime, .. } => *mtime,
                    notedthat_core::ObjectEventKind::Deleted => unreachable!(),
                },
            }
        );
    }

    #[tokio::test]
    async fn indexer_backpressure_wins_and_nothing_is_published() {
        let storage = TestStorage::default();
        let events = RecordingPublisher::recording();
        let (indexer_tx, _rx) = mpsc::channel(1);
        indexer_tx
            .try_send(IndexEvent::Tombstone {
                kb: kb(),
                object_key: path(),
            })
            .expect("fill the queue");

        let err = commit(
            &storage,
            &sinks(&indexer_tx, &events, notedthat_core::EventSource::Http),
            &kb(),
            &path(),
            Bytes::from_static(b"# Test"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .unwrap_err();

        assert!(matches!(err, WriteError::IndexerBackpressureUpsert));
        assert!(events.events().is_empty(), "no event behind a 503");
    }

    #[tokio::test]
    async fn a_refused_publish_fails_the_write_but_the_object_stays_stored() {
        let storage = TestStorage::default();
        let events = RecordingPublisher::refusing();
        let (indexer_tx, mut rx) = mpsc::channel(8);

        let err = commit(
            &storage,
            &sinks(&indexer_tx, &events, notedthat_core::EventSource::Http),
            &kb(),
            &path(),
            Bytes::from_static(b"# Test"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(
                err,
                WriteError::EventPublishFailed {
                    after: WriteEffect::Stored
                }
            ),
            "{err:?}"
        );
        assert!(
            storage
                .objects
                .lock()
                .expect("mutex not poisoned")
                .contains_key("test-kb/test.md"),
            "the bytes were stored before publishing failed"
        );
        rx.recv().await.expect("the index event was still enqueued");
    }

    #[tokio::test]
    async fn a_delete_publishes_a_deleted_event_and_a_refusal_says_deleted() {
        let storage = TestStorage::default();
        let (indexer_tx, _rx) = mpsc::channel(8);

        let events = RecordingPublisher::recording();
        commit_delete(
            &storage,
            &sinks(&indexer_tx, &events, notedthat_core::EventSource::Mcp),
            &kb(),
            &path_named("gone.md"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("delete succeeds");
        let published = events.events();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].kind, notedthat_core::ObjectEventKind::Deleted);
        assert_eq!(published[0].source, notedthat_core::EventSource::Mcp);

        let refusing = RecordingPublisher::refusing();
        let err = commit_delete(
            &storage,
            &sinks(&indexer_tx, &refusing, notedthat_core::EventSource::Http),
            &kb(),
            &path_named("gone.md"),
            ConditionalHeaders::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            WriteError::EventPublishFailed {
                after: WriteEffect::Deleted
            }
        ));
    }

    #[tokio::test]
    async fn a_copy_heads_the_destination_for_the_stamp_only_when_publishing() {
        let storage = TestStorage::default();
        let (indexer_tx, _rx) = mpsc::channel(8);
        let events = RecordingPublisher::recording();

        let outcome = commit_copy(
            &storage,
            &sinks(&indexer_tx, &events, notedthat_core::EventSource::Webdav),
            &kb(),
            &path_named("src.md"),
            &path_named("dst.md"),
            CopyObjectOptions::default(),
        )
        .await
        .expect("copy succeeds");

        let published = events.events();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].object_key.as_str(), "dst.md");
        match &published[0].kind {
            notedthat_core::ObjectEventKind::Written {
                etag, size, mime, ..
            } => {
                assert_eq!(Some(etag), outcome.etag.as_ref());
                assert_eq!(*size, 7, "size comes from the HEAD after the copy");
                assert_eq!(mime, "text/markdown");
            }
            notedthat_core::ObjectEventKind::Deleted => panic!("a copy is a write"),
        }
    }
}

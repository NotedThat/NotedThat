//! Shared commit operations for object writes and deletes.

use bytes::Bytes;
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, PutOutcome, Storage, StorageError};
use notedthat_indexer::IndexEvent;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;

use crate::WriteError;
use crate::mime::sniff_content_type;

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

/// Store an object and enqueue a best-effort index upsert event.
pub async fn commit(
    storage: &dyn Storage,
    indexer_tx: &Sender<IndexEvent>,
    kb: &KbSlug,
    path: &ObjectPath,
    bytes: Bytes,
    caller_content_type: Option<&str>,
    conditionals: ConditionalHeaders,
) -> Result<PutOutcome, WriteError> {
    check_size(bytes.len() as u64, MAX_UPLOAD_BYTES)?;
    let mime = sniff_content_type(caller_content_type, path);
    let outcome = storage
        .put_object(kb, path, bytes, Some(&mime), conditionals)
        .await?;

    let event = IndexEvent::Upsert {
        kb: kb.clone(),
        object_key: path.clone(),
        etag: outcome.etag.clone().unwrap_or_default(),
        mtime: current_unix_seconds(),
    };
    log_try_send(indexer_tx.try_send(event));

    Ok(outcome)
}

/// Delete an object idempotently and enqueue a best-effort tombstone event.
pub async fn commit_delete(
    storage: &dyn Storage,
    indexer_tx: &Sender<IndexEvent>,
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
    log_try_send(indexer_tx.try_send(event));

    Ok(())
}

fn log_try_send(result: Result<(), TrySendError<IndexEvent>>) {
    match result {
        Ok(()) => {}
        Err(TrySendError::Full(ev)) => {
            tracing::warn!(
                target: "notedthat::indexing",
                kb = %ev.kb().as_str(),
                path = %ev.object_key().as_str(),
                "INDEX_QUEUE_FULL"
            );
        }
        Err(TrySendError::Closed(ev)) => {
            tracing::error!(
                target: "notedthat::indexing",
                kb = %ev.kb().as_str(),
                path = %ev.object_key().as_str(),
                "INDEX_QUEUE_CLOSED"
            );
        }
    }
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use notedthat_core::{KbManifest, ListResponse, ObjectMeta, ObjectRead};
    use std::collections::HashMap;
    use std::sync::Mutex;
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
            _kb: &KbSlug,
            _path: &ObjectPath,
            _conditionals: ConditionalHeaders,
        ) -> Result<ObjectMeta, StorageError> {
            unimplemented!()
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

    #[tokio::test]
    async fn successful_put_enqueues_event() {
        let storage = TestStorage::default();
        let kb = kb();
        let path = path();
        let (indexer_tx, mut rx) = mpsc::channel(1024);

        let outcome = commit(
            &storage,
            &indexer_tx,
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
    async fn full_queue_returns_write_success() {
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
            &indexer_tx,
            &kb,
            &path,
            Bytes::from_static(b"# Test"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await;

        assert!(
            outcome.is_ok(),
            "write should succeed even if queue is full"
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
            &indexer_tx,
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
            &indexer_tx,
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
}

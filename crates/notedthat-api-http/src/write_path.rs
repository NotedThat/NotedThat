//! Write path: all PUT handlers must call `commit()` here.

use bytes::Bytes;
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, PutOutcome, Storage};
use notedthat_indexer::IndexEvent;
use tokio::sync::mpsc::error::TrySendError;

use crate::error::ApiError;

/// Store an object, replacing any existing content at the same path.
///
/// On successful put, an `IndexEvent::Upsert` is enqueued via `try_send`
/// (non-blocking). On queue-full or queue-closed, we log and return write
/// success anyway — indexing is best-effort per D38 / D4.
pub async fn commit(
    storage: &dyn Storage,
    indexer_tx: &tokio::sync::mpsc::Sender<IndexEvent>,
    kb: &KbSlug,
    path: &ObjectPath,
    bytes: Bytes,
    content_type: Option<&str>,
    conditionals: ConditionalHeaders,
) -> Result<PutOutcome, ApiError> {
    let outcome = storage
        .put_object(kb, path, bytes, content_type, conditionals)
        .await?;

    let event = IndexEvent::Upsert {
        kb: kb.clone(),
        object_key: path.clone(),
        etag: outcome.etag.clone().unwrap_or_default(),
        mtime: current_unix_seconds(),
    };
    match indexer_tx.try_send(event) {
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
    Ok(outcome)
}

fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn successful_put_enqueues_event() {
        let storage = crate::testing::InMemoryStorage::default();
        let kb = KbSlug::try_new("test-kb").expect("valid kb slug");
        let path = ObjectPath::try_from_str("test.md").expect("valid path");
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

        // Verify event was enqueued
        let event = rx.recv().await.expect("event should be enqueued");
        assert_eq!(event.kb().as_str(), "test-kb");
        assert_eq!(event.object_key().as_str(), "test.md");
    }

    #[tokio::test]
    async fn full_queue_returns_write_success() {
        let storage = crate::testing::InMemoryStorage::default();
        let kb = KbSlug::try_new("test-kb").expect("valid kb slug");
        let path = ObjectPath::try_from_str("test.md").expect("valid path");
        let (indexer_tx, _rx) = mpsc::channel(1);

        // Fill the queue
        let dummy_event = IndexEvent::Upsert {
            kb: kb.clone(),
            object_key: path.clone(),
            etag: "dummy".to_string(),
            mtime: 0,
        };
        indexer_tx
            .try_send(dummy_event)
            .expect("first send should succeed");

        // Now the queue is full; commit should still succeed
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
        let storage = crate::testing::InMemoryStorage::default();
        let kb = KbSlug::try_new("test-kb").expect("valid kb slug");
        let path = ObjectPath::try_from_str("test.md").expect("valid path");
        let (indexer_tx, rx) = mpsc::channel(1024);

        // Close the receiver to close the channel
        drop(rx);

        // commit should still succeed
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
        let storage = crate::testing::InMemoryStorage::default();
        let kb = KbSlug::try_new("test-kb").expect("valid kb slug");
        let path = ObjectPath::try_from_str("test.md").expect("valid path");
        let (indexer_tx, mut rx) = mpsc::channel(1024);

        // Create a precondition that will fail
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

        // Verify no event was enqueued
        assert!(
            rx.try_recv().is_err(),
            "no event should be enqueued on put failure"
        );
    }
}

use bytes::Bytes;
use notedthat_core::StorageError;
use notedthat_indexer::IndexEvent;
use tokio::sync::mpsc;

use super::support::{
    ReplaceArgs, Script, TestStorage, conditionals, expect_replace_err, kb, path, run_replace,
    run_replace_with,
};

#[tokio::test]
async fn stale_caller_if_match_returns_precondition_failed_no_retry() {
    let storage = TestStorage::with_body(b"hello world");
    let err = expect_replace_err(
        run_replace_with(
            &storage,
            ReplaceArgs::one("world", "planet"),
            conditionals(Some("\"stale\"")),
            1024,
            None,
        )
        .await,
    );

    assert!(matches!(
        err,
        crate::WriteError::Storage(StorageError::PreconditionFailed)
    ));
    let calls = storage.calls();
    assert_eq!(calls.head, 1);
    assert_eq!(calls.get, 0);
    assert_eq!(calls.put, 0);
    assert_eq!(storage.body(), Bytes::from_static(b"hello world"));
}

#[tokio::test]
async fn window_412_from_get_absorbed_by_two_retries_then_surfaces_precondition_failed() {
    for failures in [1, 2] {
        let storage = TestStorage::with_script(
            b"hello world",
            Script {
                get_failures_remaining: failures,
                ..Script::default()
            },
        );

        run_replace(&storage, ReplaceArgs::one("world", "planet"))
            .await
            .expect("GET precondition window is absorbed before max attempts");

        assert_eq!(storage.body(), Bytes::from_static(b"hello planet"));
    }

    let storage = TestStorage::with_script(
        b"hello world",
        Script {
            get_failures_remaining: 3,
            ..Script::default()
        },
    );
    let err = expect_replace_err(run_replace(&storage, ReplaceArgs::one("world", "planet")).await);

    assert!(matches!(
        err,
        crate::WriteError::Storage(StorageError::PreconditionFailed)
    ));
    let calls = storage.calls();
    assert_eq!(calls.head, 3);
    assert_eq!(calls.get, 3);
    assert_eq!(calls.put, 0);
}

#[tokio::test]
async fn window_412_from_put_absorbed_by_two_retries() {
    let storage = TestStorage::with_script(
        b"hello world",
        Script {
            put_failures_remaining: 2,
            ..Script::default()
        },
    );

    run_replace(&storage, ReplaceArgs::one("world", "planet"))
        .await
        .expect("PUT precondition window is absorbed before max attempts");

    assert_eq!(storage.body(), Bytes::from_static(b"hello planet"));
    let calls = storage.calls();
    assert_eq!(calls.head, 3);
    assert_eq!(calls.get, 3);
    assert_eq!(calls.put, 3);
}

#[tokio::test]
async fn indexer_channel_full_returns_backpressure_upsert_after_successful_put() {
    let storage = TestStorage::with_body(b"hello world");
    let (indexer_tx, _rx) = mpsc::channel(1);
    indexer_tx
        .try_send(IndexEvent::Upsert {
            kb: kb(),
            object_key: path(),
            etag: "queued".to_string(),
            mtime: 0,
        })
        .expect("prefill indexer channel");
    let err = expect_replace_err(
        run_replace_with(
            &storage,
            ReplaceArgs::one("world", "planet"),
            conditionals(Some("etag1")),
            1024,
            Some(indexer_tx),
        )
        .await,
    );

    assert!(matches!(err, crate::WriteError::IndexerBackpressureUpsert));
    assert_eq!(
        storage.read().await.bytes,
        Bytes::from_static(b"hello planet")
    );
}

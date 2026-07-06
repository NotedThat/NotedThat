use super::support::{Script, TestStorage, conditionals, run_patch};
use bytes::Bytes;
use notedthat_core::{ByteRange, StorageError};

use crate::{PatchMode, WriteError};

fn byte_patch() -> PatchMode {
    PatchMode::Bytes {
        range: ByteRange::FromStart { first: 0, last: 1 },
        body: Bytes::from_static(b"xy"),
    }
}

#[tokio::test]
async fn succeeds_on_first_attempt_without_retry() {
    let storage = TestStorage::with_script(b"0123456789", Script::default());

    let (outcome, _rx) = run_patch(&storage, byte_patch(), conditionals(Some("etag1")), 1024)
        .await
        .expect("patch succeeds");

    assert_eq!(outcome.etag.as_deref(), Some("etag2"));
    assert_eq!(storage.body(), Bytes::from_static(b"xy23456789"));
    let calls = storage.calls();
    assert_eq!(calls.head, 1);
    assert_eq!(calls.get, 1);
    assert_eq!(calls.put, 1);
}

#[tokio::test]
async fn retries_two_put_precondition_failures_then_succeeds() {
    let storage = TestStorage::with_script(
        b"0123456789",
        Script {
            put_failures_remaining: 2,
            ..Script::default()
        },
    );

    run_patch(&storage, byte_patch(), conditionals(Some("etag1")), 1024)
        .await
        .expect("third attempt succeeds");

    assert_eq!(storage.body(), Bytes::from_static(b"xy23456789"));
    let calls = storage.calls();
    assert_eq!(calls.head, 3);
    assert_eq!(calls.get, 3);
    assert_eq!(calls.put, 3);
}

#[tokio::test]
async fn propagates_third_put_precondition_failure() {
    let storage = TestStorage::with_script(
        b"0123456789",
        Script {
            put_failures_remaining: 3,
            ..Script::default()
        },
    );

    let err = run_patch(&storage, byte_patch(), conditionals(Some("etag1")), 1024)
        .await
        .expect_err("third precondition failure propagates");

    assert!(matches!(
        err,
        WriteError::Storage(StorageError::PreconditionFailed)
    ));
    let calls = storage.calls();
    assert_eq!(calls.head, 3);
    assert_eq!(calls.get, 3);
    assert_eq!(calls.put, 3);
}

#[tokio::test]
async fn retries_two_get_precondition_failures_then_succeeds() {
    let storage = TestStorage::with_script(
        b"0123456789",
        Script {
            get_failures_remaining: 2,
            ..Script::default()
        },
    );

    run_patch(&storage, byte_patch(), conditionals(Some("etag1")), 1024)
        .await
        .expect("third attempt succeeds");

    assert_eq!(storage.body(), Bytes::from_static(b"xy23456789"));
    let calls = storage.calls();
    assert_eq!(calls.head, 3);
    assert_eq!(calls.get, 3);
    assert_eq!(calls.put, 1);
}

#[tokio::test]
async fn stale_caller_precondition_is_not_retried() {
    let storage = TestStorage::with_script(b"0123456789", Script::default());

    let err = run_patch(&storage, byte_patch(), conditionals(Some("stale")), 1024)
        .await
        .expect_err("caller precondition fails permanently");

    assert!(matches!(
        err,
        WriteError::Storage(StorageError::PreconditionFailed)
    ));
    let calls = storage.calls();
    assert_eq!(calls.head, 1);
    assert_eq!(calls.get, 0);
    assert_eq!(calls.put, 0);
}

#[tokio::test]
async fn retry_rechecks_caller_precondition_against_advanced_head_etag() {
    let storage = TestStorage::with_script(
        b"0123456789",
        Script {
            put_failures_remaining: 1,
            advance_etag_on_put_failure: true,
            ..Script::default()
        },
    );

    let err = run_patch(&storage, byte_patch(), conditionals(Some("etag1")), 1024)
        .await
        .expect_err("retry sees stale caller If-Match");

    assert!(matches!(
        err,
        WriteError::Storage(StorageError::PreconditionFailed)
    ));
    assert_eq!(storage.body(), Bytes::from_static(b"0123456789"));
    let calls = storage.calls();
    assert_eq!(calls.head, 2);
    assert_eq!(calls.get, 1);
    assert_eq!(calls.put, 1);
}

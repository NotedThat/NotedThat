use super::support::{Script, TestStorage, conditionals, run_patch};
use bytes::Bytes;
use notedthat_core::testing::compute_etag;
use notedthat_core::{ByteRange, StorageError};

use crate::{PatchMode, WriteError};

fn byte_patch() -> PatchMode {
    PatchMode::Bytes {
        range: ByteRange::FromStart { first: 0, last: 1 },
        body: Bytes::from_static(b"xy"),
    }
}

fn append() -> PatchMode {
    PatchMode::Append {
        body: Bytes::from_static(b"ab"),
    }
}

#[tokio::test]
async fn succeeds_on_first_attempt_without_retry() {
    let storage = TestStorage::with_script(b"0123456789", Script::default()).await;

    let (outcome, _rx) = run_patch(&storage, byte_patch(), conditionals(Some("etag1")), 1024)
        .await
        .expect("patch succeeds");

    assert_eq!(outcome.etag, Some(compute_etag(b"xy23456789")));
    assert_eq!(storage.body().await, Bytes::from_static(b"xy23456789"));
    let calls = storage.calls();
    assert_eq!(calls.head, 1);
    assert_eq!(calls.get, 1);
    assert_eq!(calls.put, 1);
}

#[tokio::test]
async fn append_without_if_match_retries_two_put_precondition_failures() {
    let storage = TestStorage::with_script(
        b"0123456789",
        Script {
            put_failures_remaining: 2,
            ..Script::default()
        },
    )
    .await;

    run_patch(&storage, append(), conditionals(None), 1024)
        .await
        .expect("third attempt succeeds");

    assert_eq!(storage.body().await, Bytes::from_static(b"0123456789ab"));
    let calls = storage.calls();
    assert_eq!(calls.head, 3);
    assert_eq!(calls.get, 3);
    assert_eq!(calls.put, 3);
}

#[tokio::test]
async fn append_without_if_match_retry_anchors_on_a_fresh_head() {
    // A concurrent writer moves the ETag to `etag2` as the first PUT fails; a
    // retry that reused `etag1` would 412 on every later GET and PUT.
    let storage = TestStorage::with_script(
        b"0123456789",
        Script {
            put_failures_remaining: 1,
            advance_etag_on_put_failure: true,
            ..Script::default()
        },
    )
    .await;

    run_patch(&storage, append(), conditionals(None), 1024)
        .await
        .expect("second attempt succeeds against the new ETag");

    assert_eq!(storage.body().await, Bytes::from_static(b"0123456789ab"));
    let calls = storage.calls();
    assert_eq!(calls.head, 2);
    assert_eq!(calls.get, 2);
    assert_eq!(calls.put, 2);
}

#[tokio::test]
async fn append_without_if_match_propagates_third_put_precondition_failure() {
    let storage = TestStorage::with_script(
        b"0123456789",
        Script {
            put_failures_remaining: 3,
            ..Script::default()
        },
    )
    .await;

    let err = run_patch(&storage, append(), conditionals(None), 1024)
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
async fn append_without_if_match_retries_two_get_precondition_failures() {
    let storage = TestStorage::with_script(
        b"0123456789",
        Script {
            get_failures_remaining: 2,
            ..Script::default()
        },
    )
    .await;

    run_patch(&storage, append(), conditionals(None), 1024)
        .await
        .expect("third attempt succeeds");

    assert_eq!(storage.body().await, Bytes::from_static(b"0123456789ab"));
    let calls = storage.calls();
    assert_eq!(calls.head, 3);
    assert_eq!(calls.get, 3);
    assert_eq!(calls.put, 1);
}

#[tokio::test]
async fn stale_caller_precondition_is_not_retried() {
    for mode in [byte_patch(), append()] {
        let storage = TestStorage::with_script(b"0123456789", Script::default()).await;

        let err = run_patch(&storage, mode, conditionals(Some("stale")), 1024)
            .await
            .expect_err("caller precondition fails permanently");

        assert!(matches!(
            err,
            WriteError::Storage(StorageError::PreconditionFailed)
        ));
        assert_eq!(storage.body().await, Bytes::from_static(b"0123456789"));
        let calls = storage.calls();
        assert_eq!(calls.head, 1);
        assert_eq!(calls.get, 0);
        assert_eq!(calls.put, 0);
    }
}

#[tokio::test]
async fn put_precondition_failure_with_caller_if_match_is_not_retried() {
    for mode in [byte_patch(), append()] {
        let storage = TestStorage::with_script(
            b"0123456789",
            Script {
                put_failures_remaining: 1,
                ..Script::default()
            },
        )
        .await;

        let err = run_patch(&storage, mode, conditionals(Some("etag1")), 1024)
            .await
            .expect_err("a caller If-Match makes the PUT 412 final");

        assert!(matches!(
            err,
            WriteError::Storage(StorageError::PreconditionFailed)
        ));
        assert_eq!(storage.body().await, Bytes::from_static(b"0123456789"));
        let calls = storage.calls();
        assert_eq!(calls.head, 1);
        assert_eq!(calls.get, 1);
        assert_eq!(calls.put, 1);
    }
}

#[tokio::test]
async fn get_precondition_failure_with_caller_if_match_is_not_retried() {
    let storage = TestStorage::with_script(
        b"0123456789",
        Script {
            get_failures_remaining: 1,
            ..Script::default()
        },
    )
    .await;

    let err = run_patch(&storage, byte_patch(), conditionals(Some("etag1")), 1024)
        .await
        .expect_err("a caller If-Match makes the GET 412 final");

    assert!(matches!(
        err,
        WriteError::Storage(StorageError::PreconditionFailed)
    ));
    assert_eq!(storage.body().await, Bytes::from_static(b"0123456789"));
    let calls = storage.calls();
    assert_eq!(calls.head, 1);
    assert_eq!(calls.get, 1);
    assert_eq!(calls.put, 0);
}

use super::support::{TestStorage, conditionals, run_patch};
use bytes::Bytes;
use notedthat_core::{ByteRange, LineRange, StorageError};

use crate::{PatchMode, WriteError};

#[tokio::test]
async fn replaces_byte_range_and_enqueues_event() {
    let storage = TestStorage::with_body(b"0123456789TAIL");

    let (_outcome, mut rx) = run_patch(
        &storage,
        PatchMode::Bytes {
            range: ByteRange::FromStart { first: 0, last: 9 },
            body: Bytes::from_static(b"REPLACED"),
        },
        conditionals(Some("etag1")),
        1024,
    )
    .await
    .expect("byte patch succeeds");

    assert_eq!(storage.body(), Bytes::from_static(b"REPLACEDTAIL"));
    let event = rx.recv().await.expect("index event enqueued");
    assert_eq!(event.kb().as_str(), "test-kb");
    assert_eq!(event.object_key().as_str(), "test.md");
}

#[tokio::test]
async fn replaces_line_range() {
    let storage = TestStorage::with_body(b"one\ntwo\nthree\nfour\nfive\n");

    run_patch(
        &storage,
        PatchMode::Lines {
            range: LineRange::FromStart { first: 2, last: 3 },
            body: Bytes::from_static(b"TWO\nTHREE\n"),
        },
        conditionals(Some("etag1")),
        1024,
    )
    .await
    .expect("line patch succeeds");

    assert_eq!(
        storage.body(),
        Bytes::from_static(b"one\nTWO\nTHREE\nfour\nfive\n")
    );
}

#[tokio::test]
async fn inserts_before_line() {
    let storage = TestStorage::with_body(b"one\ntwo\nthree\nfour\nfive\n");

    run_patch(
        &storage,
        PatchMode::Lines {
            range: LineRange::Insert { before: 3 },
            body: Bytes::from_static(b"inserted\n"),
        },
        conditionals(Some("etag1")),
        1024,
    )
    .await
    .expect("line insert succeeds");

    assert_eq!(
        storage.body(),
        Bytes::from_static(b"one\ntwo\ninserted\nthree\nfour\nfive\n")
    );
}

#[tokio::test]
async fn appends_with_caller_if_match() {
    let storage = TestStorage::with_body(b"base");

    run_patch(
        &storage,
        PatchMode::Append {
            body: Bytes::from_static(b"appended"),
        },
        conditionals(Some("etag1")),
        1024,
    )
    .await
    .expect("append succeeds");

    assert_eq!(storage.body(), Bytes::from_static(b"baseappended"));
}

#[tokio::test]
async fn appends_without_caller_if_match() {
    let storage = TestStorage::with_body(b"base");

    run_patch(
        &storage,
        PatchMode::Append {
            body: Bytes::from_static(b"appended"),
        },
        conditionals(None),
        1024,
    )
    .await
    .expect("append without caller If-Match succeeds");

    assert_eq!(storage.body(), Bytes::from_static(b"baseappended"));
}

#[tokio::test]
async fn byte_patch_without_if_match_is_invalid() {
    let storage = TestStorage::with_body(b"base");

    let err = run_patch(
        &storage,
        PatchMode::Bytes {
            range: ByteRange::FromStart { first: 0, last: 1 },
            body: Bytes::from_static(b"xx"),
        },
        conditionals(None),
        1024,
    )
    .await
    .expect_err("missing If-Match fails");

    assert!(matches!(err, WriteError::PatchInvalidRange { .. }));
}

#[tokio::test]
async fn stale_caller_if_match_is_precondition_failed() {
    let storage = TestStorage::with_body(b"base");

    let err = run_patch(
        &storage,
        PatchMode::Bytes {
            range: ByteRange::FromStart { first: 0, last: 1 },
            body: Bytes::from_static(b"xx"),
        },
        conditionals(Some("stale-etag")),
        1024,
    )
    .await
    .expect_err("stale If-Match fails");

    assert!(matches!(
        err,
        WriteError::Storage(StorageError::PreconditionFailed)
    ));
}

#[tokio::test]
async fn star_if_match_is_invalid() {
    let storage = TestStorage::with_body(b"base");

    let err = run_patch(
        &storage,
        PatchMode::Bytes {
            range: ByteRange::FromStart { first: 0, last: 1 },
            body: Bytes::from_static(b"xx"),
        },
        conditionals(Some("*")),
        1024,
    )
    .await
    .expect_err("star If-Match fails");

    assert!(matches!(err, WriteError::PatchInvalidRange { .. }));
}

#[tokio::test]
async fn pre_splice_size_gate_rejects_without_mutation() {
    let storage = TestStorage::with_body(b"too-large");

    let err = run_patch(
        &storage,
        PatchMode::Bytes {
            range: ByteRange::FromStart { first: 0, last: 2 },
            body: Bytes::from_static(b"ok"),
        },
        conditionals(Some("etag1")),
        1,
    )
    .await
    .expect_err("oversized object fails");

    assert!(matches!(err, WriteError::PatchTooLarge { .. }));
    assert_eq!(storage.body(), Bytes::from_static(b"too-large"));
}

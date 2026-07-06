use bytes::Bytes;

use super::support::{
    ReplaceArgs, TestStorage, conditionals, expect_replace_err, run_replace, run_replace_with,
};

#[tokio::test]
async fn single_match_happy_replaces_once() {
    let storage = TestStorage::with_body(b"hello world");
    let before = storage.read().await;
    let outcome = run_replace(&storage, ReplaceArgs::one("world", "planet"))
        .await
        .expect("single match replace succeeds");

    assert_eq!(outcome.match_count, 1);
    let after = storage.read().await;
    assert_eq!(after.bytes, Bytes::from_static(b"hello planet"));
    assert_ne!(after.meta.etag, before.meta.etag);
}

#[tokio::test]
async fn replace_all_true_across_three_non_overlapping_matches() {
    let storage = TestStorage::with_body(b"a b a b a");
    let outcome = run_replace(&storage, ReplaceArgs::all("a", "Z"))
        .await
        .expect("replace_all succeeds");

    assert_eq!(outcome.match_count, 3);
    assert_eq!(storage.read().await.bytes, Bytes::from_static(b"Z b Z b Z"));
}

#[tokio::test]
async fn zero_matches_returns_replace_no_match_and_leaves_storage_untouched() {
    let storage = TestStorage::with_body(b"foo");
    let before = storage.read().await;
    let err = expect_replace_err(run_replace(&storage, ReplaceArgs::one("bar", "baz")).await);

    assert!(matches!(err, crate::WriteError::ReplaceNoMatch));
    let after = storage.read().await;
    assert_eq!(after.bytes, before.bytes);
    assert_eq!(after.meta.etag, before.meta.etag);
}

#[tokio::test]
async fn multiple_matches_with_replace_all_false_returns_ambiguous_with_count() {
    let storage = TestStorage::with_body(b"a b a");
    let before = storage.read().await;
    let err = expect_replace_err(run_replace(&storage, ReplaceArgs::one("a", "Z")).await);

    assert!(matches!(
        err,
        crate::WriteError::ReplaceAmbiguous { count: 2 }
    ));
    let after = storage.read().await;
    assert_eq!(after.bytes, before.bytes);
    assert_eq!(after.meta.etag, before.meta.etag);
}

#[tokio::test]
async fn empty_new_string_deletes_in_place() {
    let storage = TestStorage::with_body(b"prefix_MID_suffix");
    let outcome = run_replace(&storage, ReplaceArgs::one("MID", ""))
        .await
        .expect("empty replacement succeeds");

    assert_eq!(outcome.match_count, 1);
    assert_eq!(
        storage.read().await.bytes,
        Bytes::from_static(b"prefix__suffix")
    );
}

#[tokio::test]
async fn crlf_line_ending_matches_exact_bytes_not_bare_newline() {
    let storage = TestStorage::with_body(b"a\r\nb");
    let crlf_outcome = run_replace(&storage, ReplaceArgs::one("\r\n", "|"))
        .await
        .expect("CRLF byte substring matches once");
    assert_eq!(crlf_outcome.match_count, 1);
    assert_eq!(storage.read().await.bytes, Bytes::from_static(b"a|b"));

    let storage = TestStorage::with_body(b"a\r\nb");
    let lf_outcome = run_replace(&storage, ReplaceArgs::one("\n", "|"))
        .await
        .expect("bare LF byte inside CRLF matches once");
    assert_eq!(lf_outcome.match_count, 1);
    assert_eq!(storage.read().await.bytes, Bytes::from_static(b"a\r|b"));
}

#[tokio::test]
async fn multi_byte_utf8_old_string_matches_without_splitting_codepoints() {
    let storage = TestStorage::with_body("café".as_bytes());
    let outcome = run_replace(&storage, ReplaceArgs::one("café", "cafe"))
        .await
        .expect("multi-byte UTF-8 old string matches");

    assert_eq!(outcome.match_count, 1);
    assert_eq!(storage.read().await.bytes, Bytes::from_static(b"cafe"));
}

#[tokio::test]
async fn post_splice_size_over_max_patchable_returns_patch_too_large() {
    let storage = TestStorage::with_body(b"hello world xes");
    let before = storage.read().await;
    let err = expect_replace_err(
        run_replace_with(
            &storage,
            ReplaceArgs::one("x", "ABCDEFGHIJKL"),
            conditionals(Some("etag1")),
            20,
            None,
        )
        .await,
    );

    assert!(matches!(
        err,
        crate::WriteError::PatchTooLarge {
            size: 26,
            limit: 20
        }
    ));
    let after = storage.read().await;
    assert_eq!(after.bytes, before.bytes);
    assert_eq!(after.meta.etag, before.meta.etag);
}

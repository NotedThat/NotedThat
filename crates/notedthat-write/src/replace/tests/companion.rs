use bytes::Bytes;

use super::support::{
    ReplaceArgs, TestStorage, conditionals, expect_replace_err, run_replace, run_replace_with,
};

#[tokio::test]
async fn replace_all_true_with_zero_matches_returns_no_match() {
    let storage = TestStorage::with_body(b"foo");
    let err = expect_replace_err(run_replace(&storage, ReplaceArgs::all("bar", "baz")).await);

    assert!(matches!(err, crate::WriteError::ReplaceNoMatch));
    assert_eq!(storage.body(), Bytes::from_static(b"foo"));
}

#[tokio::test]
async fn ambiguous_match_fires_at_exactly_two_matches() {
    let storage = TestStorage::with_body(b"aa");
    let err = expect_replace_err(run_replace(&storage, ReplaceArgs::one("a", "Z")).await);

    assert!(matches!(
        err,
        crate::WriteError::ReplaceAmbiguous { count: 2 }
    ));
    assert_eq!(storage.body(), Bytes::from_static(b"aa"));
}

#[tokio::test]
async fn nul_byte_in_old_string_matches_correctly() {
    let storage = TestStorage::with_body(b"a\0b");
    let outcome = run_replace(&storage, ReplaceArgs::one("\0", "X"))
        .await
        .expect("NUL byte old_string matches");

    assert_eq!(outcome.match_count, 1);
    assert_eq!(storage.body(), Bytes::from_static(b"aXb"));
}

#[tokio::test]
async fn content_type_preserved_from_get_response() {
    let storage = TestStorage::with_body_and_content_type(b"hello world", "text/markdown");
    run_replace(&storage, ReplaceArgs::one("world", "planet"))
        .await
        .expect("replace preserves GET content type when caller omits one");

    let after = storage.read().await;
    assert_eq!(after.meta.content_type.as_deref(), Some("text/markdown"));
    assert_eq!(after.bytes, Bytes::from_static(b"hello planet"));
}

#[tokio::test]
async fn single_pass_splice_o_n_scaling() {
    let mut body = vec![b'a'; 1_048_576];
    for index in 0..1000 {
        body[index * 1024] = b'x';
    }
    let storage = TestStorage::with_bytes(Bytes::from(body), Some("text/plain"));

    let outcome = run_replace_with(
        &storage,
        ReplaceArgs::all("x", "Y"),
        conditionals(Some("etag1")),
        2_097_152,
        None,
    )
    .await
    .expect("large replace_all completes in one pass");

    let after = storage.body();
    assert_eq!(outcome.match_count, 1000);
    assert!(!after.contains(&b'x'));
    let replacement_count = after
        .iter()
        .fold(0usize, |count, byte| count + usize::from(*byte == b'Y'));
    assert_eq!(replacement_count, 1000);
}

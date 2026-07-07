//! E2E integration tests for replace against `SeaweedFS` + Qdrant testcontainers.
//!
//! Requires Docker. Run with:
//! ```sh
//! cargo test -p notedthat-server --locked --test it_replace -- --include-ignored
//! ```
#![allow(missing_docs)]

#[path = "support/patch_env.rs"]
mod patch_env;

use patch_env::{assert_replace_success, PatchServer};

const NORMAL_MAX_PATCHABLE_SIZE: u64 = 10 * 1024 * 1024;

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn single_match_replace_updates_content_and_advances_etag() {
    // Given: a simple object with a known ETag.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let first_etag = server.put_text("single.md", "hello world").await;

    // When: a single-match replace is issued with If-Match precondition.
    let response = server
        .replace_json("single.md", &first_etag, "world", "planet", false)
        .await;

    // Then: the match is replaced, the ETag advances, and the content is updated.
    let second_etag = assert_replace_success(response, 1).await;
    assert_ne!(second_etag, first_etag);
    assert_eq!(server.get_text("single.md").await, "hello planet");
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn replace_all_across_multiple_occurrences_completes_in_one_request() {
    // Given: an object with multiple occurrences of the target string.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let first_etag = server.put_text("replace-all.md", "a b a b a").await;

    // When: replace_all is requested with If-Match precondition.
    let response = server
        .replace_json("replace-all.md", &first_etag, "a", "Z", true)
        .await;

    // Then: all three occurrences are replaced in one request.
    let second_etag = assert_replace_success(response, 3).await;
    assert_ne!(second_etag, first_etag);
    assert_eq!(server.get_text("replace-all.md").await, "Z b Z b Z");
}

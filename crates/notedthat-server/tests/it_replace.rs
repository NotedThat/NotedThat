//! E2E integration tests for replace against `SeaweedFS` + Qdrant testcontainers.
//!
//! Requires Docker. Run with:
//! ```sh
//! cargo test -p notedthat-server --locked --test it_replace -- --include-ignored
//! ```
#![allow(missing_docs)]

#[path = "support/patch_env.rs"]
mod patch_env;

use patch_env::{PatchServer, assert_replace_success, assert_error_code, API_TOKEN};
use reqwest::StatusCode;

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

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn no_match_returns_422_and_leaves_storage_untouched() {
    // Given: an object with known content and ETag.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let first_etag = server.put_text("nomatch.md", "foo").await;

    // When: a replace is issued with a string that does not match.
    let response = server
        .replace_json("nomatch.md", &first_etag, "bar", "x", false)
        .await;

    // Then: the request returns 422 with error code "no_match", and content is unchanged.
    assert_error_code(response, StatusCode::UNPROCESSABLE_ENTITY, "no_match").await;
    assert_eq!(server.get_text("nomatch.md").await, "foo");
    assert_eq!(server.head_text_status("nomatch.md").await, StatusCode::OK);
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn ambiguous_match_returns_422_with_count_and_leaves_storage_untouched() {
    // Given: an object with multiple occurrences of the target string.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let first_etag = server.put_text("ambiguous.md", "a b a").await;

    // When: a single-match replace is issued (replace_all=false) with ambiguous content.
    let response = server
        .replace_json("ambiguous.md", &first_etag, "a", "Z", false)
        .await;

    // Then: the request returns 422 with error code "ambiguous_match" and match_count=2.
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let json = response
        .json::<serde_json::Value>()
        .await
        .expect("error response should be JSON");
    assert_eq!(json["error"], "ambiguous_match");
    assert_eq!(json["match_count"].as_u64(), Some(2));
    assert_eq!(server.get_text("ambiguous.md").await, "a b a");
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn replace_all_true_with_zero_matches_still_returns_no_match() {
    // Given: an object with known content and ETag.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let first_etag = server.put_text("replace-all-zero.md", "foo").await;

    // When: a replace_all is issued with a string that does not match.
    let response = server
        .replace_json("replace-all-zero.md", &first_etag, "bar", "x", true)
        .await;

    // Then: the request returns 422 with error code "no_match", and content is unchanged.
    assert_error_code(response, StatusCode::UNPROCESSABLE_ENTITY, "no_match").await;
    assert_eq!(server.get_text("replace-all-zero.md").await, "foo");
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn replace_on_nonexistent_path_returns_404() {
    // Given: a server with no object at the target path.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;

    // When: a replace is issued to a path that does not exist.
    let url = format!(
        "{}/v1/knowledgebases/{}/replace/never-existed.md",
        server.base_url, server.kb
    );
    let response = server
        .client
        .post(url)
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Content-Type", "application/json")
        .header("If-Match", "\"anything\"")
        .body(r#"{"old_string":"x","new_string":"y"}"#)
        .send()
        .await
        .expect("replace request");

    // Then: the request returns 404 with error code "not_found".
    assert_error_code(response, StatusCode::NOT_FOUND, "not_found").await;
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn if_match_star_on_replace_returns_400_invalid_request() {
    // Given: an object with known content and ETag.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let _first_etag = server.put_text("ifmatch-star.md", "foo").await;

    // When: a replace is issued with If-Match: * (wildcard).
    let response = server
        .replace_json("ifmatch-star.md", "*", "foo", "bar", false)
        .await;

    // Then: the request returns 400 with error code "invalid_request", and content is unchanged.
    assert_error_code(response, StatusCode::BAD_REQUEST, "invalid_request").await;
    assert_eq!(server.get_text("ifmatch-star.md").await, "foo");
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn if_match_multi_value_on_replace_returns_400_invalid_request() {
    // Given: an object with known content and ETag.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let _first_etag = server.put_text("ifmatch-multi.md", "foo").await;

    // When: a replace is issued with If-Match: "a","b" (multiple values).
    let response = server
        .replace_json("ifmatch-multi.md", "\"a\",\"b\"", "foo", "bar", false)
        .await;

    // Then: the request returns 400 with error code "invalid_request", and content is unchanged.
    assert_error_code(response, StatusCode::BAD_REQUEST, "invalid_request").await;
    assert_eq!(server.get_text("ifmatch-multi.md").await, "foo");
}

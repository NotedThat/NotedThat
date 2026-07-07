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

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn concurrent_replaces_with_same_if_match_surface_conflicts() {
    // Given: three clients share one starting ETag.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let starting_etag = server.put_text("concurrent.md", "hello world").await;

    // When: all three attempt concurrent replaces with the same If-Match precondition.
    let mut handles = Vec::new();
    for suffix in ["A", "B", "C"] {
        let server_client = server.client.clone();
        let url = format!(
            "{}/v1/knowledgebases/{}/replace/concurrent.md",
            server.base_url, server.kb
        );
        let etag = starting_etag.clone();
        let payload = serde_json::json!({
            "old_string": "world",
            "new_string": format!("world-{}", suffix),
            "replace_all": false,
        });
        handles.push(tokio::spawn(async move {
            server_client
                .post(url)
                .header("Authorization", format!("Bearer {}", API_TOKEN))
                .header("Content-Type", "application/json")
                .header("If-Match", etag)
                .body(payload.to_string())
                .send()
                .await
                .expect("concurrent replace should return")
                .status()
        }));
    }

    // Then: exactly one wins, and the sustained stale preconditions surface 412s.
    let mut statuses = Vec::new();
    for h in handles {
        statuses.push(h.await.expect("task join"));
    }
    let successes = statuses
        .iter()
        .filter(|s| **s == StatusCode::OK)
        .count();
    let conflicts = statuses
        .iter()
        .filter(|s| **s == StatusCode::PRECONDITION_FAILED)
        .count();
    assert_eq!(
        successes, 1,
        "exactly one replace should succeed: {statuses:?}"
    );
    assert!(
        conflicts >= 2,
        "at least two replaces should fail with 412: {statuses:?}"
    );
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn concurrent_replace_and_byte_patch_with_same_if_match_surface_conflicts() {
    // Given: one starting ETag shared by PATCH and replace clients.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let starting_etag = server.put_text("cross.md", "hello world").await;

    // When: a PATCH and a replace race with the same If-Match precondition.
    let patch_url = format!(
        "{}/v1/knowledgebases/{}/cross.md",
        server.base_url, server.kb
    );
    let patch_client = server.client.clone();
    let patch_etag = starting_etag.clone();
    let patch_handle = tokio::spawn(async move {
        patch_client
            .patch(patch_url)
            .header("Authorization", format!("Bearer {}", API_TOKEN))
            .header("Content-Range", "bytes 6-10/*")
            .header("If-Match", patch_etag)
            .body("PLNTS")
            .send()
            .await
            .expect("patch should return")
            .status()
    });

    let replace_url = format!(
        "{}/v1/knowledgebases/{}/replace/cross.md",
        server.base_url, server.kb
    );
    let replace_client = server.client.clone();
    let replace_etag = starting_etag.clone();
    let replace_handle = tokio::spawn(async move {
        replace_client
            .post(replace_url)
            .header("Authorization", format!("Bearer {}", API_TOKEN))
            .header("Content-Type", "application/json")
            .header("If-Match", replace_etag)
            .body(
                serde_json::json!({
                    "old_string": "world",
                    "new_string": "planet"
                })
                .to_string(),
            )
            .send()
            .await
            .expect("replace should return")
            .status()
    });

    let patch_status = patch_handle.await.expect("patch join");
    let replace_status = replace_handle.await.expect("replace join");

    // Then: exactly one succeeds, the other fails with 412, and the final body is one of the two intended writes.
    let successes = [patch_status, replace_status]
        .iter()
        .filter(|s| **s == StatusCode::OK)
        .count();
    let conflicts = [patch_status, replace_status]
        .iter()
        .filter(|s| **s == StatusCode::PRECONDITION_FAILED)
        .count();
    assert_eq!(
        successes, 1,
        "exactly one of PATCH/replace should win: patch={patch_status:?}, replace={replace_status:?}"
    );
    assert_eq!(
        conflicts, 1,
        "the other must fail with 412: patch={patch_status:?}, replace={replace_status:?}"
    );

    let final_body = server.get_text("cross.md").await;
    assert!(
        final_body == "hello PLNTS" || final_body == "hello planet",
        "final body must be one of the two intended writes, got {final_body:?}"
    );
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn concurrent_replace_and_delete_with_same_if_match_never_lose_writes_silently() {
    // Given: one starting ETag shared by delete and replace clients.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let starting_etag = server.put_text("racy.md", "hello world").await;

    // When: a DELETE and a replace race with the same If-Match precondition.
    let delete_url = format!(
        "{}/v1/knowledgebases/{}/racy.md",
        server.base_url, server.kb
    );
    let delete_client = server.client.clone();
    let delete_etag = starting_etag.clone();
    let delete_handle = tokio::spawn(async move {
        delete_client
            .delete(delete_url)
            .header("Authorization", format!("Bearer {}", API_TOKEN))
            .header("If-Match", delete_etag)
            .send()
            .await
            .expect("delete should return")
            .status()
    });

    let replace_url = format!(
        "{}/v1/knowledgebases/{}/replace/racy.md",
        server.base_url, server.kb
    );
    let replace_client = server.client.clone();
    let replace_etag = starting_etag.clone();
    let replace_handle = tokio::spawn(async move {
        replace_client
            .post(replace_url)
            .header("Authorization", format!("Bearer {}", API_TOKEN))
            .header("Content-Type", "application/json")
            .header("If-Match", replace_etag)
            .body(
                serde_json::json!({
                    "old_string": "world",
                    "new_string": "planet"
                })
                .to_string(),
            )
            .send()
            .await
            .expect("replace should return")
            .status()
    });

    let delete_status = delete_handle.await.expect("delete join");
    let replace_status = replace_handle.await.expect("replace join");

    // Then: exactly one operation succeeds, and the final state is consistent (no silent data loss).
    match server.head_text_status("racy.md").await {
        StatusCode::OK => {
            assert_eq!(
                replace_status, StatusCode::OK,
                "object still present but replace didn't win: replace={replace_status:?}, delete={delete_status:?}"
            );
            assert!(
                matches!(delete_status, StatusCode::PRECONDITION_FAILED | StatusCode::NOT_FOUND),
                "object exists so delete must have failed with 412 or 404: delete={delete_status:?}"
            );
            assert_eq!(server.get_text("racy.md").await, "hello planet");
        }
        StatusCode::NOT_FOUND => {
            assert_eq!(
                delete_status, StatusCode::NO_CONTENT,
                "object gone but delete didn't return 204: delete={delete_status:?}, replace={replace_status:?}"
            );
            assert!(
                matches!(replace_status, StatusCode::PRECONDITION_FAILED | StatusCode::NOT_FOUND),
                "object gone so replace must have failed with 412 or 404: replace={replace_status:?}"
            );
        }
        other => panic!(
            "unexpected HEAD status after concurrent replace+delete: {other:?}"
        ),
    }
}

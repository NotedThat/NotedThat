//! E2E integration tests for PATCH against `SeaweedFS` + Qdrant testcontainers.
//!
//! Requires Docker. Run with:
//! ```sh
//! cargo test -p notedthat-server --locked --test it_patch -- --include-ignored
//! ```
#![allow(missing_docs)]

#[path = "support/patch_env.rs"]
mod patch_env;

use patch_env::{API_TOKEN, PatchServer, assert_error_code, etag, mcp_call_tool, mcp_request};
use reqwest::StatusCode;

const NORMAL_MAX_PATCHABLE_SIZE: u64 = 10 * 1024 * 1024;

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn bytes_mode_patch_replaces_requested_byte_span() {
    // Given: a 100-byte object and its current ETag.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let original = "0123456789".repeat(10);
    let first_etag = server.put_text("bytes.md", &original).await;

    // When: bytes 0-9 are patched under the matching If-Match precondition.
    let response = server
        .patch_content_range("bytes.md", "bytes 0-9/*", Some(&first_etag), "ABCDEFGHIJ")
        .await;

    // Then: the body is spliced and the ETag advances.
    assert_eq!(response.status(), StatusCode::OK);
    let patched_etag = etag(&response);
    assert_ne!(patched_etag, first_etag);
    assert_eq!(
        server.get_text("bytes.md").await,
        format!("ABCDEFGHIJ{}", &original[10..])
    );
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn lines_mode_patch_replaces_inclusive_line_span() {
    // Given: a five-line object and its current ETag.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let first_etag = server
        .put_text("lines.md", "one\ntwo\nthree\nfour\nfive\n")
        .await;

    // When: lines 2-3 are patched under the matching If-Match precondition.
    let response = server
        .patch_content_range("lines.md", "lines 2-3/*", Some(&first_etag), "TWO\nTHREE\n")
        .await;

    // Then: only the selected line span is replaced.
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        server.get_text("lines.md").await,
        "one\nTWO\nTHREE\nfour\nfive\n"
    );
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn append_with_explicit_if_match_grows_object() {
    // Given: an object with a caller-visible ETag.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let head_etag = server.put_text("append-if-match.md", "alpha\n").await;

    // When: append mode is requested with that ETag.
    let response = server
        .patch_append("append-if-match.md", Some(&head_etag), "beta\n")
        .await;

    // Then: the object grows by the appended content.
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(server.get_text("append-if-match.md").await, "alpha\nbeta\n");
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn append_without_if_match_grows_object_via_http() {
    // Given: an existing object.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    server.put_text("append-no-if-match.md", "alpha\n").await;

    // When: append mode is requested without If-Match.
    let response = server
        .patch_append("append-no-if-match.md", None, "beta\n")
        .await;

    // Then: the server obtains the ETag internally and appends in one HTTP PATCH.
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        server.get_text("append-no-if-match.md").await,
        "alpha\nbeta\n"
    );
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn mcp_append_without_if_match_grows_object() {
    // Given: the HTTP MCP endpoint is available for an existing object.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    server.put_text("mcp-append.md", "alpha\n").await;
    let initialize = mcp_request(
        &server.client,
        &server.mcp_url,
        0,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "notedthat-patch-e2e", "version": "0" }
        }),
    )
    .await;
    assert!(
        initialize.get("result").is_some(),
        "initialize failed: {initialize}"
    );

    // When: the MCP append tool omits if_match.
    let tool_response = mcp_call_tool(
        &server.client,
        &server.mcp_url,
        1,
        "append",
        serde_json::json!({
            "kb": server.kb,
            "path": "mcp-append.md",
            "content": "beta\n"
        }),
    )
    .await;

    // Then: the tool succeeds and the object grows. The MCP client's single-PATCH/no-HEAD
    // wire contract is covered by notedthat-mcp append unit tests; this E2E verifies it through
    // the real HTTP MCP surface backed by SeaweedFS and Qdrant.
    assert!(
        tool_response.get("result").is_some(),
        "append failed: {tool_response}"
    );
    assert_eq!(server.get_text("mcp-append.md").await, "alpha\nbeta\n");
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn sequential_patch_uses_advanced_etag_for_second_write() {
    // Given: an object and its initial ETag.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let first_etag = server.put_text("sequential.md", "one\ntwo\nthree\n").await;

    // When: two patches are issued in sequence, with the second using the first patch's ETag.
    let first_response = server
        .patch_content_range("sequential.md", "lines 1-1/*", Some(&first_etag), "ONE\n")
        .await;
    assert_eq!(first_response.status(), StatusCode::OK);
    let second_etag = etag(&first_response);
    let second_response = server
        .patch_content_range("sequential.md", "lines 2-2/*", Some(&second_etag), "TWO\n")
        .await;

    // Then: both writes land and the second ETag also advances.
    assert_eq!(second_response.status(), StatusCode::OK);
    assert_ne!(etag(&second_response), second_etag);
    assert_eq!(server.get_text("sequential.md").await, "ONE\nTWO\nthree\n");
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn concurrent_patches_with_same_if_match_surface_conflicts() {
    // Given: three clients share one starting ETag.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let starting_etag = server.put_text("conflict.md", "one\ntwo\nthree\n").await;
    let url = server.object_url("conflict.md");

    // When: all three attempt the same conditional line patch concurrently.
    let mut handles = Vec::new();
    for payload in ["winner-a\n", "winner-b\n", "winner-c\n"] {
        let client = server.client.clone();
        let url = url.clone();
        let etag = starting_etag.clone();
        handles.push(tokio::spawn(async move {
            client
                .patch(url)
                .header("Authorization", format!("Bearer {API_TOKEN}"))
                .header("Content-Range", "lines 1-1/*")
                .header("If-Match", etag)
                .body(payload.to_owned())
                .send()
                .await
                .expect("concurrent PATCH should return")
                .status()
        }));
    }

    // Then: exactly one wins, and the sustained stale preconditions surface 412s.
    let mut statuses = Vec::new();
    for handle in handles {
        statuses.push(handle.await.expect("PATCH task should join"));
    }
    let successes = statuses
        .iter()
        .filter(|status| **status == StatusCode::OK)
        .count();
    let conflicts = statuses
        .iter()
        .filter(|status| **status == StatusCode::PRECONDITION_FAILED)
        .count();
    assert_eq!(
        successes, 1,
        "exactly one PATCH should succeed: {statuses:?}"
    );
    assert!(
        conflicts >= 2,
        "at least two PATCHes should fail with 412: {statuses:?}"
    );
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn patch_rejects_object_larger_than_max_patchable_size_before_splice() {
    // Given: an object already larger than the configured patch cap.
    let server = PatchServer::start(10).await;
    server.put_text("pre-cap.md", "already too large").await;

    // When: any patch mode tries to edit that object.
    let response = server.patch_append("pre-cap.md", None, "!").await;

    // Then: the server rejects the pre-splice size.
    assert_error_code(response, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large").await;
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn patch_rejects_result_larger_than_max_patchable_size_after_splice() {
    // Given: a 15-byte object under a 20-byte patch cap.
    let server = PatchServer::start(20).await;
    server.put_text("post-cap.md", "123456789012345").await;

    // When: append mode would produce a 25-byte object.
    let response = server.patch_append("post-cap.md", None, "ABCDEFGHIJ").await;

    // Then: the server rejects the post-splice result.
    assert_error_code(response, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large").await;
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn bytes_patch_without_if_match_returns_invalid_request() {
    // Given: an existing object.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    server.put_text("missing-if-match.md", "0123456789").await;

    // When: bytes mode omits If-Match.
    let response = server
        .patch_content_range("missing-if-match.md", "bytes 0-1/*", None, "AB")
        .await;

    // Then: the request is rejected before mutation.
    assert_error_code(response, StatusCode::BAD_REQUEST, "invalid_request").await;
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn if_match_star_on_patch_returns_invalid_request() {
    // Given: an existing object.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    server.put_text("star.md", "0123456789").await;

    // When: PATCH uses If-Match: *.
    let response = server
        .patch_content_range("star.md", "bytes 0-1/*", Some("*"), "AB")
        .await;

    // Then: the request is rejected as invalid.
    assert_error_code(response, StatusCode::BAD_REQUEST, "invalid_request").await;
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn line_patch_out_of_range_returns_dual_416_headers_and_empty_body() {
    // Given: a five-line object with a known ETag.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let body = "one\ntwo\nthree\nfour\nfive\n";
    let first_etag = server.put_text("out-of-range.md", body).await;

    // When: the requested line span starts beyond EOF.
    let response = server
        .patch_content_range(
            "out-of-range.md",
            "lines 100-200/*",
            Some(&first_etag),
            "replacement\n",
        )
        .await;

    // Then: both range spaces are reported and the response body is empty.
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()["content-range"], "lines */5");
    assert_eq!(
        response.headers()["x-content-range-bytes"],
        format!("*/{}", body.len())
    );
    let error_body = response.text().await.expect("416 body should read");
    assert!(
        error_body.is_empty(),
        "416 body should be empty, got {error_body:?}"
    );
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn patch_persists_body_when_indexer_accepts_event() {
    // Given: a real server whose indexer queue accepts events.
    let server = PatchServer::start(NORMAL_MAX_PATCHABLE_SIZE).await;
    let first_etag = server.put_text("ghost-state.md", "alpha\n").await;

    // When: PATCH succeeds.
    let response = server
        .patch_append("ghost-state.md", Some(&first_etag), "beta\n")
        .await;

    // Then: the storage state is not ghosted; the patched bytes are observable via GET.
    // Full ghost-state (503 indexer backpressure) is tested at the unit level in the HTTP handler tests.
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(server.get_text("ghost-state.md").await, "alpha\nbeta\n");
}

//! E2E integration tests for line-range GET against real `SeaweedFS` + Qdrant testcontainers.
//!
//! Requires Docker. Run with:
//! ```sh
//! cargo test -p notedthat-server --locked --test it_line_range_read -- --include-ignored
//! ```
#![allow(missing_docs)]

#[path = "support/line_range_env.rs"]
mod line_range_env;

use line_range_env::{
    API_TOKEN, LINES_1_TO_5, LINES_2_TO_4, LINES_18_TO_20, TWENTY_LINE_FIXTURE,
    assert_content_range_bytes, fixture_server,
};
use reqwest::StatusCode;

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn line_range_get_returns_206_with_correct_lines() {
    // Given: a server backed by real SeaweedFS and Qdrant contains a 20-line Markdown note.
    let server = fixture_server().await;

    // When: the note is read with an inclusive line range.
    let response = server.get_hello_with_range("lines=1-5").await;

    // Then: the HTTP response exposes the requested line slice and both range headers.
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["content-range"], "lines 1-5/20");
    assert_content_range_bytes(
        &response,
        &format!("0-{}/{}", LINES_1_TO_5.len() - 1, TWENTY_LINE_FIXTURE.len()),
    );
    let body = response.text().await.expect("line range body should read");
    assert_eq!(body, LINES_1_TO_5);
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn suffix_range_returns_last_three_lines() {
    // Given: a server backed by real SeaweedFS and Qdrant contains a 20-line Markdown note.
    let server = fixture_server().await;

    // When: the note is read with a suffix line range.
    let response = server.get_hello_with_range("lines=-3").await;

    // Then: only the final three lines are returned.
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()["content-range"], "lines 18-20/20");
    let body = response
        .text()
        .await
        .expect("suffix range body should read");
    assert_eq!(body, LINES_18_TO_20);
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn out_of_range_line_returns_416_with_dual_headers() {
    // Given: a server backed by real SeaweedFS and Qdrant contains a 20-line Markdown note.
    let server = fixture_server().await;

    // When: a line range starts beyond EOF.
    let response = server.get_hello_with_range("lines=100-200").await;

    // Then: the server rejects it with line and byte unsatisfied range headers.
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()["content-range"], "lines */20");
    assert_content_range_bytes(&response, &format!("*/{}", TWENTY_LINE_FIXTURE.len()));
    let body = response.text().await.expect("416 body should read");
    assert!(body.is_empty(), "416 body should be empty, got {body:?}");
}

#[tokio::test]
#[ignore = "requires SeaweedFS + Qdrant testcontainers"]
async fn mcp_read_line_range_returns_correct_slice() {
    // Given: MCP HTTP is enabled for a server containing a 20-line Markdown note.
    let server = fixture_server().await;

    let initialize = mcp_request(
        &server.client,
        &server.mcp_url,
        0,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "notedthat-line-range-e2e", "version": "0" }
        }),
    )
    .await;
    assert!(
        initialize.get("result").is_some(),
        "initialize should succeed before tools/call: {initialize}"
    );

    // When: the MCP read tool is invoked with line_start=2 and line_end=4.
    let response = mcp_call_tool(
        &server.client,
        &server.mcp_url,
        1,
        "read",
        serde_json::json!({
            "kb": server.kb,
            "path": "hello.md",
            "line_start": 2,
            "line_end": 4,
        }),
    )
    .await;

    // Then: the tool content is the exact requested line slice.
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("MCP read response should contain text: {response}"));
    assert_eq!(text, LINES_2_TO_4);
}

async fn mcp_request(
    client: &reqwest::Client,
    mcp_url: &str,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let response = client
        .post(mcp_url)
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("MCP HTTP request failed");
    assert!(
        response.status().is_success(),
        "MCP HTTP {method} should succeed, got {}",
        response.status()
    );
    response
        .json::<serde_json::Value>()
        .await
        .expect("MCP HTTP response must be JSON")
}

async fn mcp_call_tool(
    client: &reqwest::Client,
    mcp_url: &str,
    id: u64,
    tool_name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    mcp_request(
        client,
        mcp_url,
        id,
        "tools/call",
        serde_json::json!({
            "name": tool_name,
            "arguments": arguments,
        }),
    )
    .await
}

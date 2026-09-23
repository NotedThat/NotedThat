//! E2E integration tests for line-range GET against a server running on in-process backends.
//!
//! No Docker, and not `#[ignore]`d: these run in the ordinary pass.
//! ```sh
//! cargo test -p notedthat-server --locked --test it_line_range_read
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
async fn line_range_get_returns_206_with_correct_lines() {
    // Given: a server on in-process backends contains a 20-line Markdown note.
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
async fn suffix_range_returns_last_three_lines() {
    // Given: a server on in-process backends contains a 20-line Markdown note.
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
async fn out_of_range_line_returns_416_with_dual_headers() {
    // Given: a server on in-process backends contains a 20-line Markdown note.
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
async fn mcp_read_line_range_returns_correct_slice() {
    // Given: MCP HTTP is enabled for a server containing a 20-line Markdown note.
    let server = fixture_server().await;

    let mcp = notedthat_mcp::testing::McpSession::connect(&server.base_url, API_TOKEN);
    let initialize = mcp.initialize().await;
    assert!(
        initialize.get("result").is_some(),
        "initialize should succeed before tools/call: {initialize}"
    );

    // When: the MCP read tool is invoked with line_start=2 and line_end=4.
    let response = mcp
        .call_tool(
            1,
            "read",
            &serde_json::json!({
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

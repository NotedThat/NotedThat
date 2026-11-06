//! E2E: the streamable HTTP transport's `POST` answers, over a real TCP
//! listener with one reused client connection. No Docker, and not
//! `#[ignore]`d.
//!
//! ```sh
//! cargo test -p notedthat-server --locked --test mcp_http_e2e
//! ```
#![allow(missing_docs)]

use std::time::Duration;

use notedthat_mcp::testing::McpSession;
use notedthat_server::testing::patch_server::{API_TOKEN, PatchServer};

/// Well under rmcp's 15 s SSE keep-alive: a `POST` stream held open by it
/// would run into this long before its first heartbeat.
const ENDS_WITHIN: Duration = Duration::from_secs(2);

/// Each `POST` answer stream ends once its answer is sent, so the session's
/// next request is answered at once (#391). Only the `GET` notification leg
/// stays open.
#[tokio::test]
async fn each_post_answer_stream_ends_with_its_answer() {
    let server = PatchServer::start(10 * 1024 * 1024).await;
    let mcp = McpSession::connect(&server.base_url, API_TOKEN);
    let init = tokio::time::timeout(ENDS_WITHIN, mcp.initialize())
        .await
        .expect("the initialize stream ends with its answer");
    assert!(init.get("result").is_some(), "{init}");

    let initialized = mcp
        .post(&serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized",
        }))
        .await;
    assert_eq!(initialized.status(), reqwest::StatusCode::ACCEPTED);

    for id in 1..=3 {
        let answer = tokio::time::timeout(ENDS_WITHIN, async {
            let response = mcp
                .post(&serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "method": "tools/list",
                }))
                .await;
            McpSession::message_of(response).await
        })
        .await
        .unwrap_or_else(|_| panic!("tools/list #{id}: its stream did not end"))
        .expect("an answer frame");
        assert_eq!(answer["id"], id, "{answer}");
        assert!(answer["result"]["tools"].is_array(), "{answer}");
    }
}

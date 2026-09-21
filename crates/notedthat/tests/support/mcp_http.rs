//! An MCP client over the streamable HTTP transport, for suites that drive
//! `POST /mcp` on a real server.
//!
//! The transport is stateless JSON-response mode (D31): every request is one
//! complete JSON-RPC exchange, so a "session" here is just a base URL and a
//! credential. `initialize` is still sent where a test wants to assert on its
//! answer, but nothing depends on it having happened.

#![allow(dead_code)]

use std::time::Duration;

/// A caller of one server's `/mcp`, presenting one credential.
pub struct McpSession {
    client: reqwest::Client,
    mcp_url: String,
    token: Option<String>,
}

impl McpSession {
    /// A session for the server at `http_url`, presenting `token`.
    pub fn connect(http_url: &str, token: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            mcp_url: format!("{http_url}/mcp"),
            token: Some(token.to_string()),
        }
    }

    /// The raw HTTP answer to one JSON-RPC request, for a test about the
    /// transport's own refusals (a `401` is HTTP, not JSON-RPC).
    pub async fn send(
        &self,
        id: u64,
        method: &str,
        params: &serde_json::Value,
    ) -> reqwest::Response {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut request = self
            .client
            .post(&self.mcp_url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&body);
        if let Some(token) = &self.token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        request.send().await.expect("MCP HTTP request failed")
    }

    /// One JSON-RPC exchange; the HTTP layer must succeed and the answer must
    /// be JSON-RPC 2.0. A tool error arrives inside the JSON, as it would
    /// over any transport.
    pub async fn request(
        &self,
        id: u64,
        method: &str,
        params: &serde_json::Value,
    ) -> serde_json::Value {
        let response = self.send(id, method, params).await;
        let status = response.status();
        let text = response.text().await.expect("MCP HTTP body");
        assert!(
            status.is_success(),
            "MCP HTTP {method} should succeed, got {status}: {text}"
        );
        let value: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|_| panic!("invalid JSON: {text:?}"));
        assert_eq!(
            value.get("jsonrpc").and_then(serde_json::Value::as_str),
            Some("2.0"),
            "expected JSON-RPC 2.0: {value}"
        );
        value
    }

    /// `initialize`, with the answer.
    pub async fn initialize(&self) -> serde_json::Value {
        self.request(
            0,
            "initialize",
            &serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }),
        )
        .await
    }

    /// What a client does before its first tool call: `initialize`, then the
    /// `initialized` notification. Over the stateless transport the server
    /// keeps nothing between the two, so this only proves both are accepted.
    pub async fn session_init(&self) {
        let _ = self.initialize().await;
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        let response = self
            .client
            .post(&self.mcp_url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header(
                "Authorization",
                format!("Bearer {}", self.token.as_deref().unwrap_or_default()),
            )
            .json(&notification)
            .send()
            .await
            .expect("initialized notification");
        assert!(
            response.status().is_success(),
            "initialized notification should be accepted, got {}",
            response.status()
        );
    }

    /// `tools/call`, with the answer.
    pub async fn call_tool(
        &self,
        id: u64,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> serde_json::Value {
        self.request(
            id,
            "tools/call",
            &serde_json::json!({
                "name": tool_name,
                "arguments": args,
            }),
        )
        .await
    }
}

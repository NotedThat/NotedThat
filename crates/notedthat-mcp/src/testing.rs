//! Test support: an MCP client over the streamable HTTP transport, for every
//! suite in the workspace that drives `/mcp` on a real server.
//!
//! Behind the `test-support` feature, like `notedthat_core::testing`. The
//! client speaks the stateful transport (D66): it keeps the `Mcp-Session-Id`
//! that `initialize` returns, sends it on every later request, reads answers
//! whether they come as `application/json` or SSE-framed, and opens the `GET`
//! notification leg. It also still works against a stateless server, which is
//! what let it replace the older per-suite helpers before the transport
//! changed.

#![allow(clippy::missing_panics_doc)]

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use crate::sse::SseParser;

const SESSION_HEADER: &str = "mcp-session-id";

/// A caller of one server's `/mcp`, presenting one credential, holding at
/// most one session.
pub struct McpSession {
    client: reqwest::Client,
    /// No total timeout: the notification leg is open for as long as the test
    /// wants it.
    stream_client: reqwest::Client,
    mcp_url: String,
    token: Option<String>,
    session_id: Mutex<Option<String>>,
}

impl McpSession {
    /// A session for the server at `http_url`, presenting `token`.
    #[must_use]
    pub fn connect(http_url: &str, token: &str) -> Self {
        Self::build(http_url, Some(token.to_owned()))
    }

    /// A session presenting no credential at all — an anonymous caller must
    /// not send an empty bearer and be refused for the wrong reason.
    #[must_use]
    pub fn anonymous(http_url: &str) -> Self {
        Self::build(http_url, None)
    }

    fn build(http_url: &str, token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            stream_client: reqwest::Client::builder()
                .build()
                .expect("reqwest stream client"),
            mcp_url: format!("{}/mcp", http_url.trim_end_matches('/')),
            token,
            session_id: Mutex::new(None),
        }
    }

    /// The session id `initialize` returned, if the server issued one.
    #[must_use]
    pub fn session_id(&self) -> Option<String> {
        self.session_id.lock().expect("session id lock").clone()
    }

    /// The `/mcp` URL this session talks to.
    #[must_use]
    pub fn mcp_url(&self) -> &str {
        &self.mcp_url
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => request.header("Authorization", format!("Bearer {token}")),
            None => request,
        }
    }

    fn with_session(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.session_id() {
            Some(id) => request.header(SESSION_HEADER, id),
            None => request,
        }
    }

    /// One `POST /mcp` with the streamable-HTTP headers, the credential when
    /// the session has one, and the session id when it is known.
    pub async fn post(&self, body: &serde_json::Value) -> reqwest::Response {
        let request = self
            .client
            .post(&self.mcp_url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(body);
        self.with_session(self.authorize(request))
            .send()
            .await
            .expect("MCP HTTP request failed")
    }

    /// The raw HTTP answer to one JSON-RPC request, for a test about the
    /// transport's own refusals (a `401` is HTTP, not JSON-RPC).
    pub async fn send(
        &self,
        id: u64,
        method: &str,
        params: &serde_json::Value,
    ) -> reqwest::Response {
        self.post(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await
    }

    /// One JSON-RPC notification (no id); the raw HTTP answer.
    pub async fn notify(&self, method: &str, params: &serde_json::Value) -> reqwest::Response {
        self.post(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    /// One JSON-RPC exchange; the HTTP layer must succeed and the answer must
    /// be JSON-RPC 2.0, whether it arrives as JSON or as an SSE frame. A tool
    /// error arrives inside the JSON, as it would over any transport.
    ///
    /// A stateful server refuses anything but `initialize` without a session,
    /// so the first request of a session that was never initialized runs
    /// [`Self::session_init`] first — the older helpers never initialized, and
    /// this is what keeps their call sites unchanged.
    ///
    /// An `initialize` asked for here is [`Self::initialize`], which is the one
    /// path that captures the `Mcp-Session-Id` from the answer's headers.
    /// Sending it through [`Self::send`] instead would answer correctly and
    /// leave the client with no session, so the *next* call would open a second
    /// one and everything after would run on that — two sessions per test,
    /// assertions made on one and work done on the other, and anything
    /// session-scoped silently testing nothing.
    pub async fn request(
        &self,
        id: u64,
        method: &str,
        params: &serde_json::Value,
    ) -> serde_json::Value {
        if method == "initialize" {
            return self.initialize().await;
        }
        if self.session_id().is_none() {
            self.session_init().await;
        }
        let response = self.send(id, method, params).await;
        let status = response.status();
        let value = Self::message_of(response).await;
        let value = value.unwrap_or_else(|text| {
            panic!("MCP HTTP {method} should answer JSON-RPC, got {status}: {text}")
        });
        assert!(
            status.is_success(),
            "MCP HTTP {method} should succeed, got {status}: {value}"
        );
        assert_eq!(
            value.get("jsonrpc").and_then(serde_json::Value::as_str),
            Some("2.0"),
            "expected JSON-RPC 2.0: {value}"
        );
        // `message_of` returns the first frame carrying data, and this server
        // now pushes messages of its own. rmcp answers a `POST` on that
        // request's own stream, so today the first frame is the answer — but a
        // crossed reply would otherwise surface as a panic on `value["result"]`
        // somewhere unrelated, with no hint that the messages were swapped.
        assert_eq!(
            value.get("id").and_then(serde_json::Value::as_u64),
            Some(id),
            "answer belongs to a different request: {value}"
        );
        value
    }

    /// The JSON-RPC message in `response`: the body when it is JSON, the first
    /// frame carrying data when it is an event stream. `Err` carries the raw
    /// text when neither yields a message.
    pub async fn message_of(response: reqwest::Response) -> Result<serde_json::Value, String> {
        let is_sse = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        let text = response.text().await.expect("MCP HTTP body");
        if is_sse {
            let mut parser = SseParser::new();
            let mut frames = parser.feed(text.as_bytes());
            // A stream that ended without a blank line still has its last frame.
            frames.extend(parser.feed(b"\n\n"));
            frames
                .into_iter()
                .find(crate::sse::SseEvent::has_data)
                .and_then(|frame| serde_json::from_str(&frame.data).ok())
                .ok_or(text)
        } else {
            serde_json::from_str(&text).map_err(|_| text)
        }
    }

    /// `initialize`, with the answer; remembers the session id when the
    /// server issues one.
    pub async fn initialize(&self) -> serde_json::Value {
        let response = self
            .send(
                0,
                "initialize",
                &serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" }
                }),
            )
            .await;
        let status = response.status();
        if let Some(id) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().expect("session id lock") = Some(id.to_owned());
        }
        let value = Self::message_of(response).await.unwrap_or_else(|text| {
            panic!("initialize should answer JSON-RPC, got {status}: {text}")
        });
        assert!(status.is_success(), "initialize got {status}: {value}");
        value
    }

    /// What a client does before its first tool call: `initialize`, then the
    /// `initialized` notification.
    pub async fn session_init(&self) {
        let _ = self.initialize().await;
        let response = self
            .notify("notifications/initialized", &serde_json::json!({}))
            .await;
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

    /// `DELETE /mcp` for this session; the raw answer. The session id is
    /// forgotten, so the next `request` opens a new one.
    pub async fn close(&self) -> reqwest::Response {
        let request = self.client.delete(&self.mcp_url);
        let response = self
            .with_session(self.authorize(request))
            .send()
            .await
            .expect("MCP DELETE failed");
        *self.session_id.lock().expect("session id lock") = None;
        response
    }

    /// Open the server-to-client notification leg (`GET /mcp`) for this
    /// session, initializing first when it has none; the raw answer.
    pub async fn open_notifications(&self) -> reqwest::Response {
        if self.session_id().is_none() {
            self.session_init().await;
        }
        let request = self
            .stream_client
            .get(&self.mcp_url)
            .header("Accept", "text/event-stream");
        self.with_session(self.authorize(request))
            .send()
            .await
            .expect("MCP GET failed")
    }

    /// The notification leg as a reader, asserting it opened.
    pub async fn notifications(&self) -> NotificationStream {
        let response = self.open_notifications().await;
        assert_eq!(
            response.status(),
            reqwest::StatusCode::OK,
            "GET /mcp should open the notification leg"
        );
        NotificationStream::open(response)
    }
}

/// The server-to-client leg of a session: every JSON-RPC message the server
/// pushes, in order. rmcp's priming frame and its keep-alive comments are
/// skipped.
pub struct NotificationStream {
    response: reqwest::Response,
    parser: SseParser,
    pending: VecDeque<serde_json::Value>,
    ended: bool,
}

impl NotificationStream {
    /// Read an open `text/event-stream` response.
    #[must_use]
    pub fn open(response: reqwest::Response) -> Self {
        Self {
            response,
            parser: SseParser::new(),
            pending: VecDeque::new(),
            ended: false,
        }
    }

    /// Pull one more chunk into `pending`; `false` once the stream has ended.
    async fn pull(&mut self) -> bool {
        if self.ended {
            return false;
        }
        let Ok(Some(bytes)) = self.response.chunk().await else {
            self.ended = true;
            return false;
        };
        for frame in self.parser.feed(&bytes) {
            if frame.has_data()
                && let Ok(value) = serde_json::from_str(&frame.data)
            {
                self.pending.push_back(value);
            }
        }
        true
    }

    /// The next message, within `timeout`; panics when none arrives.
    pub async fn next(&mut self, timeout: Duration) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(value) = self.pending.pop_front() {
                return value;
            }
            let pulled = tokio::time::timeout_at(deadline, self.pull())
                .await
                .unwrap_or_else(|_| panic!("no MCP notification within {timeout:?}"));
            assert!(pulled, "the MCP notification leg ended");
        }
    }

    /// Every message that arrives within `window`.
    pub async fn collect_for(&mut self, window: Duration) -> Vec<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + window;
        while let Ok(true) = tokio::time::timeout_at(deadline, self.pull()).await {}
        self.pending.drain(..).collect()
    }

    /// Assert that no message arrives within `quiet` — and that the leg was
    /// there not to receive one.
    ///
    /// [`Self::collect_for`] stops the moment the body ends, so a closed or
    /// broken notification leg drains to the same empty vec as a healthy quiet
    /// one. Without the second assertion, a regression that closed the leg on
    /// the first write would turn every negative assertion in the subscription
    /// suite green at once.
    pub async fn expect_silence(&mut self, quiet: Duration) {
        let seen = self.collect_for(quiet).await;
        assert!(
            seen.is_empty(),
            "expected no MCP notification, got {seen:?}"
        );
        assert!(
            !self.ended(),
            "the notification leg closed instead of staying silent"
        );
    }

    /// Whether the server has closed the leg.
    #[must_use]
    pub fn ended(&self) -> bool {
        self.ended
    }
}

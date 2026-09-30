use reqwest::{Response, StatusCode};
use tokio::task::JoinHandle;

use super::{SERVER_READY_TIMEOUT, wait_for_http};

/// The service token the server accepts and every request here sends.
pub const API_TOKEN: &str = "e2e-test-token";

pub use super::backends::in_memory_backends;

/// A real server over in-process backends, for the write-path suites.
///
/// Aborted on drop.
pub struct PatchServer {
    /// A plain client; each request adds its own credentials.
    pub client: reqwest::Client,
    /// `http://host:port` of the product listener.
    pub base_url: String,
    /// The streamable HTTP MCP endpoint.
    pub mcp_url: String,
    /// One MCP session as the service token, shared by every tool call a test
    /// makes: the stateful transport binds a session per `initialize`, and a
    /// suite should not open one per call.
    pub mcp: notedthat_mcp::testing::McpSession,
    /// The one knowledge base the server declares, unique per server.
    pub kb: String,
    server_handle: JoinHandle<()>,
}

impl PatchServer {
    /// A server over the in-process backends, patching objects up to
    /// `max_patchable_size` bytes.
    pub async fn start(max_patchable_size: u64) -> Self {
        Self::start_with_events(max_patchable_size, None).await
    }

    /// A server whose writes are announced on `events`.
    pub async fn start_with_events(
        max_patchable_size: u64,
        events: Option<std::sync::Arc<dyn notedthat_core::EventPublisher>>,
    ) -> Self {
        let backends = crate::run::Backends {
            events,
            ..in_memory_backends()
        };
        Self::start_with_backends(max_patchable_size, backends).await
    }

    /// A server over exactly these backends — for a test that needs one of
    /// them to misbehave. Start from [`in_memory_backends`].
    pub async fn start_with_backends(
        max_patchable_size: u64,
        backends: crate::run::Backends,
    ) -> Self {
        Self::start_with_config(max_patchable_size, backends, |_| {}).await
    }

    /// A server over `backends` whose configuration `adjust` changes first —
    /// for a test about a setting the defaults do not exercise.
    ///
    /// # Panics
    ///
    /// If the server does not answer `/healthz` within 30 seconds.
    pub async fn start_with_config(
        max_patchable_size: u64,
        backends: crate::run::Backends,
        adjust: impl FnOnce(&mut crate::config::Config),
    ) -> Self {
        let runtime = super::backends::runtime(API_TOKEN, max_patchable_size, backends);
        let mut config = runtime.config;
        adjust(&mut config);
        let backends = runtime.backends;
        let base_url = format!("http://{}", config.listen_addr);
        let mcp_url = format!("{base_url}/mcp");
        let server_handle = tokio::spawn(async move {
            crate::run::run_with(config, backends)
                .await
                .expect("server run failed");
        });

        wait_for_http(&format!("{base_url}/healthz"), SERVER_READY_TIMEOUT).await;

        let mcp = notedthat_mcp::testing::McpSession::connect(&base_url, API_TOKEN);
        Self {
            client: reqwest::Client::new(),
            base_url,
            mcp_url,
            mcp,
            kb: runtime.kb,
            server_handle,
        }
    }

    /// The REST URL of `path` in [`Self::kb`].
    pub fn object_url(&self, path: &str) -> String {
        format!(
            "{}/api/v1/knowledgebases/{}/{}",
            self.base_url, self.kb, path
        )
    }

    /// Write `body` as Markdown, asserting `201` or `204`, and return its `ETag`.
    ///
    /// # Panics
    ///
    /// If the request cannot be sent, the answer is neither `201` nor `204`, or
    /// it has no `ETag`.
    pub async fn put_text(&self, path: &str, body: &str) -> String {
        let response = self
            .client
            .put(self.object_url(path))
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .header("Content-Type", "text/markdown")
            .body(body.to_owned())
            .send()
            .await
            .expect("PUT object failed");
        // 201 for a new object, 204 when it replaces one (RFC 9110 §9.3.4).
        assert!(
            matches!(
                response.status(),
                StatusCode::CREATED | StatusCode::NO_CONTENT
            ),
            "PUT answered {}",
            response.status()
        );
        etag(&response)
    }

    /// Read `path`, asserting `200`, as text.
    ///
    /// # Panics
    ///
    /// If the request cannot be sent, the answer is not `200`, or the body
    /// cannot be read.
    pub async fn get_text(&self, path: &str) -> String {
        self.get(path)
            .await
            .text()
            .await
            .expect("GET body should read")
    }

    /// Read `path`, asserting `200`.
    ///
    /// # Panics
    ///
    /// If the request cannot be sent, or the answer is not `200`.
    pub async fn get(&self, path: &str) -> Response {
        let response = self
            .client
            .get(self.object_url(path))
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .send()
            .await
            .expect("GET object failed");
        assert_eq!(response.status(), StatusCode::OK);
        response
    }

    /// `PATCH` `path` with a `Content-Range`, conditional on `if_match` when given.
    ///
    /// # Panics
    ///
    /// If the request cannot be sent.
    pub async fn patch_content_range(
        &self,
        path: &str,
        content_range: &str,
        if_match: Option<&str>,
        body: &str,
    ) -> Response {
        let mut request = self
            .client
            .patch(self.object_url(path))
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .header("Content-Range", content_range)
            .body(body.to_owned());
        if let Some(etag) = if_match {
            request = request.header("If-Match", etag);
        }
        request.send().await.expect("PATCH object failed")
    }

    /// `PATCH` `path` in append mode, conditional on `if_match` when given.
    ///
    /// # Panics
    ///
    /// If the request cannot be sent.
    pub async fn patch_append(&self, path: &str, if_match: Option<&str>, body: &str) -> Response {
        let mut request = self
            .client
            .patch(self.object_url(path))
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .header("NT-Patch-Mode", "append")
            .body(body.to_owned());
        if let Some(etag) = if_match {
            request = request.header("If-Match", etag);
        }
        request.send().await.expect("PATCH append failed")
    }

    /// Replace `old_string` with `new_string` in `path` through the replace route,
    /// conditional on `if_match`.
    ///
    /// # Panics
    ///
    /// If the request cannot be sent.
    pub async fn replace_json(
        &self,
        path: &str,
        if_match: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Response {
        let url = format!(
            "{}/api/v1/knowledgebases/{}/replace/{}",
            self.base_url, self.kb, path
        );
        let body = serde_json::json!({
            "old_string": old_string,
            "new_string": new_string,
            "replace_all": replace_all,
        });
        self.client
            .post(url)
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .header("Content-Type", "application/json")
            .header("If-Match", if_match)
            .body(body.to_string())
            .send()
            .await
            .expect("replace request should return")
    }

    /// `HEAD` `path` and return only the status.
    ///
    /// # Panics
    ///
    /// If the request cannot be sent.
    pub async fn head_text_status(&self, path: &str) -> StatusCode {
        self.client
            .head(self.object_url(path))
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .send()
            .await
            .expect("head request should return")
            .status()
    }
}

impl Drop for PatchServer {
    fn drop(&mut self) {
        self.server_handle.abort();
    }
}

/// The `ETag` header of `response`.
///
/// # Panics
///
/// If `response` has no `ETag` header, or it is not visible ASCII.
pub fn etag(response: &Response) -> String {
    response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| panic!("response should contain ETag: {:?}", response.headers()))
        .to_owned()
}

/// Assert `response` is `status` with the JSON error `code`.
///
/// # Panics
///
/// If `response` is not `status`, its body is not JSON, or its `error` is
/// not `code`.
pub async fn assert_error_code(response: Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    let json = response
        .json::<serde_json::Value>()
        .await
        .expect("error response should be JSON");
    assert_eq!(json["error"], code);
}

/// Assert a replace answered `200` with `expected_match_count` matches and
/// the same `ETag` in header and body, and return that `ETag`.
///
/// # Panics
///
/// If `resp` is not `200`, has no valid `ETag` header, or its body is not
/// JSON; or if that body's `match_count` is not `expected_match_count`, its
/// `total_bytes` is not a number, or its `etag` differs from the header's.
pub async fn assert_replace_success(resp: Response, expected_match_count: u64) -> String {
    assert_eq!(resp.status(), StatusCode::OK, "replace should return 200");
    let etag_header = resp
        .headers()
        .get("etag")
        .expect("etag header should be present")
        .to_str()
        .expect("etag should be valid str")
        .to_owned();
    let body: serde_json::Value = resp.json().await.expect("json body");
    assert_eq!(
        body["match_count"].as_u64(),
        Some(expected_match_count),
        "match_count mismatch"
    );
    assert!(
        body["total_bytes"].is_number(),
        "total_bytes should be a number"
    );
    assert_eq!(
        body["etag"].as_str().map(str::to_owned),
        Some(etag_header.clone()),
        "etag body == header"
    );
    etag_header
}

/// Send one JSON-RPC `method` over `mcp` and return its response.
pub async fn mcp_request(
    mcp: &notedthat_mcp::testing::McpSession,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    mcp.request(id, method, &params).await
}

/// Call the MCP tool `tool_name` over `mcp` and return its response.
pub async fn mcp_call_tool(
    mcp: &notedthat_mcp::testing::McpSession,
    id: u64,
    tool_name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    mcp.call_tool(id, tool_name, &arguments).await
}

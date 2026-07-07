#![expect(
    dead_code,
    reason = "replace helpers are shared by forthcoming E2E cases"
)]

#[path = "patch_backends.rs"]
mod patch_backends;

use reqwest::{Response, StatusCode};
use std::time::Duration;
use tokio::task::JoinHandle;

pub const API_TOKEN: &str = "e2e-test-token";

pub struct PatchServer {
    pub client: reqwest::Client,
    pub base_url: String,
    pub mcp_url: String,
    pub kb: String,
    server_handle: JoinHandle<()>,
    _guards: patch_backends::BackendGuards,
}

impl PatchServer {
    pub async fn start(max_patchable_size: u64) -> Self {
        let runtime = patch_backends::start_runtime(max_patchable_size).await;
        let config = runtime.config;
        let base_url = format!("http://{}", config.listen_addr);
        let mcp_url = format!("http://{}/mcp", config.mcp_http_bind);
        let server_handle = tokio::spawn(async move {
            notedthat_server::run::run(config)
                .await
                .expect("server run failed");
        });

        wait_for_http(&format!("{base_url}/healthz"), Duration::from_secs(10)).await;

        Self {
            client: reqwest::Client::new(),
            base_url,
            mcp_url,
            kb: runtime.kb,
            server_handle,
            _guards: runtime.guards,
        }
    }

    pub fn object_url(&self, path: &str) -> String {
        format!("{}/v1/knowledgebases/{}/{}", self.base_url, self.kb, path)
    }

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
        assert_eq!(response.status(), StatusCode::CREATED);
        etag(&response)
    }

    pub async fn get_text(&self, path: &str) -> String {
        self.get(path)
            .await
            .text()
            .await
            .expect("GET body should read")
    }

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

    pub async fn replace_json(
        &self,
        path: &str,
        if_match: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Response {
        let url = format!(
            "{}/v1/knowledgebases/{}/replace/{}",
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

pub fn etag(response: &Response) -> String {
    response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| panic!("response should contain ETag: {:?}", response.headers()))
        .to_owned()
}

pub async fn assert_error_code(response: Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    let json = response
        .json::<serde_json::Value>()
        .await
        .expect("error response should be JSON");
    assert_eq!(json["error"], code);
}

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

pub async fn mcp_request(
    client: &reqwest::Client,
    mcp_url: &str,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let response = client
        .post(mcp_url)
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
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

pub async fn mcp_call_tool(
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
        serde_json::json!({"name": tool_name, "arguments": arguments}),
    )
    .await
}

async fn wait_for_http(url: &str, timeout: Duration) {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        assert!(
            tokio::time::Instant::now() <= deadline,
            "HTTP server did not become ready at {url}"
        );
        if client
            .get(url)
            .send()
            .await
            .map(|response| response.status().is_success())
            .unwrap_or(false)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

use reqwest::{Method, Response, StatusCode, header::HeaderMap};
use std::time::Duration;

use super::{
    public_read_env::{API_TOKEN, PUBLIC_KB},
    public_read_server::ServerInstance,
};

pub struct WireResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl WireResponse {
    pub fn text(&self) -> &str {
        std::str::from_utf8(&self.body).expect("UTF-8 response")
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("JSON response")
    }
}

pub async fn wire(label: &str, response: Response) -> WireResponse {
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.bytes().await.expect("wire response body").to_vec();
    println!(
        "WIRE {label} -> {status}; allow={:?}; challenge={:?}; range={:?}; body={}",
        headers.get("allow"),
        headers.get("www-authenticate"),
        headers.get("content-range"),
        String::from_utf8_lossy(&body).replace('\n', "\\n")
    );
    WireResponse {
        status,
        headers,
        body,
    }
}

pub async fn search(
    client: &reqwest::Client,
    server: &ServerInstance,
    query: &str,
    token: Option<&str>,
) -> WireResponse {
    let mut request = client
        .post(format!(
            "{}/api/v1/knowledgebases/{PUBLIC_KB}/search",
            server.http_url
        ))
        .json(&serde_json::json!({"query": query, "limit": 10}));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    wire(
        "HTTP POST search",
        request.send().await.expect("search response"),
    )
    .await
}

pub async fn wait_indexed(client: &reqwest::Client, server: &ServerInstance, key: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "search index readiness timed out for {key}"
        );
        let response = client
            .post(format!(
                "{}/api/v1/knowledgebases/{PUBLIC_KB}/search",
                server.http_url
            ))
            .bearer_auth(API_TOKEN)
            .json(&serde_json::json!({"query": key, "limit": 10}))
            .send()
            .await
            .expect("search readiness response");
        let status = response.status();
        let json: serde_json::Value = response.json().await.expect("search readiness JSON");
        if status == StatusCode::OK
            && json["hits"]
                .as_array()
                .is_some_and(|hits| hits.iter().any(|hit| hit["object_key"] == key))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub fn assert_http_401(response: &WireResponse) {
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    let json = response.json();
    assert_eq!(json["error"], "unauthorized");
    assert!(json["message"].as_str().is_some_and(|message| {
        message.contains("valid Bearer token") && message.contains("Authorization")
    }));
}

pub fn method(value: &str) -> Method {
    Method::from_bytes(value.as_bytes()).expect("fixture HTTP method")
}

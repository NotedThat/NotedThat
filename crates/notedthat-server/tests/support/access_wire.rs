use reqwest::{Response, StatusCode, header::HeaderMap};
use std::time::Duration;

use super::{
    access_env::{API_TOKEN, PUBLIC_KB},
    access_server::ServerInstance,
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
    let wire = WireResponse {
        status: response.status(),
        headers: response.headers().clone(),
        body: response.bytes().await.expect("wire response body").to_vec(),
    };
    println!(
        "WIRE {label} -> {}; allow={:?}; challenge={:?}; range={:?}; body={}",
        wire.status,
        wire.headers.get("allow"),
        wire.headers.get("www-authenticate"),
        wire.headers.get("content-range"),
        String::from_utf8_lossy(&wire.body).replace('\n', "\\n")
    );
    wire
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

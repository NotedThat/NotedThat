//! Cross-surface E2E: `WebDAV` PUT → HTTP search.
//!
//! Run with: cargo test -p notedthat-server --test `webdav_cross_surface_e2e` -- --ignored --nocapture

#![allow(missing_docs)]

use base64::Engine as _;
use std::time::Duration;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const UNIQUE_PHRASE: &str = "unique-phrase-cross-surface-42";
const WEBDAV_USER: &str = "e2e-webdav-user";
const WEBDAV_PASS: &str = "e2e-webdav-pass";
const API_TOKEN: &str = "e2e-test-token";

// SeaweedFS 4.18 requires an IAM config file to accept signed S3 requests without this the
// S3 gateway rejects all signed requests. target path is the FIRST arg in with_copy_to.
const SEAWEEDFS_S3_CONFIG: &[u8] = br#"{"identities":[{"name":"test","credentials":[{"accessKey":"any","secretKey":"any"}],"actions":["Admin","Read","Write","List","Tagging"]}]}"#;

fn basic_auth(user: &str, pass: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {encoded}")
}

async fn start_seaweedfs() -> (impl std::any::Any, String) {
    let container = GenericImage::new("chrislusf/seaweedfs", "4.18")
        .with_exposed_port(8333_u16.tcp())
        .with_wait_for(WaitFor::seconds(5))
        .with_copy_to("/tmp/s3.json", SEAWEEDFS_S3_CONFIG.to_vec())
        .with_cmd(["server", "-s3", "-filer", "-s3.config=/tmp/s3.json"])
        .start()
        .await
        .expect("failed to start SeaweedFS testcontainer");
    let port = container
        .get_host_port_ipv4(8333_u16)
        .await
        .expect("failed to get SeaweedFS port");
    (container, format!("http://127.0.0.1:{port}"))
}

async fn start_qdrant() -> (impl std::any::Any, String) {
    let container = GenericImage::new("qdrant/qdrant", "v1.15.4")
        .with_exposed_port(6334_u16.tcp())
        .with_wait_for(WaitFor::seconds(5))
        .start()
        .await
        .expect("failed to start qdrant/qdrant:v1.15.4 — is Docker running?");
    let port = container
        .get_host_port_ipv4(6334_u16)
        .await
        .expect("failed to get Qdrant gRPC port");
    (container, format!("http://127.0.0.1:{port}"))
}

fn embedding_response(dim: usize, count: usize) -> serde_json::Value {
    let data: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            let v: Vec<f32> = (0..dim)
                .map(|j| if j == i % dim { 1.0_f32 } else { 0.0_f32 })
                .collect();
            serde_json::json!({"index": i, "embedding": v, "object": "embedding"})
        })
        .collect();
    serde_json::json!({"object": "list", "data": data})
}

fn test_config_with_webdav(
    http_addr: std::net::SocketAddr,
    dav_addr: std::net::SocketAddr,
    s3_url: &str,
    qdrant_url: &str,
    embedder_url: &str,
) -> notedthat_server::config::Config {
    use notedthat_core::{KbSlug, TenantSlug};
    use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
    use notedthat_storage_s3::S3Config;
    use std::collections::BTreeMap;

    let mut kbs = BTreeMap::new();
    kbs.insert("notes".to_string(), KbSlug::try_new("notes").unwrap());

    Config {
        api_token: API_TOKEN.to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr: http_addr,
        s3: S3Config {
            endpoint_url: Some(s3_url.to_string()),
            region: "us-east-1".to_string(),
            access_key_id: "any".to_string(),
            secret_access_key: "any".to_string(),
            force_path_style: true,
        },
        log_format: LogFormat::Pretty,
        qdrant: ServerQdrantConfig {
            url: qdrant_url.to_string(),
            api_key: None,
        },
        embedder: EmbedderConfig {
            // OpenAiCompatibleEmbedder appends /v1/embeddings itself — pass base URL only.
            endpoint_url: embedder_url.to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dimensions: 4,
            batch_size: 32,
            timeout_ms: 30_000,
            max_retries: 3,
            max_input_tokens: 8192,
        },
        webdav_listen_addr: dav_addr,
        webdav_username: WEBDAV_USER.to_string(),
        webdav_password: WEBDAV_PASS.to_string(),
        mcp_http_bind: dav_addr,
        mcp_http_enabled: true,
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "::1".to_string(),
        ],
        max_patchable_size: 10 * 1024 * 1024,
    }
}

async fn free_addr() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to port 0");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    addr
}

fn free_dav_addr() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind to port 0");
    listener.local_addr().expect("local_addr")
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
            .is_ok_and(|r| r.status().is_success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn wait_for_dav_options(base_url: &str, timeout: Duration) {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        assert!(
            tokio::time::Instant::now() <= deadline,
            "WebDAV server did not become ready at {base_url}"
        );
        let ok = client
            .request(reqwest::Method::OPTIONS, format!("{base_url}/"))
            .header("Authorization", basic_auth(WEBDAV_USER, WEBDAV_PASS))
            .send()
            .await
            .is_ok_and(|r| r.status().as_u16() == 204);
        if ok {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn poll_search(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    phrase: &str,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        let resp = client
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(serde_json::json!({"query": phrase, "limit": 5}).to_string())
            .send()
            .await;
        if let Ok(r) = resp
            && let Ok(json) = r.json::<serde_json::Value>().await
            && json["hits"].as_array().map_or(0, Vec::len) > 0
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn poll_search_gone(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    phrase: &str,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::Instant::now() > deadline {
            return false;
        }
        let resp = client
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(serde_json::json!({"query": phrase, "limit": 5}).to_string())
            .send()
            .await;
        if let Ok(r) = resp
            && let Ok(json) = r.json::<serde_json::Value>().await
            && json["hits"].as_array().map_or(0, Vec::len) == 0
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test]
#[ignore = "requires Docker (SeaweedFS + Qdrant testcontainers)"]
async fn webdav_put_becomes_searchable_via_http() {
    let (_seaweed, s3_url) = start_seaweedfs().await;
    let (_qdrant, qdrant_url) = start_qdrant().await;
    let mock_embedder = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&mock_embedder)
        .await;

    let http_addr = free_addr().await;
    let dav_addr = free_dav_addr();
    let config = test_config_with_webdav(
        http_addr,
        dav_addr,
        &s3_url,
        &qdrant_url,
        &mock_embedder.uri(),
    );

    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run(config)
            .await
            .expect("server run failed");
    });

    let http_url = format!("http://{http_addr}");
    let dav_url = format!("http://{dav_addr}");
    wait_for_http(&format!("{http_url}/healthz"), Duration::from_secs(10)).await;
    wait_for_dav_options(&dav_url, Duration::from_secs(10)).await;

    let client = reqwest::Client::new();

    let body = format!("# Cross-Surface Test\n\n{UNIQUE_PHRASE}");
    let resp = client
        .put(format!("{dav_url}/notes/e2e.md"))
        .header("Authorization", basic_auth(WEBDAV_USER, WEBDAV_PASS))
        .header("Content-Type", "application/octet-stream")
        .body(body)
        .send()
        .await
        .expect("WebDAV PUT request failed");
    assert_eq!(
        resp.status().as_u16(),
        201,
        "WebDAV PUT should return 201 Created"
    );

    let resp = client
        .get(format!("{http_url}/v1/knowledgebases/notes/e2e.md"))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .send()
        .await
        .expect("HTTP GET request failed");
    assert_eq!(resp.status().as_u16(), 200, "HTTP GET should return 200 OK");
    let body_text = resp.text().await.expect("failed to read GET response body");
    assert!(
        body_text.contains(UNIQUE_PHRASE),
        "HTTP GET body should contain the unique phrase; got: {body_text:?}"
    );

    let search_url = format!("{http_url}/v1/knowledgebases/notes/search");
    let found = poll_search(
        &client,
        &search_url,
        API_TOKEN,
        UNIQUE_PHRASE,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        found,
        "WebDAV PUT should become searchable via HTTP /search within 5 s"
    );

    let resp = client
        .delete(format!("{dav_url}/notes/e2e.md"))
        .header("Authorization", basic_auth(WEBDAV_USER, WEBDAV_PASS))
        .send()
        .await
        .expect("WebDAV DELETE request failed");
    assert_eq!(
        resp.status().as_u16(),
        204,
        "WebDAV DELETE should return 204 No Content"
    );

    let gone = poll_search_gone(
        &client,
        &search_url,
        API_TOKEN,
        UNIQUE_PHRASE,
        Duration::from_secs(5),
    )
    .await;
    assert!(
        gone,
        "WebDAV DELETE should remove the search hit via tombstone within 5 s"
    );

    server_handle.abort();
}

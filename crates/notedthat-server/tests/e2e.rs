//! End-to-end tests for `notedthat-server` against a real `SeaweedFS` testcontainer.
//!
//! These tests are marked `#[ignore]` because they require Docker. Run with:
//! ```sh
//! cargo test -p notedthat-server --locked -- --include-ignored
//! ```
#![allow(missing_docs)]

use notedthat_core::{KbSlug, TenantSlug};
use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
use notedthat_storage_s3::S3Config;
use std::collections::BTreeMap;
use std::time::Duration;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

async fn start_seaweedfs() -> (impl std::any::Any, String) {
    // SeaweedFS 4.18 requires an IAM config file to accept signed S3 requests.
    let s3_iam = serde_json::json!({
        "identities": [{
            "name": "test",
            "credentials": [{"accessKey": "any", "secretKey": "any"}],
            "actions": ["Admin", "Read", "Write", "List", "Tagging"]
        }]
    });
    let config_bytes = serde_json::to_vec(&s3_iam).expect("serialize IAM config");
    let container = GenericImage::new("chrislusf/seaweedfs", "4.18")
        .with_exposed_port(8333_u16.tcp())
        .with_wait_for(WaitFor::seconds(5))
        .with_cmd(["server", "-s3", "-filer", "-s3.config=/tmp/s3.json"])
        .with_copy_to("/tmp/s3.json", config_bytes)
        .start()
        .await
        .expect("failed to start SeaweedFS testcontainer");
    let port = container
        .get_host_port_ipv4(8333_u16)
        .await
        .expect("failed to get port");
    (container, format!("http://127.0.0.1:{port}"))
}

fn test_config(listen_addr: std::net::SocketAddr, endpoint: &str) -> Config {
    let mut kbs = BTreeMap::new();
    kbs.insert("notes".to_string(), KbSlug::try_new("notes").unwrap());
    Config {
        api_token: "e2e-test-token".to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr,
        s3: S3Config {
            endpoint_url: Some(endpoint.to_string()),
            region: "us-east-1".to_string(),
            access_key_id: "any".to_string(),
            secret_access_key: "any".to_string(),
            force_path_style: true,
        },
        log_format: LogFormat::Pretty,
        qdrant: ServerQdrantConfig {
            url: "http://127.0.0.1:6334".to_string(),
            api_key: None,
        },
        embedder: EmbedderConfig {
            endpoint_url: "http://127.0.0.1:9999".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dimensions: 3,
            batch_size: 32,
            timeout_ms: 30_000,
            max_retries: 3,
            max_input_tokens: 8192,
        },
        webdav_listen_addr: free_dav_addr(),
        webdav_username: "e2e-webdav-user".to_string(),
        webdav_password: "e2e-webdav-pass".to_string(),
        mcp_http_bind: free_dav_addr(),
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

fn free_dav_addr() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind to port 0");
    listener.local_addr().expect("local_addr")
}

async fn free_addr() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to port 0");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    addr
}

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn e2e_healthz_and_put_get() {
    let (_container, endpoint) = start_seaweedfs().await;

    let bound_addr = free_addr().await;
    let config = test_config(bound_addr, &endpoint);
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run(config)
            .await
            .expect("server run failed");
    });

    tokio::time::sleep(Duration::from_secs(2)).await;

    let client = reqwest::Client::new();
    let base = format!("http://{bound_addr}");

    let resp = client.get(format!("{base}/healthz")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let resp = client
        .get(format!("{base}/v1/knowledgebases"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    let resp = client
        .put(format!("{base}/v1/knowledgebases/notes/hello.md"))
        .header("authorization", "Bearer e2e-test-token")
        .header("content-type", "text/markdown")
        .body("# Hello")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);

    let resp = client
        .get(format!("{base}/v1/knowledgebases/notes/hello.md"))
        .header("authorization", "Bearer e2e-test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(body, "# Hello");

    server_handle.abort();
}

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn e2e_list_and_delete() {
    let (_container, endpoint) = start_seaweedfs().await;

    let bound_addr = free_addr().await;
    let config = test_config(bound_addr, &endpoint);
    let server_handle = tokio::spawn(async move {
        notedthat_server::run::run(config)
            .await
            .expect("server run failed");
    });

    tokio::time::sleep(Duration::from_secs(2)).await;

    let client = reqwest::Client::new();
    let base = format!("http://{bound_addr}");

    for name in &["file1.md", "file2.md"] {
        client
            .put(format!("{base}/v1/knowledgebases/notes/{name}"))
            .header("authorization", "Bearer e2e-test-token")
            .body("content")
            .send()
            .await
            .unwrap();
    }

    let resp = client
        .get(format!("{base}/v1/knowledgebases/notes"))
        .header("authorization", "Bearer e2e-test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    let count = json["objects"].as_array().map_or(0, Vec::len);
    assert!(count >= 2, "expected at least 2 objects, got {count}");

    let resp = client
        .delete(format!("{base}/v1/knowledgebases/notes/file1.md"))
        .header("authorization", "Bearer e2e-test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    let resp = client
        .delete(format!("{base}/v1/knowledgebases/notes/file1.md"))
        .header("authorization", "Bearer e2e-test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    server_handle.abort();
}

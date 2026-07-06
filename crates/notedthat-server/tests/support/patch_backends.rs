use notedthat_core::{KbSlug, TenantSlug};
use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
use notedthat_storage_s3::S3Config;
use std::collections::BTreeMap;
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method, matchers::path};

use super::API_TOKEN;

const SEAWEEDFS_S3_CONFIG: &[u8] = br#"{"identities":[{"name":"test","credentials":[{"accessKey":"any","secretKey":"any"}],"actions":["Admin","Read","Write","List","Tagging"]}]}"#;

pub(super) struct RuntimeParts {
    pub(super) config: Config,
    pub(super) kb: String,
    pub(super) guards: BackendGuards,
}

pub(super) struct BackendGuards {
    _seaweed: Box<dyn std::any::Any + Send>,
    _qdrant: Box<dyn std::any::Any + Send>,
    _embedder: MockServer,
}

#[derive(Clone, Copy)]
struct ListenerAddrs {
    http: std::net::SocketAddr,
    dav: std::net::SocketAddr,
    mcp: std::net::SocketAddr,
}

#[derive(Clone, Copy)]
struct BackendUrls<'a> {
    s3: &'a str,
    qdrant: &'a str,
    embedder: &'a str,
}

pub(super) async fn start_runtime(max_patchable_size: u64) -> RuntimeParts {
    let (seaweed, s3_url) = start_seaweedfs().await;
    let (qdrant, qdrant_url) = start_qdrant().await;
    let embedder = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(embedding_response(4, 1)))
        .mount(&embedder)
        .await;

    let listeners = ListenerAddrs {
        http: free_addr().await,
        dav: free_addr().await,
        mcp: free_addr().await,
    };
    let kb = unique_kb();
    let embedder_url = embedder.uri();
    let config = test_config(
        &kb,
        listeners,
        BackendUrls {
            s3: &s3_url,
            qdrant: &qdrant_url,
            embedder: &embedder_url,
        },
        max_patchable_size,
    );

    RuntimeParts {
        config,
        kb,
        guards: BackendGuards {
            _seaweed: Box::new(seaweed),
            _qdrant: Box::new(qdrant),
            _embedder: embedder,
        },
    }
}

async fn start_seaweedfs() -> (impl std::any::Any + Send, String) {
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

async fn start_qdrant() -> (impl std::any::Any + Send, String) {
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

fn test_config(
    kb: &str,
    listeners: ListenerAddrs,
    backends: BackendUrls<'_>,
    max_patchable_size: u64,
) -> Config {
    let mut kbs = BTreeMap::new();
    kbs.insert(
        kb.to_string(),
        KbSlug::try_new(kb).expect("test KB slug is valid"),
    );

    Config {
        api_token: API_TOKEN.to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr: listeners.http,
        s3: S3Config {
            endpoint_url: Some(backends.s3.to_string()),
            region: "us-east-1".to_string(),
            access_key_id: "any".to_string(),
            secret_access_key: "any".to_string(),
            force_path_style: true,
        },
        log_format: LogFormat::Pretty,
        qdrant: ServerQdrantConfig {
            url: backends.qdrant.to_string(),
            api_key: None,
        },
        embedder: EmbedderConfig {
            endpoint_url: backends.embedder.to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dimensions: 4,
            batch_size: 32,
            timeout_ms: 30_000,
            max_retries: 3,
            max_input_tokens: 8192,
        },
        webdav_listen_addr: listeners.dav,
        webdav_username: "e2e-webdav-user".to_string(),
        webdav_password: "e2e-webdav-pass".to_string(),
        mcp_http_bind: listeners.mcp,
        mcp_http_enabled: true,
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "::1".to_string(),
        ],
        max_patchable_size,
    }
}

fn embedding_response(dim: usize, count: usize) -> serde_json::Value {
    let data = (0..count)
        .map(|i| {
            let embedding: Vec<f32> = (0..dim)
                .map(|j| if j == i % dim { 1.0 } else { 0.0 })
                .collect();
            serde_json::json!({"index": i, "embedding": embedding, "object": "embedding"})
        })
        .collect::<Vec<_>>();
    serde_json::json!({"object": "list", "data": data})
}

async fn free_addr() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind to port 0");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    addr
}

fn unique_kb() -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    format!("patch-{nonce}")
}

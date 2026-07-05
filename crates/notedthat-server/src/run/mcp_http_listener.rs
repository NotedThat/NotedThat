use super::mcp_http::{bind_listener, internal_http_api_url};
use crate::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
use notedthat_core::{KbSlug, TenantSlug};
use notedthat_storage_s3::S3Config;
use std::{collections::BTreeMap, net::SocketAddr};

fn test_config(mcp_http_bind: SocketAddr) -> Config {
    let mut kbs = BTreeMap::new();
    kbs.insert(
        "notes".to_string(),
        KbSlug::try_new("notes").expect("test KB slug is valid"),
    );
    Config {
        api_token: "test-token".to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr: "127.0.0.1:0".parse().expect("test HTTP addr is valid"),
        s3: S3Config {
            endpoint_url: Some("http://127.0.0.1:8333".to_string()),
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
        webdav_listen_addr: "127.0.0.1:0".parse().expect("test DAV addr is valid"),
        webdav_username: "webdav-user".to_string(),
        webdav_password: "webdav-pass".to_string(),
        mcp_http_bind,
        mcp_http_enabled: true,
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "::1".to_string(),
        ],
    }
}

#[tokio::test]
async fn enabled_mcp_http_listener_binds_and_logs_address() {
    // Given: MCP HTTP is enabled on an ephemeral loopback port.
    let config = test_config("127.0.0.1:0".parse().expect("test MCP addr is valid"));

    // When: the MCP listener is bound before serving starts.
    let listener = bind_listener(&config)
        .await
        .expect("MCP listener should bind")
        .expect("MCP listener should be enabled");

    // Then: the actual bound address is available for logging and serving.
    let addr = listener
        .local_addr()
        .expect("bound listener has local addr");
    assert_eq!(
        addr.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    assert_ne!(addr.port(), 0);
}

#[test]
fn internal_http_api_url_uses_actual_bound_socket() {
    // Given: actual listener addresses after binding, including ephemeral ports.
    let wildcard_v4 = SocketAddr::from(([0, 0, 0, 0], 49_123));
    let wildcard_v6 = SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 49_124));
    let concrete = SocketAddr::from(([192, 0, 2, 10], 49_125));

    // When/Then: wildcard binds are mapped to loopback with the same actual port.
    assert_eq!(internal_http_api_url(wildcard_v4), "http://127.0.0.1:49123");
    assert_eq!(internal_http_api_url(wildcard_v6), "http://[::1]:49124");
    assert_eq!(internal_http_api_url(concrete), "http://192.0.2.10:49125");
}

#[tokio::test]
async fn disabled_mcp_http_listener_returns_none() {
    // Given: MCP HTTP is disabled in config.
    let mut config = test_config("127.0.0.1:0".parse().expect("test MCP addr is valid"));
    config.mcp_http_enabled = false;

    // When: the MCP listener is bound.
    let listener = bind_listener(&config)
        .await
        .expect("bind_listener should succeed");

    // Then: no listener is returned (MCP HTTP is skipped).
    assert!(listener.is_none());
}

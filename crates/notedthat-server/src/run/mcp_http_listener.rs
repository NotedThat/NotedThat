use super::mcp_http::{bind_listener, build_router, internal_http_api_url};
use crate::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
use anyhow::Context as _;
use axum::{Router, routing::get};
use notedthat_core::{KbSlug, TenantSlug};
use notedthat_storage_s3::S3Config;
use std::{collections::BTreeMap, net::SocketAddr, time::Duration};
use tokio::{io::AsyncReadExt as _, net::TcpStream};
use tokio_util::sync::CancellationToken;

const SHUTDOWN_BOUND: Duration = Duration::from_secs(15);

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
        max_patchable_size: 100 * 1024 * 1024,
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

#[tokio::test]
async fn three_listener_shutdown_closes_listeners_and_active_mcp_session() {
    // Given: HTTP API, WebDAV, and MCP HTTP listeners are all bound to ephemeral loopback ports.
    let config = test_config("127.0.0.1:0".parse().expect("test MCP addr is valid"));
    let http_listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .expect("HTTP listener should bind");
    let http_addr = http_listener
        .local_addr()
        .expect("HTTP listener exposes its bound addr");
    let dav_listener = tokio::net::TcpListener::bind(config.webdav_listen_addr)
        .await
        .expect("WebDAV listener should bind");
    let dav_addr = dav_listener
        .local_addr()
        .expect("WebDAV listener exposes its bound addr");
    let mcp_listener = bind_listener(&config)
        .await
        .expect("MCP listener bind should succeed")
        .expect("MCP listener should be enabled");
    let mcp_addr = mcp_listener
        .local_addr()
        .expect("MCP listener exposes its bound addr");

    let shutdown_token = CancellationToken::new();
    let http_shutdown = shutdown_token.clone();
    let dav_shutdown = shutdown_token.clone();
    let mcp_shutdown = shutdown_token.child_token();
    let internal_api_url = internal_http_api_url(http_addr);
    let mcp_app = build_router(&config, &internal_api_url, mcp_shutdown.clone())
        .expect("MCP router should build without external infrastructure");

    let http_handle = tokio::spawn(async move {
        axum::serve(
            http_listener,
            Router::new().route("/healthz", get(|| async { "ok" })),
        )
        .with_graceful_shutdown(async move { http_shutdown.cancelled().await })
        .await
        .context("HTTP listener failed")
    });
    let dav_handle = tokio::spawn(async move {
        axum::serve(
            dav_listener,
            Router::new().route("/", get(|| async { "ok" })),
        )
        .with_graceful_shutdown(async move { dav_shutdown.cancelled().await })
        .await
        .context("WebDAV listener failed")
    });
    let mcp_handle = tokio::spawn(async move {
        axum::serve(mcp_listener, mcp_app)
            .with_graceful_shutdown(async move { mcp_shutdown.cancelled().await })
            .await
            .context("MCP HTTP listener failed")
    });

    // When: each listener accepts connections and an MCP connection is held open across shutdown.
    let http_stream = TcpStream::connect(http_addr)
        .await
        .expect("HTTP listener should accept connections");
    let dav_stream = TcpStream::connect(dav_addr)
        .await
        .expect("WebDAV listener should accept connections");
    let mut mcp_stream = TcpStream::connect(mcp_addr)
        .await
        .expect("MCP listener should accept a held-open connection");
    drop(http_stream);
    drop(dav_stream);

    shutdown_token.cancel();
    let server_result = tokio::time::timeout(SHUTDOWN_BOUND, async move {
        let (http_result, dav_result, mcp_result) =
            tokio::try_join!(http_handle, dav_handle, mcp_handle).expect("listeners should join");
        http_result?;
        dav_result?;
        mcp_result?;
        anyhow::Ok(())
    })
    .await
    .expect("all three listeners should quiesce within 15s");
    server_result.expect("all three listeners should exit cleanly");

    // Then: the held-open MCP connection is closed within the same bounded shutdown budget.
    let mut byte = [0_u8; 1];
    let bytes_read = tokio::time::timeout(SHUTDOWN_BOUND, mcp_stream.read(&mut byte))
        .await
        .expect("held-open MCP connection should close within 15s")
        .expect("MCP stream read should resolve cleanly");
    assert_eq!(bytes_read, 0, "MCP connection should close on cancellation");
    eprintln!(
        "all three listeners quiesced within 15s; held-open MCP connection closed within 15s"
    );
}

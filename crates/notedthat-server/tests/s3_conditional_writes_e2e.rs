//! The `s3` backend's startup check that conditional writes are enforced (D70), through
//! the real `run` path.
//!
//! `run_with` takes its storage as given and so never builds an `S3Storage`; `run` does,
//! so these tests call `run` with the S3 endpoint pointed at a wiremock server that
//! stores every `PUT` whatever its preconditions say — the behaviour `SPECIFICATIONS.md`
//! §8.1 records for Garage and `SeaweedFS` < 4.09. Qdrant and the embedder stay
//! unroutable: the check runs before provisioning touches either, which is what lets a
//! test tell a refusal by the check from a failure after it.
//!
//! This is also the "deliberately broken backend contract fails the job" #87 asks for:
//! every test here fails if the check stops refusing a backend that loses writes.
//!
//! Run with: `cargo test -p notedthat-server --test s3_conditional_writes_e2e`
#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::time::Duration;

use notedthat_core::{KbSlug, TenantSlug};
use notedthat_server::config::{
    Config, EmbedderConfig, LogFormat, ServerQdrantConfig, StorageConfig,
};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const KB: &str = "notes";

/// An S3 endpoint that creates every bucket, stores every `PUT` — conditional or not —
/// and deletes every object.
async fn backend_ignoring_preconditions() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"0123abcd\""))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    server
}

fn config(endpoint: &str, allow_unenforced: bool) -> Config {
    let mut kbs = BTreeMap::new();
    kbs.insert(KB.to_string(), KbSlug::try_new(KB).unwrap());
    let StorageConfig::S3(mut s3) = notedthat_server::config::unroutable_storage_placeholder()
    else {
        panic!("the placeholder selects s3")
    };
    s3.endpoint_url = Some(endpoint.to_string());
    s3.reconcile_on_startup = false;
    s3.allow_unenforced_conditional_writes = allow_unenforced;
    Config {
        api_token: "conditional-writes-token".to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr: notedthat_api_http::testing::reserve_addr(),
        metrics_listen_addr: None,
        storage: StorageConfig::S3(s3),
        events: notedthat_server::config::EventsConfig::None,
        log_format: LogFormat::Pretty,
        qdrant: ServerQdrantConfig {
            url: "http://127.0.0.1:1".to_string(),
            api_key: None,
            timeout_ms: 2_000,
            connect_timeout_ms: 1_000,
        },
        embedder: EmbedderConfig {
            endpoint_url: "http://127.0.0.1:1".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dimensions: 3,
            batch_size: 32,
            timeout_ms: 2_000,
            max_retries: 0,
            max_input_tokens: 8192,
        },
        webdav_username: "conditional-writes-user".to_string(),
        webdav_password: "conditional-writes-pass".to_string(),
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        mcp_anonymous: notedthat_server::config::McpAnonymous::Auto,
        max_patchable_size: 10 * 1024 * 1024,
        ready_probe_interval_ms: 5_000,
        mcp_max_read_bytes: 16 * 1024 * 1024,
        mcp_max_sessions: notedthat_mcp::DEFAULT_MAX_SESSIONS,
        request_bounds: notedthat_server::config::RequestBoundsConfig::default(),
        staging: notedthat_core::StagingConfig::default(),
        oidc: None,
    }
}

/// Run the server to its first failure, bounded: every test here expects startup to
/// end, and a server that starts serving instead is the regression.
async fn startup_error(config: Config) -> String {
    let result = tokio::time::timeout(Duration::from_secs(30), notedthat_server::run::run(config))
        .await
        .expect("startup must end rather than serve");
    format!("{:#}", result.expect_err("startup must fail"))
}

#[tokio::test]
async fn a_backend_that_ignores_preconditions_refuses_startup() {
    let server = backend_ignoring_preconditions().await;

    let error = startup_error(config(&server.uri(), false)).await;

    for needle in [
        "'notes'",
        "If-Match",
        "silently lost",
        "NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES",
    ] {
        assert!(error.contains(needle), "{error:?} should name {needle}");
    }
    assert!(
        !error.contains("NOTEDTHAT_QDRANT_URL"),
        "refused before provisioning reached Qdrant: {error}"
    );
}

#[tokio::test]
async fn the_opt_out_gets_past_the_check() {
    let server = backend_ignoring_preconditions().await;

    let error = startup_error(config(&server.uri(), true)).await;

    assert!(
        !error.contains("NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES"),
        "the opt-out accepted the backend: {error}"
    );
    assert!(
        error.contains("NOTEDTHAT_QDRANT_URL"),
        "startup went on to provisioning and stopped at the unroutable Qdrant: {error}"
    );
    let probe_puts = server
        .received_requests()
        .await
        .expect("request recording")
        .iter()
        .filter(|r| {
            r.url
                .path()
                .contains("/.notedthat/conditional-write-probe-")
        })
        .count();
    assert!(probe_puts > 0, "the check ran before provisioning failed");
}

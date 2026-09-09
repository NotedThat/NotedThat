//! In-process backends for the server E2E suites.
//!
//! These used to start a `SeaweedFS` container, a Qdrant container and a wiremock
//! embedder per test. The subject of those tests is the server — its routes,
//! its MCP and `WebDAV` surfaces, its startup and shutdown — not any backend's
//! wire protocol, so the whole runtime is now assembled from
//! [`notedthat_server::run::Backends`] over in-memory implementations and handed
//! to `run_with`. Everything else about startup is the real path: the same
//! provisioning, the same indexer worker, the same listeners.

use notedthat_api_http::testing::InMemoryStorage;
use notedthat_core::{KbSlug, TenantSlug};
use notedthat_indexer::testing::{InMemoryVectorStore, StubEmbedder};
use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
use notedthat_server::run::Backends;
use std::collections::BTreeMap;
use std::sync::Arc;

use super::API_TOKEN;

/// Vector width the stub embedder and the provisioned collection agree on.
const EMBEDDING_DIM: u32 = 4;

pub(super) struct RuntimeParts {
    pub(super) config: Config,
    pub(super) kb: String,
    pub(super) backends: Backends,
}

#[derive(Clone, Copy)]
struct ListenerAddrs {
    http: std::net::SocketAddr,
}

pub(super) fn start_runtime(max_patchable_size: u64) -> RuntimeParts {
    let listeners = ListenerAddrs {
        http: notedthat_api_http::testing::reserve_addr(),
    };
    let kb = unique_kb();
    let config = test_config(&kb, listeners, max_patchable_size);

    RuntimeParts {
        config,
        kb,
        backends: in_memory_backends(),
    }
}

/// Storage, vector store and embedder, all in-process.
pub(super) fn in_memory_backends() -> Backends {
    Backends {
        storage: Arc::new(InMemoryStorage::default()),
        store: Arc::new(InMemoryVectorStore::new()),
        embedder: Arc::new(StubEmbedder::new(EMBEDDING_DIM as usize)),
    }
}

/// Config for a server whose backends are injected.
///
/// The S3 and Qdrant sections still have to be populated — `Config` is the
/// production type — but nothing reads them, because `run_with` never builds a
/// client from them. They are pointed at unroutable placeholders so that a
/// regression which *does* reach for them fails loudly instead of quietly
/// talking to something real.
fn test_config(kb: &str, listeners: ListenerAddrs, max_patchable_size: u64) -> Config {
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
        storage: notedthat_server::config::unroutable_storage_placeholder(),
        log_format: LogFormat::Pretty,
        qdrant: ServerQdrantConfig {
            url: "http://127.0.0.1:1".to_string(),
            api_key: None,
            timeout_ms: 30_000,
            connect_timeout_ms: 10_000,
        },
        embedder: EmbedderConfig {
            endpoint_url: "http://127.0.0.1:1".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dimensions: EMBEDDING_DIM,
            batch_size: 1,
            timeout_ms: 30_000,
            max_retries: 3,
            max_input_tokens: 8192,
        },
        webdav_username: "e2e-webdav-user".to_string(),
        webdav_password: "e2e-webdav-pass".to_string(),
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec![
            "127.0.0.1".to_string(),
            "localhost".to_string(),
            "::1".to_string(),
        ],
        max_patchable_size,
        staging: notedthat_core::StagingConfig::default(),
    }
}

fn unique_kb() -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    format!("patch-{nonce}")
}

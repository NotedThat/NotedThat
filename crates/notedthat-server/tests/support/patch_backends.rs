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
use notedthat_core::{EventPublisher, KbSlug};
use notedthat_indexer::testing::{InMemoryVectorStore, StubEmbedder};
use notedthat_server::config::{Config, EmbedderConfig};
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
    start_runtime_with_events(max_patchable_size, None)
}

/// A runtime over an event log, so the events route has something to serve.
pub(super) fn start_runtime_with_events(
    max_patchable_size: u64,
    events: Option<Arc<dyn EventPublisher>>,
) -> RuntimeParts {
    start_runtime_with_backends(
        max_patchable_size,
        Backends {
            events,
            ..in_memory_backends()
        },
    )
}

/// A runtime over exactly `backends`.
pub(super) fn start_runtime_with_backends(
    max_patchable_size: u64,
    backends: Backends,
) -> RuntimeParts {
    let listeners = ListenerAddrs {
        http: notedthat_api_http::testing::reserve_addr(),
    };
    let kb = unique_kb();
    let config = test_config(&kb, listeners, max_patchable_size);

    RuntimeParts {
        config,
        kb,
        backends,
    }
}

/// Storage, vector store and embedder, all in-process.
pub(super) fn in_memory_backends() -> Backends {
    Backends {
        storage: Arc::new(InMemoryStorage::default()),
        store: Arc::new(InMemoryVectorStore::new()),
        embedder: Arc::new(StubEmbedder::new(EMBEDDING_DIM as usize)),
        events: None,
    }
}

/// Config for a server whose backends are injected.
///
/// `run_with` never builds a client from the storage, Qdrant or embedder
/// sections, so they keep [`Config::for_tests`]'s unroutable endpoints; only
/// the embedder's vector width has to match the injected stores.
fn test_config(kb: &str, listeners: ListenerAddrs, max_patchable_size: u64) -> Config {
    let mut kbs = BTreeMap::new();
    kbs.insert(
        kb.to_string(),
        KbSlug::try_new(kb).expect("test KB slug is valid"),
    );

    Config {
        api_token: API_TOKEN.to_string(),
        kbs,
        listen_addr: listeners.http,
        webdav_username: "e2e-webdav-user".to_string(),
        webdav_password: "e2e-webdav-pass".to_string(),
        max_patchable_size,
        embedder: EmbedderConfig {
            dimensions: EMBEDDING_DIM,
            batch_size: 1,
            ..Config::for_tests().embedder
        },
        ..Config::for_tests()
    }
}

fn unique_kb() -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    format!("patch-{nonce}")
}

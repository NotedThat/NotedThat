//! In-process backends for the server E2E suites.
//!
//! These used to start a `SeaweedFS` container, a Qdrant container and a wiremock
//! embedder per test. The subject of those tests is the server — its routes,
//! its MCP and `WebDAV` surfaces, its startup and shutdown — not any backend's
//! wire protocol, so the whole runtime is now assembled from
//! [`crate::run::Backends`] over in-memory implementations and handed
//! to [`run_with`](crate::run::run_with). Everything else about startup is the real path: the same
//! provisioning, the same indexer worker, the same listeners.

use crate::config::{Config, EmbedderConfig};
use crate::run::Backends;
use notedthat_api_http::testing::InMemoryStorage;
use notedthat_core::KbSlug;
use notedthat_indexer::testing::{InMemoryVectorStore, StubEmbedder};
use std::collections::BTreeMap;
use std::sync::Arc;

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

/// A runtime over exactly `backends`, accepting `api_token` as its service token.
pub(super) fn runtime(
    api_token: &str,
    max_patchable_size: u64,
    backends: Backends,
) -> RuntimeParts {
    let listeners = ListenerAddrs {
        http: notedthat_api_http::testing::reserve_addr(),
    };
    let kb = unique_kb();
    let config = test_config(api_token, &kb, listeners, max_patchable_size);

    RuntimeParts {
        config,
        kb,
        backends,
    }
}

/// Storage, vector store and embedder, all in-process.
pub fn in_memory_backends() -> Backends {
    Backends {
        storage: Arc::new(InMemoryStorage::default()),
        store: Arc::new(InMemoryVectorStore::new()),
        embedder: Arc::new(StubEmbedder::new(EMBEDDING_DIM as usize)),
        events: None,
    }
}

/// Config for a server whose backends are injected; the rest is
/// [`Config::for_tests`], whose backend addresses are unroutable.
fn test_config(
    api_token: &str,
    kb: &str,
    listeners: ListenerAddrs,
    max_patchable_size: u64,
) -> Config {
    let base = Config::for_tests();
    Config {
        api_token: api_token.to_string(),
        kbs: BTreeMap::from([(
            kb.to_string(),
            KbSlug::try_new(kb).expect("test KB slug is valid"),
        )]),
        listen_addr: listeners.http,
        embedder: EmbedderConfig {
            dimensions: EMBEDDING_DIM,
            batch_size: 1,
            ..base.embedder
        },
        max_patchable_size,
        ..base
    }
}

fn unique_kb() -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_nanos();
    format!("patch-{nonce}")
}

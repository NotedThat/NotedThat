//! Shared application state for the axum router.

use notedthat_core::{AccessPolicy, Authenticator, EventPublisher, EventSource, KbSlug, Storage};
use notedthat_indexer::Searcher;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Application state shared across all axum handlers.
///
/// This is cloned cheaply for each request (all fields are behind [`Arc`]).
#[derive(Clone)]
pub struct AppState {
    /// The backing storage implementation (injected at startup).
    pub storage: Arc<dyn Storage>,
    /// Canonical map of slug string → [`KbSlug`] for declared knowledge bases.
    pub declared_kbs: Arc<BTreeMap<String, KbSlug>>,
    /// Startup snapshot of manifest access policies, keyed by KB slug.
    ///
    /// One `Arc` per policy so a request-scoped authorization handle can hold
    /// its knowledge base's policy cheaply instead of borrowing the whole map.
    pub access_policies: Arc<BTreeMap<String, Arc<AccessPolicy>>>,
    /// The credential rules every request's principal is resolved through.
    pub authenticator: Arc<Authenticator>,
    /// Maximum accepted PUT body size in bytes (16 MiB in M2).
    pub max_body_size: u64,
    /// Maximum object size eligible for patch operations, in bytes.
    pub max_patchable_size: u64,
    /// Sender half of the async indexing queue.
    pub indexer_tx: tokio::sync::mpsc::Sender<notedthat_indexer::IndexEvent>,
    /// The search implementation (injected at startup).
    pub searcher: Arc<dyn Searcher>,
    /// The object change event log, when `NOTEDTHAT_EVENTS_BACKEND` selects one.
    /// `None` is the default: writes are not announced and the events route
    /// answers 404.
    pub events: Option<Arc<dyn EventPublisher>>,
}

impl AppState {
    /// Where a write made through this surface is reported, attributed to
    /// `source` — the HTTP API itself, or MCP when the request says so.
    pub(crate) fn sinks(&self, source: EventSource) -> notedthat_write::WriteSinks<'_> {
        notedthat_write::WriteSinks::new(&self.indexer_tx, self.events.as_deref(), source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::InMemoryStorage;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn minimal_state(tx: tokio::sync::mpsc::Sender<notedthat_indexer::IndexEvent>) -> AppState {
        AppState {
            storage: Arc::new(InMemoryStorage::default()),
            declared_kbs: Arc::new(BTreeMap::new()),
            access_policies: Arc::new(BTreeMap::new()),
            authenticator: Arc::new(Authenticator::new("token")),
            max_body_size: 1024,
            max_patchable_size: 1024,
            indexer_tx: tx,
            searcher: Arc::new(crate::testing::NoopSearcher),
            events: None,
        }
    }

    #[tokio::test]
    async fn clone_shares_same_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let state = minimal_state(tx);
        let cloned = state.clone();

        let event = notedthat_indexer::IndexEvent::Tombstone {
            kb: notedthat_core::KbSlug::try_new("test").expect("valid kb slug"),
            object_key: notedthat_core::ObjectPath::try_from("a.md").expect("valid path"),
        };
        cloned
            .indexer_tx
            .send(event.clone())
            .await
            .expect("send on cloned tx");

        let received = rx.recv().await.expect("receive on original rx");
        assert_eq!(
            received, event,
            "cloned Sender must share the same underlying channel"
        );
    }
}

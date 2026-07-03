//! Shared application state for the axum router.

use notedthat_core::{KbSlug, Storage};
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
    /// Static Bearer token for authenticating API requests.
    pub bearer_token: Arc<String>,
    /// Maximum accepted PUT body size in bytes (16 MiB in M2).
    pub max_body_size: u64,
    /// Sender half of the async indexing queue.
    pub indexer_tx: tokio::sync::mpsc::Sender<notedthat_indexer::IndexEvent>,
    /// The search implementation (injected at startup).
    pub searcher: Arc<dyn Searcher>,
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
            bearer_token: Arc::new("token".to_string()),
            max_body_size: 1024,
            indexer_tx: tx,
            searcher: Arc::new(crate::testing::NoopSearcher),
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

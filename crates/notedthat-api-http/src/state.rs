//! Shared application state for the axum router.

use notedthat_core::{
    AccessPolicy, Authenticator, EventPublisher, EventSource, KbDetails, KbSlug, Storage,
};
use notedthat_indexer::{IndexHealth, IndexQueueSender, Searcher};

use crate::readiness::ReadinessReceiver;
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
    /// Startup snapshot of each manifest's display name and description,
    /// keyed by KB slug: what `GET /api/v1/knowledgebases` shows (#98).
    pub kb_details: Arc<BTreeMap<String, KbDetails>>,
    /// The credential rules every request's principal is resolved through.
    pub authenticator: Arc<Authenticator>,
    /// Maximum accepted PUT body size in bytes (16 MiB in M2).
    pub max_body_size: u64,
    /// Maximum object size eligible for patch operations, in bytes.
    pub max_patchable_size: u64,
    /// Sender half of the async indexing queue.
    pub indexer_tx: IndexQueueSender,
    /// The search implementation (injected at startup).
    pub searcher: Arc<dyn Searcher>,
    /// The object change event log, when `NOTEDTHAT_EVENTS_BACKEND` selects one.
    /// `None` is the default: writes are not announced and the events route
    /// answers 404.
    pub events: Option<Arc<dyn EventPublisher>>,
    /// The per-knowledge-base index health record, shared with the indexer
    /// worker and every write path (#97).
    pub index_health: Arc<IndexHealth>,
    /// The latest background probe of the storage and vector backends.
    /// `/readyz` reads it and never probes inline.
    pub readiness: ReadinessReceiver,
    /// What `POST …/index/reconcile` starts (D67): a comparison of one
    /// knowledge base's storage against the index, run by the server. `None`
    /// where the backend has no on-demand pass — the `fs` backend today, whose
    /// watcher covers it — and the route answers `404`.
    pub reconcile: Option<Arc<dyn ReconcileTrigger>>,
}

/// A knowledge base already has a pass running; a second one now would not see
/// what the first is past, so the caller retries once `last_reconcile` moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileBusy;

/// Starts a reconciliation pass for one knowledge base (D67).
///
/// Synchronous on purpose: an implementation only claims the knowledge base's
/// slot and spawns the pass, so the route answers before any bucket is listed.
pub trait ReconcileTrigger: Send + Sync {
    /// Start a pass, or report that one is already running.
    ///
    /// # Errors
    ///
    /// [`ReconcileBusy`] when the knowledge base's previous pass has not finished.
    fn trigger(&self, kb: &KbSlug) -> Result<(), ReconcileBusy>;
}

impl AppState {
    /// Where a write made through this surface is reported, attributed to
    /// `source` — the HTTP API itself, or MCP when the request says so.
    pub(crate) fn sinks(&self, source: EventSource) -> notedthat_write::WriteSinks<'_> {
        notedthat_write::WriteSinks::new(
            &self.indexer_tx,
            self.events.as_deref(),
            &self.index_health,
            source,
        )
    }
}

#[cfg(any(test, feature = "test-support"))]
impl AppState {
    /// A complete state for tests, to spread into a fixture with
    /// `AppState { searcher, ..AppState::for_tests(storage, kbs) }`.
    ///
    /// A fixture names only what its test depends on, and a new field is added
    /// here rather than in every fixture. Every declared knowledge base is open
    /// to any signed-in caller and shows its slug as its name; the bearer token
    /// is `test-token`; both size limits are 16 MiB. The index queue is closed:
    /// its receiver is dropped, so a write still succeeds but logs
    /// `INDEX_QUEUE_CLOSED` and marks the worker stopped in `index_health`. A
    /// test that inspects what was enqueued, or reads the index status after a
    /// write, brings its own `indexer_tx`. Search finds nothing, every backend
    /// reports ready, and there is no event log and no reconciler.
    #[must_use]
    pub fn for_tests(storage: Arc<dyn Storage>, declared_kbs: BTreeMap<String, KbSlug>) -> Self {
        let (indexer_tx, _) = tokio::sync::mpsc::channel(1);
        Self {
            storage,
            access_policies: Arc::new(notedthat_core::signed_in_policies(&declared_kbs)),
            kb_details: Arc::new(notedthat_core::slug_kb_details(&declared_kbs)),
            declared_kbs: Arc::new(declared_kbs),
            authenticator: Arc::new(Authenticator::new("test-token")),
            max_body_size: 16 * 1024 * 1024,
            max_patchable_size: 16 * 1024 * 1024,
            indexer_tx: IndexQueueSender::from_mpsc(indexer_tx),
            searcher: Arc::new(crate::testing::NoopSearcher),
            events: None,
            index_health: Arc::new(IndexHealth::new()),
            readiness: crate::testing::ready_receiver(),
            reconcile: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::InMemoryStorage;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[allow(clippy::needless_pass_by_value)]
    fn minimal_state(tx: tokio::sync::mpsc::Sender<notedthat_indexer::IndexEvent>) -> AppState {
        AppState {
            indexer_tx: (&tx).into(),
            ..AppState::for_tests(Arc::new(InMemoryStorage::default()), BTreeMap::new())
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

//! Test helpers for the HTTP API crate.
//!
//! The in-memory [`Storage`] fake and the [`compute_etag`] helper live in
//! [`notedthat_core::testing`] and are re-exported here for backward
//! compatibility with existing test imports. The `Searcher` fakes and the
//! `AppState` factories stay here because they are `api-http`-shaped.
//!
//! Only available when the `test-support` feature is enabled or under
//! `cfg(test)`. **Never enable `test-support` in production builds.**

use async_trait::async_trait;
use notedthat_core::{KbSlug, Storage};
use std::collections::BTreeMap;
use std::sync::Arc;

// Re-export the fake `Storage` implementation and the ETag helper from
// `notedthat-core` so existing imports (`notedthat_api_http::testing::InMemoryStorage`,
// `notedthat_api_http::testing::compute_etag`) continue to compile unchanged.
pub use notedthat_core::testing::{InMemoryStorage, compute_etag, reserve_addr};

/// A `Searcher` that always returns an empty `SearchResponse`.
/// Used as the default searcher in test `AppState` instances so existing tests
/// don't need to mock the search path.
pub struct NoopSearcher;

#[async_trait]
impl notedthat_indexer::Searcher for NoopSearcher {
    async fn search(
        &self,
        _kb: &KbSlug,
        _request: notedthat_core::search::ValidatedRequest,
    ) -> Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError> {
        Ok(notedthat_core::search::SearchResponse::empty())
    }
}

/// A scriptable `Searcher` for unit tests. Pre-load responses via `push_response`.
#[cfg(feature = "test-support")]
pub struct MockSearcher {
    responses: std::sync::Mutex<
        std::collections::VecDeque<
            Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError>,
        >,
    >,
}

#[cfg(feature = "test-support")]
impl MockSearcher {
    /// Create a new empty `MockSearcher`.
    pub fn new() -> Self {
        Self {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// Push a response to the queue. Responses are returned in FIFO order.
    pub fn push_response(
        &self,
        r: Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError>,
    ) {
        self.responses.lock().unwrap().push_back(r);
    }

    /// Alias for `push_response` — matches the plan's specified API.
    pub fn set_response(
        &self,
        r: Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError>,
    ) {
        self.push_response(r);
    }
}

#[cfg(feature = "test-support")]
#[async_trait]
impl notedthat_indexer::Searcher for MockSearcher {
    async fn search(
        &self,
        _kb: &KbSlug,
        _request: notedthat_core::search::ValidatedRequest,
    ) -> Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(notedthat_core::search::SearchResponse::empty()))
    }
}

#[cfg(feature = "test-support")]
impl Default for MockSearcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a test [`crate::state::AppState`] discarding the indexer receiver.
pub fn test_app_state_with_default_channel(
    storage: Arc<dyn Storage>,
    declared_kbs: Arc<BTreeMap<String, KbSlug>>,
    bearer_token: Arc<String>,
    max_body_size: u64,
) -> crate::state::AppState {
    let (indexer_tx, _) = tokio::sync::mpsc::channel(1024);
    crate::state::AppState {
        storage,
        declared_kbs,
        public_read_policies: Arc::new(BTreeMap::new()),
        bearer_token,
        max_body_size,
        max_patchable_size: max_body_size,
        indexer_tx,
        searcher: Arc::new(NoopSearcher),
    }
}

/// Build a test [`crate::state::AppState`] returning both the state and the indexer receiver.
pub fn test_app_state_with_channel(
    storage: Arc<dyn Storage>,
    declared_kbs: Arc<BTreeMap<String, KbSlug>>,
    bearer_token: Arc<String>,
    max_body_size: u64,
) -> (
    crate::state::AppState,
    tokio::sync::mpsc::Receiver<notedthat_indexer::IndexEvent>,
) {
    let (indexer_tx, rx) = tokio::sync::mpsc::channel(1024);
    (
        crate::state::AppState {
            storage,
            declared_kbs,
            public_read_policies: Arc::new(BTreeMap::new()),
            bearer_token,
            max_body_size,
            max_patchable_size: max_body_size,
            indexer_tx,
            searcher: Arc::new(NoopSearcher),
        },
        rx,
    )
}

//! Test helpers for the HTTP API crate.
//!
//! The in-memory [`Storage`](notedthat_core::Storage) fake and the [`compute_etag`] helper live in
//! [`notedthat_core::testing`] and are re-exported here for backward
//! compatibility with existing test imports. The `Searcher` fakes and the
//! readiness receivers stay here because they are `api-http`-shaped; a whole
//! test state comes from [`crate::state::AppState::for_tests`].
//!
//! Only available when the `test-support` feature is enabled or under
//! `cfg(test)`. **Never enable `test-support` in production builds.**

use async_trait::async_trait;
use notedthat_core::KbSlug;

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
        _key_filter: Option<notedthat_indexer::KeyPredicate<'_>>,
    ) -> Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError> {
        Ok(notedthat_core::search::SearchResponse::empty())
    }
}

/// A scriptable `Searcher` for unit tests. Pre-load responses via `push_response`.
///
/// It honours the key predicate it is handed the way `HybridSearcher` does —
/// a scripted hit outside the caller's grant is withheld — and counts the
/// calls that carried one, so a test can tell "the route filtered the
/// response" from "the route delegated the grant to the searcher".
#[cfg(feature = "test-support")]
pub struct MockSearcher {
    responses: std::sync::Mutex<
        std::collections::VecDeque<
            Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError>,
        >,
    >,
    calls: std::sync::atomic::AtomicUsize,
    scoped_calls: std::sync::atomic::AtomicUsize,
}

#[cfg(feature = "test-support")]
impl MockSearcher {
    /// Create a new empty `MockSearcher`.
    pub fn new() -> Self {
        Self {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            scoped_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// How many times [`notedthat_indexer::Searcher::search`] has been called.
    ///
    /// Lets a test assert that a refused request never reached the backend —
    /// which is the difference between "returns no hits" and "costs no
    /// embedding round-trip".
    pub fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// How many of those calls carried a key predicate.
    ///
    /// The route hands the caller's `search` grant to the searcher so that it
    /// is applied before the page is cut (D56); this is how a test proves the
    /// hand-over happened rather than only that the response came out right.
    pub fn scoped_calls(&self) -> usize {
        self.scoped_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Push a response to the queue. Responses are returned in FIFO order.
    ///
    /// # Panics
    ///
    /// Panics if a previous holder of the response queue's lock panicked.
    pub fn push_response(
        &self,
        r: Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError>,
    ) {
        self.responses.lock().unwrap().push_back(r);
    }
}

#[cfg(feature = "test-support")]
#[async_trait]
impl notedthat_indexer::Searcher for MockSearcher {
    async fn search(
        &self,
        _kb: &KbSlug,
        _request: notedthat_core::search::ValidatedRequest,
        key_filter: Option<notedthat_indexer::KeyPredicate<'_>>,
    ) -> Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if key_filter.is_some() {
            self.scoped_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        let mut response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(notedthat_core::search::SearchResponse::empty()))?;
        if let Some(allows) = key_filter {
            response.hits.retain(|hit| allows(hit.object_key.as_str()));
        }
        Ok(response)
    }
}

#[cfg(feature = "test-support")]
impl Default for MockSearcher {
    fn default() -> Self {
        Self::new()
    }
}

/// A readiness receiver reporting every backend ok, with no poller behind it.
///
/// The sender is dropped, which a `watch` receiver survives: `borrow()` keeps
/// answering with this snapshot for the state's whole life.
#[must_use]
pub fn ready_receiver() -> crate::readiness::ReadinessReceiver {
    receiver_for(crate::readiness::ReadinessSnapshot::ok("memory", "memory"))
}

/// A readiness receiver pinned to `snapshot`, for a route test that needs a
/// backend to look unavailable.
#[must_use]
pub fn receiver_for(
    snapshot: crate::readiness::ReadinessSnapshot,
) -> crate::readiness::ReadinessReceiver {
    let (_sender, receiver) = tokio::sync::watch::channel(snapshot);
    receiver
}

/// A [`ReconcileTrigger`](crate::state::ReconcileTrigger) that records what it
/// was asked and answers busy on demand, for route tests.
#[derive(Debug, Default)]
pub struct RecordingReconcile {
    /// Every slug a trigger was accepted for, in order.
    pub triggered: std::sync::Mutex<Vec<String>>,
    /// While set, every trigger answers [`crate::state::ReconcileBusy`].
    pub busy: std::sync::atomic::AtomicBool,
}

impl crate::state::ReconcileTrigger for RecordingReconcile {
    fn trigger(&self, kb: &KbSlug) -> Result<(), crate::state::ReconcileBusy> {
        if self.busy.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::state::ReconcileBusy);
        }
        self.triggered
            .lock()
            .expect("mutex not poisoned")
            .push(kb.as_str().to_string());
        Ok(())
    }
}

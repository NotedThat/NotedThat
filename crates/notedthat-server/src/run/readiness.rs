//! The background prober behind `/readyz` (D57).
//!
//! Every `NOTEDTHAT_READY_PROBE_INTERVAL_MS` the storage backend and the vector
//! store are asked whether they are there — `Storage::probe` on the first
//! declared knowledge base's bucket, `VectorStore::probe` — with each answer
//! bounded by that same interval, and the result is published as a
//! [`ReadinessSnapshot`] on a `watch` channel the HTTP state holds. The route
//! only ever reads the latest value, so a probe storm from an orchestrator
//! costs the backends nothing, and an outage is reported within two intervals:
//! one of waiting for the next tick, one for the probe to time out.
//!
//! A backend's own error is logged, once when a check fails and once when it
//! recovers, and never published: the route is unauthenticated, and a client
//! error can quote the endpoint it failed to reach.

use std::sync::Arc;
use std::time::Duration;

use notedthat_api_http::readiness::{Check, ReadinessReceiver, ReadinessSnapshot, Unready};
use notedthat_core::{KbSlug, Storage, StorageError};
use notedthat_indexer::{VectorStore, VectorStoreError};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// The selector value the vector store answers to. There is one.
const SEARCH_BACKEND: &str = "qdrant";

pub(super) struct ReadinessPoller {
    storage: Arc<dyn Storage>,
    store: Arc<dyn VectorStore>,
    /// The knowledge base whose bucket stands in for "storage is reachable".
    witness: KbSlug,
    interval: Duration,
    tx: watch::Sender<ReadinessSnapshot>,
}

/// One probe's failure, for the log line and nothing else.
struct Detail {
    check: &'static str,
    error: String,
}

/// Fold a bounded probe into its check, keeping the error text for the log.
fn settle<E: std::fmt::Display>(
    check: &'static str,
    backend: &'static str,
    result: Result<Result<(), E>, tokio::time::error::Elapsed>,
    classify: impl FnOnce(&E) -> Unready,
    details: &mut Vec<Detail>,
) -> Check {
    let (reason, error) = match result {
        Ok(Ok(())) => return Check::ok(backend),
        Ok(Err(error)) => (classify(&error), error.to_string()),
        Err(elapsed) => (Unready::Timeout, elapsed.to_string()),
    };
    details.push(Detail { check, error });
    Check::unready(backend, reason)
}

impl ReadinessPoller {
    /// Publishes every check ok at once — startup provisioning has just reached
    /// both backends — and hands back the receiver the HTTP state holds.
    pub(super) fn new(
        storage: Arc<dyn Storage>,
        store: Arc<dyn VectorStore>,
        witness: KbSlug,
        storage_backend: &'static str,
        interval: Duration,
    ) -> (Self, ReadinessReceiver) {
        let (tx, rx) = watch::channel(ReadinessSnapshot::ok(storage_backend, SEARCH_BACKEND));
        (
            Self {
                storage,
                store,
                witness,
                interval,
                tx,
            },
            rx,
        )
    }

    /// Probe until `shutdown` is cancelled. The first probe runs at once.
    pub(super) async fn run(self, shutdown: CancellationToken) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                _ = ticker.tick() => {
                    let (next, details) = self.probe_once().await;
                    self.publish(next, &details);
                }
            }
        }
    }

    async fn probe_once(&self) -> (ReadinessSnapshot, Vec<Detail>) {
        let deadline = self.interval;
        let (storage, search) = tokio::join!(
            tokio::time::timeout(deadline, self.storage.probe(&self.witness)),
            tokio::time::timeout(deadline, self.store.probe()),
        );
        let storage_backend = self.tx.borrow().storage.backend;
        let mut details = Vec::new();
        let storage = settle(
            "storage",
            storage_backend,
            storage,
            |error| match error {
                StorageError::BucketNotFound { .. } => Unready::NotFound,
                _ => Unready::Unreachable,
            },
            &mut details,
        );
        let search = settle(
            "search",
            SEARCH_BACKEND,
            search,
            |error| match error {
                VectorStoreError::CollectionNotFound { .. } => Unready::NotFound,
                VectorStoreError::Backend { .. } => Unready::Unreachable,
            },
            &mut details,
        );
        (ReadinessSnapshot { storage, search }, details)
    }

    /// Publish `next`, logging each check that changed state — not each tick,
    /// so a long outage is one line, not one per interval.
    fn publish(&self, next: ReadinessSnapshot, details: &[Detail]) {
        let previous = self.tx.borrow().clone();
        for (name, before, after) in [
            ("storage", &previous.storage, &next.storage),
            ("search", &previous.search, &next.search),
        ] {
            match (&before.outcome, &after.outcome) {
                (Err(_), Ok(())) => info!(
                    target: "notedthat::readiness",
                    check = name,
                    backend = after.backend,
                    "READINESS_RESTORED"
                ),
                (before, Err(reason)) if before.as_ref().err() != Some(reason) => warn!(
                    target: "notedthat::readiness",
                    check = name,
                    backend = after.backend,
                    reason = reason.as_str(),
                    error = details
                        .iter()
                        .find(|d| d.check == name)
                        .map_or("", |d| d.error.as_str()),
                    "READINESS_LOST: /readyz answers 503 until this backend answers again"
                ),
                _ => {}
            }
        }
        self.tx.send_replace(next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notedthat_core::testing::InMemoryStorage;
    use notedthat_indexer::testing::InMemoryVectorStore;

    const TICK: Duration = Duration::from_millis(10);
    const PATIENCE: Duration = Duration::from_secs(5);

    fn kb() -> KbSlug {
        KbSlug::try_new("notes").unwrap()
    }

    async fn provisioned() -> (InMemoryStorage, InMemoryVectorStore) {
        let storage = InMemoryStorage::default();
        storage.ensure_bucket(&kb()).await.unwrap();
        (storage, InMemoryVectorStore::new())
    }

    fn start(
        storage: Arc<dyn Storage>,
        store: Arc<dyn VectorStore>,
    ) -> (
        ReadinessReceiver,
        CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        let (poller, rx) = ReadinessPoller::new(storage, store, kb(), "fs", TICK);
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(poller.run(shutdown.child_token()));
        (rx, shutdown, handle)
    }

    /// Wait, bounded, until the snapshot satisfies `expected`.
    async fn wait_for(
        rx: &mut ReadinessReceiver,
        what: &str,
        expected: impl Fn(&ReadinessSnapshot) -> bool,
    ) -> ReadinessSnapshot {
        let waited = tokio::time::timeout(PATIENCE, rx.wait_for(|s| expected(s)))
            .await
            .map(|guard| guard.expect("poller still running").clone());
        match waited {
            Ok(snapshot) => snapshot,
            Err(elapsed) => {
                panic!(
                    "timed out ({elapsed}) waiting for {what}: {:?}",
                    rx.borrow()
                )
            }
        }
    }

    #[tokio::test]
    async fn starts_ready_with_the_configured_labels() {
        let (storage, store) = provisioned().await;
        let (rx, shutdown, handle) = start(Arc::new(storage), Arc::new(store));
        let snapshot = rx.borrow().clone();
        assert_eq!(snapshot, ReadinessSnapshot::ok("fs", "qdrant"));
        assert!(snapshot.is_ready());
        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn search_going_away_and_coming_back_is_reported_without_a_restart() {
        let (storage, store) = provisioned().await;
        let (mut rx, shutdown, handle) = start(Arc::new(storage), Arc::new(store.clone()));

        store.set_reachable(false);
        let down = wait_for(&mut rx, "search unreachable", |s| {
            s.search.outcome == Err(Unready::Unreachable)
        })
        .await;
        assert_eq!(down.storage, Check::ok("fs"), "storage is unaffected");
        assert!(!down.is_ready());

        store.set_reachable(true);
        let back = wait_for(&mut rx, "search ok again", |s| s.search.outcome.is_ok()).await;
        assert!(back.is_ready());

        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn storage_going_away_and_coming_back_is_reported_without_a_restart() {
        let (storage, store) = provisioned().await;
        let (mut rx, shutdown, handle) = start(Arc::new(storage.clone()), Arc::new(store));

        storage.set_reachable(false);
        let down = wait_for(&mut rx, "storage unreachable", |s| {
            s.storage.outcome == Err(Unready::Unreachable)
        })
        .await;
        assert_eq!(down.search, Check::ok("qdrant"));

        storage.set_reachable(true);
        wait_for(&mut rx, "storage ok again", |s| s.storage.outcome.is_ok()).await;

        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn a_missing_witness_bucket_is_not_found() {
        // Never provisioned: the bucket `/readyz` looks for is not there.
        let storage = InMemoryStorage::default();
        let (mut rx, shutdown, handle) =
            start(Arc::new(storage), Arc::new(InMemoryVectorStore::new()));
        wait_for(&mut rx, "storage not_found", |s| {
            s.storage.outcome == Err(Unready::NotFound)
        })
        .await;
        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn a_probe_that_outlasts_the_interval_is_a_timeout() {
        let (storage, store) = provisioned().await;
        store.set_probe_latency(Duration::from_secs(60));
        let (mut rx, shutdown, handle) = start(Arc::new(storage), Arc::new(store));
        let snapshot = wait_for(&mut rx, "search timeout", |s| {
            s.search.outcome == Err(Unready::Timeout)
        })
        .await;
        assert_eq!(snapshot.storage, Check::ok("fs"));
        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn stops_promptly_when_cancelled_mid_probe() {
        let (storage, store) = provisioned().await;
        store.set_probe_latency(Duration::from_secs(60));
        let (_rx, shutdown, handle) = start(Arc::new(storage), Arc::new(store));
        // Give the first tick a moment to enter the hanging probe.
        tokio::time::sleep(Duration::from_millis(2)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("cancellation interrupts an in-flight probe")
            .unwrap();
    }
}

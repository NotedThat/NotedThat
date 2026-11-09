//! The background prober behind `/readyz` (D64).
//!
//! Every `NOTEDTHAT_READY_PROBE_INTERVAL_MS` the storage backend and the vector
//! store are asked whether they are there — `Storage::probe` on the bucket of
//! every declared knowledge base, each on its own, and `VectorStore::probe` — with
//! each answer bounded by that same interval, and the result is published as a
//! [`ReadinessSnapshot`] on a `watch` channel the HTTP state holds. The route
//! only ever reads the latest value, so a probe storm from an orchestrator
//! costs the backends nothing, and an outage is reported within two intervals:
//! one of waiting for the next tick, one for the probe to time out.
//!
//! Each bucket and the vector store have at most one probe in flight. The
//! storage check is the worst of the buckets' latest answers, so a hung bucket
//! makes it `timeout` without holding back the other buckets' probes (#384).
//! A probe that outlasts the
//! interval is reported as `timeout` and then *waited for*, not abandoned and
//! restarted: the `fs` probe is a `spawn_blocking` `stat`, and dropping its
//! future does not free the thread a hung mount is holding, so restarting it
//! every tick would leak one blocked thread per interval until the pool was
//! gone and every storage operation queued behind dead probes. This way a hung
//! backend costs one thread, once, and readiness keeps saying `timeout`.
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
    /// Every declared knowledge base; each bucket is probed every interval (#384).
    kbs: Vec<KbSlug>,
    interval: Duration,
    tx: watch::Sender<ReadinessSnapshot>,
}

/// Fold a bounded probe into its check, with the error text for the log line
/// and nothing else.
fn settle<E: std::fmt::Display>(
    backend: &'static str,
    result: Result<Result<(), E>, tokio::time::error::Elapsed>,
    classify: impl FnOnce(&E) -> Unready,
) -> (Check, Option<String>) {
    match result {
        Ok(Ok(())) => (Check::ok(backend), None),
        Ok(Err(error)) => (
            Check::unready(backend, classify(&error)),
            Some(error.to_string()),
        ),
        Err(elapsed) => (
            Check::unready(backend, Unready::Timeout),
            Some(elapsed.to_string()),
        ),
    }
}

/// A failed bucket probe, naming the knowledge base for the log line.
#[derive(Debug)]
struct KbProbeError {
    kb: KbSlug,
    error: StorageError,
}

impl std::fmt::Display for KbProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kb, self.error)
    }
}

/// A check and the error text for its log line, as `settle` folds them.
type Verdict = (Check, Option<String>);

/// One storage verdict for every knowledge base, the first of the worst: an
/// outage (unreachable, timeout) over a missing bucket over ok.
fn worst(verdicts: &[Verdict]) -> Option<&Verdict> {
    let rank = |(check, _): &&Verdict| match check.outcome {
        Ok(()) => 0,
        Err(reason) if reason.is_outage() => 2,
        Err(_) => 1,
    };
    // `max_by_key` keeps the last of equals; reversed, that is the first.
    verdicts.iter().rev().max_by_key(rank)
}

/// Which half of the snapshot a probe loop owns.
#[derive(Clone, Copy)]
enum Slot {
    Storage,
    Search,
}

impl Slot {
    fn name(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            Self::Search => "search",
        }
    }

    fn of(self, snapshot: &ReadinessSnapshot) -> &Check {
        match self {
            Self::Storage => &snapshot.storage,
            Self::Search => &snapshot.search,
        }
    }

    fn of_mut(self, snapshot: &mut ReadinessSnapshot) -> &mut Check {
        match self {
            Self::Storage => &mut snapshot.storage,
            Self::Search => &mut snapshot.search,
        }
    }
}

impl ReadinessPoller {
    /// Publishes every check ok at once — startup provisioning has just reached
    /// both backends — and hands back the receiver the HTTP state holds.
    ///
    /// `conditional_writes` is what the `s3` backend's startup check found (D70). It
    /// is published as given and never probed again: the loop owns only storage and
    /// search.
    pub(super) fn new(
        storage: Arc<dyn Storage>,
        store: Arc<dyn VectorStore>,
        kbs: Vec<KbSlug>,
        storage_backend: &'static str,
        interval: Duration,
        conditional_writes: Option<Check>,
    ) -> (Self, ReadinessReceiver) {
        let mut initial = ReadinessSnapshot::ok(storage_backend, SEARCH_BACKEND);
        initial.conditional_writes = conditional_writes;
        let (tx, rx) = watch::channel(initial);
        (
            Self {
                storage,
                store,
                kbs,
                interval,
                tx,
            },
            rx,
        )
    }

    /// Probe until `shutdown` is cancelled. The first probe of each backend
    /// runs at once; every bucket and the vector store are probed
    /// independently, so one that hangs never delays another's answer.
    pub(super) async fn run(self, shutdown: CancellationToken) {
        let storage_backend = self.tx.borrow().storage.backend;
        let verdicts =
            std::sync::Mutex::new(vec![(Check::ok(storage_backend), None); self.kbs.len()]);
        let (this, verdicts, shutdown) = (&self, &verdicts, &shutdown);
        let buckets = self.kbs.iter().enumerate().map(|(i, kb)| {
            this.probe_loop(
                storage_backend,
                move || async move {
                    this.storage.probe(kb).await.map_err(|error| KbProbeError {
                        kb: kb.clone(),
                        error,
                    })
                },
                |failed: &KbProbeError| match failed.error {
                    StorageError::BucketNotFound { .. } => Unready::NotFound,
                    _ => Unready::Unreachable,
                },
                shutdown,
                move |check, error| {
                    let mut verdicts = verdicts.lock().expect("no probe panics holding it");
                    verdicts[i] = (check, error.map(str::to_owned));
                    if let Some((check, error)) = worst(&verdicts) {
                        this.publish(Slot::Storage, check.clone(), error.as_deref());
                    }
                },
            )
        });
        let search = self.probe_loop(
            SEARCH_BACKEND,
            || self.store.probe(),
            |error| match error {
                VectorStoreError::CollectionNotFound { .. } => Unready::NotFound,
                VectorStoreError::Backend { .. } => Unready::Unreachable,
            },
            shutdown,
            |check, error| self.publish(Slot::Search, check, error),
        );
        tokio::join!(futures::future::join_all(buckets), search);
    }

    /// One probe's loop: probe, report, sleep an interval, again — with at
    /// most one probe in flight. A probe that outlasts the interval is
    /// reported as `timeout` and then awaited to its end (whatever it
    /// eventually says is reported too), never dropped and started over.
    async fn probe_loop<E, F, Fut>(
        &self,
        backend: &'static str,
        probe: F,
        classify: impl Fn(&E) -> Unready,
        shutdown: &CancellationToken,
        report: impl Fn(Check, Option<&str>),
    ) where
        E: std::fmt::Display,
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<(), E>>,
    {
        loop {
            let fut = probe();
            tokio::pin!(fut);
            let bounded = tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                result = tokio::time::timeout(self.interval, &mut fut) => result,
            };
            let timed_out = bounded.is_err();
            let (check, error) = settle(backend, bounded, &classify);
            report(check, error.as_deref());
            if timed_out {
                // The same probe, to its end: see the module doc.
                let late = tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return,
                    result = &mut fut => result,
                };
                let (check, error) = settle(backend, Ok(late), &classify);
                report(check, error.as_deref());
            }
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(self.interval) => {}
            }
        }
    }

    /// Publish one check, logging it when it changed state — not each tick,
    /// so a long outage is one line, not one per interval.
    fn publish(&self, slot: Slot, after: Check, error: Option<&str>) {
        let previous = slot.of(&self.tx.borrow()).clone();
        let name = slot.name();
        match (&previous.outcome, &after.outcome) {
            (Err(_), Ok(())) => info!(
                target: "notedthat::readiness",
                check = name,
                backend = after.backend,
                "READINESS_RESTORED"
            ),
            (before, Err(reason)) if before.as_ref().err() != Some(reason) => {
                let error = error.unwrap_or("");
                if reason.is_outage() {
                    warn!(
                        target: "notedthat::readiness",
                        check = name,
                        backend = after.backend,
                        reason = reason.as_str(),
                        error,
                        "READINESS_LOST: /readyz answers 503 until this backend answers again"
                    );
                } else {
                    warn!(
                        target: "notedthat::readiness",
                        check = name,
                        backend = after.backend,
                        reason = reason.as_str(),
                        error,
                        "READINESS_DEGRADED: the backend answered, but what /readyz probes for is \
                         gone; /readyz stays 200 and nothing re-creates it while the process runs"
                    );
                }
            }
            _ => {}
        }
        self.tx
            .send_modify(|snapshot| *slot.of_mut(snapshot) = after);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notedthat_core::testing::{InMemoryStorage, ScriptedStorage, StorageCall, StorageOp};
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
        let (poller, rx) = ReadinessPoller::new(storage, store, vec![kb()], "fs", TICK, None);
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

    /// The startup finding on conditional writes is never probed again (D70), so no
    /// storage or search publish may overwrite it.
    #[tokio::test]
    async fn a_conditional_writes_finding_survives_every_probe() {
        let (storage, store) = provisioned().await;
        let finding = Check::unready("s3", Unready::PreconditionsNotEnforced);
        let (poller, mut rx) = ReadinessPoller::new(
            Arc::new(storage.clone()),
            Arc::new(store),
            vec![kb()],
            "s3",
            TICK,
            Some(finding.clone()),
        );
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(poller.run(shutdown.child_token()));

        storage.set_reachable(false);
        let down = wait_for(&mut rx, "storage unreachable", |s| {
            s.storage.outcome == Err(Unready::Unreachable)
        })
        .await;
        assert_eq!(down.conditional_writes, Some(finding.clone()));

        storage.set_reachable(true);
        let back = wait_for(&mut rx, "storage ok again", |s| s.storage.outcome.is_ok()).await;
        assert_eq!(back.conditional_writes, Some(finding));
        assert!(back.is_ready() && back.is_degraded());

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
    async fn a_missing_bucket_is_not_found() {
        // Never provisioned: the bucket `/readyz` looks for is not there.
        let storage = InMemoryStorage::default();
        let (mut rx, shutdown, handle) =
            start(Arc::new(storage), Arc::new(InMemoryVectorStore::new()));
        let snapshot = wait_for(&mut rx, "storage not_found", |s| {
            s.storage.outcome == Err(Unready::NotFound)
        })
        .await;
        assert!(
            snapshot.is_ready(),
            "a gone bucket is reported, not an outage"
        );
        shutdown.cancel();
        handle.await.unwrap();
    }

    fn second() -> KbSlug {
        KbSlug::try_new("zeta").unwrap()
    }

    /// Start a poller over `notes` and `zeta`, in the order `Config::kbs` sorts them.
    fn start_two(
        storage: ScriptedStorage,
    ) -> (
        ReadinessReceiver,
        CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        let (poller, rx) = ReadinessPoller::new(
            Arc::new(storage),
            Arc::new(InMemoryVectorStore::new()),
            vec![kb(), second()],
            "fs",
            TICK,
            None,
        );
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(poller.run(shutdown.child_token()));
        (rx, shutdown, handle)
    }

    fn probed(storage: &ScriptedStorage, slug: &str) -> usize {
        storage
            .calls()
            .iter()
            .filter(|call| matches!(call, StorageCall::Probe { kb } if kb == slug))
            .count()
    }

    /// Not only the first knowledge base: a bucket gone from any of them is
    /// reported, every interval, so the alert on the probe's errors fires for it (#384).
    #[tokio::test]
    async fn a_missing_bucket_behind_the_first_knowledge_base_is_not_found() {
        let storage = ScriptedStorage::with_kbs([&kb()]);
        let (mut rx, shutdown, handle) = start_two(storage.clone());
        let snapshot = wait_for(&mut rx, "storage not_found", |s| {
            s.storage.outcome == Err(Unready::NotFound)
        })
        .await;
        assert!(
            snapshot.is_ready(),
            "a gone bucket is reported, not an outage"
        );
        wait_for(&mut rx, "a second interval", |_| {
            probed(&storage, "zeta") >= 2
        })
        .await;
        assert!(
            probed(&storage, "notes") >= 2,
            "every knowledge base, every interval"
        );
        shutdown.cancel();
        handle.await.unwrap();
    }

    /// One knowledge base's hung probe makes the check a timeout within the
    /// interval, and the others keep being probed every interval meanwhile (#384).
    #[tokio::test]
    async fn a_hung_probe_for_one_knowledge_base_is_a_timeout() {
        let storage = ScriptedStorage::with_kbs([&kb(), &second()]);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        storage.before(StorageOp::Probe, move |_| {
            // `join_all` enters the loops in order, so the second call is `zeta`'s.
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                if call == 1 {
                    tokio::time::sleep(Duration::from_mins(1)).await;
                }
            }
        });
        let (mut rx, shutdown, handle) = start_two(storage.clone());
        wait_for(&mut rx, "storage timeout", |s| {
            s.storage.outcome == Err(Unready::Timeout)
        })
        .await;
        wait_for(&mut rx, "notes probed again while zeta hangs", |_| {
            probed(&storage, "notes") >= 3
        })
        .await;
        assert_eq!(
            probed(&storage, "zeta"),
            1,
            "the hung probe is not restarted"
        );
        shutdown.cancel();
        handle.await.unwrap();
    }

    fn missing() -> StorageError {
        StorageError::BucketNotFound {
            bucket: "gone".into(),
        }
    }

    #[test]
    fn the_worst_probe_is_the_storage_verdict() {
        let ok = || (Check::ok("fs"), None);
        let bad = |reason, text: &str| (Check::unready("fs", reason), Some(text.to_owned()));
        assert_eq!(worst(&[ok(), ok()]), Some(&ok()));
        assert_eq!(worst(&[]), None);
        let found = KbProbeError {
            kb: second(),
            error: missing(),
        };
        assert_eq!(found.to_string(), "zeta: bucket not found: gone");
        let missing = bad(Unready::NotFound, "missing");
        assert_eq!(worst(&[ok(), missing.clone()]), Some(&missing));
        // An outage outranks a missing bucket, in either order; equals keep the first.
        let down = bad(Unready::Unreachable, "down");
        let hung = bad(Unready::Timeout, "hung");
        assert_eq!(worst(&[missing.clone(), down.clone()]), Some(&down));
        assert_eq!(worst(&[down.clone(), missing]), Some(&down));
        assert_eq!(worst(&[hung.clone(), down]), Some(&hung));
    }

    #[tokio::test]
    async fn a_probe_that_outlasts_the_interval_is_a_timeout() {
        let (storage, store) = provisioned().await;
        store.set_probe_latency(Duration::from_mins(1));
        let (mut rx, shutdown, handle) = start(Arc::new(storage), Arc::new(store));
        let snapshot = wait_for(&mut rx, "search timeout", |s| {
            s.search.outcome == Err(Unready::Timeout)
        })
        .await;
        assert_eq!(snapshot.storage, Check::ok("fs"));
        shutdown.cancel();
        handle.await.unwrap();
    }

    /// A probe that hangs is reported as `timeout` and then waited for — the
    /// same probe, not a fresh one every tick — so a hung `fs` mount holds one
    /// blocking thread rather than one per interval. When it finally answers,
    /// what it says is published.
    #[tokio::test]
    async fn a_hung_probe_is_not_started_again_until_it_answers() {
        let (storage, store) = provisioned().await;
        store.set_probe_latency(TICK * 10);
        let (mut rx, shutdown, handle) = start(Arc::new(storage), Arc::new(store.clone()));
        wait_for(&mut rx, "search timeout", |s| {
            s.search.outcome == Err(Unready::Timeout)
        })
        .await;

        // Several intervals pass while the first probe is still in flight.
        tokio::time::sleep(TICK * 5).await;
        assert_eq!(
            store.probe_calls(),
            1,
            "a hung probe is awaited, never abandoned and restarted"
        );
        assert_eq!(rx.borrow().search.outcome, Err(Unready::Timeout));

        // It answers at last: its verdict is published, and only then does the
        // next probe go out.
        wait_for(&mut rx, "search ok once the late probe answers", |s| {
            s.search.outcome.is_ok()
        })
        .await;
        assert!(store.probe_calls() >= 1);
        wait_for(&mut rx, "a second probe", |_| store.probe_calls() >= 2).await;

        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_probe_in_flight() {
        // A long interval, so the probe's own deadline cannot be what ends it,
        // and a probe that outlasts the test; the first tick is immediate, so
        // the probe is in flight by the time the poller has been polled once.
        let (storage, store) = provisioned().await;
        store.set_probe_latency(Duration::from_mins(1));
        let (poller, _rx) = ReadinessPoller::new(
            Arc::new(storage),
            Arc::new(store.clone()),
            vec![kb()],
            "fs",
            Duration::from_mins(1),
            None,
        );
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(poller.run(shutdown.child_token()));
        let entered = async {
            while store.probe_calls() == 0 {
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(PATIENCE, entered)
            .await
            .expect("the first tick is immediate, so a probe is entered promptly");

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("cancellation interrupts an in-flight probe")
            .unwrap();
    }
}

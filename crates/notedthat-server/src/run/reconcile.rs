//! Comparing a knowledge base's bucket against the search index on the `s3` backend
//! (D66): once at startup, and whenever the operator asks.
//!
//! S3 has no change feed `NotedThat` can subscribe to portably, so unlike the `fs`
//! backend (D50) nothing is watched; a pass is the only mechanism. What a pass does is
//! the shared comparison in [`notedthat_core::reconcile`]: list the bucket — one `LIST`
//! per thousand keys, each entry carrying its `ETag` — read what the index holds, and
//! enqueue an [`IndexEvent::Refresh`] for every key the two sides disagree on. The
//! worker re-reads each one and skips it if the index already holds that `ETag`, so an
//! unchanged object is never fetched or embedded, and a key gone from the bucket
//! becomes a tombstone through the worker's missing-object path — never a tombstone
//! enqueued from here, for the reason D50 gives.
//!
//! One pass per knowledge base at a time. A second request while one runs is refused
//! rather than queued: the running pass is already past keys a later change could touch,
//! so it would not satisfy the request, and the operator is better told to retry once
//! `last_reconcile` advances.

use std::collections::BTreeMap;
use std::sync::Arc;

use notedthat_api_http::state::{ReconcileBusy, ReconcileTrigger};
use notedthat_core::reconcile::{IndexedEtag, compare, walk_etags};
use notedthat_core::{KbSlug, Storage};
use notedthat_indexer::{IndexEvent, IndexHealth, ReconcileSummary, RefreshOrigin, VectorStore};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{error, info, warn};

/// The indexing queue as a reconciliation pass sees it: every event it enqueues is also
/// counted on the health record, like a write's is (#97).
#[derive(Clone)]
pub(super) struct IndexSink {
    pub(super) tx: mpsc::Sender<IndexEvent>,
    pub(super) health: Arc<IndexHealth>,
}

impl IndexSink {
    /// Enqueue, blocking rather than dropping (D50). `Err` means the worker is
    /// gone and there is nothing left to enqueue to.
    pub(super) async fn send(&self, event: IndexEvent) -> Result<(), ()> {
        let kb = event.kb().as_str().to_string();
        self.tx.send(event).await.map_err(|_| ())?;
        self.health.enqueued(&kb);
        Ok(())
    }
}

pub(super) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Runs reconciliation passes for the `s3` backend, one per knowledge base at a time.
pub(super) struct Reconciler {
    inner: Arc<Inner>,
}

struct Inner {
    storage: Arc<dyn Storage>,
    store: Arc<dyn VectorStore>,
    sink: IndexSink,
    /// One slot per declared knowledge base: whoever holds it is that base's running pass.
    slots: BTreeMap<String, Arc<Mutex<()>>>,
    tracker: TaskTracker,
    cancel: CancellationToken,
}

impl Reconciler {
    pub(super) fn new(
        storage: Arc<dyn Storage>,
        store: Arc<dyn VectorStore>,
        indexer_tx: mpsc::Sender<IndexEvent>,
        health: Arc<IndexHealth>,
        kbs: &[KbSlug],
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Inner {
                storage,
                store,
                sink: IndexSink {
                    tx: indexer_tx,
                    health,
                },
                slots: kbs
                    .iter()
                    .map(|kb| (kb.as_str().to_string(), Arc::new(Mutex::new(()))))
                    .collect(),
                tracker: TaskTracker::new(),
                cancel: CancellationToken::new(),
            }),
        })
    }

    /// Compare every knowledge base once, in the background, one after another.
    ///
    /// Each is marked `stale` first, so `/index` says the comparison is outstanding until
    /// its pass completes (D62). A knowledge base whose slot an operator pass already
    /// holds is left to that pass.
    pub(super) fn spawn_startup_pass(&self, kbs: Vec<KbSlug>) {
        let inner = Arc::clone(&self.inner);
        for kb in &kbs {
            inner.sink.health.mark_stale(kb.as_str());
        }
        self.inner.tracker.spawn(async move {
            for kb in kbs {
                let Some(slot) = inner.slots.get(kb.as_str()) else {
                    continue;
                };
                let Ok(_held) = slot.clone().try_lock_owned() else {
                    info!(
                        target: "notedthat::reconcile",
                        kb = %kb.as_str(),
                        "a requested pass is already running; the startup pass leaves it to that"
                    );
                    continue;
                };
                tokio::select! {
                    biased;
                    () = inner.cancel.cancelled() => break,
                    () = run_pass(&inner, &kb, "startup") => {}
                }
            }
        });
    }

    /// Stop every running pass and wait for it to let go of the indexing queue.
    ///
    /// Ordering matters at shutdown: a pass holds a sender on the queue, so draining that
    /// queue while one still runs would be chasing a live producer.
    pub(super) async fn stop(&self) {
        self.inner.cancel.cancel();
        self.inner.tracker.close();
        self.inner.tracker.wait().await;
    }
}

impl ReconcileTrigger for Reconciler {
    fn trigger(&self, kb: &KbSlug) -> Result<(), ReconcileBusy> {
        let Some(slot) = self.inner.slots.get(kb.as_str()) else {
            // Every declared knowledge base has a slot; the route resolved the slug
            // against the same declaration, so this cannot happen. Refusing is the
            // answer that cannot start a pass nothing tracks.
            return Err(ReconcileBusy);
        };
        let held = slot.clone().try_lock_owned().map_err(|_| ReconcileBusy)?;
        if self.inner.tracker.is_closed() {
            return Err(ReconcileBusy);
        }
        let inner = Arc::clone(&self.inner);
        let kb = kb.clone();
        self.inner.tracker.spawn(async move {
            let _held = held;
            tokio::select! {
                biased;
                () = inner.cancel.cancelled() => {}
                () = run_pass(&inner, &kb, "requested") => {}
            }
        });
        Ok(())
    }
}

/// One pass over one knowledge base: the `s3` twin of the `fs` bridge's comparison.
///
/// A pass that could not read the index enqueues nothing and leaves the health record as
/// it was, `stale` included: without knowing what is indexed there is nothing to compare
/// against, and guessing would mean either re-embedding everything or silently doing
/// nothing. A pass that could not list the bucket does the same — a partial listing
/// would report every unlisted key as orphaned, the one outcome nothing would repair.
async fn run_pass(inner: &Inner, kb: &KbSlug, cause: &str) {
    let indexed = match inner.store.indexed_objects(kb, None).await {
        Ok(indexed) => indexed,
        Err(notedthat_indexer::VectorStoreError::CollectionNotFound { .. }) => {
            error!(
                target: "notedthat::reconcile",
                kb = %kb.as_str(),
                cause,
                "S3_RECONCILE_SKIPPED: this knowledge base has no search collection, so \
                 the pass was skipped and nothing in it will be indexed. It was provisioned \
                 at startup and has since been dropped; restart to re-provision it."
            );
            return;
        }
        Err(error) => {
            error!(
                target: "notedthat::reconcile",
                kb = %kb.as_str(),
                cause,
                %error,
                "S3_RECONCILE_SKIPPED: could not read the index, so this pass was skipped"
            );
            return;
        }
    };
    let indexed: Vec<IndexedEtag> = indexed
        .into_iter()
        .map(|entry| IndexedEtag {
            key: entry.object_key,
            etag: entry.etag,
        })
        .collect();

    let in_bucket = match walk_etags(inner.storage.as_ref(), kb, None).await {
        Ok(in_bucket) => in_bucket,
        Err(error) => {
            warn!(
                target: "notedthat::reconcile",
                kb = %kb.as_str(),
                cause,
                %error,
                "S3_RECONCILE_INCOMPLETE: could not list the bucket, so this pass was incomplete"
            );
            return;
        }
    };

    let reconciliation = compare(in_bucket, &indexed, None);
    for object_key in reconciliation.keys {
        let event = IndexEvent::Refresh {
            kb: kb.clone(),
            object_key,
            origin: RefreshOrigin::Reconcile,
        };
        if inner.sink.send(event).await.is_err() {
            // The worker is gone: shutdown. Nothing to record — the report would say
            // the pass completed, and it did not.
            return;
        }
    }

    // A completed pass, whatever it found, has enqueued every difference: nothing is
    // unobserved any more, and the report is worth showing (#97).
    let report = reconciliation.report;
    inner.sink.health.reconciled(
        kb.as_str(),
        ReconcileSummary {
            at: now_unix(),
            scope: None,
            objects_on_disk: report.objects_on_disk,
            unchanged: report.unchanged,
            changed: report.changed,
            orphaned: report.orphaned,
        },
    );
    if report.is_clean() {
        info!(
            target: "notedthat::reconcile",
            kb = %kb.as_str(),
            cause,
            objects = report.objects_on_disk,
            "already in step with the index"
        );
    } else {
        info!(
            target: "notedthat::reconcile",
            kb = %kb.as_str(),
            cause,
            objects = report.objects_on_disk,
            changed = report.changed,
            orphaned = report.orphaned,
            "enqueued objects whose index entries are out of date"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use notedthat_core::testing::InMemoryStorage;
    use notedthat_core::{ConditionalHeaders, ObjectPath};
    use notedthat_indexer::testing::InMemoryVectorStore;
    use std::time::Duration;

    fn kb() -> KbSlug {
        KbSlug::try_new("notes").unwrap()
    }

    async fn seeded(keys: &[&str]) -> Arc<InMemoryStorage> {
        let storage = InMemoryStorage::with_kbs([&kb()]);
        for key in keys {
            storage
                .put_object(
                    &kb(),
                    &ObjectPath::try_from(*key).unwrap(),
                    Bytes::from(format!("body of {key}")),
                    Some("text/markdown"),
                    ConditionalHeaders::default(),
                )
                .await
                .unwrap();
        }
        Arc::new(storage)
    }

    async fn reconciler(
        storage: Arc<InMemoryStorage>,
        capacity: usize,
    ) -> (
        Arc<Reconciler>,
        mpsc::Receiver<IndexEvent>,
        Arc<IndexHealth>,
    ) {
        let (tx, rx) = mpsc::channel(capacity);
        let health = Arc::new(IndexHealth::new());
        // Provisioned, as startup leaves it: a pass over a knowledge base with no
        // collection is skipped, by design.
        let store = Arc::new(InMemoryVectorStore::new());
        store.create_collection(&kb(), 3).await.unwrap();
        let reconciler = Reconciler::new(storage, store, tx, health.clone(), &[kb()]);
        (reconciler, rx, health)
    }

    /// Wait, bounded, for the knowledge base's health record to show a completed pass.
    async fn wait_reconciled(health: &IndexHealth) -> ReconcileSummary {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(summary) = health.snapshot(kb().as_str()).last_reconcile {
                return summary;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the pass did not complete"
            );
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_pass_enqueues_one_refresh_per_key_the_index_lacks_and_stamps_the_record() {
        let storage = seeded(&["a.md", "b.md"]).await;
        let (reconciler, mut rx, health) = reconciler(storage, 16).await;

        reconciler.trigger(&kb()).unwrap();

        let summary = wait_reconciled(&health).await;
        assert_eq!(
            (
                summary.objects_on_disk,
                summary.changed,
                summary.unchanged,
                summary.orphaned
            ),
            (2, 2, 0, 0)
        );
        assert_eq!(summary.scope, None);
        let mut keys = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                IndexEvent::Refresh {
                    object_key,
                    origin: RefreshOrigin::Reconcile,
                    ..
                } => keys.push(object_key.as_str().to_string()),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(keys, ["a.md", "b.md"]);
        assert_eq!(
            health.snapshot(kb().as_str()).pending,
            2,
            "counted on the health record"
        );
    }

    #[tokio::test]
    async fn a_second_request_is_busy_until_the_pass_lets_go() {
        // A 1-slot queue with nothing draining it: the pass blocks on its second send.
        let storage = seeded(&["a.md", "b.md", "c.md"]).await;
        let (reconciler, mut rx, health) = reconciler(storage, 1).await;

        reconciler.trigger(&kb()).unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a refresh was enqueued")
            .expect("open");
        assert!(matches!(first, IndexEvent::Refresh { .. }));
        assert_eq!(
            reconciler.trigger(&kb()),
            Err(ReconcileBusy),
            "the pass is still holding the slot"
        );

        // Drain, and the slot is released once the pass completes and its task ends.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let _ = rx.try_recv();
            if reconciler.trigger(&kb()).is_ok() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "slot never released");
            tokio::task::yield_now().await;
        }
        assert!(health.snapshot(kb().as_str()).last_reconcile.is_some());
    }

    #[tokio::test]
    async fn a_startup_pass_marks_every_knowledge_base_stale_until_it_completes() {
        let storage = seeded(&["a.md"]).await;
        let (reconciler, mut rx, health) = reconciler(storage, 16).await;

        reconciler.spawn_startup_pass(vec![kb()]);

        // Stale is set synchronously, before the pass has run.
        assert!(health.snapshot(kb().as_str()).stale_since.is_some());
        let summary = wait_reconciled(&health).await;
        assert_eq!(summary.changed, 1);
        assert!(health.snapshot(kb().as_str()).stale_since.is_none());
        assert!(matches!(rx.try_recv(), Ok(IndexEvent::Refresh { .. })));
        reconciler.stop().await;
    }

    #[tokio::test]
    async fn stop_returns_while_a_pass_is_blocked_on_a_full_queue() {
        let storage = seeded(&["a.md", "b.md", "c.md"]).await;
        let (reconciler, mut rx, _health) = reconciler(storage, 1).await;

        reconciler.trigger(&kb()).unwrap();
        // The first send filled the queue; the pass is now blocked on the second.
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a refresh was enqueued");
        // Do not drain: the blocked send must be interrupted by cancellation alone.
        let stopped = tokio::time::timeout(Duration::from_secs(2), reconciler.stop()).await;
        assert!(
            stopped.is_ok(),
            "stop() waited on a pass that cancellation should end"
        );
        assert_eq!(
            reconciler.trigger(&kb()),
            Err(ReconcileBusy),
            "nothing starts after stop"
        );
    }
}

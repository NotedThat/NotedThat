//! Per-knowledge-base index health: what the queue, the worker and the `fs`
//! bridge have recently done, folded into one state a caller can act on.
//!
//! D38 keeps the queue best-effort — no job ids, no durable state — so this is
//! not a job tracker. It is a small record every producer stamps as it goes
//! (`enqueued`, `started`, `succeeded`, `failed`, …) and one derived answer:
//! is this knowledge base's index `healthy`, `indexing`, `backpressured`,
//! `stale` or `failed` right now (#97). Nothing here holds document bytes,
//! credentials or the queue's contents; the failure summary is the pipeline's
//! own one-line error, bounded.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long after a queue-full rejection a knowledge base still reports
/// `backpressured`. Backpressure is a moment, not a condition the record can
/// see end, so it decays; the route also reports it live while the queue is
/// full.
pub const BACKPRESSURE_WINDOW: Duration = Duration::from_secs(30);

/// Longest failure summary retained, in characters. The pipeline's errors are
/// one line already; this keeps a backend that answers with a body from
/// turning the health view into a log.
pub const FAILURE_SUMMARY_MAX_CHARS: usize = 200;

/// The derived state of one knowledge base's index, most severe first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    /// The most recent outcome was a failure, or the worker is gone.
    Failed,
    /// Changes may have gone unobserved (`fs` watch lost or overflowed) and
    /// the rescan that repairs that has not completed yet.
    Stale,
    /// Writers were refused with `503` within [`BACKPRESSURE_WINDOW`], or the
    /// queue is full right now.
    Backpressured,
    /// Events for this knowledge base are queued or in progress.
    Indexing,
    /// Nothing pending, nothing failed since the last success, nothing lost.
    Healthy,
}

impl IndexState {
    /// The wire name of the state.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Stale => "stale",
            Self::Backpressured => "backpressured",
            Self::Indexing => "indexing",
            Self::Healthy => "healthy",
        }
    }
}

/// The most recent indexing failure in a knowledge base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexFailure {
    /// Unix seconds.
    pub at: i64,
    /// The object the failed event was for.
    pub object_key: String,
    /// The pipeline's error, first line, at most
    /// [`FAILURE_SUMMARY_MAX_CHARS`] characters.
    pub summary: String,
}

/// What the last completed `fs` reconciliation pass found (D50).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileSummary {
    /// Unix seconds.
    pub at: i64,
    /// Objects the walk found in storage.
    pub objects_on_disk: usize,
    /// Objects already indexed from exactly these bytes.
    pub unchanged: usize,
    /// Objects new to the index, or indexed from different bytes.
    pub changed: usize,
    /// Keys the index holds that storage no longer has.
    pub orphaned: usize,
}

/// One knowledge base's record, as it stands at the moment of the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbHealthSnapshot {
    /// The derived state.
    pub state: IndexState,
    /// Events enqueued for this knowledge base and not yet taken by the worker.
    pub pending: usize,
    /// Whether the worker is still running.
    pub worker_alive: bool,
    /// Unix seconds of the last event that completed, including a tombstone
    /// and a refresh that found nothing to do.
    pub last_indexed_at: Option<i64>,
    /// The most recent failure, whether or not a success has followed it.
    pub last_failure: Option<IndexFailure>,
    /// Unix seconds of the last queue-full rejection.
    pub last_backpressure_at: Option<i64>,
    /// Unix seconds since which changes may have gone unobserved.
    pub stale_since: Option<i64>,
    /// The last completed reconciliation pass, `fs` backend only.
    pub last_reconcile: Option<ReconcileSummary>,
}

#[derive(Debug, Default, Clone)]
struct KbRecord {
    /// Events handed to the queue, ever. Monotonic, like `completed`: the two
    /// are recorded by different tasks in no fixed order — the worker can take
    /// an event and finish it before the writer's `enqueued` lands — and a
    /// difference of two counters shrugs that off, where a single up/down
    /// counter drifted by one for good every time it lost that race.
    enqueued: u64,
    /// Events the worker finished, succeeded or failed.
    completed: u64,
    /// Whether the most recent completed event failed. Kept as a flag rather
    /// than derived from timestamps, which are whole seconds and would tie.
    last_outcome_failed: bool,
    last_indexed_at: Option<i64>,
    last_failure: Option<IndexFailure>,
    last_backpressure_at: Option<i64>,
    stale_since: Option<i64>,
    last_reconcile: Option<ReconcileSummary>,
}

#[derive(Debug)]
struct Inner {
    kbs: BTreeMap<String, KbRecord>,
    worker_alive: bool,
}

/// The shared health record: one per process, stamped by every producer,
/// read by the health route.
#[derive(Debug)]
pub struct IndexHealth {
    inner: Mutex<Inner>,
}

impl Default for IndexHealth {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                kbs: BTreeMap::new(),
                worker_alive: true,
            }),
        }
    }
}

impl IndexHealth {
    /// A fresh record: every knowledge base healthy, the worker alive.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An event for `kb` entered the queue.
    pub fn enqueued(&self, kb: &str) {
        self.record(kb, |r, _| r.enqueued += 1);
    }

    /// An event for `kb` completed. Counts it done: `pending` covers queued
    /// *and* in-flight work, so an object still being embedded keeps the
    /// knowledge base `indexing` until this or [`Self::failed`] is recorded.
    pub fn succeeded(&self, kb: &str) {
        self.record(kb, |r, now| {
            r.completed += 1;
            r.last_indexed_at = Some(now);
            r.last_outcome_failed = false;
        });
    }

    /// An event for `kb` failed; `summary` is the pipeline's error.
    pub fn failed(&self, kb: &str, object_key: &str, summary: &str) {
        let summary = bound_summary(summary);
        self.record(kb, move |r, now| {
            r.completed += 1;
            r.last_failure = Some(IndexFailure {
                at: now,
                object_key: object_key.to_string(),
                summary,
            });
            r.last_outcome_failed = true;
        });
    }

    /// A write to `kb` was refused because the queue was full (D38).
    pub fn backpressured(&self, kb: &str) {
        self.record(kb, |r, now| r.last_backpressure_at = Some(now));
    }

    /// Changes to `kb` may have gone unobserved until a rescan completes.
    pub fn mark_stale(&self, kb: &str) {
        self.record(kb, |r, now| {
            r.stale_since.get_or_insert(now);
        });
    }

    /// A reconciliation pass over `kb` completed: whatever it found is now
    /// queued, so nothing is unobserved any more.
    pub fn reconciled(&self, kb: &str, summary: ReconcileSummary) {
        self.record(kb, move |r, _| {
            r.last_reconcile = Some(summary);
            r.stale_since = None;
        });
    }

    /// The worker's loop has ended. Every knowledge base is `failed` from here
    /// on: nothing will drain the queue until the process restarts.
    pub fn worker_stopped(&self) {
        self.lock().worker_alive = false;
    }

    /// The record for `kb` as it stands now, `now` being Unix seconds.
    #[must_use]
    pub fn snapshot(&self, kb: &str) -> KbHealthSnapshot {
        let now = now_unix();
        let inner = self.lock();
        let record = inner.kbs.get(kb).cloned().unwrap_or_default();
        let state = derive_state(&record, inner.worker_alive, now);
        KbHealthSnapshot {
            state,
            pending: pending(&record),
            worker_alive: inner.worker_alive,
            last_indexed_at: record.last_indexed_at,
            last_failure: record.last_failure,
            last_backpressure_at: record.last_backpressure_at,
            stale_since: record.stale_since,
            last_reconcile: record.last_reconcile,
        }
    }

    fn record(&self, kb: &str, update: impl FnOnce(&mut KbRecord, i64)) {
        let now = now_unix();
        let mut inner = self.lock();
        let record = inner.kbs.entry(kb.to_string()).or_default();
        update(record, now);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock means a producer panicked mid-update; the record is
        // still a set of plain values, so reading it is the better outcome.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn derive_state(record: &KbRecord, worker_alive: bool, now: i64) -> IndexState {
    if !worker_alive {
        return IndexState::Failed;
    }
    if record.last_outcome_failed {
        return IndexState::Failed;
    }
    if record.stale_since.is_some() {
        return IndexState::Stale;
    }
    let window = i64::try_from(BACKPRESSURE_WINDOW.as_secs()).unwrap_or(i64::MAX);
    if record
        .last_backpressure_at
        .is_some_and(|at| now.saturating_sub(at) <= window)
    {
        return IndexState::Backpressured;
    }
    if pending(record) > 0 {
        return IndexState::Indexing;
    }
    IndexState::Healthy
}

/// Queued plus in-flight events. A completion observed before its own
/// enqueue reads as a transient `0`, never as a negative or a stuck `1`.
fn pending(record: &KbRecord) -> usize {
    usize::try_from(record.enqueued.saturating_sub(record.completed)).unwrap_or(usize::MAX)
}

/// The first line of `summary`, cut to [`FAILURE_SUMMARY_MAX_CHARS`].
fn bound_summary(summary: &str) -> String {
    let line = summary.lines().next().unwrap_or_default().trim();
    let mut bounded: String = line.chars().take(FAILURE_SUMMARY_MAX_CHARS).collect();
    if line.chars().count() > FAILURE_SUMMARY_MAX_CHARS {
        bounded.push('…');
    }
    bounded
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(at: i64) -> ReconcileSummary {
        ReconcileSummary {
            at,
            objects_on_disk: 3,
            unchanged: 2,
            changed: 1,
            orphaned: 0,
        }
    }

    #[test]
    fn an_unknown_knowledge_base_is_healthy_with_nothing_recorded() {
        let health = IndexHealth::new();
        let snapshot = health.snapshot("notes");
        assert_eq!(snapshot.state, IndexState::Healthy);
        assert_eq!(snapshot.pending, 0);
        assert!(snapshot.worker_alive);
        assert_eq!(snapshot.last_indexed_at, None);
        assert_eq!(snapshot.last_failure, None);
        assert_eq!(snapshot.last_reconcile, None);
    }

    #[test]
    fn pending_counts_enqueued_events_until_the_worker_finishes_them() {
        let health = IndexHealth::new();
        health.enqueued("notes");
        health.enqueued("notes");
        assert_eq!(health.snapshot("notes").pending, 2);
        assert_eq!(health.snapshot("notes").state, IndexState::Indexing);
        // The first is done; the second is still being embedded: `indexing`.
        health.succeeded("notes");
        assert_eq!(health.snapshot("notes").pending, 1);
        assert_eq!(health.snapshot("notes").state, IndexState::Indexing);
        health.failed("notes", "b.md", "embedder down");
        let snapshot = health.snapshot("notes");
        assert_eq!(snapshot.pending, 0);
        assert!(snapshot.last_indexed_at.is_some());
        // Other knowledge bases are untouched.
        assert_eq!(health.snapshot("other").pending, 0);
    }

    /// The writer records `enqueued` after handing the event over, and the
    /// worker runs on its own task: it can finish the event first. Two
    /// monotonic counters make that a transient `0`, not a `1` that never
    /// drains.
    #[test]
    fn a_completion_observed_before_its_enqueue_does_not_leave_pending_stuck() {
        let health = IndexHealth::new();
        health.succeeded("notes");
        assert_eq!(health.snapshot("notes").pending, 0);
        health.enqueued("notes");
        assert_eq!(health.snapshot("notes").pending, 0);
        assert_eq!(health.snapshot("notes").state, IndexState::Healthy);
    }

    #[test]
    fn a_failure_is_the_state_until_a_later_success() {
        let health = IndexHealth::new();
        health.failed("notes", "a.md", "embedder.embed failed: connection refused");
        let snapshot = health.snapshot("notes");
        assert_eq!(snapshot.state, IndexState::Failed);
        let failure = snapshot.last_failure.expect("recorded");
        assert_eq!(failure.object_key, "a.md");
        assert_eq!(failure.summary, "embedder.embed failed: connection refused");

        // The next success supersedes it as the state, while the failure
        // stays visible as the most recent one.
        health.succeeded("notes");
        let snapshot = health.snapshot("notes");
        assert_eq!(snapshot.state, IndexState::Healthy);
        assert!(snapshot.last_failure.is_some());
    }

    #[test]
    fn a_failure_summary_is_one_bounded_line() {
        let health = IndexHealth::new();
        let long = "x".repeat(FAILURE_SUMMARY_MAX_CHARS + 50);
        health.failed(
            "notes",
            "a.md",
            &format!("  {long}\nsecond line with a body"),
        );
        let summary = health.snapshot("notes").last_failure.unwrap().summary;
        assert_eq!(summary.chars().count(), FAILURE_SUMMARY_MAX_CHARS + 1);
        assert!(summary.ends_with('…'));
        assert!(!summary.contains("second line"));
    }

    #[test]
    fn backpressure_is_reported_for_a_window_and_outranks_indexing() {
        let health = IndexHealth::new();
        health.enqueued("notes");
        health.backpressured("notes");
        let snapshot = health.snapshot("notes");
        assert_eq!(snapshot.state, IndexState::Backpressured);
        assert!(snapshot.last_backpressure_at.is_some());
        // The window is relative to now; a stale stamp is below the threshold.
        let old = KbRecord {
            last_backpressure_at: Some(0),
            ..KbRecord::default()
        };
        assert_eq!(
            derive_state(&old, true, i64::MAX / 2),
            IndexState::Healthy,
            "a rejection outside the window no longer counts"
        );
    }

    #[test]
    fn stale_holds_until_a_pass_completes_and_outranks_backpressure() {
        let health = IndexHealth::new();
        health.backpressured("notes");
        health.mark_stale("notes");
        let first = health.snapshot("notes").stale_since.expect("stamped");
        health.mark_stale("notes");
        assert_eq!(
            health.snapshot("notes").stale_since,
            Some(first),
            "a second request does not move the stamp"
        );
        assert_eq!(health.snapshot("notes").state, IndexState::Stale);

        health.reconciled("notes", summary(first));
        let snapshot = health.snapshot("notes");
        assert_eq!(snapshot.stale_since, None);
        assert_eq!(snapshot.last_reconcile, Some(summary(first)));
        assert_eq!(snapshot.state, IndexState::Backpressured);
    }

    #[test]
    fn a_failure_outranks_everything_and_a_stopped_worker_fails_every_base() {
        let health = IndexHealth::new();
        health.mark_stale("notes");
        health.backpressured("notes");
        health.enqueued("notes");
        health.failed("notes", "a.md", "vector store upsert failed");
        assert_eq!(health.snapshot("notes").state, IndexState::Failed);

        health.worker_stopped();
        for kb in ["notes", "never-seen"] {
            let snapshot = health.snapshot(kb);
            assert_eq!(snapshot.state, IndexState::Failed, "{kb}");
            assert!(!snapshot.worker_alive);
        }
    }

    #[test]
    fn state_names_are_stable_wire_values() {
        assert_eq!(IndexState::Failed.as_str(), "failed");
        assert_eq!(IndexState::Stale.as_str(), "stale");
        assert_eq!(IndexState::Backpressured.as_str(), "backpressured");
        assert_eq!(IndexState::Indexing.as_str(), "indexing");
        assert_eq!(IndexState::Healthy.as_str(), "healthy");
    }
}

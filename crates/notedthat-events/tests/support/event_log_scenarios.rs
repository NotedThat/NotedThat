//! The event log integration suite, written once and run against every `EventPublisher`.
//!
//! Each scenario here is an *absolute* assertion about one adapter: "a position the log
//! no longer holds is `Gone`", "resuming at the latest id delivers the very next event".
//! Every scenario takes an [`EventLogFixture`] and runs unchanged over the process-local
//! ring and over a real `JetStream` stream. The two expansions live in
//! `events_integration_memory.rs` and `events_integration_nats.rs`; neither contains a test
//! body, only a fixture and one macro call.
//!
//! Every surface — the write path, the indexer worker, the SSE route — holds the log behind
//! `Arc<dyn EventPublisher>`, so a difference between the adapters is a behaviour change an
//! operator gets for free by flipping `NOTEDTHAT_EVENTS_BACKEND`. The E2E suites run on the
//! ring alone and cannot see it; this is what can.
//!
//! # Adding a scenario
//!
//! Write the `async fn` here, then add its name to [`event_log_scenarios!`]. Every backend
//! picks it up; nothing else needs editing. The name becomes the knowledge base slug, so
//! keep it to 38 characters and `[a-z0-9_]` — [`KbSlug`] enforces the character set and
//! [`kb_for`] converts the underscores; the isolation scenario appends `-b` to it.
//!
//! # What is deliberately not here
//!
//! `StreamError::Lagged` is a mechanism of the ring — a broadcast receiver that fell more
//! than a whole ring behind — and has no counterpart on the broker, whose ordered consumer
//! is pulled at the subscriber's pace. It is pinned by `memory.rs`'s own unit tests.
//!
//! Ids are never asserted literally beyond "greater than zero": both adapters happen to
//! start a fresh log at 1, but the contract is only that ids are strictly increasing and
//! that a subscriber resuming after one sees exactly what came later.

#![allow(dead_code)]

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use notedthat_core::{
    EventId, EventPublisher, EventSource, EventStream, KbSlug, ObjectEvent, ObjectEventKind,
    ObjectPath, SubscribeError,
};

/// Every scenario in this file, expanded by whatever `$emit` the caller supplies.
///
/// The list is the single place a backend's test binary learns what to run, which is what
/// keeps the expansions from drifting apart: a scenario cannot be added to one backend
/// and forgotten on the other.
macro_rules! event_log_scenarios {
    ($emit:ident) => {
        $emit!(publish_ids_are_what_subscribers_see);
        $emit!(a_fresh_subscriber_sees_only_live);
        $emit!(replay_starts_after_the_position);
        $emit!(resuming_at_the_latest_id_is_live_only);
        $emit!(resuming_from_zero_replays_everything);
        $emit!(kbs_are_isolated_and_share_ids);
        $emit!(events_round_trip_intact);
        $emit!(index_outcomes_round_trip_intact);
        $emit!(a_retained_out_position_is_gone);
        $emit!(a_position_ahead_of_the_log_is_gone);
        $emit!(a_burst_is_delivered_once_in_order);
        $emit!(a_subscription_outlives_its_siblings);
        $emit!(the_log_is_ready_and_named);
    };
}

pub(crate) use event_log_scenarios;

/// How many of the retention scenario's events `retain_out` leaves unretrievable.
pub const DROPPED: usize = 2;

/// How long to wait for an event that must arrive.
pub const WAIT: Duration = Duration::from_secs(5);

/// How long to watch a stream that must stay quiet. The same for both backends so the
/// scenario bodies stay shared; the broker's delivery latency sits well inside it.
pub const QUIET: Duration = Duration::from_millis(300);

/// One adapter under test, with the single backend-specific operation the contract needs.
#[async_trait]
pub trait EventLogFixture: Send + Sync {
    /// The log every scenario publishes to and subscribes on.
    fn log(&self) -> &dyn EventPublisher;

    /// The scenario's own knowledge base.
    fn kb(&self) -> &KbSlug;

    /// How many events [`a_retained_out_position_is_gone`] publishes so that, once
    /// [`retain_out`](Self::retain_out) has run, exactly the first [`DROPPED`] are gone:
    /// the ring's capacity plus `DROPPED`, or any small number for a broker that is purged
    /// explicitly.
    fn retention_batch(&self) -> usize;

    /// Make every event with an id below `first_kept` unavailable to replay. The ring
    /// already did this when the batch overflowed its capacity, so its fixture does
    /// nothing; the broker's fixture purges the stream up to `first_kept`.
    async fn retain_out(&self, first_kept: EventId);
}

/// The knowledge base a scenario runs in, derived from its own name.
///
/// One knowledge base per scenario is what keeps scenarios that share a log apart; on the
/// broker every scenario also gets a stream of its own.
///
/// # Panics
///
/// If the scenario name is not a valid [`KbSlug`] once underscores become hyphens —
/// in practice, if it exceeds 40 characters.
pub fn kb_for(scenario: &str) -> KbSlug {
    let slug = scenario.replace('_', "-");
    KbSlug::try_new(slug).unwrap_or_else(|error| {
        panic!("scenario name `{scenario}` is not a usable knowledge base slug: {error}")
    })
}

fn path(key: &str) -> ObjectPath {
    ObjectPath::try_from_str(key).expect("test key is a valid ObjectPath")
}

/// A write of `key`, stamped as the HTTP API would.
pub fn written(kb: &KbSlug, key: &str) -> ObjectEvent {
    ObjectEvent::written(
        kb.clone(),
        path(key),
        format!("\"{key}\""),
        key.len() as u64,
        "text/markdown".into(),
        1_700_000_000,
        EventSource::Http,
    )
}

/// A deletion of `key`.
pub fn deleted(kb: &KbSlug, key: &str) -> ObjectEvent {
    ObjectEvent::deleted(kb.clone(), path(key), EventSource::Http)
}

async fn publish(log: &dyn EventPublisher, event: ObjectEvent) -> EventId {
    log.publish(event).await.expect("publish succeeds")
}

async fn subscribe(log: &dyn EventPublisher, kb: &KbSlug, after: Option<EventId>) -> EventStream {
    log.subscribe(kb, after)
        .await
        .unwrap_or_else(|error| panic!("subscribe after {after:?} succeeds: {error}"))
}

/// The next event, which must arrive within [`WAIT`].
pub async fn next(stream: &mut EventStream) -> (EventId, ObjectEvent) {
    tokio::time::timeout(WAIT, stream.next())
        .await
        .expect("an event within WAIT")
        .expect("stream still open")
        .expect("not a stream error")
}

/// The next `n` ids, in delivery order.
pub async fn ids(stream: &mut EventStream, n: usize) -> Vec<EventId> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(next(stream).await.0);
    }
    out
}

/// Assert that nothing arrives on `stream` for [`QUIET`].
pub async fn nothing_pending(stream: &mut EventStream) {
    if let Ok(item) = tokio::time::timeout(QUIET, stream.next()).await {
        panic!("expected no further event, got {item:?}");
    }
}

fn assert_strictly_increasing(ids: &[EventId]) {
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "ids must strictly increase: {ids:?}"
    );
}

// ─── Scenarios ──────────────────────────────────────────────────────────────

pub async fn publish_ids_are_what_subscribers_see(fx: &dyn EventLogFixture) {
    let (log, kb) = (fx.log(), fx.kb());
    let mut returned = Vec::new();
    for key in ["a.md", "b.md", "c.md"] {
        returned.push(publish(log, written(kb, key)).await);
    }
    assert!(
        returned.iter().all(|id| id.0 > 0),
        "ids are positive: {returned:?}"
    );
    assert_strictly_increasing(&returned);

    let mut stream = subscribe(log, kb, Some(EventId(0))).await;
    let seen = ids(&mut stream, 3).await;
    assert_eq!(
        seen, returned,
        "the subscriber sees the ids publish returned"
    );
    nothing_pending(&mut stream).await;
}

pub async fn a_fresh_subscriber_sees_only_live(fx: &dyn EventLogFixture) {
    let (log, kb) = (fx.log(), fx.kb());
    publish(log, written(kb, "old.md")).await;
    let mut stream = subscribe(log, kb, None).await;
    let new = publish(log, written(kb, "new.md")).await;
    let (id, event) = next(&mut stream).await;
    assert_eq!(id, new);
    assert_eq!(event.object_key.as_str(), "new.md");
    nothing_pending(&mut stream).await;
}

pub async fn replay_starts_after_the_position(fx: &dyn EventLogFixture) {
    let (log, kb) = (fx.log(), fx.kb());
    let a = publish(log, written(kb, "a.md")).await;
    let b = publish(log, written(kb, "b.md")).await;
    let c = publish(log, written(kb, "c.md")).await;

    let mut stream = subscribe(log, kb, Some(a)).await;
    assert_eq!(ids(&mut stream, 2).await, vec![b, c]);
    let d = publish(log, written(kb, "d.md")).await;
    assert_eq!(
        next(&mut stream).await.0,
        d,
        "live events follow the replay"
    );
    nothing_pending(&mut stream).await;
}

/// The reconnect every client makes after a network blip: it is caught up, and the very
/// next event must not slip through the gap between reading the log's end and the
/// subscription coming up.
pub async fn resuming_at_the_latest_id_is_live_only(fx: &dyn EventLogFixture) {
    let (log, kb) = (fx.log(), fx.kb());
    let a = publish(log, written(kb, "a.md")).await;
    let mut stream = subscribe(log, kb, Some(a)).await;
    nothing_pending(&mut stream).await;
    let b = publish(log, written(kb, "b.md")).await;
    assert_eq!(next(&mut stream).await.0, b);
}

pub async fn resuming_from_zero_replays_everything(fx: &dyn EventLogFixture) {
    let (log, kb) = (fx.log(), fx.kb());
    let a = publish(log, written(kb, "a.md")).await;
    let b = publish(log, deleted(kb, "a.md")).await;
    let mut stream = subscribe(log, kb, Some(EventId(0))).await;
    assert_eq!(ids(&mut stream, 2).await, vec![a, b]);
    nothing_pending(&mut stream).await;
}

pub async fn kbs_are_isolated_and_share_ids(fx: &dyn EventLogFixture) {
    let (log, kb) = (fx.log(), fx.kb());
    let other = KbSlug::try_new(format!("{kb}-b")).expect("sibling slug");

    let a1 = publish(log, written(kb, "a.md")).await;
    let b1 = publish(log, written(&other, "b.md")).await;
    let a2 = publish(log, written(kb, "c.md")).await;
    let b2 = publish(log, written(&other, "d.md")).await;
    assert_strictly_increasing(&[a1, b1, a2, b2]);

    let mut mine = subscribe(log, kb, Some(EventId(0))).await;
    let seen = ids(&mut mine, 2).await;
    assert_eq!(seen, vec![a1, a2], "only this knowledge base's events");
    nothing_pending(&mut mine).await;

    let mut theirs = subscribe(log, &other, Some(EventId(0))).await;
    let seen = ids(&mut theirs, 2).await;
    assert_eq!(seen, vec![b1, b2]);
    nothing_pending(&mut theirs).await;

    // A live publish on one side stays on that side.
    let a3 = publish(log, written(kb, "e.md")).await;
    assert_eq!(next(&mut mine).await.0, a3);
    nothing_pending(&mut theirs).await;
}

pub async fn events_round_trip_intact(fx: &dyn EventLogFixture) {
    let (log, kb) = (fx.log(), fx.kb());
    let mut stream = subscribe(log, kb, None).await;

    let write = ObjectEvent::written(
        kb.clone(),
        path("inbox/memo.mp3"),
        "\"9a3f\"".into(),
        48_213_011,
        "audio/mpeg".into(),
        1_757_950_000,
        EventSource::Webdav,
    );
    let delete = ObjectEvent::deleted(kb.clone(), path("inbox/old.md"), EventSource::Mcp);
    let write_id = publish(log, write.clone()).await;
    let delete_id = publish(log, delete.clone()).await;

    let (id, got) = next(&mut stream).await;
    assert_eq!(id, write_id);
    assert_eq!(got, write, "a write arrives exactly as published");
    assert_eq!(got.kind.name(), "object.written");
    assert_eq!(got.kind.mime(), Some("audio/mpeg"));
    assert!(
        matches!(
            got.kind,
            ObjectEventKind::Written {
                size: 48_213_011,
                ..
            }
        ),
        "{:?}",
        got.kind
    );

    let (id, got) = next(&mut stream).await;
    assert_eq!(id, delete_id);
    assert_eq!(got, delete, "a deletion arrives exactly as published");
    assert_eq!(got.kind.name(), "object.deleted");
    assert_eq!(got.kind.mime(), None);
    assert_eq!(got.source, EventSource::Mcp);
}

/// The indexer's verdicts (D64) ride the same log as the writes that caused them and
/// come back whole: the kind name a subscriber filters on, the stamp it correlates
/// with the write, the chunk count, and — for a failure — the summary and the
/// absence of what `HEAD` never established.
pub async fn index_outcomes_round_trip_intact(fx: &dyn EventLogFixture) {
    let (log, kb) = (fx.log(), fx.kb());
    let mut stream = subscribe(log, kb, None).await;

    let indexed = ObjectEvent::indexed(
        kb.clone(),
        path("inbox/memo.md"),
        "\"b71c\"".into(),
        "text/markdown".into(),
        3,
    );
    let failed = ObjectEvent::index_failed(
        kb.clone(),
        path("inbox/broken.md"),
        None,
        None,
        "storage.head_object failed: backend unavailable".into(),
    );
    let indexed_id = publish(log, indexed.clone()).await;
    let failed_id = publish(log, failed.clone()).await;

    let (id, got) = next(&mut stream).await;
    assert_eq!(id, indexed_id);
    assert_eq!(got, indexed, "a success arrives exactly as published");
    assert_eq!(got.kind.name(), "object.indexed");
    assert_eq!(got.kind.mime(), Some("text/markdown"));
    assert_eq!(got.kind.etag(), Some("\"b71c\""));
    assert_eq!(got.source, EventSource::Indexer);
    assert!(
        matches!(got.kind, ObjectEventKind::Indexed { chunks: 3, .. }),
        "{:?}",
        got.kind
    );

    let (id, got) = next(&mut stream).await;
    assert_eq!(id, failed_id);
    assert_eq!(got, failed, "a failure arrives exactly as published");
    assert_eq!(got.kind.name(), "object.index_failed");
    assert_eq!(got.kind.mime(), None, "HEAD never ran, so no content type");
    assert_eq!(got.kind.etag(), None);
    assert!(
        matches!(
            &got.kind,
            ObjectEventKind::IndexFailed { summary: Some(summary), .. }
                if summary == "storage.head_object failed: backend unavailable"
        ),
        "{:?}",
        got.kind
    );
}

pub async fn a_retained_out_position_is_gone(fx: &dyn EventLogFixture) {
    let (log, kb) = (fx.log(), fx.kb());
    let batch = fx.retention_batch();
    assert!(
        batch > DROPPED + 1,
        "the batch must leave something retained"
    );

    let mut published = Vec::with_capacity(batch);
    for i in 0..batch {
        published.push(publish(log, written(kb, &format!("{i}.md"))).await);
    }
    let first_kept = published[DROPPED];
    fx.retain_out(first_kept).await;

    // Behind the log: refused, naming what is still there.
    let err = log
        .subscribe(kb, Some(published[0]))
        .await
        .err()
        .expect("a position before the retained range is gone");
    match err {
        SubscribeError::Gone { requested, oldest } => {
            assert_eq!(requested, published[0]);
            assert_eq!(oldest, first_kept, "names the oldest retained id");
        }
        other @ SubscribeError::Unavailable { .. } => panic!("expected Gone, got {other:?}"),
    }

    // Exactly at the edge: the next event is the first retained one, nothing skipped.
    let mut edge = subscribe(log, kb, Some(published[DROPPED - 1])).await;
    assert_eq!(next(&mut edge).await.0, first_kept);
    let rest = ids(&mut edge, batch - DROPPED - 1).await;
    assert_eq!(rest, published[DROPPED + 1..].to_vec());
    nothing_pending(&mut edge).await;

    // And the log still takes live subscribers and events.
    let mut live = subscribe(log, kb, None).await;
    let more = publish(log, written(kb, "more.md")).await;
    assert_eq!(next(&mut live).await.0, more);
}

/// A client from before the log started again — a restarted `memory` process, a
/// recreated stream — names ids that never existed here. Whatever was published since is
/// exactly what it has missed, so it is told to resync rather than resumed from "now".
pub async fn a_position_ahead_of_the_log_is_gone(fx: &dyn EventLogFixture) {
    let (log, kb) = (fx.log(), fx.kb());

    let err = log
        .subscribe(kb, Some(EventId(7)))
        .await
        .err()
        .expect("ahead of an empty log is gone");
    match err {
        SubscribeError::Gone { requested, oldest } => {
            assert_eq!(requested, EventId(7));
            assert_eq!(oldest, EventId(1), "the next id the log will hand out");
        }
        other @ SubscribeError::Unavailable { .. } => panic!("expected Gone, got {other:?}"),
    }

    let a = publish(log, written(kb, "a.md")).await;
    let err = log
        .subscribe(kb, Some(EventId(a.0 + 5)))
        .await
        .err()
        .expect("ahead of a non-empty log is gone");
    match err {
        SubscribeError::Gone { requested, oldest } => {
            assert_eq!(requested, EventId(a.0 + 5));
            assert_eq!(oldest, a);
        }
        other @ SubscribeError::Unavailable { .. } => panic!("expected Gone, got {other:?}"),
    }

    // The latest id itself is not ahead: it is caught up.
    let mut stream = subscribe(log, kb, Some(a)).await;
    let b = publish(log, written(kb, "b.md")).await;
    assert_eq!(next(&mut stream).await.0, b);
}

pub async fn a_burst_is_delivered_once_in_order(fx: &dyn EventLogFixture) {
    let (log, kb) = (fx.log(), fx.kb());
    let mut stream = subscribe(log, kb, Some(EventId(0))).await;

    let mut published = Vec::new();
    for i in 0..16 {
        published.push(publish(log, written(kb, &format!("{i}.md"))).await);
    }
    assert_strictly_increasing(&published);

    let seen = ids(&mut stream, published.len()).await;
    assert_eq!(seen, published, "every event once, in order");
    nothing_pending(&mut stream).await;
}

pub async fn a_subscription_outlives_its_siblings(fx: &dyn EventLogFixture) {
    let (log, kb) = (fx.log(), fx.kb());
    let mut survivor = subscribe(log, kb, None).await;
    let doomed = subscribe(log, kb, None).await;

    let a = publish(log, written(kb, "a.md")).await;
    assert_eq!(next(&mut survivor).await.0, a);

    drop(doomed);

    let b = publish(log, written(kb, "b.md")).await;
    assert_eq!(
        next(&mut survivor).await.0,
        b,
        "dropping one subscription does not disturb another"
    );
    nothing_pending(&mut survivor).await;
}

pub async fn the_log_is_ready_and_named(fx: &dyn EventLogFixture) {
    let log = fx.log();
    assert!(log.ready(), "a freshly built log is ready");
    assert!(!log.backend_name().is_empty());
}

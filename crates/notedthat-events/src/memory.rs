//! The `memory` adapter: a process-local ring buffer.
//!
//! Replay survives a subscriber's reconnect but not a restart, and two replicas
//! each have their own log — so this is the dev and single-process choice, and
//! the honest one for the `fs` backend, which is one process per root anyway
//! (D49). It is also what every E2E suite runs on, which is why its semantics
//! are pinned as carefully as the broker's.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use notedthat_core::{
    EventId, EventPublisher, EventStream, KbSlug, ObjectEvent, PublishError, StreamError,
    SubscribeError,
};
use tokio::sync::broadcast;

type Entry = Arc<(EventId, ObjectEvent)>;

/// A ring of the last `capacity` events plus a broadcast channel for live ones.
pub struct MemoryPublisher {
    inner: Arc<Inner>,
}

struct Inner {
    ring: Mutex<Ring>,
    tx: broadcast::Sender<Entry>,
}

struct Ring {
    next_id: u64,
    capacity: usize,
    buf: VecDeque<Entry>,
}

impl MemoryPublisher {
    /// A ring retaining `capacity` events (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (tx, _) = broadcast::channel(capacity);
        Self {
            inner: Arc::new(Inner {
                ring: Mutex::new(Ring {
                    next_id: 1,
                    capacity,
                    buf: VecDeque::with_capacity(capacity),
                }),
                tx,
            }),
        }
    }
}

impl Default for MemoryPublisher {
    fn default() -> Self {
        Self::new(crate::config::DEFAULT_MEMORY_CAPACITY)
    }
}

impl Ring {
    fn oldest(&self) -> Option<EventId> {
        self.buf.front().map(|e| e.0)
    }

    fn after(&self, cursor: EventId) -> VecDeque<Entry> {
        self.buf.iter().filter(|e| e.0 > cursor).cloned().collect()
    }
}

/// One subscriber's position: the backlog still to drain, the live receiver,
/// and the cursor — the last id it has passed, delivered or not, so a lag can
/// be refilled from the ring without re-sending or skipping anything. Zero
/// when it attached before anything was published, so that a lag from there is
/// still measured against the ring rather than waved through.
struct Subscription {
    inner: Arc<Inner>,
    kb: KbSlug,
    backlog: VecDeque<Entry>,
    rx: broadcast::Receiver<Entry>,
    cursor: EventId,
}

enum Step {
    Yield(Entry),
    Skip,
    Lagged,
    Closed,
}

impl Subscription {
    async fn step(&mut self) -> Step {
        let entry = if let Some(entry) = self.backlog.pop_front() {
            entry
        } else {
            match self.rx.recv().await {
                Ok(entry) => entry,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // The ring is at least as deep as the channel, so whatever the
                    // channel dropped is still in the ring unless the subscriber is
                    // more than a whole ring behind.
                    let ring = self.inner.ring.lock().expect("ring mutex not poisoned");
                    self.rx = self.inner.tx.subscribe();
                    let refill = ring.after(self.cursor);
                    let gap = ring
                        .oldest()
                        .is_some_and(|oldest| oldest.0 > self.cursor.0 + 1);
                    drop(ring);
                    if gap {
                        return Step::Lagged;
                    }
                    self.backlog = refill;
                    return Step::Skip;
                }
                Err(broadcast::error::RecvError::Closed) => return Step::Closed,
            }
        };
        // Events the broadcast delivered that the backlog already covered.
        if entry.0 <= self.cursor {
            return Step::Skip;
        }
        self.cursor = entry.0;
        if entry.1.kb == self.kb {
            Step::Yield(entry)
        } else {
            Step::Skip
        }
    }
}

#[async_trait]
impl EventPublisher for MemoryPublisher {
    async fn publish(&self, event: ObjectEvent) -> Result<EventId, PublishError> {
        let mut ring = self.inner.ring.lock().expect("ring mutex not poisoned");
        let id = EventId(ring.next_id);
        ring.next_id += 1;
        let entry: Entry = Arc::new((id, event));
        ring.buf.push_back(entry.clone());
        while ring.buf.len() > ring.capacity {
            ring.buf.pop_front();
        }
        // Sent under the lock so a subscriber snapshotting the ring sees every
        // event exactly once: in its backlog, or on its receiver, never both.
        let _ = self.inner.tx.send(entry);
        Ok(id)
    }

    async fn subscribe(
        &self,
        kb: &KbSlug,
        after: Option<EventId>,
    ) -> Result<EventStream, SubscribeError> {
        let ring = self.inner.ring.lock().expect("ring mutex not poisoned");
        let rx = self.inner.tx.subscribe();
        // The last id ever handed out; zero before the first publish.
        let latest = EventId(ring.next_id - 1);
        if let Some(requested) = after {
            // Behind the ring, events were dropped. Ahead of it — a client from
            // before a restart of this process, whose ids began again at one —
            // the ids it names never existed here and whatever was published
            // since the restart is exactly what it has missed. Both are told to
            // resync rather than silently resumed from "now".
            let oldest = ring.oldest().unwrap_or(EventId(ring.next_id));
            let behind = requested < latest && requested.0 + 1 < oldest.0;
            if behind || requested > latest {
                return Err(SubscribeError::Gone { requested, oldest });
            }
        }
        // No position means "from now": nothing replays, and the cursor starts at
        // the latest id so a lag can still be measured from here.
        let (backlog, cursor) = match after {
            Some(after) if after < latest => (ring.after(after), after),
            _ => (VecDeque::new(), latest),
        };
        drop(ring);

        let subscription = Subscription {
            inner: self.inner.clone(),
            kb: kb.clone(),
            backlog,
            rx,
            cursor,
        };
        let stream = futures::stream::unfold(Some(subscription), |state| async move {
            let mut sub = state?;
            loop {
                match sub.step().await {
                    Step::Yield(entry) => {
                        return Some((Ok((entry.0, entry.1.clone())), Some(sub)));
                    }
                    Step::Skip => {}
                    Step::Lagged => {
                        let resume_after = sub.cursor;
                        return Some((Err(StreamError::Lagged { resume_after }), None));
                    }
                    Step::Closed => return None,
                }
            }
        });
        Ok(stream.boxed())
    }

    fn ready(&self) -> bool {
        true
    }

    fn backend_name(&self) -> &'static str {
        "memory"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notedthat_core::{EventSource, ObjectPath};
    use std::time::Duration;

    fn kb(name: &str) -> KbSlug {
        KbSlug::try_new(name).expect("valid slug")
    }

    fn written(kb_name: &str, key: &str) -> ObjectEvent {
        ObjectEvent::written(
            kb(kb_name),
            ObjectPath::try_from_str(key).expect("valid path"),
            format!("\"{key}\""),
            1,
            "text/markdown".into(),
            0,
            EventSource::Http,
        )
    }

    async fn next_id(stream: &mut EventStream) -> EventId {
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("an event within 2s")
            .expect("stream still open")
            .expect("not an error")
            .0
    }

    async fn nothing_pending(stream: &mut EventStream) {
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.next())
                .await
                .is_err(),
            "expected no further event"
        );
    }

    #[tokio::test]
    async fn ids_start_at_one_and_strictly_increase() {
        let publisher = MemoryPublisher::new(8);
        let a = publisher.publish(written("notes", "a.md")).await.unwrap();
        let b = publisher.publish(written("notes", "b.md")).await.unwrap();
        assert_eq!(a, EventId(1));
        assert_eq!(b, EventId(2));
    }

    #[tokio::test]
    async fn a_fresh_subscriber_sees_only_live_events() {
        let publisher = MemoryPublisher::new(8);
        publisher.publish(written("notes", "old.md")).await.unwrap();
        let mut stream = publisher.subscribe(&kb("notes"), None).await.unwrap();
        publisher.publish(written("notes", "new.md")).await.unwrap();
        // No position means from now: the old event does not replay.
        assert_eq!(next_id(&mut stream).await, EventId(2));
        nothing_pending(&mut stream).await;
    }

    #[tokio::test]
    async fn replay_starts_strictly_after_the_requested_id() {
        let publisher = MemoryPublisher::new(8);
        for key in ["a.md", "b.md", "c.md"] {
            publisher.publish(written("notes", key)).await.unwrap();
        }
        let mut stream = publisher
            .subscribe(&kb("notes"), Some(EventId(1)))
            .await
            .unwrap();
        assert_eq!(next_id(&mut stream).await, EventId(2));
        assert_eq!(next_id(&mut stream).await, EventId(3));
        publisher.publish(written("notes", "d.md")).await.unwrap();
        assert_eq!(next_id(&mut stream).await, EventId(4));
        nothing_pending(&mut stream).await;
    }

    #[tokio::test]
    async fn resuming_at_the_latest_id_yields_only_live_events() {
        let publisher = MemoryPublisher::new(8);
        publisher.publish(written("notes", "a.md")).await.unwrap();
        let mut stream = publisher
            .subscribe(&kb("notes"), Some(EventId(1)))
            .await
            .unwrap();
        nothing_pending(&mut stream).await;
        publisher.publish(written("notes", "b.md")).await.unwrap();
        assert_eq!(next_id(&mut stream).await, EventId(2));
    }

    #[tokio::test]
    async fn knowledge_bases_are_isolated_but_share_the_id_space() {
        let publisher = MemoryPublisher::new(8);
        publisher.publish(written("notes", "a.md")).await.unwrap();
        publisher.publish(written("wiki", "b.md")).await.unwrap();
        publisher.publish(written("notes", "c.md")).await.unwrap();
        let from_start = Some(EventId(0));
        let mut notes = publisher.subscribe(&kb("notes"), from_start).await.unwrap();
        assert_eq!(next_id(&mut notes).await, EventId(1));
        assert_eq!(next_id(&mut notes).await, EventId(3));
        nothing_pending(&mut notes).await;
        let mut wiki = publisher.subscribe(&kb("wiki"), from_start).await.unwrap();
        assert_eq!(next_id(&mut wiki).await, EventId(2));
        nothing_pending(&mut wiki).await;
    }

    #[tokio::test]
    async fn a_retained_out_position_is_gone_and_names_the_oldest() {
        let publisher = MemoryPublisher::new(3);
        for key in ["a", "b", "c", "d", "e"] {
            publisher.publish(written("notes", key)).await.unwrap();
        }
        // Ring holds 3, 4, 5. Resuming after 2 is exactly at the edge and fine.
        let mut edge = publisher
            .subscribe(&kb("notes"), Some(EventId(2)))
            .await
            .unwrap();
        assert_eq!(next_id(&mut edge).await, EventId(3));
        // Resuming after 1 would skip 2 silently; refuse.
        let err = publisher
            .subscribe(&kb("notes"), Some(EventId(1)))
            .await
            .err()
            .expect("gone");
        assert!(
            matches!(
                err,
                SubscribeError::Gone {
                    requested: EventId(1),
                    oldest: EventId(3)
                }
            ),
            "{err:?}"
        );
        // A position ahead of the log never existed here: gone, not live.
        let err = publisher
            .subscribe(&kb("notes"), Some(EventId(99)))
            .await
            .err()
            .expect("gone");
        assert!(
            matches!(
                err,
                SubscribeError::Gone {
                    requested: EventId(99),
                    oldest: EventId(3)
                }
            ),
            "{err:?}"
        );
        // Exactly at the latest id is caught up and live.
        let mut caught_up = publisher
            .subscribe(&kb("notes"), Some(EventId(5)))
            .await
            .unwrap();
        nothing_pending(&mut caught_up).await;
        publisher.publish(written("notes", "f")).await.unwrap();
        assert_eq!(next_id(&mut caught_up).await, EventId(6));
    }

    #[tokio::test]
    async fn a_position_ahead_of_an_empty_ring_is_gone_and_names_the_next_id() {
        // A client from before this process restarted: nothing it asks for
        // exists here, and it must resync rather than miss what comes next.
        let publisher = MemoryPublisher::new(3);
        let err = publisher
            .subscribe(&kb("notes"), Some(EventId(7)))
            .await
            .err()
            .expect("gone");
        assert!(
            matches!(
                err,
                SubscribeError::Gone {
                    requested: EventId(7),
                    oldest: EventId(1)
                }
            ),
            "{err:?}"
        );
        // Zero is "from the very start" and is fine on an empty ring.
        let mut stream = publisher
            .subscribe(&kb("notes"), Some(EventId(0)))
            .await
            .unwrap();
        publisher.publish(written("notes", "a")).await.unwrap();
        assert_eq!(next_id(&mut stream).await, EventId(1));
    }

    #[tokio::test]
    async fn a_subscriber_to_an_empty_ring_that_lags_a_whole_ring_is_told_so() {
        // Attached before the first publish, then never polled: the channel
        // drops the first four, the ring keeps 5-8, and 1-4 are unrecoverable.
        let publisher = MemoryPublisher::new(4);
        let mut stream = publisher.subscribe(&kb("notes"), None).await.unwrap();
        for key in 1..=8 {
            publisher
                .publish(written("notes", &key.to_string()))
                .await
                .unwrap();
        }
        let item = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("an item")
            .expect("stream open");
        assert!(
            matches!(
                item,
                Err(StreamError::Lagged {
                    resume_after: EventId(0)
                })
            ),
            "{item:?}"
        );
        // And the reconnect that follows is refused rather than skipping 1-4.
        let err = publisher
            .subscribe(&kb("notes"), Some(EventId(0)))
            .await
            .err()
            .expect("gone");
        assert!(matches!(err, SubscribeError::Gone { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn nothing_is_lost_or_duplicated_around_subscribe() {
        let publisher = Arc::new(MemoryPublisher::new(1000));
        let writer = {
            let publisher = publisher.clone();
            tokio::spawn(async move {
                for i in 0..200 {
                    publisher
                        .publish(written("notes", &format!("{i}.md")))
                        .await
                        .unwrap();
                    tokio::task::yield_now().await;
                }
            })
        };
        tokio::task::yield_now().await;
        let mut stream = publisher
            .subscribe(&kb("notes"), Some(EventId(0)))
            .await
            .unwrap();
        writer.await.unwrap();
        let mut seen = Vec::new();
        for _ in 0..200 {
            seen.push(next_id(&mut stream).await.0);
        }
        assert_eq!(seen, (1..=200).collect::<Vec<u64>>());
        nothing_pending(&mut stream).await;
    }

    #[tokio::test]
    async fn a_lagging_subscriber_is_refilled_from_the_ring_or_told_to_resume() {
        let publisher = MemoryPublisher::new(4);
        let mut stream = publisher.subscribe(&kb("notes"), None).await.unwrap();
        // Four events fit the channel and the ring: no lag at all.
        for key in ["a", "b", "c", "d"] {
            publisher.publish(written("notes", key)).await.unwrap();
        }
        for expected in 1..=4 {
            assert_eq!(next_id(&mut stream).await, EventId(expected));
        }
        // Twenty more without polling: the channel drops sixteen, the ring keeps
        // only 21-24, and 5-20 are unrecoverable — so the stream says so.
        for key in 5..=24 {
            publisher
                .publish(written("notes", &key.to_string()))
                .await
                .unwrap();
        }
        let item = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("an item")
            .expect("stream open");
        assert!(
            matches!(
                item,
                Err(StreamError::Lagged {
                    resume_after: EventId(4)
                })
            ),
            "{item:?}"
        );
        assert!(
            stream.next().await.is_none(),
            "the stream ends after Lagged"
        );
    }

    #[tokio::test]
    async fn the_memory_adapter_is_always_ready() {
        let publisher = MemoryPublisher::default();
        assert!(publisher.ready());
        assert_eq!(publisher.backend_name(), "memory");
    }
}

#![allow(missing_docs)]

//! The event log integration suite, run against the `memory` ring.
//!
//! Every test body lives in `support/event_log_scenarios.rs` and is shared with
//! `events_integration_nats.rs`, which runs the same bodies against a real `JetStream`
//! stream. This file contributes only the fixture: a `MemoryPublisher` of its own per
//! scenario.
//!
//! The ring is what every E2E suite in the workspace runs on, and the honest choice for
//! any single-process deployment, so it is held to the same suite as the broker: a green
//! E2E run is only worth something if the log under it behaves correctly rather than
//! merely consistently.
//!
//! Run with: `cargo test -p notedthat-events --test events_integration_memory`

#[path = "support/event_log_scenarios.rs"]
mod scenarios;

use async_trait::async_trait;
use notedthat_core::{EventId, EventPublisher, KbSlug};
use notedthat_events::MemoryPublisher;
use scenarios::{DROPPED, EventLogFixture, event_log_scenarios, kb_for};

/// Deep enough that no scenario's burst overflows the broadcast channel, shallow enough
/// that the retention scenario's batch stays small.
const MEMORY_CAPACITY: usize = 32;

struct Fixture {
    log: MemoryPublisher,
    kb: KbSlug,
}

fn fixture(scenario: &str) -> Fixture {
    Fixture {
        log: MemoryPublisher::new(MEMORY_CAPACITY),
        kb: kb_for(scenario),
    }
}

#[async_trait]
impl EventLogFixture for Fixture {
    fn log(&self) -> &dyn EventPublisher {
        &self.log
    }

    fn kb(&self) -> &KbSlug {
        &self.kb
    }

    fn retention_batch(&self) -> usize {
        MEMORY_CAPACITY + DROPPED
    }

    /// The ring evicted the oldest `DROPPED` events when the batch overflowed its
    /// capacity; there is nothing left to do.
    async fn retain_out(&self, _first_kept: EventId) {}
}

macro_rules! memory_scenario {
    ($name:ident) => {
        #[tokio::test]
        async fn $name() {
            let fixture = fixture(stringify!($name));
            scenarios::$name(&fixture).await;
        }
    };
}

event_log_scenarios!(memory_scenario);

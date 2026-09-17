#![cfg(feature = "nats")]
#![allow(missing_docs)]

//! The event log integration suite, run against a real NATS `JetStream` stream.
//!
//! Every test body lives in `support/event_log_scenarios.rs` and is shared with
//! `events_integration_memory.rs`, which runs the same bodies against the process-local
//! ring. This file contributes only the fixture: a NATS container, a `NatsPublisher`
//! pointed at it, and one stream — and one knowledge base — per scenario.
//!
//! This is the half that needs Docker, so it is `#[ignore]` and runs in CI's integration
//! job. Run with:
//! ```sh
//! cargo test -p notedthat-events --locked --test events_integration_nats -- --include-ignored
//! ```
//!
//! **One container for the run, one scenario on it at a time.** The adapter captures a
//! fixed subject root, and `JetStream` refuses a second stream whose subjects overlap an
//! existing one's, so the scenarios cannot each have a stream of their own the way the S3
//! suite's scenarios each have a bucket. Sharing one stream is not an option either: the
//! stream sequence is the event id, so concurrent scenarios would see each other's ids as
//! gaps and one scenario's purge would retain out another's events. Instead every scenario
//! takes a turn on a mutex, deletes the stream, and lets `NatsPublisher::connect` create it
//! afresh, so each starts from sequence 1 on a log nobody else touches.
//!
//! The container is shared through a `Weak` rather than a `static` holding it:
//! `testcontainers` removes a container on `Drop` and has no reaper process, so a container
//! parked in a `static` is never dropped and outlives the test run. Held as a `Weak`, it is
//! started by whichever test needs it first, shared by every test overlapping that one, and
//! removed when the last of them finishes. Every scenario takes its `Arc` *before* queueing
//! for its turn, so under the default parallelism the container lives for the whole run;
//! `--test-threads=1` turns that into one boot and one removal per scenario. **Do not run
//! this suite serialized** — it serializes itself where it matters.

#[path = "support/event_log_scenarios.rs"]
mod scenarios;

use std::sync::{Arc, Weak};
use std::time::Duration;

use async_nats::jetstream;

use async_trait::async_trait;
use notedthat_core::{EventId, EventPublisher, KbSlug};
use notedthat_events::{NatsConfig, NatsPublisher};
use scenarios::{EventLogFixture, event_log_scenarios, kb_for};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

struct Broker {
    _container: ContainerAsync<GenericImage>,
    url: String,
}

/// Start NATS with `JetStream` and wait for the line it prints once it is serving.
///
/// Never a fixed sleep: on a loaded machine the container is not ready when the sleep
/// expires and the first connect fails against a client timeout, reported as something
/// else entirely.
async fn start_broker() -> Broker {
    let container = GenericImage::new("nats", "2.12-alpine")
        .with_exposed_port(4222_u16.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Server is ready"))
        .with_cmd(["-js"])
        .start()
        .await
        .expect("start NATS container");
    let port = container
        .get_host_port_ipv4(4222_u16)
        .await
        .expect("NATS mapped port");
    Broker {
        _container: container,
        url: format!("nats://127.0.0.1:{port}"),
    }
}

/// The container every currently-running scenario shares.
///
/// The lock is held across the start so that a cold suite boots one container rather than
/// one per test that raced to find the slot empty. See the module comment for why `Weak`.
static SHARED: tokio::sync::Mutex<Weak<Broker>> = tokio::sync::Mutex::const_new(Weak::new());

async fn shared_broker() -> Arc<Broker> {
    let mut slot = SHARED.lock().await;
    if let Some(running) = slot.upgrade() {
        return running;
    }
    let started = Arc::new(start_broker().await);
    *slot = Arc::downgrade(&started);
    started
}

/// The one stream every scenario runs on, in turn.
const STREAM: &str = "nt-integration";

/// Whose turn it is on [`STREAM`]. See the module comment.
static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    _broker: Arc<Broker>,
    _turn: tokio::sync::MutexGuard<'static, ()>,
    js: jetstream::Context,
    log: NatsPublisher,
    kb: KbSlug,
}

async fn fixture(scenario: &str) -> Fixture {
    // The container first, so it stays up while this scenario waits for its turn.
    let broker = shared_broker().await;
    let turn = TURN.lock().await;

    // A fresh stream for every scenario: delete whatever the previous turn left, then let
    // the adapter create it as it would at server start-up, so sequences begin at 1.
    let client = async_nats::connect(&broker.url)
        .await
        .expect("test client connects");
    let js = jetstream::new(client);
    let _ = js.delete_stream(STREAM).await;
    assert!(
        js.get_stream(STREAM).await.is_err(),
        "the previous scenario's stream must be gone before this one starts"
    );

    let log = NatsPublisher::connect(&NatsConfig {
        url: broker.url.clone(),
        stream: STREAM.to_string(),
        max_age: Duration::from_secs(3600),
    })
    .await
    .expect("connect to the NATS container");
    Fixture {
        _broker: broker,
        _turn: turn,
        js,
        log,
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

    /// The broker retains by age, so any small batch will do; `retain_out` purges.
    fn retention_batch(&self) -> usize {
        6
    }

    /// Purge the stream below `first_kept`, as retention would once the events aged out.
    async fn retain_out(&self, first_kept: EventId) {
        let stream = self.js.get_stream(STREAM).await.expect("stream exists");
        stream
            .purge()
            .sequence(first_kept.0)
            .await
            .expect("purge below first_kept");
    }
}

macro_rules! nats_scenario {
    ($name:ident) => {
        #[tokio::test]
        #[ignore = "requires a NATS JetStream testcontainer"]
        async fn $name() {
            let fixture = fixture(stringify!($name)).await;
            scenarios::$name(&fixture).await;
        }
    };
}

event_log_scenarios!(nats_scenario);

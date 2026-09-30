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
    broker: Arc<Broker>,
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
        connect: notedthat_nats::NatsConnectConfig::plain(broker.url.clone()),
        stream: STREAM.to_string(),
        max_age: Duration::from_secs(3600),
    })
    .await
    .expect("connect to the NATS container");
    Fixture {
        broker,
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

/// What an application consuming the stream directly relies on (`docs/NATS.md`): the
/// subject, the two headers and a payload that is exactly the SSE `data:` object.
#[tokio::test]
#[ignore = "requires a NATS JetStream testcontainer"]
async fn every_message_carries_the_public_contract() {
    let fixture = fixture("contract").await;
    let event = notedthat_core::ObjectEvent::written(
        fixture.kb.clone(),
        notedthat_core::ObjectPath::try_from_str("inbox/memo.md").unwrap(),
        "\"9a3f\"".to_string(),
        12,
        "text/markdown".to_string(),
        1_757_950_000,
        notedthat_core::EventSource::Http,
    );
    let id = fixture.log.publish(event.clone()).await.expect("publish");

    let stream = fixture.js.get_stream(STREAM).await.expect("stream exists");
    let raw = stream.get_raw_message(id.0).await.expect("message stored");
    assert_eq!(
        raw.subject.as_str(),
        format!("notedthat.events.{}.written", fixture.kb.as_str())
    );
    assert_eq!(
        raw.headers
            .get(notedthat_nats::SCHEMA_HEADER)
            .map(async_nats::HeaderValue::as_str),
        Some(notedthat_events::EVENT_SCHEMA)
    );
    assert_eq!(notedthat_events::EVENT_SCHEMA, "object-event/1");
    let message_id = raw
        .headers
        .get("Nats-Msg-Id")
        .map(|value| value.as_str().to_string())
        .expect("every message carries a Nats-Msg-Id");
    assert!(!message_id.is_empty());

    let payload: serde_json::Value = serde_json::from_slice(&raw.payload).unwrap();
    assert_eq!(payload, serde_json::to_value(&event).unwrap());
    assert_eq!(payload["event"], "object.written");
    assert_eq!(payload["object_key"], "inbox/memo.md");
}

/// Two publishes of one event get two ids: deduplication is per publish, never per
/// content, so a rewrite of identical bytes is still announced.
#[tokio::test]
#[ignore = "requires a NATS JetStream testcontainer"]
async fn each_publish_has_its_own_message_id_and_a_repeated_id_is_dropped() {
    let fixture = fixture("dedup").await;
    let event = notedthat_core::ObjectEvent::deleted(
        fixture.kb.clone(),
        notedthat_core::ObjectPath::try_from_str("inbox/old.md").unwrap(),
        notedthat_core::EventSource::Webdav,
    );
    let first = fixture.log.publish(event.clone()).await.unwrap();
    let second = fixture.log.publish(event).await.unwrap();
    assert_ne!(first, second, "identical content is announced twice");

    // What the id buys: a retried publish under the same id is stored once.
    let subject = format!("notedthat.events.{}.deleted", fixture.kb.as_str());
    let publish = || {
        fixture.js.send_publish(
            subject.clone(),
            async_nats::jetstream::message::PublishMessage::build()
                .payload("{}".into())
                .message_id("retried-once"),
        )
    };
    let a = publish().await.unwrap().await.unwrap();
    let b = publish().await.unwrap().await.unwrap();
    assert!(!a.duplicate);
    assert!(b.duplicate);
    assert_eq!(a.sequence, b.sequence);
}

/// An existing stream follows the settings `JetStream` can change in place and refuses
/// to start over one it cannot.
#[tokio::test]
#[ignore = "requires a NATS JetStream testcontainer"]
async fn an_existing_stream_follows_changeable_settings_and_refuses_the_rest() {
    let fixture = fixture("settings").await;
    let url = fixture.broker.url.clone();
    let mut config = NatsConfig {
        connect: notedthat_nats::NatsConnectConfig::plain(url),
        stream: STREAM.to_string(),
        max_age: Duration::from_secs(600),
    };
    config.connect.streams.duplicate_window = Some(Duration::from_secs(30));
    NatsPublisher::connect(&config)
        .await
        .expect("a changed window and retention are applied");
    let info = fixture
        .js
        .get_stream(STREAM)
        .await
        .unwrap()
        .info()
        .await
        .unwrap()
        .config
        .clone();
    assert_eq!(info.duplicate_window, Duration::from_secs(30));
    assert_eq!(info.max_age, Duration::from_secs(600));

    config.connect.streams.storage = Some(notedthat_nats::NatsStorage::Memory);
    let Err(error) = NatsPublisher::connect(&config).await else {
        panic!("a different storage type must refuse startup");
    };
    let message = error.to_string();
    assert!(message.contains("storage"), "{message}");
    assert!(message.contains("cannot change it in place"), "{message}");
}

/// An upgrade that introduced the shared stream settings does not reset a stream an
/// operator tuned by hand: what is not configured is kept as found.
#[tokio::test]
#[ignore = "requires a NATS JetStream testcontainer"]
async fn unconfigured_stream_settings_are_kept_as_found() {
    let fixture = fixture("kept").await;
    let mut stream = fixture.js.get_stream(STREAM).await.unwrap();
    let tuned = async_nats::jetstream::stream::Config {
        max_messages: 5_000,
        duplicate_window: Duration::from_secs(45),
        storage: async_nats::jetstream::stream::StorageType::File,
        ..stream.info().await.unwrap().config.clone()
    };
    fixture.js.update_stream(&tuned).await.unwrap();

    NatsPublisher::connect(&NatsConfig {
        connect: notedthat_nats::NatsConnectConfig::plain(fixture.broker.url.clone()),
        stream: STREAM.to_string(),
        max_age: Duration::from_secs(3600),
    })
    .await
    .expect("an existing stream with unconfigured settings is accepted");
    let found = stream.info().await.unwrap().config.clone();
    assert_eq!(found.max_messages, 5_000);
    assert_eq!(found.duplicate_window, Duration::from_secs(45));
}

/// A retention shorter than the default duplicate window starts, fresh and on an
/// existing stream alike: nats-server refuses a window longer than `max_age`.
#[tokio::test]
#[ignore = "requires a NATS JetStream testcontainer"]
async fn a_short_retention_takes_the_default_duplicate_window_with_it() {
    let fixture = fixture("short").await;
    let short = NatsConfig {
        connect: notedthat_nats::NatsConnectConfig::plain(fixture.broker.url.clone()),
        stream: STREAM.to_string(),
        max_age: Duration::from_secs(60),
    };
    NatsPublisher::connect(&short)
        .await
        .expect("an existing stream is shortened with its window");
    let mut stream = fixture.js.get_stream(STREAM).await.unwrap();
    let found = stream.info().await.unwrap().config.clone();
    assert_eq!(found.max_age, Duration::from_secs(60));
    assert_eq!(found.duplicate_window, Duration::from_secs(60));

    fixture.js.delete_stream(STREAM).await.unwrap();
    NatsPublisher::connect(&short)
        .await
        .expect("a fresh stream is created with a window that fits");
    let found = stream.info().await.unwrap().config.clone();
    assert_eq!(found.duplicate_window, Duration::from_secs(60));
}

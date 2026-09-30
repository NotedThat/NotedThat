//! The `nats` adapter: one `JetStream` stream shared by every replica.
//!
//! The stream sequence is the event id, so ids are strictly increasing across
//! replicas and across knowledge bases alike — a subscriber to one knowledge
//! base sees gaps where other knowledge bases' events sit, which is normal.
//! Every event is published to
//! `notedthat.events.<kb>.<written|deleted|indexed|index_failed>` with a
//! `Nats-Msg-Id` and a `NotedThat-Schema: object-event/1` header — the public
//! contract `docs/NATS.md` documents for applications that consume the stream
//! directly — and a subscription is an ordered, ack-less consumer filtered to
//! `notedthat.events.<kb>.>` that starts one past the requested position.
//!
//! Retention is the stream's `max_age`. A position older than the stream's
//! first sequence is [`SubscribeError::Gone`], decided on the global sequence:
//! conservative for a knowledge base whose own events all survived, but never
//! a silent skip.

use async_nats::Client;
use async_nats::jetstream::consumer::DeliverPolicy;
use async_nats::jetstream::consumer::pull::OrderedConfig;
use async_nats::jetstream::context::PublishErrorKind;
use async_nats::jetstream::message::PublishMessage;
use async_nats::jetstream::stream::{DiscardPolicy, RetentionPolicy};
use async_nats::jetstream::{self, Context};
use async_trait::async_trait;
use futures::StreamExt;
use notedthat_core::{
    EventId, EventPublisher, EventStream, KbSlug, ObjectEvent, ObjectEventKind, PublishError,
    StreamError, SubscribeError,
};

use notedthat_core::metrics::name;
use notedthat_nats::{
    NatsConnectError, SCHEMA_HEADER, StreamSetupError, StreamSpec, TIMEOUT, ensure_stream,
};

use crate::config::NatsConfig;

/// The schema every event message declares in [`SCHEMA_HEADER`].
pub const EVENT_SCHEMA: &str = "object-event/1";

/// Every subject this adapter publishes sits under this root; the stream
/// subscribes to `notedthat.events.>`.
pub const SUBJECT_ROOT: &str = "notedthat.events";

/// Errors from bringing the adapter up.
#[derive(Debug, thiserror::Error)]
pub enum NatsError {
    /// The server could not be reached.
    #[error(transparent)]
    Connect(#[from] NatsConnectError),
    /// The stream could not be brought up, or is not ours.
    #[error(transparent)]
    Stream(#[from] StreamSetupError),
}

/// The `JetStream` event log.
pub struct NatsPublisher {
    client: Client,
    js: Context,
    stream: String,
}

/// The subject filter the stream captures.
fn stream_subject() -> String {
    format!("{SUBJECT_ROOT}.>")
}

/// The subject one knowledge base's events are published under.
fn kb_subject(kb: &KbSlug) -> String {
    format!("{SUBJECT_ROOT}.{}.>", kb.as_str())
}

/// The subject one event is published to.
fn event_subject(event: &ObjectEvent) -> String {
    let kind = match event.kind {
        ObjectEventKind::Written { .. } => "written",
        ObjectEventKind::Deleted => "deleted",
        ObjectEventKind::Indexed { .. } => "indexed",
        ObjectEventKind::IndexFailed { .. } => "index_failed",
    };
    format!("{SUBJECT_ROOT}.{}.{kind}", event.kb.as_str())
}

/// Whether resuming after `after` cannot be honoured: events between it and
/// the stream's first retained sequence were retained out, or `after` is ahead
/// of the stream altogether (the stream was recreated, so the position never
/// existed here and whatever was published since is exactly what is missed).
///
/// `first` and `last` are the stream's current bounds. A stream that has never
/// held a message reports `(0, 0)`; once everything aged out of it, `(n + 1, n)`,
/// and a position below `n` did miss events.
fn is_gone(after: EventId, first: u64, last: u64) -> bool {
    (after.0 < last && after.0 + 1 < first) || after.0 > last
}

impl NatsPublisher {
    /// Open a connection of its own, then [`Self::open`] the stream on it.
    ///
    /// # Errors
    ///
    /// As [`notedthat_nats::connect`] and [`Self::open`].
    pub async fn connect(config: &NatsConfig) -> Result<Self, NatsError> {
        let client = notedthat_nats::connect(&config.connect, "notedthat-server").await?;
        Self::open(client, config).await
    }

    /// Create the stream on an existing connection if it does not exist yet,
    /// or bring its settings in line with the configuration if it does.
    ///
    /// # Errors
    ///
    /// The stream cannot be created, inspected or updated, or a stream by that
    /// name already captures other subjects or has an unchangeable setting that
    /// differs (see [`ensure_stream`]).
    pub async fn open(client: Client, config: &NatsConfig) -> Result<Self, NatsError> {
        let mut js = jetstream::new(client.clone());
        js.set_timeout(TIMEOUT);
        ensure_stream(
            &js,
            &StreamSpec {
                name: config.stream.clone(),
                name_setting: "NOTEDTHAT_NATS_STREAM",
                subject: stream_subject(),
                retention: RetentionPolicy::Limits,
                discard: DiscardPolicy::Old,
                max_age: config.max_age,
                max_messages: None,
                settings: config.connect.streams,
            },
        )
        .await?;
        Ok(Self {
            client,
            js,
            stream: config.stream.clone(),
        })
    }

    /// Publish once with `message_id`, awaiting the acknowledgement.
    async fn publish_once(
        &self,
        subject: &str,
        payload: &bytes::Bytes,
        message_id: &str,
    ) -> Result<
        async_nats::jetstream::publish::PublishAck,
        async_nats::jetstream::context::PublishError,
    > {
        self.js
            .send_publish(
                subject.to_string(),
                PublishMessage::build()
                    .payload(payload.clone())
                    .message_id(message_id)
                    .header(SCHEMA_HEADER, EVENT_SCHEMA),
            )
            .await?
            .await
    }
}

#[async_trait]
impl EventPublisher for NatsPublisher {
    async fn publish(&self, event: ObjectEvent) -> Result<EventId, PublishError> {
        let payload: bytes::Bytes = serde_json::to_vec(&event)
            .map_err(|error| PublishError::Unavailable {
                message: format!("could not serialise event: {error}"),
            })?
            .into();
        let subject = event_subject(&event);
        // One id per logical publish, reused on the retry below: a timed-out
        // acknowledgement does not say whether the broker stored the message,
        // and the duplicate window turns the second attempt into a no-op if it
        // did. A retried *write* is a new publish with a new id — that is the
        // at-least-once contract, and it is unchanged.
        let message_id = uuid::Uuid::now_v7().to_string();
        let ack = match self.publish_once(&subject, &payload, &message_id).await {
            Err(error) if error.kind() == PublishErrorKind::TimedOut => {
                self.publish_once(&subject, &payload, &message_id).await
            }
            other => other,
        }
        .map_err(|error| PublishError::Unavailable {
            message: error.to_string(),
        })?;
        if ack.duplicate {
            metrics::counter!(name::EVENTS_PUBLISH_DEDUPLICATED).increment(1);
        }
        Ok(EventId(ack.sequence))
    }

    async fn subscribe(
        &self,
        kb: &KbSlug,
        after: Option<EventId>,
    ) -> Result<EventStream, SubscribeError> {
        let unavailable = |error: &dyn std::fmt::Display| SubscribeError::Unavailable {
            message: error.to_string(),
        };
        let mut stream = self
            .js
            .get_stream(&self.stream)
            .await
            .map_err(|error| unavailable(&error))?;
        let state = &stream
            .info()
            .await
            .map_err(|error| unavailable(&error))?
            .state;
        let (first, last) = (state.first_sequence, state.last_sequence);

        let deliver_policy = match after {
            Some(requested) if is_gone(requested, first, last) => {
                // A never-written stream reports `first == 0`; the oldest id it
                // can ever answer for is the first sequence it will hand out.
                return Err(SubscribeError::Gone {
                    requested,
                    oldest: EventId(first.max(1)),
                });
            }
            // A position at or below the end starts one past it even when that
            // is the very next sequence: `New` would skip anything published
            // between reading the bounds above and the consumer coming up, which
            // is exactly the window a reconnecting, caught-up client sits in.
            Some(requested) => DeliverPolicy::ByStartSequence {
                start_sequence: requested.0 + 1,
            },
            None => DeliverPolicy::New,
        };

        let consumer = stream
            .create_consumer(OrderedConfig {
                filter_subject: kb_subject(kb),
                deliver_policy,
                ..OrderedConfig::default()
            })
            .await
            .map_err(|error| unavailable(&error))?;
        let messages = consumer
            .messages()
            .await
            .map_err(|error| unavailable(&error))?;

        let events = messages.filter_map(|message| {
            let item = match message {
                Ok(message) => match message.info() {
                    Ok(info) => {
                        let sequence = info.stream_sequence;
                        decode(sequence, &message.payload)
                            .map(|event| Ok((EventId(sequence), event)))
                    }
                    Err(error) => Some(Err(StreamError::Broker {
                        message: format!("message without JetStream metadata: {error}"),
                    })),
                },
                Err(error) => Some(Err(StreamError::Broker {
                    message: error.to_string(),
                })),
            };
            futures::future::ready(item)
        });
        Ok(events.boxed())
    }

    fn ready(&self) -> bool {
        self.client.connection_state() == async_nats::connection::State::Connected
    }

    fn backend_name(&self) -> &'static str {
        "nats"
    }
}

/// The event in a message's payload, or `None` for one this build cannot read.
///
/// An undecodable message is skipped rather than ending the subscription: the
/// stream would end on it again after every reconnect, since a reconnect
/// resumes just before it, and so wedge every subscriber of the knowledge base
/// until retention drops it. Ids already have gaps, so a skipped one is not a
/// new shape for a subscriber. One way to get here is an event written before
/// an upgrade that made its key invalid, such as a key `ObjectPath` now
/// reserves (#279).
fn decode(sequence: u64, payload: &[u8]) -> Option<ObjectEvent> {
    match serde_json::from_slice(payload) {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::warn!(sequence, %error, "skipping an event that is not an ObjectEvent");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notedthat_core::{EventSource, ObjectPath};

    fn kb() -> KbSlug {
        KbSlug::try_new("notes").unwrap()
    }

    #[test]
    fn subjects_nest_under_the_root_by_knowledge_base_and_kind() {
        assert_eq!(stream_subject(), "notedthat.events.>");
        assert_eq!(kb_subject(&kb()), "notedthat.events.notes.>");
        let written = ObjectEvent::written(
            kb(),
            ObjectPath::try_from("a.md").unwrap(),
            "\"e\"".into(),
            1,
            "text/markdown".into(),
            0,
            EventSource::Http,
        );
        assert_eq!(event_subject(&written), "notedthat.events.notes.written");
        let deleted = ObjectEvent::deleted(
            kb(),
            ObjectPath::try_from("a.md").unwrap(),
            EventSource::Http,
        );
        assert_eq!(event_subject(&deleted), "notedthat.events.notes.deleted");
        let indexed = ObjectEvent::indexed(
            kb(),
            ObjectPath::try_from("a.md").unwrap(),
            "\"e\"".into(),
            "text/markdown".into(),
            2,
        );
        assert_eq!(event_subject(&indexed), "notedthat.events.notes.indexed");
        let failed = ObjectEvent::index_failed(
            kb(),
            ObjectPath::try_from("a.md").unwrap(),
            None,
            None,
            "embedder.embed failed".into(),
        );
        assert_eq!(
            event_subject(&failed),
            "notedthat.events.notes.index_failed"
        );
    }

    #[test]
    fn gone_means_events_between_the_position_and_the_first_retained_were_lost() {
        // Stream holds 10..=20.
        assert!(is_gone(EventId(5), 10, 20), "6..9 are lost");
        assert!(is_gone(EventId(8), 10, 20), "9 is lost");
        assert!(!is_gone(EventId(9), 10, 20), "exactly at the edge");
        assert!(!is_gone(EventId(15), 10, 20));
        assert!(!is_gone(EventId(20), 10, 20), "caught up");
        assert!(
            is_gone(EventId(99), 10, 20),
            "ahead of the stream never existed"
        );
        // Empty stream after everything aged out: 4..=20 existed and are lost.
        assert!(is_gone(EventId(3), 21, 20));
        assert!(
            !is_gone(EventId(20), 21, 20),
            "caught up before the age-out"
        );
        // Never-written stream: from the start is fine, a stale position is not.
        assert!(!is_gone(EventId(0), 0, 0));
        assert!(is_gone(EventId(7), 0, 0), "the stream was recreated");
    }

    #[test]
    fn an_undecodable_event_is_skipped_not_fatal() {
        let event = ObjectEvent::deleted(
            kb(),
            ObjectPath::try_from("a.md").unwrap(),
            EventSource::Http,
        );
        let payload = serde_json::to_vec(&event).unwrap();
        assert_eq!(decode(1, &payload), Some(event));

        // The same event for a key that is now reserved no longer decodes.
        let stale = String::from_utf8(payload)
            .unwrap()
            .replace("\"a.md\"", "\"index\"");
        assert_ne!(stale.find("\"index\""), None, "the key was rewritten");
        assert_eq!(decode(2, stale.as_bytes()), None);
        assert_eq!(decode(3, b"not json"), None);
    }
}

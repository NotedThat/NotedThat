//! The `nats` adapter: one `JetStream` stream shared by every replica.
//!
//! The stream sequence is the event id, so ids are strictly increasing across
//! replicas and across knowledge bases alike — a subscriber to one knowledge
//! base sees gaps where other knowledge bases' events sit, which is normal.
//! Every event is published to `notedthat.events.<kb>.<written|deleted>`, and
//! a subscription is an ordered, ack-less consumer filtered to
//! `notedthat.events.<kb>.>` that starts one past the requested position.
//!
//! Retention is the stream's `max_age`. A position older than the stream's
//! first sequence is [`SubscribeError::Gone`], decided on the global sequence:
//! conservative for a knowledge base whose own events all survived, but never
//! a silent skip.

use std::time::Duration;

use async_nats::jetstream::consumer::DeliverPolicy;
use async_nats::jetstream::consumer::pull::OrderedConfig;
use async_nats::jetstream::stream::{Config as StreamConfig, DiscardPolicy, RetentionPolicy};
use async_nats::jetstream::{self, Context};
use async_nats::{Client, ConnectOptions};
use async_trait::async_trait;
use futures::StreamExt;
use notedthat_core::{
    EventId, EventPublisher, EventStream, KbSlug, ObjectEvent, ObjectEventKind, PublishError,
    StreamError, SubscribeError,
};

use crate::config::NatsConfig;

/// Every subject this adapter publishes sits under this root; the stream
/// subscribes to `notedthat.events.>`.
pub const SUBJECT_ROOT: &str = "notedthat.events";

/// How long a connect, a `JetStream` API call or a publish acknowledgement may
/// take before it is a failure.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Errors from bringing the adapter up.
#[derive(Debug, thiserror::Error)]
pub enum NatsError {
    /// The server could not be reached.
    #[error("could not connect to NATS: {0}")]
    Connect(#[from] async_nats::ConnectError),
    /// The stream could not be created or read.
    #[error("could not open JetStream stream {stream}: {message}")]
    Stream {
        /// The configured stream name.
        stream: String,
        /// What the server said.
        message: String,
    },
    /// A stream by the configured name exists but is not ours.
    #[error(
        "JetStream stream {stream} exists with subjects {subjects:?}, not [\"{expected}\"]; \
         point NOTEDTHAT_NATS_STREAM at a stream NotedThat owns"
    )]
    StreamMismatch {
        /// The configured stream name.
        stream: String,
        /// The subjects the existing stream captures.
        subjects: Vec<String>,
        /// The subject filter this adapter needs.
        expected: String,
    },
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
    /// Connect, and create the stream if it does not exist yet.
    ///
    /// # Errors
    ///
    /// The server cannot be reached within a few seconds, the stream cannot be
    /// created or inspected, or a stream by that name already captures other
    /// subjects.
    pub async fn connect(config: &NatsConfig) -> Result<Self, NatsError> {
        let client = ConnectOptions::new()
            .connection_timeout(TIMEOUT)
            .request_timeout(Some(TIMEOUT))
            .name("notedthat-server")
            .connect(&config.url)
            .await?;
        let mut js = jetstream::new(client.clone());
        js.set_timeout(TIMEOUT);

        let desired = StreamConfig {
            name: config.stream.clone(),
            subjects: vec![stream_subject()],
            max_age: config.max_age,
            retention: RetentionPolicy::Limits,
            discard: DiscardPolicy::Old,
            ..StreamConfig::default()
        };
        let stream = js
            .get_or_create_stream(desired.clone())
            .await
            .map_err(|error| NatsError::Stream {
                stream: config.stream.clone(),
                message: error.to_string(),
            })?;
        let info = stream.cached_info();
        if info.config.subjects != desired.subjects {
            return Err(NatsError::StreamMismatch {
                stream: config.stream.clone(),
                subjects: info.config.subjects.clone(),
                expected: stream_subject(),
            });
        }
        if info.config.max_age != desired.max_age {
            // The operator changed the retention; the stream follows the config.
            // Only that field: the rest of the existing configuration (replicas,
            // storage, limits) is the operator's and is kept as found.
            let updated = StreamConfig {
                max_age: desired.max_age,
                ..info.config.clone()
            };
            js.update_stream(&updated)
                .await
                .map_err(|error| NatsError::Stream {
                    stream: config.stream.clone(),
                    message: format!("could not update max_age: {error}"),
                })?;
        }

        Ok(Self {
            client,
            js,
            stream: config.stream.clone(),
        })
    }
}

#[async_trait]
impl EventPublisher for NatsPublisher {
    async fn publish(&self, event: ObjectEvent) -> Result<EventId, PublishError> {
        let payload = serde_json::to_vec(&event).map_err(|error| PublishError::Unavailable {
            message: format!("could not serialise event: {error}"),
        })?;
        let ack = self
            .js
            .publish(event_subject(&event), payload.into())
            .await
            .map_err(|error| PublishError::Unavailable {
                message: error.to_string(),
            })?
            .await
            .map_err(|error| PublishError::Unavailable {
                message: error.to_string(),
            })?;
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

        let events = messages.map(|message| match message {
            Ok(message) => {
                let sequence = message
                    .info()
                    .map_err(|error| StreamError::Broker {
                        message: format!("message without JetStream metadata: {error}"),
                    })?
                    .stream_sequence;
                let event: ObjectEvent =
                    serde_json::from_slice(&message.payload).map_err(|error| {
                        StreamError::Broker {
                            message: format!("event {sequence} is not an ObjectEvent: {error}"),
                        }
                    })?;
                Ok((EventId(sequence), event))
            }
            Err(error) => Err(StreamError::Broker {
                message: error.to_string(),
            }),
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
}

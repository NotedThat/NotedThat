//! Object change events: the one shape every surface publishes and every
//! subscriber reads (SPECIFICATIONS.md §6.14, D55).
//!
//! An [`ObjectEvent`] says that one key in one knowledge base was written or
//! deleted, by which surface, and — for a write — what the bytes now look like.
//! The [`EventPublisher`] trait is the seam between the code that knows a change
//! happened (`notedthat-write` after storage acknowledges; the indexer worker
//! after re-reading a detected change) and the adapter that keeps the log
//! (`notedthat-events`: a process-local ring or a `JetStream` stream).
//!
//! Ids are the adapter's: strictly increasing within one deployment, never
//! reused, and stable across replicas when the adapter is a shared broker. The
//! HTTP layer hands them to clients as SSE `id:` fields and takes them back as
//! `Last-Event-ID`, so the adapter — not the server — decides what "after" means.

use std::fmt;
use std::pin::Pin;
use std::str::FromStr;

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::kb::ObjectMeta;
use crate::object_path::ObjectPath;
use crate::slug::KbSlug;

/// The position of one event in the log.
///
/// Rendered as a decimal on the wire; parsed back from `Last-Event-ID`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub u64);

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for EventId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<u64>().map(Self)
    }
}

/// Which surface made the change.
///
/// Informational: `Mcp` is self-declared by the MCP server's HTTP client and any
/// client can claim it. `FsWatch` and `Reconcile` are detected changes on the
/// `fs` backend (D50) and describe what the server found, not who did it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventSource {
    /// The HTTP API.
    Http,
    /// The `WebDAV` surface.
    Webdav,
    /// The MCP server, which writes through the HTTP API.
    Mcp,
    /// The `fs` backend's filesystem watcher.
    FsWatch,
    /// The `fs` backend's startup or on-demand comparison against the index.
    Reconcile,
}

impl EventSource {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Webdav => "webdav",
            Self::Mcp => "mcp",
            Self::FsWatch => "fs-watch",
            Self::Reconcile => "reconcile",
        }
    }
}

impl fmt::Display for EventSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What happened to the key.
///
/// Serialised with an `event` tag so the JSON payload is self-describing on
/// its own, without the SSE `event:` field or the broker subject around it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum ObjectEventKind {
    /// The key was created or its bytes were replaced. Neither the API nor
    /// storage distinguish the two, so neither does the event.
    #[serde(rename = "object.written")]
    Written {
        /// The `ETag` of the bytes the event describes, quoted per RFC 7232.
        etag: String,
        /// Size in bytes.
        size: u64,
        /// Content type as stored.
        mime: String,
        /// Last-modified Unix timestamp in seconds.
        mtime: i64,
    },
    /// The key no longer exists.
    #[serde(rename = "object.deleted")]
    Deleted,
}

impl ObjectEventKind {
    /// The SSE `event:` name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Written { .. } => "object.written",
            Self::Deleted => "object.deleted",
        }
    }

    /// The content type, for writes.
    #[must_use]
    pub fn mime(&self) -> Option<&str> {
        match self {
            Self::Written { mime, .. } => Some(mime),
            Self::Deleted => None,
        }
    }
}

/// One change to one object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectEvent {
    /// The knowledge base.
    pub kb: KbSlug,
    /// The key within it.
    pub object_key: ObjectPath,
    /// What happened, with the stamp of the bytes for a write.
    #[serde(flatten)]
    pub kind: ObjectEventKind,
    /// Which surface made or detected the change.
    pub source: EventSource,
    /// When the server published it, RFC 3339 in UTC.
    pub occurred_at: String,
}

impl ObjectEvent {
    /// A write, stamped now.
    #[must_use]
    pub fn written(
        kb: KbSlug,
        object_key: ObjectPath,
        etag: String,
        size: u64,
        content_type: String,
        last_modified: i64,
        source: EventSource,
    ) -> Self {
        Self {
            kb,
            object_key,
            kind: ObjectEventKind::Written {
                etag,
                size,
                mime: content_type,
                mtime: last_modified,
            },
            source,
            occurred_at: now_rfc3339(),
        }
    }

    /// A deletion, stamped now.
    #[must_use]
    pub fn deleted(kb: KbSlug, object_key: ObjectPath, source: EventSource) -> Self {
        Self {
            kb,
            object_key,
            kind: ObjectEventKind::Deleted,
            source,
            occurred_at: now_rfc3339(),
        }
    }

    /// A write described by a fresh `HEAD`, for changes the server detected
    /// rather than made.
    #[must_use]
    pub fn from_meta(
        kb: KbSlug,
        object_key: ObjectPath,
        meta: &ObjectMeta,
        source: EventSource,
    ) -> Self {
        Self::written(
            kb,
            object_key,
            meta.etag.clone().unwrap_or_default(),
            meta.size,
            meta.content_type.clone().unwrap_or_default(),
            meta.last_modified.unwrap_or(0),
            source,
        )
    }
}

/// Publishing failed after the write was already durable.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    /// The log could not take the event.
    #[error("event backend unavailable: {message}")]
    Unavailable {
        /// What the adapter reported.
        message: String,
    },
}

/// Subscribing failed before any event was delivered.
#[derive(Debug, thiserror::Error)]
pub enum SubscribeError {
    /// The requested position has been retained out; the subscriber must
    /// resync by listing rather than silently resume from "now".
    #[error("events after {requested} are no longer retained; the oldest retained id is {oldest}")]
    Gone {
        /// The `Last-Event-ID` the subscriber asked to resume after.
        requested: EventId,
        /// The oldest id the log still holds.
        oldest: EventId,
    },
    /// The log could not be read.
    #[error("event backend unavailable: {message}")]
    Unavailable {
        /// What the adapter reported.
        message: String,
    },
}

/// A live subscription ended early.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// The subscriber fell too far behind for the adapter to replay the gap.
    /// Reconnecting with `Last-Event-ID: resume_after` either replays it or
    /// answers [`SubscribeError::Gone`].
    #[error("subscriber lagged behind the event log; resume after {resume_after}")]
    Lagged {
        /// The last id the subscriber was handed.
        resume_after: EventId,
    },
    /// The adapter's connection failed mid-stream.
    #[error("event backend failed mid-stream: {message}")]
    Broker {
        /// What the adapter reported.
        message: String,
    },
}

/// Events in id order, as the adapter delivers them.
pub type EventStream =
    Pin<Box<dyn Stream<Item = Result<(EventId, ObjectEvent), StreamError>> + Send>>;

/// The event log.
///
/// `publish` must only be called once the change it describes is durable in
/// storage, so that a subscriber who `GET`s the key on receipt sees those bytes
/// or newer.
#[async_trait]
pub trait EventPublisher: Send + Sync {
    /// Append one event and return its id.
    async fn publish(&self, event: ObjectEvent) -> Result<EventId, PublishError>;

    /// Every event for `kb` with an id greater than `after`, then live events,
    /// in order. `None` means "from now".
    async fn subscribe(
        &self,
        kb: &KbSlug,
        after: Option<EventId>,
    ) -> Result<EventStream, SubscribeError>;

    /// Whether the log can currently take and serve events. Cheap; consulted
    /// by `/readyz`.
    fn ready(&self) -> bool;

    /// The selector value this adapter answers to, for logs and diagnostics.
    fn backend_name(&self) -> &'static str;
}

fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    unix_to_rfc3339(secs)
}

/// Render a Unix timestamp as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Proleptic Gregorian, civil-from-days after Howard Hinnant; timestamps before
/// the epoch render as the epoch, since nothing here can be older than that.
#[must_use]
pub fn unix_to_rfc3339(secs: i64) -> String {
    let secs = secs.max(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Shift the epoch to 0000-03-01 so leap days land at the end of the year.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> KbSlug {
        KbSlug::try_new("notes").expect("valid slug")
    }

    fn key(s: &str) -> ObjectPath {
        ObjectPath::try_from_str(s).expect("valid path")
    }

    #[test]
    fn a_write_serialises_to_the_documented_shape() {
        let mut event = ObjectEvent::written(
            kb(),
            key("inbox/memo.mp3"),
            "\"9a3f\"".into(),
            48_213_011,
            "audio/mpeg".into(),
            1_757_950_000,
            EventSource::Http,
        );
        event.occurred_at = "2026-09-15T14:33:20Z".into();
        let json = serde_json::to_value(&event).expect("serialises");
        assert_eq!(
            json,
            serde_json::json!({
                "event": "object.written",
                "kb": "notes",
                "object_key": "inbox/memo.mp3",
                "etag": "\"9a3f\"",
                "size": 48_213_011,
                "mime": "audio/mpeg",
                "mtime": 1_757_950_000,
                "source": "http",
                "occurred_at": "2026-09-15T14:33:20Z",
            })
        );
        let back: ObjectEvent = serde_json::from_value(json).expect("round trips");
        assert_eq!(back, event);
    }

    #[test]
    fn a_delete_carries_no_stamp_and_round_trips() {
        let event = ObjectEvent::deleted(kb(), key("old.md"), EventSource::Webdav);
        let json = serde_json::to_value(&event).expect("serialises");
        assert_eq!(json["event"], "object.deleted");
        assert_eq!(json["source"], "webdav");
        assert!(json.get("etag").is_none());
        assert!(json.get("mime").is_none());
        let back: ObjectEvent = serde_json::from_value(json).expect("round trips");
        assert_eq!(back, event);
        assert_eq!(event.kind.name(), "object.deleted");
        assert_eq!(event.kind.mime(), None);
    }

    #[test]
    fn sources_use_the_documented_spellings() {
        for (source, wire) in [
            (EventSource::Http, "http"),
            (EventSource::Webdav, "webdav"),
            (EventSource::Mcp, "mcp"),
            (EventSource::FsWatch, "fs-watch"),
            (EventSource::Reconcile, "reconcile"),
        ] {
            assert_eq!(source.as_str(), wire);
            assert_eq!(serde_json::to_value(source).unwrap(), wire);
        }
    }

    #[test]
    fn from_meta_takes_the_head_stamp() {
        let meta = ObjectMeta {
            key: "a.md".into(),
            size: 12,
            last_modified: Some(1_700_000_000),
            content_type: Some("text/markdown".into()),
            etag: Some("\"e1\"".into()),
        };
        let event = ObjectEvent::from_meta(kb(), key("a.md"), &meta, EventSource::FsWatch);
        assert_eq!(
            event.kind,
            ObjectEventKind::Written {
                etag: "\"e1\"".into(),
                size: 12,
                mime: "text/markdown".into(),
                mtime: 1_700_000_000,
            }
        );
        assert_eq!(event.source, EventSource::FsWatch);
    }

    #[test]
    fn event_ids_render_and_parse_as_decimals() {
        assert_eq!(EventId(4812).to_string(), "4812");
        assert_eq!("4812".parse::<EventId>().unwrap(), EventId(4812));
        assert!("".parse::<EventId>().is_err());
        assert!("-1".parse::<EventId>().is_err());
        assert!("abc".parse::<EventId>().is_err());
    }

    #[test]
    fn rfc3339_rendering_matches_known_instants() {
        assert_eq!(unix_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_to_rfc3339(1_757_950_000), "2025-09-15T15:26:40Z");
        assert_eq!(unix_to_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(unix_to_rfc3339(4_107_542_399), "2100-02-28T23:59:59Z");
        assert_eq!(unix_to_rfc3339(-5), "1970-01-01T00:00:00Z");
    }
}

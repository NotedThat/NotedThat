//! `GET /api/v1/knowledgebases/{kb_slug}/events` — object change events as
//! server-sent events (SPECIFICATIONS.md §6.14, D54).
//!
//! The stream is filtered per event by the same evaluator every other surface
//! uses: a subscriber sees a key's events only if it may `list` that key, and
//! a deletion reveals as much as a listing would, so it is filtered the same.
//! The ordering below is load-bearing — resolve, then `list` on anything, then
//! "is there a log at all" — so an undeclared or ungranted knowledge base
//! answers exactly as it does everywhere else, whether or not events are on.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::header::{CACHE_CONTROL, HeaderName, HeaderValue};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use notedthat_core::{
    Error as CoreError, EventId, ObjectEvent, ObjectEventKind, StreamError, SubscribeError, Verb,
};
use serde::Deserialize;

use crate::authz::KbAccess;
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;

/// What a disconnected client should wait before reconnecting, sent as the
/// stream's first frame so it applies before any event does.
const RETRY_HINT: Duration = Duration::from_millis(3000);
/// How often an idle stream sends a comment so proxies and idle timeouts keep
/// the connection open.
const HEARTBEAT: Duration = Duration::from_secs(15);
/// The header a client resumes with, per the SSE specification.
const LAST_EVENT_ID: &str = "last-event-id";

/// Optional server-side filters, applied after the access filter.
#[derive(Debug, Deserialize)]
pub(super) struct EventsQuery {
    prefix: Option<String>,
    event: Option<String>,
    mime: Option<String>,
}

/// The validated form of [`EventsQuery`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EventFilter {
    prefix: Option<String>,
    kind: Option<Kind>,
    mime: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Written,
    Deleted,
}

impl EventFilter {
    pub(crate) fn parse(query: EventsQuery) -> Result<Self, ApiError> {
        let kind = match query.event.as_deref() {
            None => None,
            Some("written") => Some(Kind::Written),
            Some("deleted") => Some(Kind::Deleted),
            Some(other) => {
                return Err(ApiError::Core(CoreError::InvalidInput {
                    message: format!("event must be \"written\" or \"deleted\", got \"{other}\""),
                }));
            }
        };
        Ok(Self {
            prefix: query.prefix.filter(|p| !p.is_empty()),
            kind,
            mime: query.mime.filter(|m| !m.is_empty()),
        })
    }

    /// `mime` matches a write exactly or by `type/*`; a deletion carries no
    /// mime and so never matches a `mime` filter.
    pub(crate) fn matches(&self, event: &ObjectEvent) -> bool {
        if let Some(prefix) = &self.prefix
            && !event.object_key.as_str().starts_with(prefix.as_str())
        {
            return false;
        }
        match (self.kind, &event.kind) {
            (Some(Kind::Written), ObjectEventKind::Deleted)
            | (Some(Kind::Deleted), ObjectEventKind::Written { .. }) => return false,
            _ => {}
        }
        match (&self.mime, event.kind.mime()) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(pattern), Some(mime)) => mime_matches(pattern, mime),
        }
    }
}

fn mime_matches(pattern: &str, mime: &str) -> bool {
    match pattern.strip_suffix("/*") {
        Some(kind) => mime
            .split_once('/')
            .is_some_and(|(actual, _)| actual.eq_ignore_ascii_case(kind)),
        None => pattern.eq_ignore_ascii_case(mime),
    }
}

fn parse_last_event_id(req: &Request) -> Result<Option<EventId>, ApiError> {
    let Some(raw) = req.headers().get(LAST_EVENT_ID) else {
        return Ok(None);
    };
    raw.to_str()
        .ok()
        .and_then(|s| s.trim().parse::<EventId>().ok())
        .map(Some)
        .ok_or_else(|| {
            ApiError::Core(CoreError::InvalidInput {
                message: "Last-Event-ID must be a non-negative integer".into(),
            })
        })
}

pub(super) async fn subscribe_events(
    State(state): State<AppState>,
    Path(kb_slug): Path<String>,
    Query(query): Query<EventsQuery>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let access = KbAccess::resolve(&state, &kb_slug, &req).map_err(&err)?;
    access.require_any(Verb::List).map_err(&err)?;
    let Some(events) = state.events.clone() else {
        return Err(err(ApiError::Core(CoreError::NotFound {
            resource: "object change events are not enabled on this server; \
                       set NOTEDTHAT_EVENTS_BACKEND to memory or nats"
                .into(),
        })));
    };
    let filter = EventFilter::parse(query).map_err(&err)?;
    let after = parse_last_event_id(&req).map_err(&err)?;

    let stream = events
        .subscribe(access.kb(), after)
        .await
        .map_err(|error| {
            err(match error {
                SubscribeError::Gone { requested, oldest } => {
                    ApiError::EventsGone { requested, oldest }
                }
                SubscribeError::Unavailable { message } => ApiError::EventsUnavailable { message },
            })
        })?;

    // The first frame carries the retry hint. Then each event the caller may
    // see, in log order; an adapter error ends the stream with a comment, and
    // the client's reconnect with `Last-Event-ID` either replays the gap or is
    // told it is gone.
    let head = futures::stream::once(async {
        Ok::<Event, Infallible>(Event::default().retry(RETRY_HINT).comment("subscribed"))
    });
    let body = stream
        .scan(false, |ended, item| {
            if *ended {
                return futures::future::ready(None);
            }
            *ended = item.is_err();
            futures::future::ready(Some(item))
        })
        .filter_map(move |item| {
            let frame = match item {
                Ok((id, event)) => {
                    let visible = access.allows(Verb::List, event.object_key.as_str())
                        && filter.matches(&event);
                    visible.then(|| frame_for(id, &event))
                }
                Err(error) => Some(ended(&error)),
            };
            futures::future::ready(frame.map(Ok::<Event, Infallible>))
        });

    let mut response = Sse::new(head.chain(body))
        .keep_alive(KeepAlive::new().interval(HEARTBEAT).text("keep-alive"))
        .into_response();
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    // nginx honours this per response; other proxies need `proxy_buffering off`
    // on the route (docs/API.md).
    headers.insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    Ok(response)
}

fn frame_for(id: EventId, event: &ObjectEvent) -> Event {
    let frame = Event::default().id(id.to_string()).event(event.kind.name());
    match frame.json_data(event) {
        Ok(frame) => frame,
        // The type serialises by construction; a failure here is a bug worth
        // seeing in the stream rather than a silently dropped event.
        Err(error) => Event::default()
            .id(id.to_string())
            .comment(format!("event {id} could not be serialised: {error}")),
    }
}

fn ended(error: &StreamError) -> Event {
    Event::default().comment(format!("stream ended: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notedthat_core::{EventSource, KbSlug, ObjectPath};

    fn written(key: &str, mime: &str) -> ObjectEvent {
        ObjectEvent::written(
            KbSlug::try_new("notes").unwrap(),
            ObjectPath::try_from(key).unwrap(),
            "\"e\"".into(),
            1,
            mime.into(),
            0,
            EventSource::Http,
        )
    }

    fn deleted(key: &str) -> ObjectEvent {
        ObjectEvent::deleted(
            KbSlug::try_new("notes").unwrap(),
            ObjectPath::try_from(key).unwrap(),
            EventSource::Http,
        )
    }

    fn filter(prefix: Option<&str>, event: Option<&str>, mime: Option<&str>) -> EventFilter {
        EventFilter::parse(EventsQuery {
            prefix: prefix.map(str::to_owned),
            event: event.map(str::to_owned),
            mime: mime.map(str::to_owned),
        })
        .unwrap()
    }

    #[test]
    fn no_filter_matches_everything() {
        let f = filter(None, None, None);
        assert!(f.matches(&written("a.md", "text/markdown")));
        assert!(f.matches(&deleted("a.md")));
    }

    #[test]
    fn prefix_is_a_plain_string_prefix_on_the_key() {
        let f = filter(Some("inbox/"), None, None);
        assert!(f.matches(&written("inbox/memo.mp3", "audio/mpeg")));
        assert!(f.matches(&deleted("inbox/old.md")));
        assert!(!f.matches(&written("archive/memo.mp3", "audio/mpeg")));
        assert!(!f.matches(&written("inbox.md", "text/markdown")));
    }

    #[test]
    fn event_selects_one_kind() {
        assert!(filter(None, Some("written"), None).matches(&written("a", "text/plain")));
        assert!(!filter(None, Some("written"), None).matches(&deleted("a")));
        assert!(filter(None, Some("deleted"), None).matches(&deleted("a")));
        assert!(!filter(None, Some("deleted"), None).matches(&written("a", "text/plain")));
    }

    #[test]
    fn an_unknown_event_kind_is_a_bad_request() {
        let err = EventFilter::parse(EventsQuery {
            prefix: None,
            event: Some("renamed".into()),
            mime: None,
        })
        .unwrap_err();
        assert!(matches!(
            err,
            ApiError::Core(CoreError::InvalidInput { .. })
        ));
    }

    #[test]
    fn mime_matches_exactly_or_by_type_wildcard_and_never_a_deletion() {
        let exact = filter(None, None, Some("audio/mpeg"));
        assert!(exact.matches(&written("a.mp3", "audio/mpeg")));
        assert!(exact.matches(&written("a.mp3", "AUDIO/MPEG")));
        assert!(!exact.matches(&written("a.wav", "audio/wav")));
        assert!(!exact.matches(&deleted("a.mp3")));

        let wild = filter(None, None, Some("audio/*"));
        assert!(wild.matches(&written("a.mp3", "audio/mpeg")));
        assert!(wild.matches(&written("a.wav", "audio/wav")));
        assert!(!wild.matches(&written("a.md", "text/markdown")));
        assert!(!wild.matches(&deleted("a.mp3")));
    }

    #[test]
    fn empty_filters_are_no_filters() {
        assert_eq!(filter(Some(""), None, Some("")), filter(None, None, None));
    }
}

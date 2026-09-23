//! `GET /api/v1/knowledgebases/{kb_slug}/events` — object change events and
//! the indexer's outcomes as server-sent events (SPECIFICATIONS.md §6.14, D55,
//! D65).
//!
//! The stream is filtered per event by the same evaluator every other surface
//! uses: a subscriber sees a key's events only if it may `list` that key, and
//! a deletion reveals as much as a listing would, so it is filtered the same.
//! One field is held back inside a visible frame: the `summary` on
//! `object.index_failed` follows the rule `GET …/index` applies to the same
//! string (D62) and goes only to a subscriber who may `list` the whole base.
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

use notedthat_core::metrics::{label as metric_label, name as metric};

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
    Indexed,
    IndexFailed,
}

impl Kind {
    /// `written`, `deleted`, `indexed` or `index_failed`; the wire name's
    /// `object.` prefix is accepted so `?event=object.indexed` works as the
    /// event is documented.
    fn parse(raw: &str) -> Option<Self> {
        match raw.strip_prefix("object.").unwrap_or(raw) {
            "written" => Some(Self::Written),
            "deleted" => Some(Self::Deleted),
            "indexed" => Some(Self::Indexed),
            "index_failed" => Some(Self::IndexFailed),
            _ => None,
        }
    }

    fn of(kind: &ObjectEventKind) -> Self {
        match kind {
            ObjectEventKind::Written { .. } => Self::Written,
            ObjectEventKind::Deleted => Self::Deleted,
            ObjectEventKind::Indexed { .. } => Self::Indexed,
            ObjectEventKind::IndexFailed { .. } => Self::IndexFailed,
        }
    }
}

impl EventFilter {
    pub(crate) fn parse(query: EventsQuery) -> Result<Self, ApiError> {
        let kind = match query.event.as_deref() {
            None => None,
            Some(raw) => Some(Kind::parse(raw).ok_or_else(|| {
                ApiError::Core(CoreError::InvalidInput {
                    message: format!(
                        "event must be one of \"written\", \"deleted\", \"indexed\" or \
                         \"index_failed\" (an \"object.\" prefix is accepted), got \"{raw}\""
                    ),
                })
            })?),
        };
        Ok(Self {
            prefix: query.prefix.filter(|p| !p.is_empty()),
            kind,
            mime: query.mime.filter(|m| !m.is_empty()),
        })
    }

    /// `mime` matches the content type an event carries, exactly or by
    /// `type/*`: a write's, or the one the indexer's `HEAD` reported. A
    /// deletion carries none, nor does an `object.index_failed` whose failure
    /// came before `HEAD`, so neither ever matches a `mime` filter.
    pub(crate) fn matches(&self, event: &ObjectEvent) -> bool {
        if let Some(prefix) = &self.prefix
            && !event.object_key.as_str().starts_with(prefix.as_str())
        {
            return false;
        }
        if self.kind.is_some_and(|kind| kind != Kind::of(&event.kind)) {
            return false;
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

/// One live subscriber's place in `notedthat_events_subscribers` (D68).
///
/// The stream *is* the subscription: nothing is called when a client goes
/// away, the response body is simply dropped. So the decrement has to be owned
/// by the stream, and this is what owns it.
///
/// `kb` is a declared slug resolved through `KbAccess`; no principal, no peer
/// address and no `Last-Event-ID` is recorded, because a subscriber's identity
/// and its position are exactly what an exposition must not carry.
struct Subscriber {
    kb: String,
}

impl Subscriber {
    fn enter(kb: String) -> Self {
        metrics::gauge!(metric::EVENTS_SUBSCRIBERS, metric_label::KB => kb.clone()).increment(1.0);
        Self { kb }
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        metrics::gauge!(metric::EVENTS_SUBSCRIBERS, metric_label::KB => self.kb.clone())
            .decrement(1.0);
    }
}

/// Keep `guard` alive for exactly as long as `stream` is held or polled.
///
/// `unfold` moves the guard into the stream's own state, so it is dropped when
/// the stream is — no manual `Pin` projection, which matters under this
/// workspace's `unsafe_code = "forbid"`.
fn guarded<S, G>(guard: G, stream: S) -> impl futures::Stream<Item = S::Item>
where
    S: futures::Stream + Unpin,
    G: Send + 'static,
{
    futures::stream::unfold((stream, guard), |(mut stream, guard)| async move {
        stream.next().await.map(|item| (item, (stream, guard)))
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
    let kb_label = access.kb().as_str().to_string();
    let filter = EventFilter::parse(query).map_err(&err)?;
    let after = parse_last_event_id(&req).map_err(&err)?;
    let summary_visible = access.filter(Verb::List).covers_whole_kb();

    let stream = events
        .subscribe(access.kb(), after)
        .await
        .map_err(|error| {
            err(match error {
                SubscribeError::Gone { requested, oldest } => {
                    // Counted here as well as by the HTTP family's `status`
                    // label, because this is the one the `kb` breakdown is
                    // worth having: a log that has aged past its subscribers
                    // does it per knowledge base.
                    metrics::counter!(
                        metric::EVENTS_REPLAY_GONE,
                        metric_label::KB => kb_label.clone(),
                    )
                    .increment(1);
                    ApiError::EventsGone { requested, oldest }
                }
                SubscribeError::Unavailable { message } => ApiError::EventsUnavailable { message },
            })
        })?;

    // Counted only once `subscribe` has succeeded, so a `410` or an unavailable
    // broker never registers a subscriber that does not exist.
    let subscriber = Subscriber::enter(kb_label);

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
                Ok((id, mut event)) => {
                    let visible = access.allows(Verb::List, event.object_key.as_str())
                        && filter.matches(&event);
                    if visible && !summary_visible {
                        withhold_summary(&mut event);
                    }
                    visible.then(|| frame_for(id, &event))
                }
                Err(error) => Some(ended(&error)),
            };
            futures::future::ready(frame.map(Ok::<Event, Infallible>))
        });

    let mut response = Sse::new(head.chain(guarded(subscriber, body)))
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

/// Strip the failure summary from an `object.index_failed` frame.
///
/// The summary is the pipeline's own first line: it names the embedder or
/// vector-store endpoint it could not reach, and as often the key it was
/// working on, so it can carry what the per-key gate holds back elsewhere in
/// the base. `GET …/index` gives it only to a caller whose `list` grant spans
/// the whole knowledge base (`index_health::failure_view`); the stream applies
/// the same bar, per frame. Everyone who may see the key still learns that
/// indexing it failed, and which version.
fn withhold_summary(event: &mut ObjectEvent) {
    if let ObjectEventKind::IndexFailed { summary, .. } = &mut event.kind {
        *summary = None;
    }
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

    fn indexed(key: &str, mime: &str) -> ObjectEvent {
        ObjectEvent::indexed(
            KbSlug::try_new("notes").unwrap(),
            ObjectPath::try_from(key).unwrap(),
            "\"e\"".into(),
            mime.into(),
            1,
        )
    }

    fn index_failed(key: &str, mime: Option<&str>) -> ObjectEvent {
        ObjectEvent::index_failed(
            KbSlug::try_new("notes").unwrap(),
            ObjectPath::try_from(key).unwrap(),
            mime.map(|_| "\"e\"".to_owned()),
            mime.map(str::to_owned),
            "embedder.embed failed: connection refused".into(),
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
        assert!(f.matches(&indexed("a.md", "text/markdown")));
        assert!(f.matches(&index_failed("a.md", None)));
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
        let all = [
            written("a", "text/plain"),
            deleted("a"),
            indexed("a", "text/plain"),
            index_failed("a", Some("text/plain")),
        ];
        for (name, wanted) in [
            ("written", 0),
            ("deleted", 1),
            ("indexed", 2),
            ("index_failed", 3),
        ] {
            for spelling in [name.to_owned(), format!("object.{name}")] {
                let f = filter(None, Some(&spelling), None);
                for (i, event) in all.iter().enumerate() {
                    assert_eq!(
                        f.matches(event),
                        i == wanted,
                        "?event={spelling} against {}",
                        event.kind.name()
                    );
                }
            }
        }
    }

    #[test]
    fn an_unknown_event_kind_is_a_bad_request() {
        for raw in ["renamed", "object.renamed", "object.", "indexed_failed"] {
            let err = EventFilter::parse(EventsQuery {
                prefix: None,
                event: Some(raw.into()),
                mime: None,
            })
            .unwrap_err();
            assert!(
                matches!(err, ApiError::Core(CoreError::InvalidInput { .. })),
                "{raw}: {err:?}"
            );
        }
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
    fn mime_applies_to_the_indexers_outcomes_when_they_know_one() {
        let text = filter(None, None, Some("text/*"));
        assert!(text.matches(&indexed("a.md", "text/markdown")));
        assert!(!text.matches(&indexed("a.txt", "application/json")));
        assert!(text.matches(&index_failed("a.md", Some("text/plain"))));
        assert!(
            !text.matches(&index_failed("a.md", None)),
            "a failure before HEAD has no content type to match"
        );
    }

    #[test]
    fn withholding_the_summary_leaves_the_rest_of_the_failure_intact() {
        let mut event = index_failed("a.md", Some("text/plain"));
        withhold_summary(&mut event);
        assert_eq!(
            event.kind,
            ObjectEventKind::IndexFailed {
                etag: Some("\"e\"".into()),
                mime: Some("text/plain".into()),
                summary: None,
            }
        );
        let mut write = written("a.md", "text/plain");
        let before = write.clone();
        withhold_summary(&mut write);
        assert_eq!(write, before);
    }

    #[test]
    fn empty_filters_are_no_filters() {
        assert_eq!(filter(Some(""), None, Some("")), filter(None, None, None));
    }
}

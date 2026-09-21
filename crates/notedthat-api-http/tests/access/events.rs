//! `GET /api/v1/knowledgebases/{kb}/events`: the stream is filtered per event
//! by the same rules as a listing, honours `Last-Event-ID`, and answers the
//! documented statuses around the stream itself.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, HttpBody};
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use notedthat_core::{
    AccessPolicy, EventPublisher, EventSource, KbSlug, ObjectEvent, ObjectPath, Verb, Who,
};
use notedthat_events::MemoryPublisher;
use tower::ServiceExt;

use super::fixture::{
    ALICE_TOKEN, BOB_TOKEN, TOKEN, app, app_with_events, grant, grant_under, json, policy,
};

const EVENTS: &str = "/api/v1/knowledgebases/notes/events";

/// One SSE frame, as a client would parse it.
#[derive(Debug, PartialEq, Eq)]
struct Frame {
    id: Option<String>,
    event: Option<String>,
    data: Option<String>,
    retry: Option<String>,
    comments: Vec<String>,
}

fn parse_frames(text: &str) -> Vec<Frame> {
    text.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let mut frame = Frame {
                id: None,
                event: None,
                data: None,
                retry: None,
                comments: Vec::new(),
            };
            for line in block.lines() {
                if let Some(comment) = line.strip_prefix(':') {
                    frame.comments.push(comment.trim_start().to_string());
                } else if let Some((field, value)) = line.split_once(':') {
                    let value = value.strip_prefix(' ').unwrap_or(value).to_string();
                    match field {
                        "id" => frame.id = Some(value),
                        "event" => frame.event = Some(value),
                        "data" => frame.data = Some(value),
                        "retry" => frame.retry = Some(value),
                        _ => {}
                    }
                }
            }
            frame
        })
        .collect()
}

/// Read the stream until `wanted` event frames (frames with `data`) have
/// arrived, then stop. The stream itself never ends.
async fn read_events(response: axum::response::Response, wanted: usize) -> Vec<Frame> {
    let mut body = response.into_body().into_data_stream();
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let events = parse_frames(&text)
            .into_iter()
            .filter(|f| f.data.is_some())
            .count();
        if events >= wanted {
            break;
        }
        let chunk = tokio::time::timeout_at(deadline, body.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {wanted} events; got:\n{text}"))
            .expect("stream open")
            .expect("chunk");
        text.push_str(std::str::from_utf8(&chunk).expect("utf-8"));
    }
    parse_frames(&text)
}

/// Assert nothing more arrives for a moment.
async fn read_nothing(response: axum::response::Response) {
    let mut body = response.into_body().into_data_stream();
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
    while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, body.next()).await {
        text.push_str(std::str::from_utf8(&chunk.expect("chunk")).expect("utf-8"));
    }
    let events: Vec<Frame> = parse_frames(&text)
        .into_iter()
        .filter(|f| f.data.is_some())
        .collect();
    assert!(events.is_empty(), "expected no events, got {events:?}");
}

fn kb() -> KbSlug {
    KbSlug::try_new("notes").unwrap()
}

fn key(s: &str) -> ObjectPath {
    ObjectPath::try_from(s).unwrap()
}

fn written(k: &str, mime: &str) -> ObjectEvent {
    ObjectEvent::written(
        kb(),
        key(k),
        format!("\"{k}\""),
        3,
        mime.into(),
        0,
        EventSource::Http,
    )
}

fn deleted(k: &str) -> ObjectEvent {
    ObjectEvent::deleted(kb(), key(k), EventSource::Webdav)
}

fn indexed(k: &str, mime: &str) -> ObjectEvent {
    ObjectEvent::indexed(kb(), key(k), format!("\"{k}\""), mime.into(), 2)
}

fn index_failed(k: &str, mime: &str, summary: &str) -> ObjectEvent {
    ObjectEvent::index_failed(
        kb(),
        key(k),
        Some(format!("\"{k}\"")),
        Some(mime.into()),
        summary.into(),
    )
}

fn data_of(frame: &Frame) -> serde_json::Value {
    serde_json::from_str(frame.data.as_deref().expect("an event frame")).unwrap()
}

async fn publish_all(publisher: &MemoryPublisher, events: Vec<ObjectEvent>) {
    for event in events {
        publisher.publish(event).await.unwrap();
    }
}

fn get(uri: &str, token: Option<&str>, last_event_id: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    if let Some(id) = last_event_id {
        builder = builder.header("last-event-id", id);
    }
    builder.body(Body::empty()).unwrap()
}

fn everything() -> BTreeMap<String, AccessPolicy> {
    BTreeMap::from([
        (
            "notes".to_string(),
            policy([grant(Who::SignedIn, Verb::ALL)]),
        ),
        ("private".to_string(), AccessPolicy::empty()),
    ])
}

#[tokio::test]
async fn without_an_events_backend_the_route_is_not_found_after_access_is_checked() {
    let router = app(everything()).await;
    let response = router
        .oneshot(get(EVENTS, Some(TOKEN), None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json(response).await;
    assert_eq!(body["error"], "not_found");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("NOTEDTHAT_EVENTS_BACKEND"),
        "{body}"
    );
}

#[tokio::test]
async fn the_stream_opens_with_the_retry_hint_and_the_headers_proxies_need() {
    let publisher = Arc::new(MemoryPublisher::new(16));
    let router = app_with_events(everything(), publisher.clone()).await;
    let response = router
        .oneshot(get(EVENTS, Some(TOKEN), None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"),
        "{:?}",
        response.headers()
    );
    assert_eq!(response.headers()["cache-control"], "no-cache");
    assert_eq!(response.headers()["x-accel-buffering"], "no");
    assert!(
        response.body().size_hint().exact().is_none(),
        "an event stream has no content length"
    );

    publish_all(&publisher, vec![written("a.md", "text/markdown")]).await;
    let frames = read_events(response, 1).await;
    assert_eq!(frames[0].retry.as_deref(), Some("3000"));
    assert_eq!(frames[0].comments, vec!["subscribed"]);
    let event = frames.iter().find(|f| f.data.is_some()).unwrap();
    assert_eq!(event.event.as_deref(), Some("object.written"));
    assert_eq!(event.id.as_deref(), Some("1"));
    let data: serde_json::Value = serde_json::from_str(event.data.as_deref().unwrap()).unwrap();
    assert_eq!(data["event"], "object.written");
    assert_eq!(data["kb"], "notes");
    assert_eq!(data["object_key"], "a.md");
    assert_eq!(data["mime"], "text/markdown");
    assert_eq!(data["source"], "http");
}

#[tokio::test]
async fn an_anonymous_subscriber_sees_only_what_an_anyone_list_rule_grants() {
    let publisher = Arc::new(MemoryPublisher::new(16));
    let policies = BTreeMap::from([(
        "notes".to_string(),
        policy([
            grant(Who::SignedIn, Verb::ALL),
            grant_under(Who::Anyone, [Verb::List, Verb::Read], &["public/**"]),
        ]),
    )]);
    let router = app_with_events(policies, publisher.clone()).await;
    let response = router.oneshot(get(EVENTS, None, None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    publish_all(
        &publisher,
        vec![
            written("internal/secret.md", "text/markdown"),
            written("public/index.md", "text/markdown"),
            deleted("internal/gone.md"),
            deleted("public/gone.md"),
        ],
    )
    .await;
    let frames = read_events(response, 2).await;
    let keys: Vec<(String, String)> = frames
        .iter()
        .filter_map(|f| f.data.as_deref())
        .map(|d| {
            let v: serde_json::Value = serde_json::from_str(d).unwrap();
            (
                v["event"].as_str().unwrap().to_string(),
                v["object_key"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            ("object.written".to_string(), "public/index.md".to_string()),
            ("object.deleted".to_string(), "public/gone.md".to_string()),
        ]
    );
}

#[tokio::test]
async fn an_anonymous_subscriber_with_no_anyone_rule_is_concealed() {
    let publisher = Arc::new(MemoryPublisher::new(16));
    let router = app_with_events(everything(), publisher).await;
    let response = router.oneshot(get(EVENTS, None, None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_user_without_list_under_hr_never_sees_hr_events_including_deletes() {
    let publisher = Arc::new(MemoryPublisher::new(16));
    let policies = BTreeMap::from([(
        "notes".to_string(),
        policy([
            grant(Who::SignedIn, Verb::ALL),
            notedthat_core::AccessRule::deny(Who::User("bob".into()), [Verb::List, Verb::Read])
                .under([notedthat_core::KeyPattern::parse("hr/**").unwrap()]),
        ]),
    )]);
    let router = app_with_events(policies, publisher.clone()).await;
    let response = router
        .oneshot(get(EVENTS, Some(BOB_TOKEN), None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    publish_all(
        &publisher,
        vec![
            written("hr/salaries.md", "text/markdown"),
            deleted("hr/old.md"),
            written("eng/roadmap.md", "text/markdown"),
            deleted("eng/old.md"),
        ],
    )
    .await;
    let frames = read_events(response, 2).await;
    let keys: Vec<String> = frames
        .iter()
        .filter_map(|f| f.data.as_deref())
        .map(|d| {
            serde_json::from_str::<serde_json::Value>(d).unwrap()["object_key"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(keys, vec!["eng/roadmap.md", "eng/old.md"]);
}

#[tokio::test]
async fn a_credential_holder_granted_nothing_is_forbidden_before_any_stream() {
    let publisher = Arc::new(MemoryPublisher::new(16));
    let policies = BTreeMap::from([("notes".to_string(), AccessPolicy::empty())]);
    let router = app_with_events(policies, publisher).await;
    let response = router
        .oneshot(get(EVENTS, Some(BOB_TOKEN), None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn prefix_event_and_mime_filters_apply_after_the_access_filter() {
    let publisher = Arc::new(MemoryPublisher::new(32));
    let router = app_with_events(everything(), publisher.clone()).await;
    let response = router
        .oneshot(get(
            &format!("{EVENTS}?prefix=inbox/&mime=audio/*"),
            Some(TOKEN),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    publish_all(
        &publisher,
        vec![
            written("inbox/memo.mp3", "audio/mpeg"),
            written("inbox/memo.mp3.md", "text/markdown"),
            indexed("inbox/memo.mp3.md", "text/markdown"),
            written("archive/talk.mp3", "audio/mpeg"),
            deleted("inbox/memo.mp3"),
            index_failed("inbox/notes.md", "text/markdown", "embedder.embed failed"),
            written("inbox/voice.wav", "audio/wav"),
        ],
    )
    .await;
    let frames = read_events(response, 2).await;
    let keys: Vec<String> = frames
        .iter()
        .filter_map(|f| f.data.as_deref())
        .map(|d| {
            serde_json::from_str::<serde_json::Value>(d).unwrap()["object_key"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(keys, vec!["inbox/memo.mp3", "inbox/voice.wav"]);

    let response = app_with_events(everything(), publisher.clone())
        .await
        .oneshot(get(
            &format!("{EVENTS}?event=deleted"),
            Some(TOKEN),
            Some("0"),
        ))
        .await
        .unwrap();
    let frames = read_events(response, 1).await;
    let only: Vec<&Frame> = frames.iter().filter(|f| f.data.is_some()).collect();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].event.as_deref(), Some("object.deleted"));
    assert_eq!(only[0].id.as_deref(), Some("5"));

    // The indexer's outcomes are selected the same way, under either spelling.
    let response = app_with_events(everything(), publisher.clone())
        .await
        .oneshot(get(
            &format!("{EVENTS}?event=indexed"),
            Some(TOKEN),
            Some("0"),
        ))
        .await
        .unwrap();
    let frames = read_events(response, 1).await;
    let only: Vec<&Frame> = frames.iter().filter(|f| f.data.is_some()).collect();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].event.as_deref(), Some("object.indexed"));
    assert_eq!(only[0].id.as_deref(), Some("3"));
    let data = data_of(only[0]);
    assert_eq!(data["object_key"], "inbox/memo.mp3.md");
    assert_eq!(data["etag"], "\"inbox/memo.mp3.md\"");
    assert_eq!(data["mime"], "text/markdown");
    assert_eq!(data["chunks"], 2);
    assert_eq!(data["source"], "indexer");

    let response = app_with_events(everything(), publisher.clone())
        .await
        .oneshot(get(
            &format!("{EVENTS}?event=object.index_failed"),
            Some(TOKEN),
            Some("0"),
        ))
        .await
        .unwrap();
    let frames = read_events(response, 1).await;
    let only: Vec<&Frame> = frames.iter().filter(|f| f.data.is_some()).collect();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].event.as_deref(), Some("object.index_failed"));
    assert_eq!(only[0].id.as_deref(), Some("6"));
    let data = data_of(only[0]);
    assert_eq!(data["object_key"], "inbox/notes.md");
    assert_eq!(
        data["summary"], "embedder.embed failed",
        "the service token may list the whole base"
    );

    // `mime=` applies to an outcome by the content type HEAD reported.
    let response = app_with_events(everything(), publisher.clone())
        .await
        .oneshot(get(
            &format!("{EVENTS}?event=indexed&mime=audio/*"),
            Some(TOKEN),
            Some("0"),
        ))
        .await
        .unwrap();
    read_nothing(response).await;

    let response = app_with_events(everything(), publisher)
        .await
        .oneshot(get(&format!("{EVENTS}?event=renamed"), Some(TOKEN), None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// The failure summary names endpoints and, as often, the key the pipeline
/// was working on; it goes to whoever may `list` the whole knowledge base —
/// the bar `GET …/index` sets for the same string — and nobody else. The rest
/// of the frame follows the ordinary per-key rule.
#[tokio::test]
async fn an_index_failure_summary_goes_only_to_a_subscriber_who_may_list_the_whole_base() {
    let publisher = Arc::new(MemoryPublisher::new(16));
    let policies = BTreeMap::from([(
        "notes".to_string(),
        policy([
            grant_under(Who::Anyone, [Verb::Read, Verb::List], &["public/**"]),
            grant(Who::SignedIn, [Verb::Read]),
            grant_under(Who::User("bob".into()), [Verb::List], &["public/**"]),
            grant(Who::User("alice".into()), [Verb::List]),
        ]),
    )]);
    publish_all(
        &publisher,
        vec![
            index_failed(
                "public/a.md",
                "text/markdown",
                "embedder.embed failed: connection refused",
            ),
            index_failed(
                "internal/b.md",
                "text/markdown",
                "storage.head_object failed: internal/b.md",
            ),
        ],
    )
    .await;

    // Anonymous: the public failure without its summary; the internal one not at all.
    let response = app_with_events(policies.clone(), publisher.clone())
        .await
        .oneshot(get(EVENTS, None, Some("0")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let frames = read_events(response, 1).await;
    let events: Vec<&Frame> = frames.iter().filter(|f| f.data.is_some()).collect();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].event.as_deref(), Some("object.index_failed"));
    let data = data_of(events[0]);
    assert_eq!(data["object_key"], "public/a.md");
    assert_eq!(data["etag"], "\"public/a.md\"");
    assert_eq!(data["mime"], "text/markdown");
    assert!(data.get("summary").is_none(), "{data}");

    // Bob may list `public/` only: same view as anonymous.
    let response = app_with_events(policies.clone(), publisher.clone())
        .await
        .oneshot(get(EVENTS, Some(BOB_TOKEN), Some("0")))
        .await
        .unwrap();
    let frames = read_events(response, 1).await;
    let events: Vec<&Frame> = frames.iter().filter(|f| f.data.is_some()).collect();
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(data_of(events[0]).get("summary").is_none());

    // Alice may list everything: both failures, summaries included.
    let response = app_with_events(policies, publisher)
        .await
        .oneshot(get(EVENTS, Some(ALICE_TOKEN), Some("0")))
        .await
        .unwrap();
    let frames = read_events(response, 2).await;
    let seen: Vec<(String, String)> = frames
        .iter()
        .filter(|f| f.data.is_some())
        .map(|f| {
            let data = data_of(f);
            (
                data["object_key"].as_str().unwrap().to_owned(),
                data["summary"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            (
                "public/a.md".to_owned(),
                "embedder.embed failed: connection refused".to_owned()
            ),
            (
                "internal/b.md".to_owned(),
                "storage.head_object failed: internal/b.md".to_owned()
            ),
        ]
    );
}

#[tokio::test]
async fn last_event_id_replays_exactly_the_events_after_it_in_order() {
    let publisher = Arc::new(MemoryPublisher::new(16));
    publish_all(
        &publisher,
        vec![
            written("a.md", "text/markdown"),
            written("b.md", "text/markdown"),
            written("c.md", "text/markdown"),
        ],
    )
    .await;
    let router = app_with_events(everything(), publisher.clone()).await;
    let response = router
        .oneshot(get(EVENTS, Some(TOKEN), Some("1")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    publish_all(&publisher, vec![written("d.md", "text/markdown")]).await;

    let frames = read_events(response, 3).await;
    let ids: Vec<&str> = frames.iter().filter_map(|f| f.id.as_deref()).collect();
    assert_eq!(ids, vec!["2", "3", "4"]);
}

#[tokio::test]
async fn without_last_event_id_only_live_events_are_delivered() {
    let publisher = Arc::new(MemoryPublisher::new(16));
    publish_all(&publisher, vec![written("old.md", "text/markdown")]).await;
    let router = app_with_events(everything(), publisher.clone()).await;
    let response = router
        .oneshot(get(EVENTS, Some(TOKEN), None))
        .await
        .unwrap();
    read_nothing(response).await;
}

#[tokio::test]
async fn a_retained_out_last_event_id_is_gone_and_names_the_oldest() {
    let publisher = Arc::new(MemoryPublisher::new(2));
    publish_all(
        &publisher,
        vec![
            written("a.md", "text/markdown"),
            written("b.md", "text/markdown"),
            written("c.md", "text/markdown"),
            written("d.md", "text/markdown"),
        ],
    )
    .await;
    let router = app_with_events(everything(), publisher).await;
    let response = router
        .oneshot(get(EVENTS, Some(TOKEN), Some("1")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::GONE);
    let body = json(response).await;
    assert_eq!(body["error"], "gone");
    assert!(
        body["message"].as_str().unwrap().contains('3'),
        "names the oldest retained id: {body}"
    );
}

#[tokio::test]
async fn a_malformed_last_event_id_is_a_bad_request() {
    let publisher = Arc::new(MemoryPublisher::new(2));
    let router = app_with_events(everything(), publisher).await;
    let response = router
        .oneshot(get(EVENTS, Some(TOKEN), Some("abc")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json(response).await["error"], "invalid_request");
}

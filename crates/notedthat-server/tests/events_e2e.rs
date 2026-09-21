//! E2E: every write surface publishes an object change event, the indexer
//! follows it with its verdict, and the SSE route delivers both — against a
//! server on in-process backends and the `memory` event log. No Docker, and
//! not `#[ignore]`d.
//!
//! ```sh
//! cargo test -p notedthat-server --locked --test events_e2e
//! ```
#![allow(missing_docs)]

#[path = "support/patch_env.rs"]
mod patch_env;
#[path = "support/sse.rs"]
mod sse;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use notedthat_events::MemoryPublisher;
use notedthat_indexer::testing::StubEmbedder;
use notedthat_indexer::{Embedder, EmbedderError};
use patch_env::{API_TOKEN, PatchServer, mcp_call_tool};
use reqwest::StatusCode;
use sse::{Subscription, change_events};

const MAX_PATCHABLE: u64 = 10 * 1024 * 1024;
const WAIT: Duration = Duration::from_secs(5);

async fn server_with_events() -> (PatchServer, Arc<MemoryPublisher>) {
    let publisher = Arc::new(MemoryPublisher::new(256));
    let server = PatchServer::start_with_events(MAX_PATCHABLE, Some(publisher.clone())).await;
    (server, publisher)
}

async fn subscribe(server: &PatchServer, query: &str) -> Subscription {
    let response = server
        .client
        .get(format!(
            "{}/api/v1/knowledgebases/{}/events{query}",
            server.base_url, server.kb
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("subscribe");
    Subscription::open(response)
}

fn basic_auth() -> String {
    let encoded =
        base64::engine::general_purpose::STANDARD.encode("e2e-webdav-user:e2e-webdav-pass");
    format!("Basic {encoded}")
}

async fn search(server: &PatchServer, query: &str) -> serde_json::Value {
    server
        .client
        .post(format!(
            "{}/api/v1/knowledgebases/{}/search",
            server.base_url, server.kb
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Content-Type", "application/json")
        .body(serde_json::json!({ "query": query, "limit": 5 }).to_string())
        .send()
        .await
        .expect("search")
        .json()
        .await
        .expect("search body")
}

async fn get_index(server: &PatchServer) -> serde_json::Value {
    server
        .client
        .get(format!(
            "{}/api/v1/knowledgebases/{}/index",
            server.base_url, server.kb
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .send()
        .await
        .expect("index")
        .json()
        .await
        .expect("index body")
}

/// An embedder whose first call fails the way an unreachable endpoint does,
/// and which behaves from then on.
struct FailFirstEmbedder {
    inner: StubEmbedder,
    failed: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Embedder for FailFirstEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedderError> {
        if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(EmbedderError::Transport("connection refused".into()));
        }
        self.inner.embed(texts).await
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    fn max_input_tokens(&self) -> usize {
        self.inner.max_input_tokens()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
}

/// The reason the events exist (#161): a subscriber that saw `object.written`
/// knows the object is searchable once `object.indexed` names the same
/// `ETag` — and not before, so the search below is made exactly once.
#[tokio::test]
async fn a_write_is_announced_then_indexed_with_the_same_etag_and_then_searchable() {
    let (server, _publisher) = server_with_events().await;
    let mut sub = subscribe(&server, "").await;
    let mut only_indexed = subscribe(&server, "?event=indexed").await;
    let mut only_written = subscribe(&server, "?event=object.written").await;

    let etag = server.put_text("idx/a.md", "alpha beta gamma").await;

    let frames = sub.events(2, WAIT).await;
    assert_eq!(
        frames
            .iter()
            .map(|f| (f.event.as_deref().unwrap(), f.key(), f.source()))
            .collect::<Vec<_>>(),
        vec![
            ("object.written", "idx/a.md", "http"),
            ("object.indexed", "idx/a.md", "indexer"),
        ]
    );
    assert!(frames[0].id_number() < frames[1].id_number());
    let written = frames[0].data.as_ref().unwrap();
    let indexed = frames[1].data.as_ref().unwrap();
    assert_eq!(written["etag"], etag);
    assert_eq!(
        indexed["etag"], etag,
        "the verdict names the version written"
    );
    assert_eq!(indexed["mime"], "text/markdown");
    assert!(indexed["chunks"].as_u64().unwrap() >= 1, "{indexed}");
    assert!(indexed.get("size").is_none() && indexed.get("mtime").is_none());
    assert!(
        indexed["occurred_at"].as_str().unwrap().ends_with('Z'),
        "{indexed}"
    );

    let body = search(&server, "alpha beta gamma").await;
    let hits = body["hits"].as_array().unwrap_or_else(|| panic!("{body}"));
    assert!(!hits.is_empty(), "searchable once indexed: {body}");
    assert!(
        hits.iter().any(|hit| hit["object_key"] == "idx/a.md"),
        "{body}"
    );

    let frames = only_indexed.events(1, WAIT).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event.as_deref(), Some("object.indexed"));
    only_indexed
        .expect_silence(Duration::from_millis(200))
        .await;
    let frames = only_written.events(1, WAIT).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event.as_deref(), Some("object.written"));
    only_written
        .expect_silence(Duration::from_millis(200))
        .await;

    let index = get_index(&server).await;
    assert_eq!(index["state"], "healthy", "{index}");
}

/// The index never gains an mp3, so nothing follows its `object.written`.
#[tokio::test]
async fn a_non_indexable_upload_is_written_but_never_indexed() {
    let (server, _publisher) = server_with_events().await;
    let mut sub = subscribe(&server, "").await;

    let response = server
        .client
        .put(server.object_url("inbox/memo.mp3"))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Content-Type", "audio/mpeg")
        .body(vec![0x49, 0x44, 0x33, 0x03])
        .send()
        .await
        .expect("PUT mp3");
    assert_eq!(response.status(), StatusCode::CREATED);

    let frames = sub.events(1, WAIT).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event.as_deref(), Some("object.written"));
    sub.expect_silence(Duration::from_millis(300)).await;
}

/// A subscriber waiting on `object.indexed` is told when it will not come,
/// with the same summary `GET …/index` reports; the retry that succeeds is
/// reported too.
#[tokio::test]
async fn a_failed_index_is_reported_and_a_retry_is_indexed() {
    let publisher = Arc::new(MemoryPublisher::new(256));
    let backends = notedthat_server::run::Backends {
        events: Some(publisher.clone()),
        embedder: Arc::new(FailFirstEmbedder {
            inner: StubEmbedder::new(4),
            failed: std::sync::atomic::AtomicBool::new(false),
        }),
        ..patch_env::in_memory_backends()
    };
    let server = PatchServer::start_with_backends(MAX_PATCHABLE, backends).await;
    let mut sub = subscribe(&server, "").await;

    let etag = server.put_text("flaky/a.md", "first try").await;
    let frames = sub.events(2, WAIT).await;
    assert_eq!(
        frames
            .iter()
            .map(|f| f.event.as_deref().unwrap())
            .collect::<Vec<_>>(),
        vec!["object.written", "object.index_failed"]
    );
    let failed = frames[1].data.as_ref().unwrap();
    assert_eq!(failed["source"], "indexer");
    assert_eq!(failed["etag"], etag);
    assert_eq!(failed["mime"], "text/markdown");
    let summary = failed["summary"]
        .as_str()
        .unwrap_or_else(|| panic!("{failed}"));
    assert!(
        summary.starts_with("embedder.embed failed"),
        "the pipeline's own error: {summary}"
    );

    let index = get_index(&server).await;
    assert_eq!(index["state"], "failed", "{index}");
    assert_eq!(index["last_failure"]["object_key"], "flaky/a.md");
    assert_eq!(
        index["last_failure"]["summary"], summary,
        "the stream and /index carry the same string"
    );

    let etag = server.put_text("flaky/a.md", "second try").await;
    let frames = sub.events(2, WAIT).await;
    assert_eq!(
        frames
            .iter()
            .map(|f| f.event.as_deref().unwrap())
            .collect::<Vec<_>>(),
        vec!["object.written", "object.indexed"]
    );
    assert_eq!(frames[1].data.as_ref().unwrap()["etag"], etag);
    let index = get_index(&server).await;
    assert_eq!(index["state"], "healthy", "{index}");
}

/// Each write publishes exactly one change event; the indexer's verdicts
/// ride along and are read past here.
#[tokio::test]
async fn http_put_patch_replace_and_delete_each_publish_one_event() {
    let (server, _publisher) = server_with_events().await;
    let mut sub = subscribe(&server, "").await;

    // PUT.
    let etag = server.put_text("notes/a.md", "alpha beta").await;
    let frames = sub.events_where(1, WAIT, change_events).await;
    assert_eq!(frames[0].event.as_deref(), Some("object.written"));
    assert_eq!(frames[0].key(), "notes/a.md");
    assert_eq!(frames[0].source(), "http");
    let data = frames[0].data.as_ref().unwrap();
    assert_eq!(data["kb"], server.kb);
    assert_eq!(data["etag"], etag);
    assert_eq!(data["size"], 10);
    assert_eq!(data["mime"], "text/markdown");
    assert!(data["mtime"].as_i64().unwrap() > 0);
    assert!(
        data["occurred_at"].as_str().unwrap().ends_with('Z'),
        "{data}"
    );

    // PATCH (append).
    let response = server
        .patch_append("notes/a.md", Some(&etag), " gamma")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let etag = patch_env::etag(&response);
    let frames = sub.events_where(1, WAIT, change_events).await;
    assert_eq!(frames[0].key(), "notes/a.md");
    assert_eq!(frames[0].data.as_ref().unwrap()["etag"], etag);
    assert_eq!(frames[0].data.as_ref().unwrap()["size"], 16);

    // POST replace.
    let response = server
        .replace_json("notes/a.md", &etag, "beta", "delta", false)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let frames = sub.events_where(1, WAIT, change_events).await;
    assert_eq!(frames[0].key(), "notes/a.md");
    assert_eq!(frames[0].source(), "http");
    let head = server.get("notes/a.md").await;
    assert_eq!(
        frames[0].data.as_ref().unwrap()["etag"],
        patch_env::etag(&head),
        "the event describes the bytes a subsequent read returns"
    );

    // DELETE.
    let response = server
        .client
        .delete(server.object_url("notes/a.md"))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .send()
        .await
        .expect("DELETE");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let frames = sub.events_where(1, WAIT, change_events).await;
    assert_eq!(frames[0].event.as_deref(), Some("object.deleted"));
    assert_eq!(frames[0].key(), "notes/a.md");
    assert_eq!(frames[0].source(), "http");
    assert!(frames[0].data.as_ref().unwrap().get("etag").is_none());

    // Ids were handed out strictly increasing, the four change events among
    // them; the indexer's verdicts on the three writes took ids of their own.
    let frames = sub.all_frames();
    let ids: Vec<u64> = frames
        .iter()
        .filter(|f| f.data.is_some())
        .map(sse::Frame::id_number)
        .collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
    assert_eq!(ids[0], 1);
    let changes = frames
        .iter()
        .filter(|f| f.data.is_some() && change_events(f))
        .count();
    assert_eq!(changes, 4, "{frames:?}");
}

#[tokio::test]
async fn webdav_put_copy_move_and_delete_are_attributed_to_webdav() {
    let (server, _publisher) = server_with_events().await;
    let mut sub = subscribe(&server, "").await;
    let dav = |path: &str| format!("{}/webdav/{}/{path}", server.base_url, server.kb);

    let response = server
        .client
        .put(dav("dav/one.md"))
        .header("Authorization", basic_auth())
        .header("Content-Type", "text/markdown")
        .body("# one")
        .send()
        .await
        .expect("PUT");
    assert_eq!(response.status(), StatusCode::CREATED);
    let frames = sub.events_where(1, WAIT, change_events).await;
    assert_eq!(frames[0].event.as_deref(), Some("object.written"));
    assert_eq!(frames[0].key(), "dav/one.md");
    assert_eq!(frames[0].source(), "webdav");
    assert_eq!(frames[0].data.as_ref().unwrap()["size"], 5);

    let response = server
        .client
        .request(
            reqwest::Method::from_bytes(b"COPY").unwrap(),
            dav("dav/one.md"),
        )
        .header("Authorization", basic_auth())
        .header("Destination", dav("dav/two.md"))
        .send()
        .await
        .expect("COPY");
    assert_eq!(response.status(), StatusCode::CREATED);
    let frames = sub.events_where(1, WAIT, change_events).await;
    assert_eq!(frames[0].event.as_deref(), Some("object.written"));
    assert_eq!(frames[0].key(), "dav/two.md");
    assert_eq!(frames[0].source(), "webdav");
    assert_eq!(
        frames[0].data.as_ref().unwrap()["size"],
        5,
        "a copy's event carries the destination's size"
    );

    // MOVE is two events: the destination written, then the source deleted.
    let response = server
        .client
        .request(
            reqwest::Method::from_bytes(b"MOVE").unwrap(),
            dav("dav/two.md"),
        )
        .header("Authorization", basic_auth())
        .header("Destination", dav("dav/three.md"))
        .send()
        .await
        .expect("MOVE");
    assert_eq!(response.status(), StatusCode::CREATED);
    let frames = sub.events_where(2, WAIT, change_events).await;
    assert_eq!(
        frames
            .iter()
            .map(|f| (f.event.as_deref().unwrap(), f.key(), f.source()))
            .collect::<Vec<_>>(),
        vec![
            ("object.written", "dav/three.md", "webdav"),
            ("object.deleted", "dav/two.md", "webdav"),
        ]
    );

    let response = server
        .client
        .delete(dav("dav/three.md"))
        .header("Authorization", basic_auth())
        .send()
        .await
        .expect("DELETE");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let frames = sub.events_where(1, WAIT, change_events).await;
    assert_eq!(frames[0].event.as_deref(), Some("object.deleted"));
    assert_eq!(frames[0].key(), "dav/three.md");
    assert_eq!(frames[0].source(), "webdav");
}

#[tokio::test]
async fn mcp_writes_are_attributed_to_mcp_and_the_header_is_informational() {
    let (server, _publisher) = server_with_events().await;
    let mut sub = subscribe(&server, "").await;

    let result = mcp_call_tool(
        &server.client,
        &server.mcp_url,
        1,
        "write",
        serde_json::json!({
            "kb": server.kb,
            "path": "mcp/note.md",
            "content": "written by a tool",
        }),
    )
    .await;
    assert!(result["error"].is_null(), "{result}");
    let frames = sub.events_where(1, WAIT, change_events).await;
    assert_eq!(frames[0].key(), "mcp/note.md");
    assert_eq!(frames[0].source(), "mcp");

    let result = mcp_call_tool(
        &server.client,
        &server.mcp_url,
        2,
        "delete",
        serde_json::json!({ "kb": server.kb, "path": "mcp/note.md" }),
    )
    .await;
    assert!(result["error"].is_null(), "{result}");
    let frames = sub.events_where(1, WAIT, change_events).await;
    assert_eq!(frames[0].event.as_deref(), Some("object.deleted"));
    assert_eq!(frames[0].source(), "mcp");

    // Any client may say so; nothing checks it.
    for (claimed, expected) in [("mcp", "mcp"), ("bogus", "http")] {
        let response = server
            .client
            .put(server.object_url("claimed.md"))
            .header("Authorization", format!("Bearer {API_TOKEN}"))
            .header("X-NotedThat-Source", claimed)
            .body("x")
            .send()
            .await
            .expect("PUT");
        assert_eq!(response.status(), StatusCode::CREATED);
        let frames = sub.events_where(1, WAIT, change_events).await;
        assert_eq!(frames[0].source(), expected, "claimed {claimed}");
    }
}

#[tokio::test]
async fn a_reconnecting_subscriber_replays_what_it_missed_and_readyz_is_ok() {
    let (server, _publisher) = server_with_events().await;

    let response = server
        .client
        .get(format!("{}/readyz", server.base_url))
        .send()
        .await
        .expect("readyz");
    assert_eq!(response.status(), StatusCode::OK);

    let mut first = subscribe(&server, "").await;
    server.put_text("r/a.md", "a").await;
    server.put_text("r/b.md", "b").await;
    let seen = first.events_where(2, WAIT, change_events).await;
    let last_id = seen[1].id.clone().unwrap();
    drop(first);

    server.put_text("r/c.md", "c").await;
    server.put_text("r/d.md", "d").await;

    let response = server
        .client
        .get(format!(
            "{}/api/v1/knowledgebases/{}/events",
            server.base_url, server.kb
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Last-Event-ID", &last_id)
        .send()
        .await
        .expect("resubscribe");
    let mut second = Subscription::open(response);
    // The replay may open with the indexer's verdict on `r/b.md`, published
    // after the write; the changes missed are exactly the two later writes.
    let replayed = second.events_where(2, WAIT, change_events).await;
    assert_eq!(
        replayed.iter().map(sse::Frame::key).collect::<Vec<_>>(),
        vec!["r/c.md", "r/d.md"]
    );
    assert!(replayed[0].id_number() > last_id.parse::<u64>().unwrap());
    second
        .expect_silence_where(Duration::from_millis(300), change_events)
        .await;

    let first_frame = &second.all_frames()[0];
    assert_eq!(first_frame.retry.as_deref(), Some("3000"));
}

#[tokio::test]
async fn filters_narrow_the_stream_and_a_stale_position_is_gone() {
    let (server, _publisher) = server_with_events().await;
    let mut audio = subscribe(&server, "?prefix=inbox/&mime=audio/*").await;

    let response = server
        .client
        .put(server.object_url("inbox/memo.mp3"))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Content-Type", "audio/mpeg")
        .body(vec![0x49, 0x44, 0x33, 0x03])
        .send()
        .await
        .expect("PUT mp3");
    assert_eq!(response.status(), StatusCode::CREATED);
    server.put_text("inbox/memo.mp3.md", "transcript").await;
    server.put_text("archive/other.md", "other").await;

    let frames = audio.events(1, WAIT).await;
    assert_eq!(frames[0].key(), "inbox/memo.mp3");
    assert_eq!(frames[0].data.as_ref().unwrap()["mime"], "audio/mpeg");
    audio.expect_silence(Duration::from_millis(300)).await;

    // The memory log was built with room for 256 events; a position older
    // than that is gone. Push the ring past its capacity first.
    for i in 0..260 {
        server.put_text(&format!("bulk/{i}.md"), "x").await;
    }
    let response = server
        .client
        .get(format!(
            "{}/api/v1/knowledgebases/{}/events",
            server.base_url, server.kb
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Last-Event-ID", "1")
        .send()
        .await
        .expect("resubscribe");
    assert_eq!(response.status(), StatusCode::GONE);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "gone");
}

#[tokio::test]
async fn without_an_events_backend_the_route_is_not_found() {
    let server = PatchServer::start(MAX_PATCHABLE).await;
    let response = server
        .client
        .get(format!(
            "{}/api/v1/knowledgebases/{}/events",
            server.base_url, server.kb
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .send()
        .await
        .expect("subscribe");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

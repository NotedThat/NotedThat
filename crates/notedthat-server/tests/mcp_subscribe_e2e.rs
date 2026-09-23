//! E2E: MCP resource subscriptions (D66) over the stateful streamable HTTP
//! transport — a real `notedthat-server` on in-process backends and the
//! `memory` event log, driven with the MCP session client. No Docker, and not
//! `#[ignore]`d.
//!
//! ```sh
//! cargo test -p notedthat-server --locked --test mcp_subscribe_e2e
//! ```
#![allow(missing_docs)]

#[path = "support/patch_env.rs"]
mod patch_env;

use std::sync::Arc;
use std::time::Duration;

use notedthat_events::MemoryPublisher;
use notedthat_mcp::testing::{McpSession, NotificationStream};
use patch_env::{API_TOKEN, PatchServer};

const MAX_PATCHABLE: u64 = 10 * 1024 * 1024;
const WAIT: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_millis(1500);

async fn server_with_events() -> PatchServer {
    let publisher = Arc::new(MemoryPublisher::new(1024));
    PatchServer::start_with_events(MAX_PATCHABLE, Some(publisher)).await
}

fn uri(server: &PatchServer, key: &str) -> String {
    format!("notedthat://{}/{key}", server.kb)
}

/// A session of its own, with its notification leg open.
async fn subscribed_session(server: &PatchServer) -> (McpSession, NotificationStream) {
    let mcp = McpSession::connect(&server.base_url, API_TOKEN);
    let stream = mcp.notifications().await;
    (mcp, stream)
}

async fn subscribe(mcp: &McpSession, id: u64, uri: &str) -> serde_json::Value {
    mcp.request(
        id,
        "resources/subscribe",
        &serde_json::json!({ "uri": uri }),
    )
    .await
}

async fn delete(server: &PatchServer, key: &str) {
    let response = server
        .client
        .delete(server.object_url(key))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .send()
        .await
        .expect("DELETE");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn initialize_advertises_subscriptions_exactly_when_an_events_backend_is_configured() {
    let with = server_with_events().await;
    let init = McpSession::connect(&with.base_url, API_TOKEN)
        .initialize()
        .await;
    assert_eq!(
        init["result"]["capabilities"]["resources"],
        serde_json::json!({ "subscribe": true, "listChanged": true }),
        "{init}"
    );

    let without = PatchServer::start(MAX_PATCHABLE).await;
    let mcp = McpSession::connect(&without.base_url, API_TOKEN);
    let init = mcp.initialize().await;
    assert_eq!(
        init["result"]["capabilities"]["resources"],
        serde_json::json!({}),
        "{init}"
    );
    // And the method it did not advertise is not there.
    without.put_text("a.md", "a").await;
    let refused = subscribe(&mcp, 1, &uri(&without, "a.md")).await;
    assert_eq!(refused["error"]["code"], -32601, "{refused}");
}

#[tokio::test]
async fn a_subscribed_resource_reports_writes_and_deletes_until_unsubscribed() {
    let server = server_with_events().await;
    server.put_text("notes/a.md", "first").await;
    server.put_text("notes/b.md", "other").await;
    let (mcp, mut stream) = subscribed_session(&server).await;
    let a = uri(&server, "notes/a.md");

    let answer = subscribe(&mcp, 1, &a).await;
    assert!(answer.get("result").is_some(), "{answer}");

    // A write to the subscribed key is reported under the subscribed URI …
    server.put_text("notes/a.md", "second").await;
    let notified = stream.next(WAIT).await;
    assert_eq!(notified["method"], "notifications/resources/updated");
    assert_eq!(notified["params"]["uri"], a, "{notified}");

    // … a write to another key is not …
    server.put_text("notes/b.md", "changed").await;
    stream.expect_silence(QUIET).await;

    // … a delete is …
    delete(&server, "notes/a.md").await;
    let notified = stream.next(WAIT).await;
    assert_eq!(notified["method"], "notifications/resources/updated");
    assert_eq!(notified["params"]["uri"], a);

    // … and after unsubscribe, nothing is.
    let answer = mcp
        .request(2, "resources/unsubscribe", &serde_json::json!({ "uri": a }))
        .await;
    assert!(answer.get("result").is_some(), "{answer}");
    server.put_text("notes/a.md", "third").await;
    stream.expect_silence(QUIET).await;

    // Unsubscribing what was never subscribed is not an error.
    let answer = mcp
        .request(
            3,
            "resources/unsubscribe",
            &serde_json::json!({ "uri": uri(&server, "never.md") }),
        )
        .await;
    assert!(answer.get("result").is_some(), "{answer}");
}

#[tokio::test]
async fn the_indexers_verdict_is_not_a_resource_update() {
    let server = server_with_events().await;
    server.put_text("idx/a.md", "alpha").await;
    let (mcp, mut stream) = subscribed_session(&server).await;
    subscribe(&mcp, 1, &uri(&server, "idx/a.md")).await;

    // One PUT is one object.written and, once indexed, one object.indexed
    // (D65); the subscriber hears about the write exactly once.
    server.put_text("idx/a.md", "beta").await;
    let notified = stream.next(WAIT).await;
    assert_eq!(notified["method"], "notifications/resources/updated");
    stream.expect_silence(QUIET).await;
}

#[tokio::test]
async fn subscribing_to_a_key_the_caller_cannot_list_is_the_concealed_not_found() {
    let server = server_with_events().await;
    let mcp = McpSession::connect(&server.base_url, API_TOKEN);

    // A key that does not exist — what a key the caller may not see also is.
    let refused = subscribe(&mcp, 1, &uri(&server, "missing.md")).await;
    assert_eq!(refused["error"]["code"], -32002, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("not_found")),
        "{refused}"
    );

    // A knowledge base that does not exist is refused the same way.
    let refused = subscribe(&mcp, 2, "notedthat://no-such-kb/a.md").await;
    assert_eq!(refused["error"]["code"], -32002, "{refused}");

    // A URI that is not a resource at all is invalid params.
    let refused = subscribe(&mcp, 3, "https://example.com/a.md").await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
}

#[tokio::test]
async fn a_burst_of_writes_is_a_bounded_number_of_list_changed_notifications() {
    let server = server_with_events().await;
    server.put_text("seed.md", "seed").await;

    // A session that never listed is never told the list changed.
    let (_quiet, mut quiet_stream) = subscribed_session(&server).await;
    // A session that listed once is, from then on.
    let (mcp, mut stream) = subscribed_session(&server).await;
    let listed = mcp
        .request(1, "resources/list", &serde_json::json!({}))
        .await;
    assert!(listed["result"]["resources"].is_array(), "{listed}");

    // The coalescer emits roughly one notification per second for as long as
    // writes keep arriving, and every write here lands before `collect_for`
    // starts — so the ceiling is a function of how long the loop took, not a
    // constant. A fixed `6` would encode "the PUT loop took under about six
    // seconds", which is comfortable against in-process backends and is not on
    // a loaded CI runner; the failure would read as a coalescing regression.
    let started = std::time::Instant::now();
    for i in 0..100 {
        server.put_text(&format!("bulk/{i}.md"), "x").await;
    }
    let ceiling = usize::try_from(started.elapsed().as_secs()).unwrap_or(usize::MAX - 3) + 3;
    let seen = stream.collect_for(Duration::from_secs(3)).await;
    let list_changed = seen
        .iter()
        .filter(|n| n["method"] == "notifications/resources/list_changed")
        .count();
    assert!(
        (1..=ceiling).contains(&list_changed),
        "100 writes should coalesce into at most {ceiling} notifications, \
         got {list_changed}: {seen:?}"
    );
    assert!(
        seen.iter()
            .all(|n| n["method"] == "notifications/resources/list_changed"),
        "no resource was subscribed, so nothing else arrives: {seen:?}"
    );
    quiet_stream.expect_silence(QUIET).await;

    // A delete is a list change too.
    delete(&server, "bulk/0.md").await;
    let notified = stream.next(WAIT).await;
    assert_eq!(notified["method"], "notifications/resources/list_changed");
}

#[tokio::test]
async fn subscriptions_die_with_the_session() {
    let server = server_with_events().await;
    server.put_text("s/a.md", "a").await;
    let (mcp, mut stream) = subscribed_session(&server).await;
    subscribe(&mcp, 1, &uri(&server, "s/a.md")).await;

    let response = mcp.close().await;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    // The notification leg ends with the session …
    let _ = stream.collect_for(Duration::from_secs(2)).await;
    assert!(stream.ended(), "DELETE /mcp closes the GET leg");

    // … and the next request is a new session, with no subscriptions. A
    // write is invisible to it; nothing leaked from the old session.
    let mut fresh = mcp.notifications().await;
    server.put_text("s/a.md", "b").await;
    fresh.expect_silence(QUIET).await;
}

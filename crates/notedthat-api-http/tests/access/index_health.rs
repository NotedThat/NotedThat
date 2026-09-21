//! `GET /api/v1/knowledgebases/{kb}/index` (#97): who may ask, and what each
//! state looks like on the wire.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedthat_core::{Verb, Who};
use notedthat_indexer::ReconcileSummary;
use tower::ServiceExt;

use super::fixture::{
    ALICE_TOKEN, BOB_TOKEN, TOKEN, app, app_with_index_side, grant, grant_under, json, policy,
};

fn only_notes(
    rules: impl IntoIterator<Item = notedthat_core::AccessRule>,
) -> BTreeMap<String, notedthat_core::AccessPolicy> {
    BTreeMap::from([("notes".to_string(), policy(rules))])
}

async fn get_index(app: axum::Router, kb: &str, token: Option<&str>) -> axum::response::Response {
    let mut builder = Request::builder().uri(format!("/api/v1/knowledgebases/{kb}/index"));
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    app.oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("response")
}

#[tokio::test]
async fn the_view_follows_the_listing_rule() {
    // Given — `notes` grants anonymous reads under one prefix; `private` has
    // no policy at all.
    let policies = only_notes([grant_under(Who::Anyone, [Verb::Read], &["public/**"])]);

    // Then — any grant at all is enough to ask, as it is enough to be listed.
    let response = get_index(app(policies.clone()).await, "notes", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    // An anonymous caller with no grant is told the base does not exist, the
    // answer every other route gives, so this one is not an oracle.
    let response = get_index(app(policies.clone()).await, "private", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // A verified identity holding nothing is refused outright.
    let response = get_index(app(policies.clone()).await, "private", Some(BOB_TOKEN)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // The service token sees every declared knowledge base listed, so it may
    // ask about each; an undeclared slug is still `404` for it.
    let response = get_index(app(policies.clone()).await, "private", Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = get_index(app(policies).await, "undeclared", Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_fresh_knowledge_base_is_healthy_with_nothing_to_report() {
    let (app, side) = app_with_index_side(only_notes([grant(Who::Anyone, [Verb::Read])]), 8).await;
    let response = get_index(app, "notes", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let body = json(response).await;
    assert_eq!(
        body,
        serde_json::json!({
            "kb_slug": "notes",
            "state": "healthy",
            "pending": 0,
            "queue": { "depth": 0, "capacity": 8 },
            "worker": "running",
            "last_indexed_at": null,
            "last_failure": null,
            "last_reconcile": null,
        })
    );
    drop(side);
}

#[tokio::test]
async fn a_write_leaves_the_knowledge_base_indexing_until_the_worker_takes_it() {
    // Given — a reader's grant, a writer's grant and a queue nobody drains.
    let (app, mut side) = app_with_index_side(
        only_notes([
            grant(Who::Anyone, [Verb::Read]),
            grant(Who::SignedIn, [Verb::Write]),
        ]),
        8,
    )
    .await;

    // When — a write commits and its event is queued.
    let put = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/knowledgebases/notes/new.md")
                .header("content-type", "text/markdown")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from("# new"))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(put.status(), StatusCode::CREATED);

    // Then — the write path counted it, and the queue shows it.
    let body = json(get_index(app.clone(), "notes", None).await).await;
    assert_eq!(body["state"], "indexing");
    assert_eq!(body["pending"], 1);
    assert_eq!(body["queue"]["depth"], 1);

    // When — the worker takes it and finishes.
    let event = side.queue_rx.recv().await.expect("queued event");
    side.health.started(event.kb().as_str());
    side.health.succeeded(event.kb().as_str());

    // Then
    let body = json(get_index(app, "notes", None).await).await;
    assert_eq!(body["state"], "healthy");
    assert_eq!(body["pending"], 0);
    assert_eq!(body["queue"]["depth"], 0);
    assert!(body["last_indexed_at"].is_string());
}

#[tokio::test]
async fn a_full_queue_backpressures_every_knowledge_base() {
    // Given — a queue with room for one event, already holding it.
    let (app, side) = app_with_index_side(
        only_notes([
            grant(Who::Anyone, [Verb::Read]),
            grant(Who::SignedIn, [Verb::Write]),
        ]),
        1,
    )
    .await;
    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/knowledgebases/notes/one.md")
                .header("content-type", "text/markdown")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from("# one"))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(first.status(), StatusCode::CREATED);

    // Then — the queue is full, and the view says so before any write is refused …
    let body = json(get_index(app.clone(), "notes", None).await).await;
    assert_eq!(body["state"], "backpressured");
    assert_eq!(
        body["queue"],
        serde_json::json!({ "depth": 1, "capacity": 1 })
    );

    // … and once one is (D38), the refusal itself is on the record.
    let second = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/knowledgebases/notes/two.md")
                .header("content-type", "text/markdown")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from("# two"))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(side.health.snapshot("notes").last_backpressure_at.is_some());
    let body = json(get_index(app, "notes", None).await).await;
    assert_eq!(body["state"], "backpressured");
}

#[tokio::test]
async fn a_failure_is_reported_and_its_key_only_to_a_caller_who_could_list_it() {
    // Given — anonymous callers may read and list under `public/` only; any
    // credential may read; alice alone may list everything.
    let (app, side) = app_with_index_side(
        only_notes([
            grant_under(Who::Anyone, [Verb::Read, Verb::List], &["public/**"]),
            grant(Who::SignedIn, [Verb::Read]),
            grant(Who::User("alice".into()), [Verb::List]),
        ]),
        8,
    )
    .await;
    side.health.failed(
        "notes",
        "internal/secret.md",
        "embedder.embed failed: connection refused",
    );

    // Then — the anonymous caller learns that indexing failed and when, but
    // neither which key outside its grant was involved nor the pipeline's
    // error, which names the deployment's own endpoints …
    let body = json(get_index(app.clone(), "notes", None).await).await;
    assert_eq!(body["state"], "failed");
    let failure = &body["last_failure"];
    assert!(failure["at"].is_string());
    assert!(
        failure.get("object_key").is_none(),
        "a key the caller could not list must not be named: {failure}"
    );
    assert!(
        failure.get("summary").is_none(),
        "the error is not described to an anonymous caller: {failure}"
    );

    // … while a credentialed caller who could list it sees both.
    let body = json(get_index(app.clone(), "notes", Some(ALICE_TOKEN)).await).await;
    assert_eq!(body["last_failure"]["object_key"], "internal/secret.md");
    assert_eq!(
        body["last_failure"]["summary"],
        "embedder.embed failed: connection refused"
    );
    // A credentialed caller who could not list the key gets the error alone.
    let body = json(get_index(app.clone(), "notes", Some(BOB_TOKEN)).await).await;
    assert_eq!(body["state"], "failed");
    assert!(body["last_failure"].get("object_key").is_none());
    assert_eq!(
        body["last_failure"]["summary"],
        "embedder.embed failed: connection refused"
    );

    // And a later success clears the state, keeping the failure on record.
    side.health.succeeded("notes");
    let body = json(get_index(app, "notes", None).await).await;
    assert_eq!(body["state"], "healthy");
    assert!(body["last_failure"].is_object());
}

#[tokio::test]
async fn stale_and_the_last_reconciliation_pass_are_reported() {
    let (app, side) = app_with_index_side(only_notes([grant(Who::Anyone, [Verb::Read])]), 8).await;

    // Given — the fs bridge asked for a rescan and has not completed it.
    side.health.mark_stale("notes");
    let body = json(get_index(app.clone(), "notes", None).await).await;
    assert_eq!(body["state"], "stale");
    assert_eq!(body["last_reconcile"], serde_json::Value::Null);

    // When — the pass completes.
    side.health.reconciled(
        "notes",
        ReconcileSummary {
            at: 1_700_000_000,
            objects_on_disk: 12,
            unchanged: 11,
            changed: 1,
            orphaned: 0,
        },
    );

    // Then
    let body = json(get_index(app, "notes", None).await).await;
    assert_eq!(body["state"], "healthy");
    assert_eq!(
        body["last_reconcile"],
        serde_json::json!({
            "at": "2023-11-14T22:13:20Z",
            "objects_on_disk": 12,
            "unchanged": 11,
            "changed": 1,
            "orphaned": 0,
        })
    );
}

#[tokio::test]
async fn a_stopped_worker_fails_every_knowledge_base() {
    let (app, side) = app_with_index_side(only_notes([grant(Who::Anyone, [Verb::Read])]), 8).await;
    side.health.worker_stopped();
    let body = json(get_index(app, "notes", None).await).await;
    assert_eq!(body["state"], "failed");
    assert_eq!(body["worker"], "stopped");
}

//! `POST /api/v1/knowledgebases/{kb}/index/reconcile` (D67): who may ask, and
//! what each answer looks like on the wire.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedthat_api_http::testing::RecordingReconcile;
use notedthat_core::{Verb, Who};
use tower::ServiceExt;

use super::fixture::{ALICE_TOKEN, BOB_TOKEN, TOKEN, app, app_with_reconcile, grant, json, policy};

fn only_notes(
    rules: impl IntoIterator<Item = notedthat_core::AccessRule>,
) -> BTreeMap<String, notedthat_core::AccessPolicy> {
    BTreeMap::from([("notes".to_string(), policy(rules))])
}

async fn post_reconcile(
    app: axum::Router,
    kb: &str,
    token: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/knowledgebases/{kb}/index/reconcile"));
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    app.oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("response")
}

#[tokio::test]
async fn the_service_token_starts_a_pass_and_is_told_so() {
    let recording = Arc::new(RecordingReconcile::default());
    let app = app_with_reconcile(only_notes([]), Some(recording.clone())).await;

    let response = post_reconcile(app, "notes", Some(TOKEN)).await;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(
        response.headers()["cache-control"],
        "no-store",
        "an operator answer is never cached"
    );
    let body = json(response).await;
    assert_eq!(
        body,
        serde_json::json!({ "kb_slug": "notes", "status": "started" })
    );
    assert_eq!(
        *recording.triggered.lock().unwrap(),
        vec!["notes".to_string()],
        "exactly one pass, for the knowledge base asked"
    );
}

#[tokio::test]
async fn an_identity_is_refused_whatever_the_rules_grant_it() {
    // Even everything: this is the operator's action, not a knowledge-base verb.
    let policies = only_notes([grant(Who::SignedIn, Verb::ALL)]);
    let recording = Arc::new(RecordingReconcile::default());

    for token in [ALICE_TOKEN, BOB_TOKEN] {
        let app = app_with_reconcile(policies.clone(), Some(recording.clone())).await;
        let response = post_reconcile(app, "notes", Some(token)).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{token}");
        assert_eq!(json(response).await["error"], "forbidden", "{token}");
    }
    assert!(recording.triggered.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_anonymous_caller_is_refused_before_any_slug_is_looked_at() {
    // `anyone` may do everything in `notes`; the operator route is still not
    // reachable without a credential, and the undeclared slug answers the same,
    // so the route is not an oracle for which knowledge bases exist.
    let policies = only_notes([grant(Who::Anyone, Verb::ALL)]);
    for kb in ["notes", "nope"] {
        let response = post_reconcile(app(policies.clone()).await, kb, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{kb}");
    }
}

#[tokio::test]
async fn an_undeclared_knowledge_base_is_not_found_for_the_operator() {
    let response = post_reconcile(app(only_notes([])).await, "nope", Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(json(response).await["error"], "not_found");
}

#[tokio::test]
async fn a_running_pass_answers_conflict() {
    let recording = Arc::new(RecordingReconcile::default());
    recording
        .busy
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let app = app_with_reconcile(only_notes([]), Some(recording)).await;

    let response = post_reconcile(app, "notes", Some(TOKEN)).await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = json(response).await;
    assert_eq!(body["error"], "conflict");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("already running"),
        "{body}"
    );
}

#[tokio::test]
async fn a_backend_without_a_pass_answers_not_found_and_says_which_has_one() {
    // The `fs` backend's shape: nothing to trigger, because its watcher and
    // startup pass already keep the index in step (D50).
    let app = app_with_reconcile(only_notes([]), None).await;

    let response = post_reconcile(app, "notes", Some(TOKEN)).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json(response).await;
    assert_eq!(body["error"], "not_found");
    assert_eq!(
        body["message"],
        "on-demand reconciliation is available on the s3 backend only"
    );
}

//! `docs/openapi.json` is the generated document, byte for byte (D77).
//!
//! The committed copy is what a client reads without running a server, so it
//! may not drift from what the server serves. When this fails, regenerate it:
//!
//! ```text
//! NOTEDTHAT_UPDATE_OPENAPI=1 cargo test -p notedthat-api-http --test openapi
//! ```
//!
//! and commit the result with the change that caused it.

#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use notedthat_api_http::router::openapi::document_json;
use notedthat_api_http::router::{OPENAPI_PATH, build_router};
use notedthat_api_http::state::AppState;
use notedthat_api_http::testing::InMemoryStorage;
use tower::ServiceExt;

const UPDATE: &str = "NOTEDTHAT_UPDATE_OPENAPI";

fn router() -> axum::Router {
    build_router(AppState::for_tests(
        Arc::new(InMemoryStorage::default()),
        BTreeMap::new(),
    ))
}

fn committed_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi.json")
}

#[test]
fn the_committed_document_is_the_generated_one() {
    let path = committed_path();
    let generated = document_json();
    if std::env::var_os(UPDATE).is_some() {
        std::fs::write(&path, generated).expect("write docs/openapi.json");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "docs/openapi.json is stale. Regenerate it with\n\n    \
         {UPDATE}=1 cargo test -p notedthat-api-http --test openapi\n\n\
         and commit the result."
    );
}

#[test]
fn the_document_is_openapi_3_1() {
    let value: serde_json::Value = serde_json::from_str(document_json()).expect("JSON");
    assert_eq!(value["openapi"], "3.1.0");
    // What utoipa parses back is what a client library will: an unknown or
    // misplaced key would fail here before it fails a consumer.
    let _: utoipa::openapi::OpenApi = serde_json::from_str(document_json()).expect("OpenAPI");
}

#[tokio::test]
async fn the_document_is_served_to_anyone() {
    let response = router()
        .oneshot(
            Request::get(OPENAPI_PATH)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    assert_eq!(body, document_json().as_bytes());
}

/// Public means public: a credential that does not verify does not turn the
/// document into a `401`, as it would anywhere under `auth_middleware`.
#[tokio::test]
async fn a_bad_credential_does_not_hide_the_document() {
    let response = router()
        .oneshot(
            Request::get(OPENAPI_PATH)
                .header(header::AUTHORIZATION, "Bearer not-the-token")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::OK);
}

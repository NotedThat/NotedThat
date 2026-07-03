//! Integration tests for the `NotedThat` HTTP API using `InMemoryStorage`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use notedthat_api_http::router::build_router;
use notedthat_api_http::state::AppState;
use notedthat_api_http::testing::InMemoryStorage;
use notedthat_core::KbSlug;
use tower::util::ServiceExt;

const TOKEN: &str = "test-token-abc";
const KB: &str = "notes";
const KB2: &str = "docs";

fn app() -> axum::Router {
    app_with_max_body_size(16 * 1024 * 1024)
}

fn app_with_max_body_size(max_body_size: u64) -> axum::Router {
    let storage = Arc::new(InMemoryStorage::default());
    let mut kbs = BTreeMap::new();
    kbs.insert(KB.to_string(), KbSlug::try_new(KB).unwrap());
    kbs.insert(KB2.to_string(), KbSlug::try_new(KB2).unwrap());
    let state = AppState {
        storage,
        declared_kbs: Arc::new(kbs),
        bearer_token: Arc::new(TOKEN.to_string()),
        max_body_size,
    };
    build_router(state)
}

fn auth() -> (&'static str, &'static str) {
    ("authorization", "Bearer test-token-abc")
}

fn authed_request(method: &str, uri: String, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(auth().0, auth().1)
        .body(body)
        .unwrap()
}

async fn put_text(app: axum::Router, kb: &str, path: &str, body: &str) -> StatusCode {
    app.oneshot(authed_request(
        "PUT",
        format!("/v1/knowledgebases/{kb}/{path}"),
        Body::from(body.to_string()),
    ))
    .await
    .unwrap()
    .status()
}

async fn response_json(resp: axum::response::Response) -> serde_json::Value {
    let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

// ─── Health probes ──────────────────────────────────────────────────────────

#[tokio::test]
async fn healthz_no_auth_required() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn readyz_no_auth_required() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["status"], "ok");
}

// ─── Auth tests ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_kbs_requires_auth() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/v1/knowledgebases")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn list_kbs_wrong_token() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/v1/knowledgebases")
                .header("authorization", "Bearer wrong-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn put_requires_auth() {
    let resp = app()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/file.md"))
                .body(Body::from("content"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn lowercase_bearer_is_accepted() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/v1/knowledgebases")
                .header("authorization", "bearer test-token-abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ─── List KBs ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_kbs_returns_declared_kbs() {
    let resp = app()
        .oneshot(authed_request(
            "GET",
            "/v1/knowledgebases".to_string(),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    let kbs = json["knowledgebases"].as_array().unwrap();
    assert_eq!(kbs.len(), 2);
    assert!(kbs.contains(&serde_json::Value::String(KB.to_string())));
    assert!(kbs.contains(&serde_json::Value::String(KB2.to_string())));
}

#[tokio::test]
async fn list_kbs_has_request_id_header() {
    let resp = app()
        .oneshot(authed_request(
            "GET",
            "/v1/knowledgebases".to_string(),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert!(resp.headers().contains_key("x-request-id"));
}

// ─── PUT / GET / HEAD / DELETE round-trip ───────────────────────────────────

#[tokio::test]
async fn put_get_round_trip_preserves_content() {
    let a = app();
    let put_resp = a
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/hello.md"))
                .header(auth().0, auth().1)
                .header("content-type", "text/markdown")
                .body(Body::from("# Hello\n\nTest.\n"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put_resp.status(), StatusCode::CREATED);

    let get_resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/hello.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    let body = to_bytes(get_resp.into_body(), 1024).await.unwrap();
    assert_eq!(&body[..], b"# Hello\n\nTest.\n");
}

#[tokio::test]
async fn put_returns_201_with_location() {
    let resp = app()
        .oneshot(authed_request(
            "PUT",
            format!("/v1/knowledgebases/{KB}/file.md"),
            Body::from("content"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        format!("/v1/knowledgebases/{KB}/file.md")
    );
}

#[tokio::test]
async fn put_returns_etag() {
    let resp = app()
        .oneshot(authed_request(
            "PUT",
            format!("/v1/knowledgebases/{KB}/etag.md"),
            Body::from("1234567890123456789012"),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        format!("/v1/knowledgebases/{KB}/etag.md")
    );
    let etag = resp.headers().get("etag").unwrap().to_str().unwrap();
    assert!(!etag.is_empty(), "PUT must return a non-empty ETag");
}

#[tokio::test]
async fn put_if_match_correct_returns_new_etag() {
    let a = app();
    let first = a
        .clone()
        .oneshot(authed_request(
            "PUT",
            format!("/v1/knowledgebases/{KB}/if-match-ok.md"),
            Body::from("first"),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_etag = first.headers().get("etag").unwrap().to_str().unwrap();

    let second = a
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/if-match-ok.md"))
                .header(auth().0, auth().1)
                .header("if-match", first_etag)
                .body(Body::from("second"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(second.status(), StatusCode::CREATED);
    let second_etag = second.headers().get("etag").unwrap().to_str().unwrap();
    assert!(
        !second_etag.is_empty(),
        "PUT must return a replacement ETag"
    );
    assert_ne!(second_etag, first_etag);
}

#[tokio::test]
async fn put_if_match_412() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "if-match-wrong.md", "first").await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/if-match-wrong.md"))
                .header(auth().0, auth().1)
                .header("if-match", "\"wrong\"")
                .body(Body::from("second"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn put_if_none_match_wildcard_conflict() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "if-none-match-existing.md", "first").await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/if-none-match-existing.md"))
                .header(auth().0, auth().1)
                .header("if-none-match", "*")
                .body(Body::from("second"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn put_if_none_match_wildcard_new() {
    let resp = app()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/if-none-match-new.md"))
                .header(auth().0, auth().1)
                .header("if-none-match", "*")
                .body(Body::from("content"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
    let etag = resp.headers().get("etag").unwrap().to_str().unwrap();
    assert!(!etag.is_empty(), "PUT must return a non-empty ETag");
}

#[tokio::test]
async fn put_if_modified_since_is_ignored() {
    let resp = app()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/if-modified-since.md"))
                .header(auth().0, auth().1)
                .header("if-modified-since", "Wed, 21 Oct 2015 07:28:00 GMT")
                .body(Body::from("content"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn put_percent_encodes_location_for_non_ascii_paths() {
    let resp = app()
        .oneshot(authed_request(
            "PUT",
            format!("/v1/knowledgebases/{KB}/caf%C3%A9%20notes.md"),
            Body::from("content"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers().get("location").unwrap().to_str().unwrap(),
        format!("/v1/knowledgebases/{KB}/caf%C3%A9%20notes.md")
    );
}

#[tokio::test]
async fn put_overwrites_existing_object() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "same.md", "first").await,
        StatusCode::CREATED
    );
    assert_eq!(
        put_text(a.clone(), KB, "same.md", "second").await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/same.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    assert_eq!(&body[..], b"second");
}

#[tokio::test]
async fn head_returns_content_length_no_body() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "file.txt", "hello world").await,
        StatusCode::CREATED
    );

    let head_resp = a
        .oneshot(authed_request(
            "HEAD",
            format!("/v1/knowledgebases/{KB}/file.txt"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(head_resp.status(), StatusCode::OK);
    assert_eq!(head_resp.headers().get("content-length").unwrap(), "11");
    let body = to_bytes(head_resp.into_body(), 1024).await.unwrap();
    assert!(body.is_empty(), "HEAD must return empty body");
}

#[tokio::test]
async fn head_echoes_content_type() {
    let a = app();
    let put_resp = a
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/typed.txt"))
                .header(auth().0, auth().1)
                .header("content-type", "text/plain")
                .body(Body::from("hello"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put_resp.status(), StatusCode::CREATED);

    let head_resp = a
        .oneshot(authed_request(
            "HEAD",
            format!("/v1/knowledgebases/{KB}/typed.txt"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(
        head_resp.headers().get("content-type").unwrap(),
        "text/plain"
    );
}

#[tokio::test]
async fn delete_returns_204() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "todel.md", "bye").await,
        StatusCode::CREATED
    );

    let del_resp = a
        .oneshot(authed_request(
            "DELETE",
            format!("/v1/knowledgebases/{KB}/todel.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(del_resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_idempotent_non_existent_returns_204() {
    let resp = app()
        .oneshot(authed_request(
            "DELETE",
            format!("/v1/knowledgebases/{KB}/does-not-exist.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_removes_object() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "gone.md", "bye").await,
        StatusCode::CREATED
    );
    let del_resp = a
        .clone()
        .oneshot(authed_request(
            "DELETE",
            format!("/v1/knowledgebases/{KB}/gone.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(del_resp.status(), StatusCode::NO_CONTENT);

    let get_resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/gone.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_if_match_correct() {
    use notedthat_api_http::testing::compute_etag;

    let content = "delete-me";
    let etag = compute_etag(content.as_bytes());

    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "cond-del.md", content).await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/knowledgebases/{KB}/cond-del.md"))
                .header(auth().0, auth().1)
                .header("if-match", etag.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_if_match_wrong() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "protected.md", "content").await,
        StatusCode::CREATED
    );

    let del_resp = a
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/knowledgebases/{KB}/protected.md"))
                .header(auth().0, auth().1)
                .header("if-match", "\"wrong-etag\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_resp.status(), StatusCode::PRECONDITION_FAILED);

    let get_resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/protected.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn delete_ignores_extra_conditionals() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "extra-cond.md", "data").await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/knowledgebases/{KB}/extra-cond.md"))
                .header(auth().0, auth().1)
                .header("if-none-match", "\"some-etag\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn delete_missing_object() {
    let resp = app()
        .oneshot(authed_request(
            "DELETE",
            format!("/v1/knowledgebases/{KB}/no-such-file.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn get_default_content_type_for_untyped_put() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "raw.bin", "abc").await,
        StatusCode::CREATED
    );
    let resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/raw.bin"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/octet-stream"
    );
}

#[tokio::test]
async fn get_returns_etag() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "etag.txt", "hello etag").await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/etag.txt"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().contains_key("etag"));
}

#[tokio::test]
async fn get_range_206() {
    let a = app();
    let body = "0123456789".repeat(10);
    assert_eq!(
        put_text(a.clone(), KB, "range.txt", &body).await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/knowledgebases/{KB}/range.txt"))
                .header(auth().0, auth().1)
                .header("range", "bytes=0-9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        "bytes 0-9/100"
    );
    assert_eq!(resp.headers().get("content-length").unwrap(), "10");
    assert!(resp.headers().contains_key("etag"));
    let response_body = to_bytes(resp.into_body(), 1024).await.unwrap();
    assert_eq!(response_body.len(), 10);
    assert_eq!(&response_body[..], b"0123456789");
}

#[tokio::test]
async fn get_range_416() {
    let a = app();
    let body = "0123456789".repeat(10);
    assert_eq!(
        put_text(a.clone(), KB, "range-416.txt", &body).await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/knowledgebases/{KB}/range-416.txt"))
                .header(auth().0, auth().1)
                .header("range", "bytes=200-300")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(resp.headers().get("content-range").unwrap(), "bytes */100");
    let response_body = to_bytes(resp.into_body(), 1024).await.unwrap();
    assert!(response_body.is_empty());
}

#[tokio::test]
async fn get_range_malformed() {
    let resp = app()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/knowledgebases/{KB}/malformed.txt"))
                .header(auth().0, auth().1)
                .header("range", "bytes=abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_range_unknown_unit() {
    let a = app();
    let body = "0123456789".repeat(10);
    assert_eq!(
        put_text(a.clone(), KB, "range-unit.txt", &body).await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/knowledgebases/{KB}/range-unit.txt"))
                .header(auth().0, auth().1)
                .header("range", "items=0-10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!resp.headers().contains_key("content-range"));
    let response_body = to_bytes(resp.into_body(), 1024).await.unwrap();
    assert_eq!(response_body.len(), 100);
}

#[tokio::test]
async fn get_if_none_match_304() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "conditional-304.txt", "conditional").await,
        StatusCode::CREATED
    );
    let first_get = a
        .clone()
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/conditional-304.txt"),
            Body::empty(),
        ))
        .await
        .unwrap();
    let etag = first_get.headers().get("etag").unwrap().to_str().unwrap();

    let resp = a
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/knowledgebases/{KB}/conditional-304.txt"))
                .header(auth().0, auth().1)
                .header("if-none-match", etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    let response_body = to_bytes(resp.into_body(), 1024).await.unwrap();
    assert!(response_body.is_empty());
}

#[tokio::test]
async fn get_if_match_412() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "conditional-412.txt", "conditional").await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/knowledgebases/{KB}/conditional-412.txt"))
                .header(auth().0, auth().1)
                .header("if-match", "\"wrong\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn objects_are_scoped_by_kb() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "shared.md", "notes").await,
        StatusCode::CREATED
    );
    assert_eq!(
        put_text(a.clone(), KB2, "shared.md", "docs").await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB2}/shared.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    assert_eq!(&body[..], b"docs");
}

// ─── LIST ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn list_objects_returns_objects() {
    let a = app();
    for i in 0..3 {
        assert_eq!(
            put_text(
                a.clone(),
                KB,
                &format!("file{i}.md"),
                &format!("content {i}")
            )
            .await,
            StatusCode::CREATED
        );
    }

    let resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    let objects = json["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 3);
}

#[tokio::test]
async fn list_objects_truncated_at_limit() {
    let a = app();
    for i in 0..5 {
        assert_eq!(
            put_text(
                a.clone(),
                KB,
                &format!("obj{i}.md"),
                &format!("content {i}")
            )
            .await,
            StatusCode::CREATED
        );
    }

    let resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}?limit=2"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["objects"].as_array().unwrap().len(), 2);
    assert_eq!(json["truncated"], true);
}

#[tokio::test]
async fn list_objects_filters_by_prefix() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "a/one.md", "1").await,
        StatusCode::CREATED
    );
    assert_eq!(
        put_text(a.clone(), KB, "a/two.md", "2").await,
        StatusCode::CREATED
    );
    assert_eq!(
        put_text(a.clone(), KB, "b/three.md", "3").await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}?prefix=a/"),
            Body::empty(),
        ))
        .await
        .unwrap();
    let json = response_json(resp).await;
    let objects = json["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 2);
    assert!(
        objects
            .iter()
            .all(|object| object["key"].as_str().unwrap().starts_with("a/"))
    );
}

#[tokio::test]
async fn list_objects_limit_zero_uses_default() {
    let a = app();
    for i in 0..3 {
        assert_eq!(
            put_text(a.clone(), KB, &format!("z{i}.md"), "z").await,
            StatusCode::CREATED
        );
    }

    let resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}?limit=0"),
            Body::empty(),
        ))
        .await
        .unwrap();
    let json = response_json(resp).await;
    assert_eq!(json["objects"].as_array().unwrap().len(), 3);
    assert_eq!(json["truncated"], false);
}

#[tokio::test]
async fn list_objects_undeclared_kb_returns_404() {
    let resp = app()
        .oneshot(authed_request(
            "GET",
            "/v1/knowledgebases/undeclared".to_string(),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ─── Error cases ────────────────────────────────────────────────────────────

#[tokio::test]
async fn put_to_undeclared_kb_returns_404() {
    let resp = app()
        .oneshot(authed_request(
            "PUT",
            "/v1/knowledgebases/undeclared/foo.md".to_string(),
            Body::from("data"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_non_existent_object_returns_404() {
    let resp = app()
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/no-such.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn head_non_existent_object_returns_404() {
    let resp = app()
        .oneshot(authed_request(
            "HEAD",
            format!("/v1/knowledgebases/{KB}/no-such.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn put_with_path_traversal_returns_400_or_404() {
    let resp = app()
        .oneshot(authed_request(
            "PUT",
            format!("/v1/knowledgebases/{KB}/../etc/passwd"),
            Body::from("evil"),
        ))
        .await
        .unwrap();
    assert!(
        resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::NOT_FOUND,
        "expected 400 or 404, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn put_with_double_slash_path_returns_400_or_404() {
    let resp = app()
        .oneshot(authed_request(
            "PUT",
            format!("/v1/knowledgebases/{KB}/foo//bar.md"),
            Body::from("bad"),
        ))
        .await
        .unwrap();
    assert!(resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unsupported_method_returns_405() {
    let resp = app()
        .oneshot(authed_request(
            "POST",
            format!("/v1/knowledgebases/{KB}/file.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn error_body_contains_request_id() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/v1/knowledgebases")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = response_json(resp).await;
    assert!(
        json["request_id"].is_string(),
        "request_id field must be a string"
    );
    assert!(!json["request_id"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn custom_request_id_is_echoed_in_error_body() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/v1/knowledgebases")
                .header("x-request-id", "req-custom")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = response_json(resp).await;
    assert_eq!(json["request_id"], "req-custom");
}

#[tokio::test]
async fn get_content_type_echoed_from_put() {
    let a = app();
    let put_resp = a
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/doc.md"))
                .header(auth().0, auth().1)
                .header("content-type", "text/markdown; charset=utf-8")
                .body(Body::from("# Doc"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put_resp.status(), StatusCode::CREATED);

    let get_resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/doc.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    let content_type = get_resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_type.contains("text/markdown"),
        "content-type must echo the PUT value"
    );
}

#[tokio::test]
async fn put_rejects_content_length_above_limit() {
    let resp = app_with_max_body_size(4)
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/too-big.md"))
                .header(auth().0, auth().1)
                .header("content-length", "5")
                .body(Body::from("x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn put_rejects_actual_body_above_limit() {
    let resp = app_with_max_body_size(4)
        .oneshot(authed_request(
            "PUT",
            format!("/v1/knowledgebases/{KB}/too-big.md"),
            Body::from("12345"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn put_allows_body_at_exact_limit() {
    let resp = app_with_max_body_size(4)
        .oneshot(authed_request(
            "PUT",
            format!("/v1/knowledgebases/{KB}/exact.md"),
            Body::from("1234"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

// ─── HEAD ETag and conditionals ──────────────────────────────────────────────

#[tokio::test]
async fn head_returns_etag() {
    let body = "x".repeat(100);
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "head-etag.txt", &body).await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(authed_request(
            "HEAD",
            format!("/v1/knowledgebases/{KB}/head-etag.txt"),
            Body::empty(),
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("etag").is_some(),
        "HEAD must return ETag header"
    );
    assert_eq!(resp.headers().get("content-length").unwrap(), "100");
    let body_bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
    assert!(body_bytes.is_empty(), "HEAD must return empty body");
}

#[tokio::test]
async fn head_ignores_range() {
    let body = "x".repeat(100);
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "head-range.txt", &body).await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("/v1/knowledgebases/{KB}/head-range.txt"))
                .header(auth().0, auth().1)
                .header("range", "bytes=0-9")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "Range header must be ignored on HEAD");
    assert_eq!(resp.headers().get("content-length").unwrap(), "100");
    assert!(
        resp.headers().get("content-range").is_none(),
        "HEAD must not emit Content-Range"
    );
}

#[tokio::test]
async fn head_if_none_match_304() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "head-304.txt", "cached content").await,
        StatusCode::CREATED
    );

    let head_resp = a
        .clone()
        .oneshot(authed_request(
            "HEAD",
            format!("/v1/knowledgebases/{KB}/head-304.txt"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(head_resp.status(), StatusCode::OK);
    let etag = head_resp
        .headers()
        .get("etag")
        .expect("ETag must be present")
        .to_str()
        .unwrap()
        .to_string();

    let resp = a
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("/v1/knowledgebases/{KB}/head-304.txt"))
                .header(auth().0, auth().1)
                .header("if-none-match", &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    let body_bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
    assert!(body_bytes.is_empty(), "304 must return empty body");
}

#[tokio::test]
async fn head_if_match_412() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "head-412.txt", "some content").await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(format!("/v1/knowledgebases/{KB}/head-412.txt"))
                .header(auth().0, auth().1)
                .header("if-match", "\"wrong-etag\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
}

// ─── M3 Cross-Cutting Scenarios ──────────────────────────────────────────────

/// End-to-end round trip: PUT → GET → 304 → conditional PUT → GET (new) → conditional DELETE → 404.
#[tokio::test]
async fn m3_round_trip() {
    let a = app();

    // Step 1: PUT object, capture ETag E1
    let put_resp = a
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/round-trip.md"))
                .header(auth().0, auth().1)
                .body(Body::from("first body"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put_resp.status(), StatusCode::CREATED);
    let e1 = put_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(!e1.is_empty());

    // Step 2: GET → 200, correct body, ETag == E1
    let get_resp = a
        .clone()
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/round-trip.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    assert_eq!(get_resp.headers().get("etag").unwrap().to_str().unwrap(), e1);
    let body = to_bytes(get_resp.into_body(), 1024).await.unwrap();
    assert_eq!(&body[..], b"first body");

    // Step 3: GET with If-None-Match: E1 → 304
    let resp = a
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/knowledgebases/{KB}/round-trip.md"))
                .header(auth().0, auth().1)
                .header("if-none-match", &e1)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);

    // Step 4: conditional PUT with If-Match: E1 + new body → 201, new ETag E2
    let put2_resp = a
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/round-trip.md"))
                .header(auth().0, auth().1)
                .header("if-match", &e1)
                .body(Body::from("second body"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put2_resp.status(), StatusCode::CREATED);
    let e2 = put2_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(e2, e1, "ETag must change when content changes");

    // Step 5: GET → 200, new body, ETag == E2
    let get2_resp = a
        .clone()
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/round-trip.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(get2_resp.status(), StatusCode::OK);
    assert_eq!(
        get2_resp.headers().get("etag").unwrap().to_str().unwrap(),
        e2
    );
    let body2 = to_bytes(get2_resp.into_body(), 1024).await.unwrap();
    assert_eq!(&body2[..], b"second body");

    // Step 6: conditional DELETE with If-Match: E2 → 204
    let del_resp = a
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/knowledgebases/{KB}/round-trip.md"))
                .header(auth().0, auth().1)
                .header("if-match", &e2)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del_resp.status(), StatusCode::NO_CONTENT);

    // Step 7: GET → 404
    let get3_resp = a
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/round-trip.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(get3_resp.status(), StatusCode::NOT_FOUND);
}

/// Precondition precedence: If-Match passes but If-None-Match: * fires because object exists.
#[tokio::test]
async fn m3_precondition_precedence() {
    let a = app();

    let put_resp = a
        .clone()
        .oneshot(authed_request(
            "PUT",
            format!("/v1/knowledgebases/{KB}/prec-precedence.md"),
            Body::from("data"),
        ))
        .await
        .unwrap();
    assert_eq!(put_resp.status(), StatusCode::CREATED);
    let e = put_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // If-Match: E (would pass alone), but If-None-Match: * fires because object exists → 412
    let resp = a
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/prec-precedence.md"))
                .header(auth().0, auth().1)
                .header("if-match", &e)
                .header("if-none-match", "*")
                .body(Body::from("new data"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PRECONDITION_FAILED,
        "If-None-Match: * must fire because object exists, even when If-Match passes"
    );
}

/// Multi-ETag If-Match list: wrong list → 412; list containing the current ETag → 201.
#[tokio::test]
async fn m3_multi_etag_if_match() {
    let a = app();

    let put_resp = a
        .clone()
        .oneshot(authed_request(
            "PUT",
            format!("/v1/knowledgebases/{KB}/multi-etag.md"),
            Body::from("content"),
        ))
        .await
        .unwrap();
    assert_eq!(put_resp.status(), StatusCode::CREATED);
    let e = put_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // If-Match list containing no matching ETag → 412
    let resp_wrong = a
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/multi-etag.md"))
                .header(auth().0, auth().1)
                .header("if-match", "\"wrong1\", \"wrong2\"")
                .body(Body::from("rejected"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp_wrong.status(),
        StatusCode::PRECONDITION_FAILED,
        "If-Match list with no matching ETag must return 412"
    );

    // If-Match list that includes the actual ETag → 201
    let if_match_with_e = format!("\"wrong1\", {e}, \"wrong2\"");
    let resp_ok = a
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!("/v1/knowledgebases/{KB}/multi-etag.md"))
                .header(auth().0, auth().1)
                .header("if-match", if_match_with_e.as_str())
                .body(Body::from("accepted"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp_ok.status(),
        StatusCode::CREATED,
        "If-Match list containing the current ETag must succeed"
    );
}

/// Zero-byte object: PUT 0 bytes, then Range: bytes=0-0 → 416 with Content-Range: bytes */0.
#[tokio::test]
async fn m3_zero_byte_object_range_416() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "zero-byte.bin", "").await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/knowledgebases/{KB}/zero-byte.bin"))
                .header(auth().0, auth().1)
                .header("range", "bytes=0-0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        "bytes */0",
        "Content-Range must reflect zero-byte object"
    );
}

/// Suffix range larger than object → 206 with full body (clamped to object size).
#[tokio::test]
async fn m3_suffix_range_clamped_206() {
    let a = app();
    let content = "hello world 12345 xxxxx"; // exactly 23 bytes
    let n = content.len();
    assert_eq!(
        put_text(a.clone(), KB, "suffix-clamp.bin", content).await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/knowledgebases/{KB}/suffix-clamp.bin"))
                .header(auth().0, auth().1)
                .header("range", "bytes=-9999")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        &format!("bytes 0-{}/{}", n - 1, n),
        "suffix range larger than object must be clamped to full range"
    );
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    assert_eq!(
        body.len(),
        n,
        "clamped suffix range must return the full object body"
    );
}

/// Range where start > end → 416 (semantically unsatisfiable even if parseable).
#[tokio::test]
async fn m3_range_start_gt_end() {
    let a = app();
    let body = "x".repeat(100);
    assert_eq!(
        put_text(a.clone(), KB, "range-inverted.bin", &body).await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/knowledgebases/{KB}/range-inverted.bin"))
                .header(auth().0, auth().1)
                .header("range", "bytes=50-10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::RANGE_NOT_SATISFIABLE,
        "bytes=50-10 (start > end) must return 416"
    );
    assert_eq!(
        resp.headers().get("content-range").unwrap(),
        "bytes */100"
    );
}

/// PUT ETag is consistent across GET and HEAD responses.
#[tokio::test]
async fn m3_etag_round_trip() {
    let a = app();

    let put_resp = a
        .clone()
        .oneshot(authed_request(
            "PUT",
            format!("/v1/knowledgebases/{KB}/etag-rt.md"),
            Body::from("etag round trip content"),
        ))
        .await
        .unwrap();
    assert_eq!(put_resp.status(), StatusCode::CREATED);
    let put_etag = put_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // GET ETag must match PUT ETag
    let get_resp = a
        .clone()
        .oneshot(authed_request(
            "GET",
            format!("/v1/knowledgebases/{KB}/etag-rt.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    let get_etag = get_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(get_etag, put_etag, "GET ETag must match PUT ETag");

    // HEAD ETag must also match
    let head_resp = a
        .oneshot(authed_request(
            "HEAD",
            format!("/v1/knowledgebases/{KB}/etag-rt.md"),
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(head_resp.status(), StatusCode::OK);
    let head_etag = head_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(head_etag, put_etag, "HEAD ETag must match PUT ETag");
}

/// Malformed HTTP-date in If-Unmodified-Since must not panic; any valid HTTP status is acceptable.
#[tokio::test]
async fn m3_malformed_date_if_unmodified_since() {
    let a = app();
    assert_eq!(
        put_text(a.clone(), KB, "bad-date.md", "data").await,
        StatusCode::CREATED
    );

    let resp = a
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/knowledgebases/{KB}/bad-date.md"))
                .header(auth().0, auth().1)
                .header("if-unmodified-since", "not-a-valid-http-date")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert!(
        (200..600).contains(&status),
        "malformed If-Unmodified-Since must not panic; got {status}"
    );
}

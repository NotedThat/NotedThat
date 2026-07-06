#![allow(missing_docs)]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use notedthat_api_http::router::build_router;
use notedthat_api_http::search_route::SEARCH_BODY_MAX_BYTES;
use notedthat_api_http::state::AppState;
use notedthat_api_http::testing::{InMemoryStorage, MockSearcher, NoopSearcher};
use notedthat_core::KbSlug;
use notedthat_core::search::{ObjectKey, SearchError, SearchHit, SearchResponse};
use tower::util::ServiceExt;

const TOKEN: &str = "test-token-abc";
const KB: &str = "notes";

fn declared_kbs() -> BTreeMap<String, KbSlug> {
    let mut kbs = BTreeMap::new();
    kbs.insert(KB.to_string(), KbSlug::try_new(KB).unwrap());
    kbs
}

fn make_app() -> axum::Router {
    let (indexer_tx, _rx) = tokio::sync::mpsc::channel(1024);
    let state = AppState {
        storage: Arc::new(InMemoryStorage::default()),
        declared_kbs: Arc::new(declared_kbs()),
        bearer_token: Arc::new(TOKEN.to_string()),
        max_body_size: 16 * 1024 * 1024,
        max_patchable_size: 16 * 1024 * 1024,
        indexer_tx,
        searcher: Arc::new(NoopSearcher),
    };
    build_router(state)
}

fn make_mock_app(mock: Arc<MockSearcher>) -> axum::Router {
    let (indexer_tx, _rx) = tokio::sync::mpsc::channel(1024);
    let state = AppState {
        storage: Arc::new(InMemoryStorage::default()),
        declared_kbs: Arc::new(declared_kbs()),
        bearer_token: Arc::new(TOKEN.to_string()),
        max_body_size: 16 * 1024 * 1024,
        max_patchable_size: 16 * 1024 * 1024,
        indexer_tx,
        searcher: mock,
    };
    build_router(state)
}

fn auth_header() -> (&'static str, &'static str) {
    ("authorization", "Bearer test-token-abc")
}

fn search_request(body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("/v1/knowledgebases/{KB}/search"))
        .header(auth_header().0, auth_header().1)
        .header("content-type", "application/json")
        .body(body.into())
        .unwrap()
}

async fn response_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("failed to read response body");
    serde_json::from_slice(&bytes).expect("response body is not valid JSON")
}

fn assert_json_content_type(resp: &axum::response::Response) {
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("application/json"),
        "expected application/json content-type, got {ct:?}"
    );
}

fn assert_error_envelope(json: &serde_json::Value, expected_error: &str) {
    assert_eq!(
        json["error"], expected_error,
        "wrong error code: expected {expected_error:?}, got {:?}",
        json["error"]
    );
    assert!(
        json["message"].is_string(),
        "message must be a string, got {:?}",
        json["message"]
    );
    assert!(
        json["request_id"].is_string(),
        "request_id must be a string, got {:?}",
        json["request_id"]
    );
}

#[tokio::test]
async fn test1_valid_request_returns_200_empty_hits() {
    let resp = make_app()
        .oneshot(search_request(r#"{"query":"install cargo"}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert_eq!(json["hits"], serde_json::json!([]));
}

#[tokio::test]
async fn test2_mock_searcher_returns_two_hits_with_correct_shape() {
    let mock = Arc::new(MockSearcher::new());
    mock.push_response(Ok(SearchResponse::new(vec![
        SearchHit {
            object_key: ObjectKey::try_new("docs/install.md").unwrap(),
            byte_start: 0,
            byte_end: 256,
            heading_path: vec!["Getting Started".into()],
            score: 0.92,
            preview: "Install cargo by running rustup install.".into(),
        },
        SearchHit {
            object_key: ObjectKey::try_new("docs/setup.md").unwrap(),
            byte_start: 512,
            byte_end: 768,
            heading_path: vec![],
            score: 0.75,
            preview: "Cargo setup and configuration guide.".into(),
        },
    ])));

    let resp = make_mock_app(mock)
        .oneshot(search_request(r#"{"query":"install cargo"}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    let hits = json["hits"].as_array().expect("hits must be an array");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0]["object_key"], "docs/install.md");
    assert_eq!(hits[0]["byte_start"], 0u64);
    assert_eq!(hits[0]["byte_end"], 256u64);
    assert!(hits[0]["score"].is_number());
    assert_eq!(
        hits[0]["preview"],
        "Install cargo by running rustup install."
    );
    assert_eq!(
        hits[0]["heading_path"],
        serde_json::json!(["Getting Started"])
    );
}

#[tokio::test]
async fn test3_empty_query_returns_400_invalid_request() {
    let resp = make_app()
        .oneshot(search_request(r#"{"query":""}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "invalid_request");
}

#[tokio::test]
async fn test4_whitespace_only_query_returns_400_invalid_request() {
    let resp = make_app()
        .oneshot(search_request(r#"{"query":"   "}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "invalid_request");
}

#[tokio::test]
async fn test5_limit_zero_returns_400_invalid_request() {
    let resp = make_app()
        .oneshot(search_request(r#"{"query":"x","limit":0}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "invalid_request");
}

#[tokio::test]
async fn test6_limit_999_clamped_to_50_returns_200() {
    let resp = make_app()
        .oneshot(search_request(r#"{"query":"x","limit":999}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert!(json["hits"].is_array());
}

#[tokio::test]
async fn test7_bad_slug_format_returns_400_invalid_request() {
    let resp = make_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/knowledgebases/BAD_SLUG!/search")
                .header(auth_header().0, auth_header().1)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"install cargo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "invalid_request");
}

#[tokio::test]
async fn test8_undeclared_kb_returns_404_not_found() {
    let resp = make_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/knowledgebases/unknown-kb/search")
                .header(auth_header().0, auth_header().1)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"install cargo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "not_found");
}

#[tokio::test]
async fn test9_missing_auth_returns_401_unauthorized() {
    let resp = make_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/knowledgebases/{KB}/search"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"install cargo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "unauthorized");
}

#[tokio::test]
async fn test10_wrong_bearer_token_returns_401_unauthorized() {
    let resp = make_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/knowledgebases/{KB}/search"))
                .header("authorization", "Bearer wrong-token")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"install cargo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "unauthorized");
}

#[tokio::test]
async fn test11_body_exceeds_64_kib_returns_413_payload_too_large() {
    let oversized_body =
        serde_json::json!({"query": "x".repeat(SEARCH_BODY_MAX_BYTES + 1)}).to_string();

    let resp = make_app()
        .oneshot(search_request(oversized_body))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "payload_too_large");
}

#[tokio::test]
async fn test12_missing_content_type_returns_400_invalid_request() {
    let resp = make_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/knowledgebases/{KB}/search"))
                .header(auth_header().0, auth_header().1)
                .body(Body::from(r#"{"query":"install cargo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "invalid_request");
}

#[tokio::test]
async fn test13_malformed_json_returns_400_invalid_request() {
    let resp = make_app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/knowledgebases/{KB}/search"))
                .header(auth_header().0, auth_header().1)
                .header("content-type", "application/json")
                .body(Body::from("{not-valid-json"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "invalid_request");
}

#[tokio::test]
async fn test14_unknown_fields_silently_ignored_returns_200() {
    let resp = make_app()
        .oneshot(search_request(r#"{"query":"x","extra":"ignored"}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = response_json(resp).await;
    assert!(json["hits"].is_array());
}

#[tokio::test]
async fn test15_backend_unavailable_returns_503() {
    let mock = Arc::new(MockSearcher::new());
    mock.push_response(Err(SearchError::BackendUnavailable {
        message: "qdrant is down".into(),
    }));

    let resp = make_mock_app(mock)
        .oneshot(search_request(r#"{"query":"install cargo"}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "backend_unavailable");
}

#[tokio::test]
async fn test16_mock_unknown_kb_returns_404_not_found() {
    let mock = Arc::new(MockSearcher::new());
    mock.push_response(Err(SearchError::UnknownKb { slug: KB.into() }));

    let resp = make_mock_app(mock)
        .oneshot(search_request(r#"{"query":"install cargo"}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "not_found");
}

#[tokio::test]
async fn test17_internal_search_error_returns_500() {
    let mock = Arc::new(MockSearcher::new());
    mock.push_response(Err(SearchError::Internal {
        message: "unexpected state in search subsystem".into(),
    }));

    let resp = make_mock_app(mock)
        .oneshot(search_request(r#"{"query":"install cargo"}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_json_content_type(&resp);
    let json = response_json(resp).await;
    assert_error_envelope(&json, "internal_error");
}

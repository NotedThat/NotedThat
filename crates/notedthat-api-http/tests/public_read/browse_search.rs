use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedthat_api_http::testing::MockSearcher;
use notedthat_core::PublicReadCapability;
use notedthat_core::search::{ObjectKey, SearchHit, SearchResponse};
use tower::ServiceExt;

use super::fixture::{TOKEN, app, app_with_searcher, json, policy};

#[tokio::test]
async fn anonymous_browse_continues_past_internal_pages_with_public_cursor_semantics() {
    // Given
    let app = app(BTreeMap::from([(
        "notes".to_string(),
        policy([PublicReadCapability::Browse]),
    )]))
    .await;

    // When
    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes?limit=1")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(first.status(), StatusCode::OK);
    let first_json = json(first).await;
    assert_eq!(first_json["objects"][0]["key"], "public.md");
    assert_eq!(first_json["truncated"], true);
    assert_eq!(first_json["next_cursor"], "public.md");

    // When
    let second = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes?limit=1&cursor=public.md")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let content = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/public.md")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let internal_prefix = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes?prefix=.notedthat%2F")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let authenticated = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    let second_json = json(second).await;
    assert_eq!(second_json["objects"][0]["key"], "z-last.md");
    assert_eq!(second_json["truncated"], false);
    assert!(second_json["next_cursor"].is_null());
    assert_eq!(content.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        json(internal_prefix).await["objects"],
        serde_json::json!([])
    );
    let authenticated_json = json(authenticated).await;
    assert!(
        authenticated_json["objects"]
            .as_array()
            .is_some_and(|objects| objects
                .iter()
                .any(|object| { object["key"] == ".notedthat/manifest.json" }))
    );
}

#[tokio::test]
async fn anonymous_search_filters_internal_hits_without_granting_other_reads() {
    // Given
    let searcher = Arc::new(MockSearcher::new());
    searcher.push_response(Ok(SearchResponse::new(vec![
        SearchHit {
            object_key: ObjectKey::try_new(".notedthat/manifest.json").expect("valid key"),
            byte_start: 0,
            byte_end: 6,
            heading_path: Vec::new(),
            score: 1.0,
            preview: "secret".into(),
            okf: None,
        },
        SearchHit {
            object_key: ObjectKey::try_new("public.md").expect("valid key"),
            byte_start: 0,
            byte_end: 6,
            heading_path: Vec::new(),
            score: 0.5,
            preview: "public".into(),
            okf: None,
        },
    ])));
    let app = app_with_searcher(
        BTreeMap::from([("notes".to_string(), policy([PublicReadCapability::Search]))]),
        searcher,
    )
    .await;

    // When
    let search = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/knowledgebases/notes/search")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"public"}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    let browse = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(search.status(), StatusCode::OK);
    let search_json = json(search).await;
    assert_eq!(search_json["hits"].as_array().map(Vec::len), Some(1));
    assert_eq!(search_json["hits"][0]["object_key"], "public.md");
    assert_eq!(browse.status(), StatusCode::UNAUTHORIZED);
}

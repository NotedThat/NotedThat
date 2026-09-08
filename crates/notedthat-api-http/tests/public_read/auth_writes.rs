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
async fn absent_credentials_can_fall_back_but_supplied_bad_credentials_cannot() {
    // Given
    let app = app(BTreeMap::from([(
        "notes".to_string(),
        policy([PublicReadCapability::Content]),
    )]))
    .await;

    for authorization in ["Basic abc", "Bearer wrong", "Bearer "] {
        // When
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/knowledgebases/notes/public.md")
                    .header("authorization", authorization)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        // Then
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response_json = json(response).await;
        assert_eq!(response_json["error"], "unauthorized");
    }

    // When
    let mut non_utf8 = Request::builder()
        .uri("/api/v1/knowledgebases/notes/public.md")
        .body(Body::empty())
        .expect("request");
    non_utf8.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_bytes(&[0xff]).expect("opaque header value"),
    );
    let non_utf8_response = app.clone().oneshot(non_utf8).await.expect("response");
    let mut duplicated = Request::builder()
        .uri("/api/v1/knowledgebases/notes/public.md")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .expect("request");
    duplicated.headers_mut().append(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer wrong"),
    );
    let duplicated_response = app.oneshot(duplicated).await.expect("response");

    // Then
    assert_eq!(non_utf8_response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(duplicated_response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn anonymous_mutation_remains_unauthorized_and_storage_is_unchanged() {
    // Given
    let app = app(BTreeMap::from([(
        "notes".to_string(),
        policy([
            PublicReadCapability::Discover,
            PublicReadCapability::Browse,
            PublicReadCapability::Content,
            PublicReadCapability::Search,
        ]),
    )]))
    .await;

    // When
    for method in ["PUT", "PATCH", "POST", "DELETE"] {
        let write = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri("/api/v1/knowledgebases/notes/new.md")
                    .body(Body::from("new body"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(write.status(), StatusCode::UNAUTHORIZED, "method {method}");
    }
    let read = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes/new.md")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(read.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn denied_anonymous_search_does_not_call_backend() {
    // Given
    let searcher = Arc::new(MockSearcher::new());
    searcher.push_response(Ok(SearchResponse::new(vec![SearchHit {
        object_key: ObjectKey::try_new("public.md").expect("valid key"),
        byte_start: 0,
        byte_end: 6,
        heading_path: Vec::new(),
        score: 1.0,
        preview: "queued response".into(),
        okf: None,
    }])));
    let app = app_with_searcher(
        BTreeMap::from([("notes".to_string(), policy([PublicReadCapability::Browse]))]),
        searcher,
    )
    .await;
    let request = |authenticated: bool| {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/v1/knowledgebases/notes/search")
            .header("content-type", "application/json");
        if authenticated {
            builder = builder.header("authorization", format!("Bearer {TOKEN}"));
        }
        builder
            .body(Body::from(r#"{"query":"public"}"#))
            .expect("request")
    };

    // When
    let denied = app.clone().oneshot(request(false)).await.expect("response");
    let authenticated = app.oneshot(request(true)).await.expect("response");

    // Then
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(authenticated.status(), StatusCode::OK);
    assert_eq!(
        json(authenticated).await["hits"][0]["preview"],
        "queued response"
    );
}

use std::collections::BTreeMap;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use notedthat_api_http::middleware::auth_context;
use notedthat_core::PublicReadCapability;
use tower::ServiceExt;

use super::fixture::{app, policy};

#[test]
fn missing_auth_context_is_anonymous() {
    let request = Request::new(Body::empty());
    assert!(auth_context(&request).is_anonymous());
}

#[tokio::test]
async fn browse_head_follows_get_including_encoded_slug() {
    for slug in ["notes", "%6Eotes"] {
        for method in ["GET", "HEAD"] {
            let app = app(BTreeMap::from([(
                "notes".into(),
                policy([PublicReadCapability::Browse]),
            )]))
            .await;
            let response = app
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!("/api/v1/knowledgebases/{slug}"))
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK, "{method} {slug}");
        }
    }
}

#[tokio::test]
async fn discovery_requires_opt_in_by_a_declared_kb() {
    for policies in [
        BTreeMap::new(),
        BTreeMap::from([("notes".into(), policy([PublicReadCapability::Browse]))]),
        BTreeMap::from([(
            "undeclared".into(),
            policy([PublicReadCapability::Discover]),
        )]),
    ] {
        for method in ["GET", "HEAD"] {
            let response = app(policies.clone())
                .await
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/api/v1/knowledgebases")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }
}

#[tokio::test]
async fn discovery_head_is_public_when_discover_is_granted() {
    let response = app(BTreeMap::from([(
        "notes".into(),
        policy([PublicReadCapability::Discover]),
    )]))
    .await
    .oneshot(
        Request::builder()
            .method("HEAD")
            .uri("/api/v1/knowledgebases")
            .body(Body::empty())
            .expect("request"),
    )
    .await
    .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
}

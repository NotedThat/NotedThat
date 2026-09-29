use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use tower::ServiceExt;

use super::fixture::{app, app_with_searcher, body, get, grant, grant_under, hrefs, policy};
use notedthat_api_http::testing::MockSearcher;
use notedthat_core::search::{ObjectKey, SearchHit, SearchResponse};
use notedthat_core::{AccessRule, KeyPattern, Verb, Who};
use std::sync::Arc;

fn searcher_returning(key: &str) -> Arc<MockSearcher> {
    let searcher = Arc::new(MockSearcher::default());
    searcher.push_response(Ok(SearchResponse::new(vec![SearchHit {
        object_key: ObjectKey::try_new(key).expect("valid key"),
        byte_start: 0,
        byte_end: 7,
        heading_path: Vec::new(),
        score: 1.0,
        preview: "preview".to_string(),
        okf: None,
    }])));
    searcher
}

#[tokio::test]
async fn search_requires_search_but_not_list_and_never_renders_a_listing() {
    let app = app(policy([grant(Who::Anyone, [Verb::Search])])).await;

    let response = get(&app, "/browse/notes/missing/?q=needle", None).await;

    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(html.contains("<form method=\"get\""), "{html}");
    assert!(html.contains("0 results"), "{html}");
    assert!(!html.contains("<table>"), "{html}");
}

#[tokio::test]
async fn invalid_search_parameters_are_request_id_bearing_html_400s() {
    let app = app(policy([grant(Who::Anyone, [Verb::Search])])).await;

    for uri in [
        "/browse/notes/?q=",
        "/browse/notes/?q=needle&q=again",
        "/browse/notes/?q=needle&limit=0",
        "/browse/notes/?q=needle&limit=51",
        "/browse/notes/?q=needle&unknown=value",
    ] {
        let response = get(&app, uri, None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
        let html = body(response).await;
        assert!(html.contains("Request "), "{uri}: {html}");
    }
}

#[tokio::test]
async fn browse_search_headers_and_head_match_get() {
    let app = app(policy([grant(Who::Anyone, [Verb::Search])])).await;
    let uri = "/browse/notes/?q=needle&limit=10";
    let get_response = get(&app, uri, None).await;
    assert_eq!(get_response.status(), StatusCode::OK);
    assert_eq!(get_response.headers()["referrer-policy"], "no-referrer");
    assert!(
        get_response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("form-action 'self'")
    );

    let head_response = app
        .oneshot(
            Request::builder()
                .method(Method::HEAD)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head_response.status(), StatusCode::OK);
    assert_eq!(head_response.headers()["referrer-policy"], "no-referrer");
    assert!(body(head_response).await.is_empty());
}

#[tokio::test]
async fn search_links_to_a_listable_folder_when_its_hit_is_denied() {
    let searcher = searcher_returning("public/drafts/note.md");
    let app = app_with_searcher(
        policy([
            grant(Who::Anyone, [Verb::Search]),
            grant_under(Who::Anyone, [Verb::List], &["public/**"]),
            AccessRule::deny(Who::Anyone, [Verb::List])
                .under([KeyPattern::parse("public/drafts/note.md").expect("valid pattern")]),
        ]),
        searcher as Arc<dyn notedthat_indexer::Searcher>,
    )
    .await;

    let html = body(get(&app, "/browse/notes/?q=needle", None).await).await;

    assert!(
        hrefs(&html)
            .iter()
            .any(|href| href == "/browse/notes/public/drafts/"),
        "{html}"
    );
}

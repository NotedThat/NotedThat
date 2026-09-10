use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use notedthat_api_http::testing::MockSearcher;
use notedthat_core::search::{ObjectKey, SearchHit, SearchResponse};
use notedthat_core::{AccessPolicy, Principal, Verb};
use tower::ServiceExt;

use super::fixture::{app_with_searcher, grant, grant_under, json, policy};

fn notes(
    rules: impl IntoIterator<Item = notedthat_core::AccessRule>,
) -> BTreeMap<String, AccessPolicy> {
    BTreeMap::from([("notes".to_string(), policy(rules))])
}

fn hit(key: &str) -> SearchHit {
    SearchHit {
        object_key: ObjectKey::try_new(key).expect("valid key"),
        byte_start: 0,
        byte_end: 7,
        heading_path: Vec::new(),
        score: 1.0,
        preview: "preview".to_string(),
        okf: None,
    }
}

fn searcher_returning(keys: &[&str]) -> Arc<MockSearcher> {
    let searcher = Arc::new(MockSearcher::default());
    searcher.set_response(Ok(SearchResponse::new(
        keys.iter().map(|key| hit(key)).collect(),
    )));
    searcher
}

async fn search(app: axum::Router) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/api/v1/knowledgebases/notes/search")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"query":"anything"}"#))
            .expect("request"),
    )
    .await
    .expect("response")
}

#[tokio::test]
async fn hits_are_filtered_by_the_search_patterns_and_not_by_the_read_patterns() {
    // Given — search over the whole base, read confined to `public/`. The two
    // are independently grantable, and search is what governs hits.
    let searcher = searcher_returning(&["public/index.md", "internal/secret.md"]);
    let app = app_with_searcher(
        notes([
            grant(Principal::Anyone, [Verb::Search]),
            grant_under(Principal::Anyone, [Verb::Read], &["public/**"]),
        ]),
        searcher as Arc<dyn notedthat_indexer::Searcher>,
    )
    .await;

    // When
    let response = search(app).await;

    // Then — a key the caller cannot GET still appears, with its preview. That
    // is the documented consequence of granting broad search with narrow read.
    assert_eq!(response.status(), StatusCode::OK);
    let hits = json(response).await;
    let keys: Vec<&str> = hits["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|hit| hit["object_key"].as_str().expect("key"))
        .collect();
    assert_eq!(keys, vec!["public/index.md", "internal/secret.md"]);
}

#[tokio::test]
async fn a_prefix_scoped_search_grant_withholds_hits_outside_its_scope() {
    // Given
    let searcher = searcher_returning(&["public/index.md", "internal/secret.md"]);
    let app = app_with_searcher(
        notes([grant_under(
            Principal::Anyone,
            [Verb::Search],
            &["public/**"],
        )]),
        searcher as Arc<dyn notedthat_indexer::Searcher>,
    )
    .await;

    // When
    let response = search(app).await;

    // Then
    assert_eq!(response.status(), StatusCode::OK);
    let keys: Vec<String> = json(response).await["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|hit| hit["object_key"].as_str().expect("key").to_string())
        .collect();
    assert_eq!(keys, vec!["public/index.md".to_string()]);
}

#[tokio::test]
async fn a_denied_search_never_reaches_the_searcher() {
    // Given — `read` without `search`. Refusing before the searcher matters
    // operationally: an unauthorized search must not cost an embedding call.
    let searcher = searcher_returning(&["public/index.md"]);
    let app = app_with_searcher(
        notes([grant(Principal::Anyone, [Verb::Read])]),
        Arc::clone(&searcher) as Arc<dyn notedthat_indexer::Searcher>,
    )
    .await;

    // When
    let response = search(app).await;

    // Then — `404` for an anonymous denial, so the status does not reveal that
    // this knowledge base is declared.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        searcher.call_count(),
        0,
        "the backend must not be consulted for a request that is refused"
    );
}

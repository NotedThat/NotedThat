use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use notedthat_api_http::router::build_router;
use notedthat_api_http::state::AppState;
use notedthat_api_http::testing::{InMemoryStorage, NoopSearcher};
use notedthat_core::{
    AccessPolicy, ConditionalHeaders, KbSlug, ObjectPath, Principal, Storage, Verb,
};
use tower::ServiceExt;

use super::fixture::{grant_under, json, listed_keys, policy};

/// A knowledge base of `count` objects, alternating between two prefixes so a
/// scoped grant has to skip roughly half of everything it scans.
async fn interleaved_app(count: usize, policy: AccessPolicy) -> axum::Router {
    let notes = KbSlug::try_new("notes").expect("valid slug");
    let storage = Arc::new(InMemoryStorage::default());
    for index in 0..count {
        let prefix = if index % 2 == 0 { "public" } else { "internal" };
        let key = format!("{prefix}/{index:04}.md");
        storage
            .put_object(
                &notes,
                &ObjectPath::try_from(key.as_str()).expect("valid path"),
                Bytes::from("body"),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .expect("seed");
    }

    let (indexer_tx, _rx) = tokio::sync::mpsc::channel(16);
    build_router(AppState {
        storage,
        declared_kbs: Arc::new(BTreeMap::from([("notes".to_string(), notes)])),
        access_policies: Arc::new(BTreeMap::from([("notes".to_string(), Arc::new(policy))])),
        bearer_token: Arc::new("token".to_string()),
        max_body_size: 16 * 1024 * 1024,
        max_patchable_size: 16 * 1024 * 1024,
        indexer_tx,
        searcher: Arc::new(NoopSearcher),
    })
}

fn public_list_grant() -> AccessPolicy {
    policy([grant_under(Principal::Anyone, [Verb::List], &["public/**"])])
}

/// Page through a listing at `limit`, following `next_cursor` to the end.
///
/// Returns every key seen, in order, plus the number of requests it took.
async fn page_through(app: &axum::Router, limit: u32) -> (Vec<String>, usize) {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    let mut requests = 0;

    loop {
        let uri = match &cursor {
            Some(cursor) => format!(
                "/api/v1/knowledgebases/notes?limit={limit}&cursor={}",
                urlencode(cursor)
            ),
            None => format!("/api/v1/knowledgebases/notes?limit={limit}"),
        };
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        requests += 1;
        assert!(requests < 500, "pagination did not terminate");

        let body = json(response).await;
        for object in body["objects"].as_array().expect("objects") {
            all.push(object["key"].as_str().expect("key").to_string());
        }

        // The invariant that survives filtering: page off the cursor, never off
        // the page length. A short page with a cursor is normal now.
        let Some(next) = body["next_cursor"].as_str() else {
            assert_eq!(
                body["truncated"],
                serde_json::json!(false),
                "no cursor must mean not truncated"
            );
            break;
        };
        cursor = Some(next.to_string());
    }

    (all, requests)
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// The test that catches the mid-page cursor bug.
///
/// When a backend page fills the API page with rows to spare, handing back that
/// page's cursor would skip every surplus row. Nothing but a full walk compared
/// against the truth detects it.
#[tokio::test]
async fn paging_through_a_filtered_listing_yields_every_key_exactly_once() {
    // Given — 250 objects, half of them outside the grant.
    let app = interleaved_app(250, public_list_grant()).await;
    let expected: Vec<String> = (0..250)
        .filter(|index| index % 2 == 0)
        .map(|index| format!("public/{index:04}.md"))
        .collect();

    // When
    let (seen, _requests) = page_through(&app, 10).await;

    // Then
    assert_eq!(
        seen, expected,
        "paging must lose nothing and duplicate nothing"
    );
}

#[tokio::test]
async fn paging_at_several_page_sizes_agrees_with_a_single_large_page() {
    // Given
    let app = interleaved_app(120, public_list_grant()).await;
    let (whole, _) = page_through(&app, 1000).await;

    // When / Then
    for limit in [1, 3, 7, 60] {
        let (paged, _) = page_through(&app, limit).await;
        assert_eq!(paged, whole, "limit={limit} disagreed with a single page");
    }
}

#[tokio::test]
async fn an_unfiltered_listing_takes_the_fast_path_and_matches_the_filtered_walk() {
    // Given — the credential holder with a whole-knowledge-base grant needs no
    // per-key work, so this is the pre-D51 code path. It must still agree.
    let app = interleaved_app(40, AccessPolicy::signed_in_full()).await;

    // When
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes?limit=1000")
                .header("authorization", "Bearer token")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(listed_keys(response).await.len(), 40);
}

#[tokio::test]
async fn a_page_may_come_back_short_while_still_reporting_a_cursor() {
    // Given — a grant matching only the very last keys, so early pages scan a
    // lot and return little. That is the scan budget working, not a bug, and a
    // client that stops on a short page would under-read.
    let app = interleaved_app(60, public_list_grant()).await;

    // When
    let (seen, requests) = page_through(&app, 5).await;

    // Then
    assert_eq!(seen.len(), 30);
    assert!(
        requests >= 6,
        "30 keys at 5 per page needs at least 6 requests, took {requests}"
    );
}

/// A knowledge base of `count` objects under one prefix, entirely outside the
/// grant, so a disjoint request has a whole scan budget to burn if it is not
/// recognised as disjoint.
async fn one_prefix_app(count: usize, prefix: &str, policy: AccessPolicy) -> axum::Router {
    let notes = KbSlug::try_new("notes").expect("valid slug");
    let storage = Arc::new(InMemoryStorage::default());
    for index in 0..count {
        let key = format!("{prefix}/{index:06}.md");
        storage
            .put_object(
                &notes,
                &ObjectPath::try_from(key.as_str()).expect("valid path"),
                Bytes::from("body"),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .expect("seed");
    }

    let (indexer_tx, _rx) = tokio::sync::mpsc::channel(16);
    build_router(AppState {
        storage,
        declared_kbs: Arc::new(BTreeMap::from([("notes".to_string(), notes)])),
        access_policies: Arc::new(BTreeMap::from([("notes".to_string(), Arc::new(policy))])),
        bearer_token: Arc::new("token".to_string()),
        max_body_size: 16 * 1024 * 1024,
        max_patchable_size: 16 * 1024 * 1024,
        indexer_tx,
        searcher: Arc::new(NoopSearcher),
    })
}

#[tokio::test]
async fn a_prefix_that_only_shares_text_with_the_grant_is_disjoint_not_scanned() {
    // Given — `public/**` granted, and a sibling prefix that shares the *text*
    // `public` but not the segment. Seeded past the scan budget
    // (`LIST_SCAN_MAX_CALLS` × `LIST_SCAN_PAGE`) so an unrecognised disjoint
    // request would spend all of it.
    let app = one_prefix_app(20_001, "public-internal", public_list_grant()).await;

    // When
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/knowledgebases/notes?prefix=public-internal/")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    // Then — no key under `public-internal/` can satisfy `public/**`, so the
    // answer is a complete empty page. `truncated: true` with a live cursor here
    // would send a client walking the whole knowledge base to receive nothing.
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(body["objects"].as_array().expect("objects").len(), 0);
    assert_eq!(body["truncated"], serde_json::json!(false));
    assert_eq!(body["next_cursor"], serde_json::Value::Null);
}

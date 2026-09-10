use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use notedthat_api_http::router::build_router;
use notedthat_api_http::state::AppState;
use notedthat_api_http::testing::{InMemoryStorage, NoopSearcher};
use notedthat_core::{
    AccessPolicy, AccessRule, ConditionalHeaders, KbSlug, KeyPattern, ObjectPath, Principal,
    Storage, Verb,
};
use tower::ServiceExt;

pub(super) const TOKEN: &str = "test-token-abc";

/// The seeded tree, chosen so every interesting case has a fixture:
///
/// - `public/` is a prefix an anonymous grant can be scoped to, with a nested
///   folder under it so parent links and depth are exercisable;
/// - `internal/` sits outside that scope;
/// - the names carry a space, HTML metacharacters and a right-to-left override;
/// - `collide` exists as both a folder and an object;
/// - `.notedthat/` must never appear.
pub(super) const SEEDED: &[&str] = &[
    "public/index.md",
    "public/a b.md",
    "public/a & b <c>.md",
    "public/report\u{202E}fdp.exe",
    "public/drafts/note.md",
    "public/drafts/deep/buried.md",
    "public/collide",
    "public/collide/inner.md",
    "internal/secret.md",
    ".notedthat/manifest.json",
];

pub(super) fn policy(rules: impl IntoIterator<Item = AccessRule>) -> AccessPolicy {
    rules.into_iter().collect()
}

pub(super) fn grant(who: Principal, verbs: impl IntoIterator<Item = Verb>) -> AccessRule {
    AccessRule::new(who, verbs)
}

pub(super) fn grant_under(
    who: Principal,
    verbs: impl IntoIterator<Item = Verb>,
    patterns: &[&str],
) -> AccessRule {
    AccessRule::new(who, verbs).under(
        patterns
            .iter()
            .map(|source| KeyPattern::parse(source).expect("valid pattern")),
    )
}

/// A router over the seeded tree. `notes` takes `policy`; `private` gets nothing.
pub(super) async fn app(policy: AccessPolicy) -> axum::Router {
    app_with_keys(policy, SEEDED).await
}

pub(super) async fn app_with_keys(policy: AccessPolicy, keys: &[&str]) -> axum::Router {
    let notes = KbSlug::try_new("notes").expect("valid slug");
    let private = KbSlug::try_new("private").expect("valid slug");
    let storage = Arc::new(InMemoryStorage::default());

    for key in keys {
        storage
            .put_object(
                &notes,
                &ObjectPath::try_from(*key).expect("valid path"),
                Bytes::from(format!("body of {key}")),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .unwrap_or_else(|error| panic!("seed {key}: {error}"));
    }

    let (indexer_tx, _rx) = tokio::sync::mpsc::channel(16);
    build_router(AppState {
        storage,
        declared_kbs: Arc::new(BTreeMap::from([
            ("notes".to_string(), notes),
            ("private".to_string(), private),
        ])),
        access_policies: Arc::new(BTreeMap::from([("notes".to_string(), Arc::new(policy))])),
        bearer_token: Arc::new(TOKEN.to_string()),
        max_body_size: 16 * 1024 * 1024,
        max_patchable_size: 16 * 1024 * 1024,
        indexer_tx,
        searcher: Arc::new(NoopSearcher),
    })
}

/// `GET uri`, anonymously unless `token` is given.
pub(super) async fn get(
    app: &axum::Router,
    uri: &str,
    token: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder().uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    app.clone()
        .oneshot(builder.body(Body::empty()).expect("request"))
        .await
        .expect("response")
}

/// Fetch a page, asserting it rendered, and return its HTML.
pub(super) async fn page(app: &axum::Router, uri: &str, token: Option<&str>) -> String {
    let response = get(app, uri, token).await;
    assert_eq!(response.status(), StatusCode::OK, "GET {uri}");
    body(response).await
}

pub(super) async fn body(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("read body");
    String::from_utf8(bytes.to_vec()).expect("UTF-8 page")
}

/// Every `href` in a page, in document order.
pub(super) fn hrefs(html: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("href=\"") {
        rest = &rest[start + 6..];
        let Some(end) = rest.find('"') else { break };
        found.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    found
}

/// The `href` of the `../` parent row.
///
/// Not simply the first link on the page: the breadcrumb comes first in the
/// document and its own links point at ancestors too.
pub(super) fn parent_link(html: &str) -> String {
    let row = html.find("<tr class=\"up\">").expect("a parent row");
    hrefs(&html[row..]).first().cloned().expect("a parent link")
}

/// The `Location` header of a redirect response.
pub(super) fn location(response: &axum::response::Response) -> String {
    response
        .headers()
        .get("location")
        .expect("Location header")
        .to_str()
        .expect("ASCII location")
        .to_string()
}

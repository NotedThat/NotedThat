use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::to_bytes;
use bytes::Bytes;
use notedthat_api_http::router::build_router;
use notedthat_api_http::state::AppState;
use notedthat_api_http::testing::{InMemoryStorage, NoopSearcher};
use notedthat_core::testing::StubTokenVerifier;
use notedthat_core::{
    AccessPolicy, AccessRule, Authenticator, ConditionalHeaders, KbSlug, KeyPattern, ObjectPath,
    Storage, Verb, Who,
};
use notedthat_indexer::Searcher;

pub(super) const TOKEN: &str = "test-token-abc";

/// A bearer the stub verifier resolves to `alice`, a member of `editors`.
pub(super) const ALICE_TOKEN: &str = "jwt-alice";
/// A bearer the stub verifier resolves to `bob`, in no group at all.
pub(super) const BOB_TOKEN: &str = "jwt-bob";

/// The credential rules every fixture app uses: the service token, plus a
/// verifier that vouches for [`ALICE_TOKEN`] and [`BOB_TOKEN`].
pub(super) fn authenticator() -> Authenticator {
    Authenticator::new(TOKEN).with_token_verifier(Arc::new(
        StubTokenVerifier::default()
            .accepting(ALICE_TOKEN, "alice", ["editors"])
            .accepting(BOB_TOKEN, "bob", []),
    ))
}

/// Build a policy from rules.
pub(super) fn policy(rules: impl IntoIterator<Item = AccessRule>) -> AccessPolicy {
    rules.into_iter().collect()
}

/// Grant `verbs` to `who` across the whole knowledge base.
pub(super) fn grant(who: Who, verbs: impl IntoIterator<Item = Verb>) -> AccessRule {
    AccessRule::new(who, verbs)
}

/// Grant `verbs` to `who`, scoped to `patterns`.
pub(super) fn grant_under(
    who: Who,
    verbs: impl IntoIterator<Item = Verb>,
    patterns: &[&str],
) -> AccessRule {
    AccessRule::new(who, verbs).under(
        patterns
            .iter()
            .map(|source| KeyPattern::parse(source).expect("valid pattern")),
    )
}

/// Every verb the credential holder needs to behave as it did before D51.
pub(super) fn signed_in_everything() -> AccessRule {
    AccessRule::new(Who::SignedIn, Verb::ALL)
}

/// The seeded knowledge base `notes`, plus an entirely private `private`.
///
/// The tree deliberately spans the interesting cases: a prefix an anonymous
/// grant can be scoped to, a sibling prefix outside it, the internal namespace,
/// and a key that sorts last.
pub(super) async fn app(policies: BTreeMap<String, AccessPolicy>) -> axum::Router {
    app_with(policies, Arc::new(NoopSearcher), authenticator()).await
}

pub(super) async fn app_with_searcher(
    policies: BTreeMap<String, AccessPolicy>,
    searcher: Arc<dyn Searcher>,
) -> axum::Router {
    app_with(policies, searcher, authenticator()).await
}

pub(super) async fn app_with_authenticator(
    policies: BTreeMap<String, AccessPolicy>,
    authenticator: Authenticator,
) -> axum::Router {
    app_with(policies, Arc::new(NoopSearcher), authenticator).await
}

async fn app_with(
    policies: BTreeMap<String, AccessPolicy>,
    searcher: Arc<dyn Searcher>,
    authenticator: Authenticator,
) -> axum::Router {
    let notes = KbSlug::try_new("notes").expect("valid slug");
    let private = KbSlug::try_new("private").expect("valid slug");
    let storage = Arc::new(InMemoryStorage::default());

    for (key, body, content_type) in [
        ("public.md", "public body", "text/markdown"),
        ("public/index.md", "public index", "text/markdown"),
        ("public/deep/note.md", "deep note", "text/markdown"),
        ("internal/secret.md", "secret", "text/markdown"),
        (".notedthat/manifest.json", "secret", "application/json"),
        ("z-last.md", "last", "text/markdown"),
    ] {
        storage
            .put_object(
                &notes,
                &ObjectPath::try_from(key).expect("valid path"),
                Bytes::from(body),
                Some(content_type),
                ConditionalHeaders::default(),
            )
            .await
            .unwrap_or_else(|error| panic!("seed {key}: {error}"));
    }

    let declared_kbs = BTreeMap::from([
        ("notes".to_string(), notes),
        ("private".to_string(), private),
    ]);
    let (indexer_tx, _indexer_rx) = tokio::sync::mpsc::channel(16);
    build_router(AppState {
        storage,
        declared_kbs: Arc::new(declared_kbs),
        access_policies: Arc::new(
            policies
                .into_iter()
                .map(|(slug, policy)| (slug, Arc::new(policy)))
                .collect(),
        ),
        authenticator: Arc::new(authenticator),
        max_body_size: 16 * 1024 * 1024,
        max_patchable_size: 16 * 1024 * 1024,
        indexer_tx,
        searcher,
        events: None,
    })
}

pub(super) async fn json(response: axum::response::Response) -> serde_json::Value {
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read response");
    serde_json::from_slice(&body).expect("JSON response")
}

/// The object keys a listing response reports, in order.
pub(super) async fn listed_keys(response: axum::response::Response) -> Vec<String> {
    json(response).await["objects"]
        .as_array()
        .expect("objects array")
        .iter()
        .map(|object| object["key"].as_str().expect("key").to_string())
        .collect()
}

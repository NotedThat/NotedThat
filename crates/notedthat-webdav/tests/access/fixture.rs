use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::Request;
use notedthat_core::{AccessPolicy, AccessRule, KbSlug, KeyPattern, Principal, Verb};
use notedthat_webdav::state::WebDavState;
use tokio::sync::mpsc;

use super::storage::MemoryStorage;

/// Build a policy granting `verbs` to `who` across the whole knowledge base.
///
/// The old fixture took capability names; verbs replaced them, and the mapping
/// is not one-to-one — `discover` is gone entirely, and `browse` split into
/// `list` for enumeration and `read` for bytes.
pub(super) fn policy(who: Principal, verbs: &[Verb]) -> AccessPolicy {
    [AccessRule::new(who, verbs.iter().copied())]
        .into_iter()
        .collect()
}

/// A policy granting `verbs` to `who`, scoped to `patterns`.
pub(super) fn scoped_policy(who: Principal, verbs: &[Verb], patterns: &[&str]) -> AccessPolicy {
    [AccessRule::new(who, verbs.iter().copied()).under(
        patterns
            .iter()
            .map(|source| KeyPattern::parse(source).expect("valid pattern")),
    )]
    .into_iter()
    .collect()
}

pub(super) fn state_with_policies(
    storage: Arc<MemoryStorage>,
    policies: BTreeMap<String, AccessPolicy>,
) -> WebDavState {
    let (indexer_tx, _indexer_rx) = mpsc::channel(8);
    WebDavState {
        username: Arc::new("user".to_string()),
        password: Arc::new("pass".to_string()),
        storage,
        declared_kbs: Arc::new(BTreeMap::from([
            (
                "discoverable".to_string(),
                KbSlug::try_new("discoverable").expect("valid KB slug"),
            ),
            (
                "private".to_string(),
                KbSlug::try_new("private").expect("valid KB slug"),
            ),
        ])),
        access_policies: Arc::new(
            policies
                .into_iter()
                .map(|(slug, policy)| (slug, Arc::new(policy)))
                .collect(),
        ),
        indexer_tx,
        staging_config: notedthat_core::StagingConfig::default(),
    }
}

pub(super) fn request(method: &str, uri: &str) -> Request<Body> {
    let scoped_uri = format!("/webdav{uri}");
    Request::builder()
        .method(method)
        .uri(scoped_uri)
        .body(if method == "PROPFIND" {
            Body::from(PROPFIND_BODY)
        } else {
            Body::empty()
        })
        .expect("valid request")
}

pub(super) async fn response_body(response: axum::response::Response) -> String {
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    String::from_utf8(body.to_vec()).expect("UTF-8 response")
}

pub(super) const PROPFIND_BODY: &str =
    r#"<?xml version="1.0"?><D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>"#;

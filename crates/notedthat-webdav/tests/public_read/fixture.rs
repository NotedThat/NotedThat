use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::Request;
use notedthat_core::{KbSlug, PublicReadCapability, PublicReadPolicy};
use notedthat_webdav::state::WebDavState;
use tokio::sync::mpsc;

use super::storage::MemoryStorage;

pub(super) fn policy(capabilities: &[&str]) -> PublicReadPolicy {
    capabilities
        .iter()
        .map(|capability| match *capability {
            "discover" => PublicReadCapability::Discover,
            "browse" => PublicReadCapability::Browse,
            "content" => PublicReadCapability::Content,
            "search" => PublicReadCapability::Search,
            unexpected => panic!("unexpected test capability: {unexpected}"),
        })
        .collect()
}

pub(super) fn state_with_policies(
    storage: Arc<MemoryStorage>,
    policies: BTreeMap<String, PublicReadPolicy>,
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
        public_read_policies: Arc::new(policies),
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

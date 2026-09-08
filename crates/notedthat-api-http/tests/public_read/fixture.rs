use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::to_bytes;
use bytes::Bytes;
use notedthat_api_http::router::build_router;
use notedthat_api_http::state::AppState;
use notedthat_api_http::testing::{InMemoryStorage, NoopSearcher};
use notedthat_core::{
    ConditionalHeaders, KbSlug, ObjectPath, PublicReadCapability, PublicReadPolicy, Storage,
};
use notedthat_indexer::Searcher;

pub(super) const TOKEN: &str = "test-token-abc";

pub(super) fn policy(
    capabilities: impl IntoIterator<Item = PublicReadCapability>,
) -> PublicReadPolicy {
    capabilities.into_iter().collect()
}

pub(super) async fn app(policies: BTreeMap<String, PublicReadPolicy>) -> axum::Router {
    app_with_searcher(policies, Arc::new(NoopSearcher)).await
}

pub(super) async fn app_with_searcher(
    policies: BTreeMap<String, PublicReadPolicy>,
    searcher: Arc<dyn Searcher>,
) -> axum::Router {
    let notes = KbSlug::try_new("notes").expect("valid slug");
    let private = KbSlug::try_new("private").expect("valid slug");
    let storage = Arc::new(InMemoryStorage::default());
    storage
        .put_object(
            &notes,
            &ObjectPath::try_from("public.md").expect("valid path"),
            Bytes::from_static(b"public body"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("seed public object");
    storage
        .put_object(
            &notes,
            &ObjectPath::try_from(".notedthat/manifest.json").expect("valid path"),
            Bytes::from_static(b"secret"),
            Some("application/json"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("seed internal object");
    storage
        .put_object(
            &notes,
            &ObjectPath::try_from("z-last.md").expect("valid path"),
            Bytes::from_static(b"last"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("seed second public object");

    let declared_kbs = BTreeMap::from([
        ("notes".to_string(), notes),
        ("private".to_string(), private),
    ]);
    let (indexer_tx, _indexer_rx) = tokio::sync::mpsc::channel(16);
    build_router(AppState {
        storage,
        declared_kbs: Arc::new(declared_kbs),
        public_read_policies: Arc::new(policies),
        bearer_token: Arc::new(TOKEN.to_string()),
        max_body_size: 16 * 1024 * 1024,
        max_patchable_size: 16 * 1024 * 1024,
        indexer_tx,
        searcher,
    })
}

pub(super) async fn json(response: axum::response::Response) -> serde_json::Value {
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read response");
    serde_json::from_slice(&body).expect("JSON response")
}

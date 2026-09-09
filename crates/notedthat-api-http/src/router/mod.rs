//! Axum router builder and HTTP handlers for the `NotedThat` API.

// `lookup_kb` is the single definition of "is this knowledge base declared";
// `crate::authz::KbAccess::resolve` is its only caller.
pub(crate) use helpers::lookup_kb;

mod health;
mod helpers;
mod kbs;
mod llms;
mod objects;

use crate::middleware::auth_middleware;
use crate::state::AppState;
use axum::extract::{DefaultBodyLimit, Request};
use axum::handler::Handler;
use axum::http::{HeaderName, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::{Json, Router};
use tower::ServiceBuilder;
use tower_http::request_id::{
    MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use health::{healthz, readyz};
use kbs::{list_kbs, list_objects};
use llms::llms_txt;
use objects::{delete_object, get_object, head_object, patch_object, post_object, put_object};

/// The API route table, declared once in two forms.
///
/// `ROUTE_*` is the path relative to [`API_V1_PREFIX`], which is what
/// `build_router` registers under `nest`. `MATCHED_*` is the absolute path,
/// which is what axum reports as `MatchedPath` and what the public-read match in
/// [`crate::middleware`] compares against. Both come from one suffix literal, so
/// the mount point and each route are each written exactly once.
///
/// The routes stay nested rather than registered absolutely and merged: a
/// `.layer()` on an absolutely-routed sub-router also wraps its fallback, and
/// merging that fallback answers every unrouted path — `/v1/...` included — with
/// the API's 401 instead of a 404.
macro_rules! api_routes {
    ($($route:ident / $matched:ident => $suffix:literal,)+) => {
        $(
            pub(crate) const $route: &str = $suffix;
            pub(crate) const $matched: &str = concat!("/api/v1", $suffix);
        )+
    };
}

/// Mount point of the versioned machine API on the unified listener (D44).
pub const API_V1_PREFIX: &str = "/api/v1";

/// Mount point reserved for the future browse surface (D44, #100).
pub const BROWSE_PREFIX: &str = "/browse";

api_routes! {
    ROUTE_KBS / MATCHED_KBS => "/knowledgebases",
    ROUTE_KB / MATCHED_KB => "/knowledgebases/{kb_slug}",
    ROUTE_KB_SEARCH / MATCHED_KB_SEARCH => "/knowledgebases/{kb_slug}/search",
    ROUTE_KB_OBJECT / MATCHED_KB_OBJECT => "/knowledgebases/{kb_slug}/{*object_path}",
}

/// Maximum body size for PUT requests: 16 MiB (D35).
pub const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// A [`MakeRequestId`] implementation that generates `UUIDv7` request IDs.
#[derive(Clone, Copy, Default)]
pub struct MakeRequestUuidV7;

impl MakeRequestId for MakeRequestUuidV7 {
    fn make_request_id<B>(&mut self, _req: &Request<B>) -> Option<RequestId> {
        let id = Uuid::now_v7().to_string();
        let hv = id.parse().ok()?;
        Some(RequestId::new(hv))
    }
}

/// Build the complete axum [`Router`] with all routes and middleware.
pub fn build_router(state: AppState) -> Router {
    let request_id_header = HeaderName::from_static("x-request-id");
    let api_routes = Router::new()
        .route(ROUTE_KBS, get(list_kbs))
        .route(ROUTE_KB, get(list_objects))
        .route(
            ROUTE_KB_SEARCH,
            axum::routing::post(crate::search_route::search_kb).layer(
                axum::extract::DefaultBodyLimit::max(crate::search_route::SEARCH_BODY_MAX_BYTES),
            ),
        )
        .route(
            ROUTE_KB_OBJECT,
            get(get_object)
                .head(head_object)
                .put(put_object)
                .delete(delete_object)
                .patch(patch_object.layer(DefaultBodyLimit::disable()))
                .post(post_object.layer(DefaultBodyLimit::disable())),
        )
        .layer(
            ServiceBuilder::new()
                .layer(DefaultBodyLimit::max(helpers::body_limit_usize(
                    MAX_BODY_BYTES,
                )))
                .layer(from_fn_with_state(state.clone(), auth_middleware)),
        )
        .with_state(state);

    // Request-id generation and tracing wrap every surface this router serves,
    // including the unauthenticated root routes. Only `auth_middleware` and the
    // API body limit stay nested on `/api/v1`.
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/llms.txt", get(llms_txt))
        .route(BROWSE_PREFIX, any(browse_not_implemented))
        .route(&format!("{BROWSE_PREFIX}/"), any(browse_not_implemented))
        .route(
            &format!("{BROWSE_PREFIX}/{{*path}}"),
            any(browse_not_implemented),
        )
        .nest(API_V1_PREFIX, api_routes)
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::new(
                    request_id_header.clone(),
                    MakeRequestUuidV7,
                ))
                .layer(PropagateRequestIdLayer::new(request_id_header))
                .layer(TraceLayer::new_for_http()),
        )
}

/// `/browse` is reserved for the future browse surface (D44, #100). Answering
/// `501 Not Implemented` makes the reservation observable; a bare 404 is
/// indistinguishable from a mistyped path.
async fn browse_not_implemented(request: Request) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .and_then(|id| id.header_value().to_str().ok())
        .unwrap_or_default()
        .to_string();

    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "error": "not_implemented",
            "message": "The browse surface is reserved and not implemented",
            "request_id": request_id,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod route_constants {
    use super::{
        API_V1_PREFIX, MATCHED_KB, MATCHED_KB_OBJECT, MATCHED_KB_SEARCH, MATCHED_KBS, ROUTE_KB,
        ROUTE_KB_OBJECT, ROUTE_KB_SEARCH, ROUTE_KBS,
    };

    /// The router registers the relative form and the middleware matches the
    /// absolute one. If the two stop agreeing, public-read authorization
    /// silently stops matching the routes it is meant to guard.
    #[test]
    fn matched_paths_are_the_nested_routes_under_the_mount_point() {
        assert_eq!(API_V1_PREFIX, "/api/v1");
        for (route, matched) in [
            (ROUTE_KBS, MATCHED_KBS),
            (ROUTE_KB, MATCHED_KB),
            (ROUTE_KB_SEARCH, MATCHED_KB_SEARCH),
            (ROUTE_KB_OBJECT, MATCHED_KB_OBJECT),
        ] {
            assert_eq!(matched, format!("{API_V1_PREFIX}{route}"));
        }
    }
}

#[cfg(test)]
mod patch_route {
    use super::*;
    use async_trait::async_trait;
    use axum::body::{Body, to_bytes};
    use axum::http::StatusCode;
    use axum::response::Response;
    use bytes::Bytes;
    use notedthat_core::{
        ByteRange, ConditionalHeaders, KbManifest, KbSlug, ListResponse, ObjectMeta, ObjectPath,
        ObjectRead, PutOutcome, Storage, StorageError,
    };
    use notedthat_indexer::IndexEvent;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tower::util::ServiceExt;

    const KB: &str = "notes";
    const OBJECT_PATH: &str = "patch.md";
    const TOKEN: &str = "test-token-abc";

    async fn router_with_object(
        initial_body: &'static [u8],
        max_patchable_size: u64,
    ) -> (axum::Router, String) {
        let kb = KbSlug::try_new(KB).unwrap();
        let object_path = ObjectPath::try_from_str(OBJECT_PATH).unwrap();
        let storage = Arc::new(crate::testing::InMemoryStorage::default());
        let outcome = storage
            .put_object(
                &kb,
                &object_path,
                Bytes::from_static(initial_body),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .unwrap();

        let (indexer_tx, _rx) = tokio::sync::mpsc::channel(16);
        let mut kbs = BTreeMap::new();
        kbs.insert(KB.to_string(), kb);
        let router = build_router(AppState {
            storage,
            access_policies: Arc::new(notedthat_core::signed_in_policies(&kbs)),
            declared_kbs: Arc::new(kbs),
            bearer_token: Arc::new(TOKEN.to_string()),
            max_body_size: MAX_BODY_BYTES,
            max_patchable_size,
            indexer_tx,
            searcher: Arc::new(crate::testing::NoopSearcher),
        });

        (router, outcome.etag.unwrap())
    }

    async fn object_with_etag(
        storage: &crate::testing::InMemoryStorage,
        kb: &KbSlug,
        body: &'static [u8],
    ) -> String {
        storage
            .put_object(
                kb,
                &ObjectPath::try_from_str(OBJECT_PATH).unwrap(),
                Bytes::from_static(body),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .unwrap()
            .etag
            .unwrap()
    }

    fn router_with_storage(
        storage: Arc<dyn Storage>,
        kb: KbSlug,
        max_patchable_size: u64,
        indexer_tx: tokio::sync::mpsc::Sender<IndexEvent>,
    ) -> axum::Router {
        let mut kbs = BTreeMap::new();
        kbs.insert(KB.to_string(), kb);
        build_router(AppState {
            storage,
            access_policies: Arc::new(notedthat_core::signed_in_policies(&kbs)),
            declared_kbs: Arc::new(kbs),
            bearer_token: Arc::new(TOKEN.to_string()),
            max_body_size: MAX_BODY_BYTES,
            max_patchable_size,
            indexer_tx,
            searcher: Arc::new(crate::testing::NoopSearcher),
        })
    }

    async fn patch_request(
        router: axum::Router,
        header_name: &'static str,
        header_value: &str,
        if_match: Option<&str>,
        body: Bytes,
    ) -> Response {
        let mut builder = Request::builder()
            .method("PATCH")
            .uri(format!("/api/v1/knowledgebases/{KB}/{OBJECT_PATH}"))
            .header("authorization", format!("Bearer {TOKEN}"))
            .header(header_name, header_value);
        if let Some(etag) = if_match {
            builder = builder.header(axum::http::header::IF_MATCH, etag);
        }

        router
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap()
    }

    async fn assert_error_code(response: Response, expected_status: StatusCode, expected: &str) {
        assert_eq!(response.status(), expected_status);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], expected);
    }

    #[tokio::test]
    async fn bytes_content_range_returns_ok_with_etag_and_location() {
        let (router, etag) = router_with_object(b"0123456789abcdefghij", MAX_BODY_BYTES).await;

        let response = patch_request(
            router,
            "content-range",
            "bytes 0-9/*",
            Some(&etag),
            Bytes::from_static(b"ABCDEFGHIJ"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(axum::http::header::ETAG).is_some());
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap(),
            &format!("/api/v1/knowledgebases/{KB}/{OBJECT_PATH}")
        );
        assert!(
            response
                .headers()
                .get(axum::http::header::CONTENT_RANGE)
                .is_none()
        );
        assert!(response.headers().get("nt-patch-mode").is_none());
    }

    #[tokio::test]
    async fn lines_content_range_returns_ok() {
        let (router, etag) = router_with_object(b"one\ntwo\nthree\nfour\n", MAX_BODY_BYTES).await;

        let response = patch_request(
            router,
            "content-range",
            "lines 2-3/*",
            Some(&etag),
            Bytes::from_static(b"TWO\nTHREE\n"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn append_mode_without_if_match_returns_ok() {
        let (router, _etag) = router_with_object(b"one\n", MAX_BODY_BYTES).await;

        let response = patch_request(
            router,
            "nt-patch-mode",
            "append",
            None,
            Bytes::from_static(b"two\n"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn append_mode_with_if_match_returns_ok() {
        let (router, etag) = router_with_object(b"one\n", MAX_BODY_BYTES).await;

        let response = patch_request(
            router,
            "nt-patch-mode",
            "append",
            Some(&etag),
            Bytes::from_static(b"two\n"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bytes_content_range_without_if_match_returns_invalid_request() {
        let (router, _etag) = router_with_object(b"0123456789", MAX_BODY_BYTES).await;

        let response = patch_request(
            router,
            "content-range",
            "bytes 0-1/*",
            None,
            Bytes::from_static(b"AB"),
        )
        .await;

        assert_error_code(response, StatusCode::BAD_REQUEST, "invalid_request").await;
    }

    #[tokio::test]
    async fn if_match_star_returns_invalid_request() {
        let (router, _etag) = router_with_object(b"0123456789", MAX_BODY_BYTES).await;

        let response = patch_request(
            router,
            "content-range",
            "bytes 0-1/*",
            Some("*"),
            Bytes::from_static(b"AB"),
        )
        .await;

        assert_error_code(response, StatusCode::BAD_REQUEST, "invalid_request").await;
    }

    #[tokio::test]
    async fn multi_value_if_match_returns_invalid_request() {
        let (router, etag) = router_with_object(b"0123456789", MAX_BODY_BYTES).await;

        let response = patch_request(
            router,
            "content-range",
            "bytes 0-1/*",
            Some(&format!("{etag}, \"other\"")),
            Bytes::from_static(b"AB"),
        )
        .await;

        assert_error_code(response, StatusCode::BAD_REQUEST, "invalid_request").await;
    }

    #[tokio::test]
    async fn nonexistent_object_returns_not_found() {
        let (router, etag) = router_with_object(b"0123456789", MAX_BODY_BYTES).await;

        let response = router
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/v1/knowledgebases/{KB}/missing.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header("content-range", "bytes 0-1/*")
                    .header(axum::http::header::IF_MATCH, etag)
                    .body(Body::from(Bytes::from_static(b"AB")))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_error_code(response, StatusCode::NOT_FOUND, "not_found").await;
    }

    #[tokio::test]
    async fn body_larger_than_max_patchable_size_returns_payload_too_large() {
        let (router, _etag) = router_with_object(b"one\n", 4).await;

        let response = patch_request(
            router,
            "nt-patch-mode",
            "append",
            None,
            Bytes::from_static(b"abcde"),
        )
        .await;

        assert_error_code(response, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large").await;
    }

    mod errors {
        use super::*;

        #[derive(Clone)]
        struct PutPreconditionFailedStorage {
            inner: crate::testing::InMemoryStorage,
        }

        #[async_trait]
        impl Storage for PutPreconditionFailedStorage {
            async fn ensure_bucket(&self, kb: &KbSlug) -> Result<(), StorageError> {
                self.inner.ensure_bucket(kb).await
            }

            async fn read_manifest(&self, kb: &KbSlug) -> Result<KbManifest, StorageError> {
                self.inner.read_manifest(kb).await
            }

            async fn write_manifest(
                &self,
                kb: &KbSlug,
                manifest: &KbManifest,
            ) -> Result<(), StorageError> {
                self.inner.write_manifest(kb, manifest).await
            }

            async fn head_object(
                &self,
                kb: &KbSlug,
                path: &ObjectPath,
                conditionals: ConditionalHeaders,
            ) -> Result<ObjectMeta, StorageError> {
                self.inner.head_object(kb, path, conditionals).await
            }

            async fn get_object(
                &self,
                kb: &KbSlug,
                path: &ObjectPath,
                range: Option<Vec<ByteRange>>,
                conditionals: ConditionalHeaders,
            ) -> Result<ObjectRead, StorageError> {
                self.inner.get_object(kb, path, range, conditionals).await
            }

            async fn get_object_stream(
                &self,
                kb: &KbSlug,
                path: &ObjectPath,
                range: Option<Vec<ByteRange>>,
                conditionals: ConditionalHeaders,
            ) -> Result<notedthat_core::ObjectStream, StorageError> {
                self.inner
                    .get_object_stream(kb, path, range, conditionals)
                    .await
            }

            async fn put_object(
                &self,
                _kb: &KbSlug,
                _path: &ObjectPath,
                _bytes: Bytes,
                _content_type: Option<&str>,
                _conditionals: ConditionalHeaders,
            ) -> Result<PutOutcome, StorageError> {
                Err(StorageError::PreconditionFailed)
            }

            async fn put_staged_object(
                &self,
                _kb: &KbSlug,
                _path: &ObjectPath,
                _body: notedthat_core::StagedBody,
                _content_type: Option<&str>,
                _conditionals: ConditionalHeaders,
            ) -> Result<PutOutcome, StorageError> {
                Err(StorageError::PreconditionFailed)
            }

            async fn copy_object(
                &self,
                kb: &KbSlug,
                source: &ObjectPath,
                destination: &ObjectPath,
                options: notedthat_core::CopyObjectOptions,
            ) -> Result<PutOutcome, StorageError> {
                self.inner
                    .copy_object(kb, source, destination, options)
                    .await
            }

            async fn delete_object(
                &self,
                kb: &KbSlug,
                path: &ObjectPath,
                conditionals: ConditionalHeaders,
            ) -> Result<(), StorageError> {
                self.inner.delete_object(kb, path, conditionals).await
            }

            async fn list_objects(
                &self,
                kb: &KbSlug,
                prefix: Option<&str>,
                limit: u32,
                cursor: Option<&str>,
            ) -> Result<ListResponse, StorageError> {
                self.inner.list_objects(kb, prefix, limit, cursor).await
            }
        }

        #[tokio::test]
        async fn missing_if_match_for_bytes_mode_returns_invalid_request() {
            let (router, _etag) = router_with_object(b"0123456789", MAX_BODY_BYTES).await;

            let response = patch_request(
                router,
                "content-range",
                "bytes 0-9/*",
                None,
                Bytes::from_static(b"ABCDEFGHIJ"),
            )
            .await;

            assert_error_code(response, StatusCode::BAD_REQUEST, "invalid_request").await;
        }

        #[tokio::test]
        async fn body_larger_than_max_patchable_size_returns_payload_too_large() {
            let (router, _etag) = router_with_object(b"one\n", 10).await;

            let response = patch_request(
                router,
                "nt-patch-mode",
                "append",
                None,
                Bytes::from_static(b"more than ten bytes"),
            )
            .await;

            assert_error_code(response, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large").await;
        }

        #[tokio::test]
        async fn pre_splice_object_larger_than_max_patchable_size_returns_payload_too_large() {
            let (router, _etag) = router_with_object(b"already too large", 10).await;

            let response = patch_request(
                router,
                "nt-patch-mode",
                "append",
                None,
                Bytes::from_static(b"!"),
            )
            .await;

            assert_error_code(response, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large").await;
        }

        #[tokio::test]
        async fn post_splice_body_larger_than_max_patchable_size_returns_payload_too_large() {
            let (router, _etag) = router_with_object(b"123456", 10).await;

            let response = patch_request(
                router,
                "nt-patch-mode",
                "append",
                None,
                Bytes::from_static(b"78901"),
            )
            .await;

            assert_error_code(response, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large").await;
        }

        #[tokio::test]
        async fn line_range_out_of_bounds_returns_dual_416_headers_and_empty_body() {
            let (router, etag) =
                router_with_object(b"one\ntwo\nthree\nfour\nfive\n", MAX_BODY_BYTES).await;

            let response = patch_request(
                router,
                "content-range",
                "lines 100-200/*",
                Some(&etag),
                Bytes::from_static(b"replacement\n"),
            )
            .await;

            assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
            assert_eq!(
                response.headers().get("content-range").unwrap(),
                "lines */5"
            );
            assert_eq!(
                response.headers().get("x-content-range-bytes").unwrap(),
                "*/24"
            );
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            assert!(body.is_empty());
        }

        #[tokio::test]
        async fn if_match_mismatch_after_retries_returns_precondition_failed_without_content_range()
        {
            let kb = KbSlug::try_new(KB).unwrap();
            let inner = crate::testing::InMemoryStorage::default();
            let etag = object_with_etag(&inner, &kb, b"0123456789").await;
            let (indexer_tx, _rx) = tokio::sync::mpsc::channel(16);
            let router = router_with_storage(
                Arc::new(PutPreconditionFailedStorage { inner }),
                kb,
                MAX_BODY_BYTES,
                indexer_tx,
            );

            let response = patch_request(
                router,
                "content-range",
                "bytes 0-1/*",
                Some(&etag),
                Bytes::from_static(b"AB"),
            )
            .await;

            assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
            assert!(
                response
                    .headers()
                    .get(axum::http::header::CONTENT_RANGE)
                    .is_none()
            );
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"], "precondition_failed");
        }

        #[tokio::test]
        async fn indexer_queue_full_returns_backend_unavailable_with_retry_after() {
            let kb = KbSlug::try_new(KB).unwrap();
            let storage = crate::testing::InMemoryStorage::default();
            let etag = object_with_etag(&storage, &kb, b"one\n").await;
            let (indexer_tx, _rx) = tokio::sync::mpsc::channel(1);
            indexer_tx
                .try_send(IndexEvent::Upsert {
                    kb: kb.clone(),
                    object_key: ObjectPath::try_from_str("queued.md").unwrap(),
                    etag: "queued".to_string(),
                    mtime: 0,
                })
                .unwrap();
            let router = router_with_storage(Arc::new(storage), kb, MAX_BODY_BYTES, indexer_tx);

            let response = patch_request(
                router,
                "nt-patch-mode",
                "append",
                Some(&etag),
                Bytes::from_static(b"two\n"),
            )
            .await;

            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.headers().get("retry-after").unwrap(), "5");
            let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"], "backend_unavailable");
        }
    }
}

#[cfg(test)]
mod line_range_get {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::StatusCode;
    use axum::response::Response;
    use bytes::Bytes;
    use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, Storage};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tower::util::ServiceExt;

    const KB: &str = "notes";
    const TOKEN: &str = "test-token-abc";

    fn twenty_line_markdown() -> String {
        markdown_lines(1, 20)
    }

    fn markdown_lines(first: u32, last: u32) -> String {
        let mut body = String::new();
        for line in first..=last {
            std::fmt::Write::write_fmt(&mut body, format_args!("line {line:02}\n")).unwrap();
        }
        body
    }

    async fn router_with_markdown_object(body: String) -> axum::Router {
        let kb = KbSlug::try_new(KB).unwrap();
        let object_path = ObjectPath::try_from_str("ranges.md").unwrap();
        let storage = Arc::new(crate::testing::InMemoryStorage::default());
        storage
            .put_object(
                &kb,
                &object_path,
                Bytes::from(body),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .unwrap();

        let (indexer_tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut kbs = BTreeMap::new();
        kbs.insert(KB.to_string(), kb);
        build_router(AppState {
            storage,
            access_policies: Arc::new(notedthat_core::signed_in_policies(&kbs)),
            declared_kbs: Arc::new(kbs),
            bearer_token: Arc::new(TOKEN.to_string()),
            max_body_size: MAX_BODY_BYTES,
            max_patchable_size: MAX_BODY_BYTES,
            indexer_tx,
            searcher: Arc::new(crate::testing::NoopSearcher),
        })
    }

    async fn get_ranges_md(router: axum::Router, range: &str) -> Response {
        router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/v1/knowledgebases/{KB}/ranges.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header(axum::http::header::RANGE, range)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn returns_first_five_lines_when_closed_range_requested() {
        let router = router_with_markdown_object(twenty_line_markdown()).await;

        let response = get_ranges_md(router, "lines=1-5").await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(body, Bytes::from(markdown_lines(1, 5)));
    }

    #[tokio::test]
    async fn returns_last_three_lines_when_suffix_range_requested() {
        let router = router_with_markdown_object(twenty_line_markdown()).await;

        let response = get_ranges_md(router, "lines=-3").await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(body, Bytes::from(markdown_lines(18, 20)));
    }

    #[tokio::test]
    async fn returns_empty_body_when_insert_range_requested() {
        let router = router_with_markdown_object(twenty_line_markdown()).await;

        let response = get_ranges_md(router, "lines=5-4").await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn returns_full_body_when_unknown_range_unit_requested() {
        let body = twenty_line_markdown();
        let router = router_with_markdown_object(body.clone()).await;

        let response = get_ranges_md(router, "items=0-5").await;

        assert_eq!(response.status(), StatusCode::OK);
        let actual = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(actual, Bytes::from(body));
    }

    mod headers {
        use super::*;

        fn ten_line_markdown() -> String {
            markdown_lines(1, 10)
        }

        #[tokio::test]
        async fn returns_line_and_byte_content_ranges_when_closed_range_requested() {
            let router = router_with_markdown_object(ten_line_markdown()).await;

            let response = get_ranges_md(router, "lines=2-4").await;

            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(
                response.headers().get("Content-Range").unwrap(),
                "lines 2-4/10"
            );
            assert_eq!(
                response.headers().get("X-Content-Range-Bytes").unwrap(),
                "8-31/80"
            );
        }

        #[tokio::test]
        async fn returns_slice_content_length_when_closed_range_requested() {
            let router = router_with_markdown_object(ten_line_markdown()).await;

            let response = get_ranges_md(router, "lines=2-4").await;

            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers().get("content-length").unwrap(), "24");
        }

        #[tokio::test]
        async fn returns_zero_length_and_empty_byte_range_when_insert_range_requested() {
            let router = router_with_markdown_object(ten_line_markdown()).await;

            let response = get_ranges_md(router, "lines=5-4").await;

            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers().get("content-length").unwrap(), "0");
            assert_eq!(
                response.headers().get("Content-Range").unwrap(),
                "lines 5-4/10"
            );
            assert_eq!(
                response.headers().get("X-Content-Range-Bytes").unwrap(),
                "32-31/80"
            );
        }

        #[tokio::test]
        async fn omits_line_byte_range_header_when_byte_range_requested() {
            let router = router_with_markdown_object(ten_line_markdown()).await;

            let response = get_ranges_md(router, "bytes=0-9").await;

            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert!(response.headers().get("X-Content-Range-Bytes").is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::StatusCode;
    use axum::response::Response;
    use bytes::Bytes;
    use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, Storage, StorageError};
    use notedthat_indexer::IndexEvent;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tower::util::ServiceExt;

    const KB: &str = "notes";
    const TOKEN: &str = "test-token-abc";

    fn router() -> axum::Router {
        let kb = KbSlug::try_new(KB).unwrap();
        let mut kbs = BTreeMap::new();
        kbs.insert(KB.to_string(), kb);
        let (indexer_tx, mut rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });

        build_router(AppState {
            storage: Arc::new(crate::testing::InMemoryStorage::default()),
            access_policies: Arc::new(notedthat_core::signed_in_policies(&kbs)),
            declared_kbs: Arc::new(kbs),
            bearer_token: Arc::new(TOKEN.to_string()),
            max_body_size: MAX_BODY_BYTES,
            max_patchable_size: MAX_BODY_BYTES,
            indexer_tx,
            searcher: Arc::new(crate::testing::NoopSearcher),
        })
    }

    async fn put_object(router: axum::Router, path: &str, body: &'static [u8]) -> Response {
        router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/v1/knowledgebases/{KB}/{path}"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header(axum::http::header::CONTENT_TYPE, "text/markdown")
                    .body(Body::from(Bytes::from_static(body)))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn put_object_etag(router: axum::Router, path: &str, body: &'static [u8]) -> String {
        let response = put_object(router, path, body).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        response
            .headers()
            .get(axum::http::header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    async fn get_object(router: axum::Router, path: &str) -> Response {
        router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/v1/knowledgebases/{KB}/{path}"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn post_replace(
        router: axum::Router,
        path: &str,
        if_match: &str,
        body: &'static [u8],
    ) -> Response {
        router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/knowledgebases/{KB}/replace/{path}"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .header(axum::http::header::IF_MATCH, if_match)
                    .body(Body::from(Bytes::from_static(body)))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn get_on_replace_prefixed_path_still_reads_object_via_catch_all() {
        let router = router();
        put_object_etag(router.clone(), "replace/foo.md", b"hi").await;

        let response = get_object(router, "replace/foo.md").await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(&body[..], b"hi");
    }

    #[tokio::test]
    async fn patch_on_replace_prefixed_path_still_reaches_patch_object() {
        let router = router();
        let etag = put_object_etag(router.clone(), "replace/bar.md", b"old\n").await;

        let response = router
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/v1/knowledgebases/{KB}/replace/bar.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header(axum::http::header::CONTENT_RANGE, "lines 1-1/*")
                    .header(axum::http::header::IF_MATCH, etag)
                    .body(Body::from(Bytes::from_static(b"new\n")))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn put_and_delete_on_replace_prefixed_path_still_work() {
        let router = router();
        let put = put_object(router.clone(), "replace/delete.md", b"gone").await;

        assert_eq!(put.status(), StatusCode::CREATED);
        let delete = router
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/knowledgebases/{KB}/replace/delete.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delete.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn post_on_non_replace_path_returns_404_not_found() {
        let response = router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/knowledgebases/{KB}/foo.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "not_found");
        assert!(
            json["message"]
                .as_str()
                .unwrap()
                .contains("supported actions: 'replace/<path>'")
        );
    }

    #[tokio::test]
    async fn post_on_replace_prefixed_path_dispatches_to_replace_handler() {
        let router = router();
        let etag = put_object_etag(router.clone(), "target.md", b"hello world").await;

        let response = post_replace(
            router,
            "target.md",
            &etag,
            br#"{"old_string":"world","new_string":"planet"}"#,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["match_count"], 1);
    }

    #[tokio::test]
    async fn post_on_replace_replace_path_targets_the_replace_prefixed_object() {
        let router = router();
        let etag = put_object_etag(router.clone(), "replace/nested.md", b"foo bar").await;

        let response = post_replace(
            router.clone(),
            "replace/nested.md",
            &etag,
            br#"{"old_string":"bar","new_string":"baz"}"#,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["match_count"], 1);
        let get = get_object(router, "replace/nested.md").await;
        let body = to_bytes(get.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(&body[..], b"foo baz");
    }

    #[tokio::test]
    async fn test_conditional_put_503_then_naive_retry_412_keeps_object_stored() {
        let kb = KbSlug::try_new(KB).unwrap();
        let object_path = ObjectPath::try_from_str("cond.md").unwrap();
        let storage = Arc::new(crate::testing::InMemoryStorage::default());

        let (indexer_tx, _rx) = tokio::sync::mpsc::channel(1);
        indexer_tx
            .try_send(IndexEvent::Upsert {
                kb: kb.clone(),
                object_key: ObjectPath::try_from_str("queued.md").unwrap(),
                etag: "etag".to_string(),
                mtime: 0,
            })
            .unwrap();

        let mut kbs = BTreeMap::new();
        kbs.insert(KB.to_string(), kb.clone());
        let state = AppState {
            storage: storage.clone(),
            access_policies: Arc::new(notedthat_core::signed_in_policies(&kbs)),
            declared_kbs: Arc::new(kbs),
            bearer_token: Arc::new(TOKEN.to_string()),
            max_body_size: MAX_BODY_BYTES,
            max_patchable_size: MAX_BODY_BYTES,
            indexer_tx,
            searcher: Arc::new(crate::testing::NoopSearcher),
        };
        let router = build_router(state);

        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/v1/knowledgebases/{KB}/cond.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header("if-none-match", "*")
                    .body(Body::from(Bytes::from_static(b"first content")))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get("retry-after").unwrap(), "5");
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("\"error\":\"backend_unavailable\""));
        assert!(body.contains("object stored; indexer queue full — retry to re-enqueue"));

        let stored = storage
            .get_object(&kb, &object_path, None, ConditionalHeaders::default())
            .await
            .unwrap();
        assert_eq!(stored.bytes, Bytes::from_static(b"first content"));

        let retry = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/v1/knowledgebases/{KB}/cond.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header("if-none-match", "*")
                    .body(Body::from(Bytes::from_static(b"second content")))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(retry.status(), StatusCode::PRECONDITION_FAILED);
        assert!(retry.headers().get("retry-after").is_none());

        let stored = storage
            .get_object(&kb, &object_path, None, ConditionalHeaders::default())
            .await
            .unwrap();
        assert_eq!(stored.bytes, Bytes::from_static(b"first content"));
    }

    #[tokio::test]
    async fn test_delete_returns_delete_specific_503_body_when_indexer_backpressure() {
        let kb = KbSlug::try_new(KB).unwrap();
        let object_path = ObjectPath::try_from_str("to-delete.md").unwrap();
        let storage = Arc::new(crate::testing::InMemoryStorage::default());
        storage
            .put_object(
                &kb,
                &object_path,
                Bytes::from_static(b"content"),
                Some("text/plain"),
                ConditionalHeaders::default(),
            )
            .await
            .unwrap();

        let (indexer_tx, _rx) = tokio::sync::mpsc::channel(1);
        indexer_tx
            .try_send(IndexEvent::Upsert {
                kb: kb.clone(),
                object_key: ObjectPath::try_from_str("queued.md").unwrap(),
                etag: "etag".to_string(),
                mtime: 0,
            })
            .unwrap();

        let mut kbs = BTreeMap::new();
        kbs.insert(KB.to_string(), kb.clone());
        let state = AppState {
            storage: storage.clone(),
            access_policies: Arc::new(notedthat_core::signed_in_policies(&kbs)),
            declared_kbs: Arc::new(kbs),
            bearer_token: Arc::new(TOKEN.to_string()),
            max_body_size: MAX_BODY_BYTES,
            max_patchable_size: MAX_BODY_BYTES,
            indexer_tx,
            searcher: Arc::new(crate::testing::NoopSearcher),
        };
        let router = build_router(state);

        let response = router
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v1/knowledgebases/{KB}/to-delete.md"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get("retry-after").unwrap(), "5");
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("\"error\":\"backend_unavailable\""));
        assert!(
            body.contains("\"message\":\"deleted from storage; retry to clear from search index\"")
        );
        assert!(!body.contains("object stored; indexer queue full — retry to re-enqueue"));

        let deleted = storage
            .get_object(&kb, &object_path, None, ConditionalHeaders::default())
            .await;
        assert!(matches!(deleted, Err(StorageError::NotFound { .. })));
    }
}

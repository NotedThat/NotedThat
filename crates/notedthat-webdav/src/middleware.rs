//! `WebDAV` middleware: basic-auth and method-interception layers.

use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use notedthat_core::{extract_basic_from_header, verify_basic_credentials};
use tower_http::request_id::RequestId;

use crate::state::WebDavState;

/// Extract the x-request-id from the request, returning "unknown" if absent.
fn extract_request_id(req: &Request) -> String {
    req.extensions()
        .get::<RequestId>()
        .and_then(|id| id.header_value().to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

/// Basic-auth middleware for the `WebDAV` listener.
///
/// Every request must carry a valid `Authorization: Basic <credentials>` header.
/// On failure returns 401 with `WWW-Authenticate: Basic realm="NotedThat"`.
/// On success passes to the next handler.
pub async fn basic_auth_middleware(
    State(state): State<WebDavState>,
    req: Request,
    next: Next,
) -> Response {
    let request_id = extract_request_id(&req);

    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    let credentials = auth_header.and_then(extract_basic_from_header);

    let authorized = credentials.as_ref().is_some_and(|(u, p)| {
        verify_basic_credentials(u, p, state.username.as_str(), state.password.as_str())
    });

    if !authorized {
        let mut response = (StatusCode::UNAUTHORIZED, "").into_response();
        response.headers_mut().insert(
            "www-authenticate",
            HeaderValue::from_static("Basic realm=\"NotedThat\""),
        );
        response.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_str(&request_id).unwrap_or(HeaderValue::from_static("unknown")),
        );
        return response;
    }

    next.run(req).await
}

#[cfg(test)]
mod basic_auth {
    mod tests {
        use super::super::*;
        use async_trait::async_trait;
        use axum::{
            Router,
            body::{Body, to_bytes},
            http::Request as HttpRequest,
            middleware::from_fn_with_state,
            routing::get,
        };
        use base64::Engine as _;
        use bytes::Bytes;
        use notedthat_core::{
            ByteRange, ConditionalHeaders, KbManifest, KbSlug, ListResponse, ObjectMeta,
            ObjectPath, ObjectRead, PutOutcome, Storage, StorageError,
        };
        use std::{collections::BTreeMap, sync::Arc};
        use tower::util::ServiceExt;
        use tower_http::request_id::{MakeRequestUuid, SetRequestIdLayer};

        #[derive(Default)]
        struct MockStorage;

        fn unavailable() -> StorageError {
            StorageError::BackendUnavailable {
                message: "mock storage is not used by auth middleware".to_string(),
            }
        }

        #[async_trait]
        impl Storage for MockStorage {
            async fn ensure_bucket(&self, _kb: &KbSlug) -> Result<(), StorageError> {
                Err(unavailable())
            }

            async fn read_manifest(&self, _kb: &KbSlug) -> Result<KbManifest, StorageError> {
                Err(unavailable())
            }

            async fn write_manifest(
                &self,
                _kb: &KbSlug,
                _manifest: &KbManifest,
            ) -> Result<(), StorageError> {
                Err(unavailable())
            }

            async fn head_object(
                &self,
                _kb: &KbSlug,
                _path: &ObjectPath,
                _conditionals: ConditionalHeaders,
            ) -> Result<ObjectMeta, StorageError> {
                Err(unavailable())
            }

            async fn get_object(
                &self,
                _kb: &KbSlug,
                _path: &ObjectPath,
                _range: Option<Vec<ByteRange>>,
                _conditionals: ConditionalHeaders,
            ) -> Result<ObjectRead, StorageError> {
                Err(unavailable())
            }

            async fn put_object(
                &self,
                _kb: &KbSlug,
                _path: &ObjectPath,
                _bytes: Bytes,
                _content_type: Option<&str>,
                _conditionals: ConditionalHeaders,
            ) -> Result<PutOutcome, StorageError> {
                Err(unavailable())
            }

            async fn delete_object(
                &self,
                _kb: &KbSlug,
                _path: &ObjectPath,
                _conditionals: ConditionalHeaders,
            ) -> Result<(), StorageError> {
                Err(unavailable())
            }

            async fn list_objects(
                &self,
                _kb: &KbSlug,
                _prefix: Option<&str>,
                _limit: u32,
            ) -> Result<ListResponse, StorageError> {
                Err(unavailable())
            }
        }

        fn test_state() -> WebDavState {
            let (indexer_tx, _rx) = tokio::sync::mpsc::channel(1024);
            WebDavState {
                username: Arc::new("testuser".to_string()),
                password: Arc::new("testpass".to_string()),
                storage: Arc::new(MockStorage),
                declared_kbs: Arc::new(BTreeMap::new()),
                indexer_tx,
            }
        }

        fn app() -> Router {
            let state = test_state();
            Router::new()
                .route("/", get(|| async { "ok" }))
                .layer(from_fn_with_state(state, basic_auth_middleware))
        }

        fn app_with_request_id() -> Router {
            let state = test_state();
            Router::new()
                .route("/", get(|| async { "ok" }))
                .layer(from_fn_with_state(state.clone(), basic_auth_middleware))
                .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        }

        fn basic_header(username: &str, password: &str) -> String {
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
            format!("Basic {encoded}")
        }

        #[tokio::test]
        async fn test_rejects_missing_auth() {
            let resp = app()
                .oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                resp.headers().get("www-authenticate").unwrap(),
                "Basic realm=\"NotedThat\""
            );
        }

        #[tokio::test]
        async fn test_rejects_malformed_auth() {
            let resp = app()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/")
                        .header("authorization", "Basic garbage!")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn test_rejects_wrong_username() {
            let resp = app()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/")
                        .header("authorization", basic_header("wronguser", "testpass"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn test_rejects_wrong_password() {
            let resp = app()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/")
                        .header("authorization", basic_header("testuser", "wrongpass"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }

        #[tokio::test]
        async fn test_accepts_correct_credentials() {
            let resp = app()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/")
                        .header("authorization", basic_header("testuser", "testpass"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn test_401_body_does_not_leak_credentials() {
            let resp = app()
                .oneshot(
                    HttpRequest::builder()
                        .uri("/")
                        .header("authorization", basic_header("testuser", "wrongpass"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(!body.contains("testuser"));
            assert!(!body.contains("testpass"));
        }

        #[tokio::test]
        async fn test_401_contains_request_id_header() {
            let resp = app_with_request_id()
                .oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            assert!(resp.headers().contains_key("x-request-id"));
        }
    }
}

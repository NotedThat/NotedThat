//! `WebDAV` middleware: basic-auth and method-interception layers.

use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
};
use notedthat_core::{
    ConditionalHeaders, KbSlug, ObjectPath, StorageError, extract_basic_from_header,
    verify_basic_credentials,
};
use std::borrow::Cow;
use std::collections::BTreeMap;
use tower_http::request_id::RequestId;

use crate::filesystem::{DavTarget, PROPFIND_TOO_LARGE_DAV_XML, ensure_propfind_target_within_cap};
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

/// Intercept OPTIONS requests and return DAV Class 1 response before dav-server.
///
/// dav-server v0.11 hardcodes `DAV: 1,2,3,sabredav-partialupdate` which violates
/// issue #22 which requires `DAV: 1` only. Interception is the only way to override.
pub async fn intercept_options(req: Request, next: Next) -> Response {
    if req.method() == axum::http::Method::OPTIONS {
        let mut response = (StatusCode::NO_CONTENT, "").into_response();
        let headers = response.headers_mut();
        headers.insert("dav", HeaderValue::from_static("1"));
        headers.insert("ms-author-via", HeaderValue::from_static("DAV"));
        headers.insert(
            "allow",
            HeaderValue::from_static(
                "OPTIONS, GET, HEAD, PROPFIND, PUT, DELETE, MKCOL, MOVE, COPY",
            ),
        );
        return response;
    }
    next.run(req).await
}

/// Intercept PROPPATCH requests and return 405 before dav-server.
///
/// dav-server v0.11 always handles PROPPATCH and returns 207. Issue #22 requires 405.
pub async fn intercept_proppatch(req: Request, next: Next) -> Response {
    if req.method().as_str() == "PROPPATCH" {
        let mut response = (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
        response.headers_mut().insert(
            "allow",
            HeaderValue::from_static(
                "OPTIONS, GET, HEAD, PROPFIND, PUT, DELETE, MKCOL, MOVE, COPY",
            ),
        );
        return response;
    }
    next.run(req).await
}

/// Intercept LOCK/UNLOCK requests and return 405 before dav-server.
///
/// dav-server v0.11 already returns 405 when no `LockSystem` is registered, but we
/// add belt-and-braces interception so future dav-server default changes cannot
/// silently enable LOCK/UNLOCK (D17: no `LockSystem`, ever, in v1).
pub async fn intercept_lock_unlock(req: Request, next: Next) -> Response {
    let method = req.method().as_str();
    if method == "LOCK" || method == "UNLOCK" {
        let mut response = (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
        response.headers_mut().insert(
            "allow",
            HeaderValue::from_static(
                "OPTIONS, GET, HEAD, PROPFIND, PUT, DELETE, MKCOL, MOVE, COPY",
            ),
        );
        return response;
    }
    next.run(req).await
}

/// Intercept over-large PROPFIND requests before dav-server swallows `read_dir` errors.
pub async fn intercept_propfind_too_large(
    State(state): State<WebDavState>,
    req: Request,
    next: Next,
) -> Response {
    if req.method().as_str() != "PROPFIND" {
        return next.run(req).await;
    }

    let Ok(target) = parse_uri_path(req.uri().path(), &state.declared_kbs) else {
        return next.run(req).await;
    };

    match ensure_propfind_target_within_cap(&state, &target).await {
        Err(dav_server::fs::FsError::InsufficientStorage) => (
            StatusCode::INSUFFICIENT_STORAGE,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/xml; charset=utf-8",
            )],
            PROPFIND_TOO_LARGE_DAV_XML,
        )
            .into_response(),
        Ok(()) | Err(_) => next.run(req).await,
    }
}

/// Intercept `WebDAV` write methods before `dav-server` so raw HTTP headers remain available.
pub async fn intercept_write_methods(
    State(state): State<WebDavState>,
    req: Request,
    next: Next,
) -> Response {
    match req.method().as_str() {
        "PUT" => handle_put(state, req).await,
        "DELETE" => handle_delete(state, req).await,
        "MOVE" => handle_move(state, req).await,
        "COPY" => handle_copy(state, req).await,
        _ => next.run(req).await,
    }
}

fn decode_uri_segment(raw_segment: &str) -> Result<Cow<'_, str>, ()> {
    percent_encoding::percent_decode_str(raw_segment)
        .decode_utf8()
        .map_err(|_| ())
}

fn validate_decoded_segment(decoded: &str) -> bool {
    !decoded.is_empty()
        && !decoded.contains('/')
        && decoded != "."
        && decoded != ".."
        && !decoded.contains('\\')
        && !decoded.contains('\0')
}

fn parse_uri_path(
    uri_path: &str,
    declared_kbs: &BTreeMap<String, KbSlug>,
) -> Result<DavTarget, ()> {
    let raw_path = uri_path.strip_prefix('/').unwrap_or(uri_path);
    if raw_path.is_empty() {
        return Ok(DavTarget::Root);
    }

    let (kb_raw, rest_raw) = raw_path.split_once('/').unwrap_or((raw_path, ""));
    let kb_decoded = decode_uri_segment(kb_raw)?;
    if !validate_decoded_segment(&kb_decoded) {
        return Err(());
    }

    let Ok(kb_slug) = KbSlug::try_new(kb_decoded.as_ref()) else {
        return Err(());
    };

    if !declared_kbs.contains_key(kb_decoded.as_ref()) {
        return Ok(DavTarget::NonDeclaredKb);
    }

    if rest_raw.is_empty() {
        return Ok(DavTarget::KbRoot(kb_slug));
    }

    let decoded_parts = rest_raw
        .split('/')
        .map(|raw_segment| {
            let decoded = decode_uri_segment(raw_segment)?;
            if !validate_decoded_segment(&decoded) {
                return Err(());
            }
            Ok(decoded)
        })
        .collect::<Result<Vec<_>, ()>>()?;
    let decoded_rest = decoded_parts
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join("/");

    ObjectPath::try_from_str(&decoded_rest)
        .map(|path| DavTarget::Object(kb_slug, path))
        .map_err(|_| ())
}

fn dav_error_body(condition: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><D:error xmlns:D="DAV:" xmlns:nt="urn:notedthat:error"><nt:{condition}/></D:error>"#
    )
}

/// HTTP 503 response for `WriteError::IndexerBackpressureUpsert`.
/// Body mirrors the HTTP API `backend_unavailable` shape so `WebDAV` clients
/// see the same semantics as HTTP clients.
fn backpressure_response() -> Response {
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"backend_unavailable","message":"object stored; indexer queue full — retry to re-enqueue"}"#,
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::HeaderName::from_static("retry-after"),
        axum::http::HeaderValue::from_static("5"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

/// HTTP 503 response for `WriteError::IndexerBackpressureTombstone` (DELETE).
fn delete_backpressure_response() -> Response {
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"backend_unavailable","message":"deleted from storage; retry to clear from search index"}"#,
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::HeaderName::from_static("retry-after"),
        axum::http::HeaderValue::from_static("5"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

fn move_destination_backpressure_response(_req_id: &str) -> Response {
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"backend_unavailable","message":"destination write succeeded but destination index event failed; source unchanged. Retry MOVE to re-enqueue destination index event."}"#,
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::HeaderName::from_static("retry-after"),
        axum::http::HeaderValue::from_static("5"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

fn copy_destination_backpressure_response() -> Response {
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"backend_unavailable","message":"destination write succeeded but destination index event failed. Retry COPY to re-enqueue destination index event."}"#,
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::HeaderName::from_static("retry-after"),
        axum::http::HeaderValue::from_static("5"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

fn move_source_tombstone_backpressure_response(_req_id: &str) -> Response {
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":"backend_unavailable","message":"destination write succeeded and source deleted from storage, but source search-index tombstone failed — search may return stale entries for the source path until retry or reindex. Retry MOVE to re-enqueue the source tombstone; the destination write is idempotent."}"#,
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::HeaderName::from_static("retry-after"),
        axum::http::HeaderValue::from_static("5"),
    );
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

async fn handle_put(state: WebDavState, req: Request) -> Response {
    let uri_path = req.uri().path().to_string();
    let Ok(target) = parse_uri_path(&uri_path, &state.declared_kbs) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (kb, path) = match target {
        DavTarget::Object(kb, path) => (kb, path),
        DavTarget::Root | DavTarget::KbRoot(_) => return StatusCode::BAD_REQUEST.into_response(),
        DavTarget::NonDeclaredKb => return StatusCode::FORBIDDEN.into_response(),
    };

    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

    if let Some(content_length) = req
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        && content_length > notedthat_write::MAX_UPLOAD_BYTES
    {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }

    let exists = state
        .storage
        .head_object(&kb, &path, ConditionalHeaders::default())
        .await
        .is_ok();

    let limit = usize::try_from(notedthat_write::MAX_UPLOAD_BYTES)
        .unwrap_or(usize::MAX)
        .saturating_add(1);
    let Ok(body_bytes) = to_bytes(req.into_body(), limit).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };

    match notedthat_write::commit(
        state.storage.as_ref(),
        &state.indexer_tx,
        &kb,
        &path,
        body_bytes,
        content_type.as_deref(),
        conditionals,
    )
    .await
    {
        Ok(outcome) => response_with_optional_etag(
            if exists {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::CREATED
            },
            outcome.etag,
        ),
        Err(notedthat_write::WriteError::TooLarge { .. }) => {
            StatusCode::PAYLOAD_TOO_LARGE.into_response()
        }
        Err(notedthat_write::WriteError::Storage(StorageError::PreconditionFailed)) => {
            StatusCode::PRECONDITION_FAILED.into_response()
        }
        Err(notedthat_write::WriteError::IndexerBackpressureUpsert) => backpressure_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn handle_delete(state: WebDavState, req: Request) -> Response {
    let uri_path = req.uri().path().to_string();
    let Ok(target) = parse_uri_path(&uri_path, &state.declared_kbs) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (kb, path) = match target {
        DavTarget::Object(kb, path) => (kb, path),
        DavTarget::Root | DavTarget::KbRoot(_) => return StatusCode::BAD_REQUEST.into_response(),
        DavTarget::NonDeclaredKb => return StatusCode::FORBIDDEN.into_response(),
    };
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

    match notedthat_write::commit_delete(
        state.storage.as_ref(),
        &state.indexer_tx,
        &kb,
        &path,
        conditionals,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(notedthat_write::WriteError::Storage(StorageError::PreconditionFailed)) => {
            StatusCode::PRECONDITION_FAILED.into_response()
        }
        Err(notedthat_write::WriteError::IndexerBackpressureTombstone) => {
            delete_backpressure_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn handle_move(state: WebDavState, req: Request) -> Response {
    handle_copy_or_move(state, req, true).await
}

async fn handle_copy(state: WebDavState, req: Request) -> Response {
    handle_copy_or_move(state, req, false).await
}

#[allow(clippy::too_many_lines)]
async fn handle_copy_or_move(state: WebDavState, req: Request, delete_source: bool) -> Response {
    let dest_header = match req
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
    {
        Some(destination) => destination.to_string(),
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    if dest_header.contains('#') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(dest_uri) = dest_header.parse::<Uri>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let req_host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if let Some(dest_authority) = dest_uri.authority() {
        let req_host_bare = req_host.split(':').next().unwrap_or(req_host);
        let dest_host = dest_authority.as_str();
        let dest_host_bare = dest_host.split(':').next().unwrap_or(dest_host);
        if !req_host_bare.eq_ignore_ascii_case(dest_host_bare) {
            return (
                StatusCode::BAD_GATEWAY,
                dav_error_body("destination-different-server"),
            )
                .into_response();
        }
    }
    let Ok(src_target) = parse_uri_path(req.uri().path(), &state.declared_kbs) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(dst_target) = parse_uri_path(dest_uri.path(), &state.declared_kbs) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (src_kb, src_obj) = match object_target_or_collection_error(src_target) {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    let (dst_kb, dst_obj) = match object_target_or_collection_error(dst_target) {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    if src_kb.as_str() != dst_kb.as_str() {
        return (
            StatusCode::FORBIDDEN,
            dav_error_body("cannot-modify-source"),
        )
            .into_response();
    }
    let src_data = match state
        .storage
        .get_object(&src_kb, &src_obj, None, ConditionalHeaders::default())
        .await
    {
        Ok(object) => object,
        Err(StorageError::NotFound { .. } | StorageError::BucketNotFound { .. }) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let dst_exists = state
        .storage
        .head_object(&dst_kb, &dst_obj, ConditionalHeaders::default())
        .await
        .is_ok();
    let content_type = src_data.meta.content_type.clone();
    let outcome = match notedthat_write::commit(
        state.storage.as_ref(),
        &state.indexer_tx,
        &dst_kb,
        &dst_obj,
        src_data.bytes,
        content_type.as_deref(),
        ConditionalHeaders::default(),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(notedthat_write::WriteError::IndexerBackpressureUpsert) => {
            return if delete_source {
                move_destination_backpressure_response("")
            } else {
                copy_destination_backpressure_response()
            };
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if delete_source {
        // NOTE: Intentional fail-visible partial-completion semantics. Destination write already
        // succeeded and source storage delete already succeeded, but the source search tombstone
        // is missing. 503 tells the client the search index may contain a stale source entry
        // until retry/reindex. Since v1 has no reindex endpoint and retrying the whole MOVE
        // will 404 on GET(src), this propagates the failure visibly.
        match notedthat_write::commit_delete(
            state.storage.as_ref(),
            &state.indexer_tx,
            &src_kb,
            &src_obj,
            ConditionalHeaders::default(),
        )
        .await
        {
            Ok(()) => {}
            Err(notedthat_write::WriteError::IndexerBackpressureTombstone) => {
                return move_source_tombstone_backpressure_response("");
            }
            Err(_) => {
                // Other commit_delete errors after a successful destination write: the source
                // object is already deleted from storage. Return 500 to signal an unexpected
                // failure in the source-tombstone step.
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    }
    response_with_optional_etag(
        if dst_exists {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::CREATED
        },
        outcome.etag,
    )
}

enum TargetError {
    Collection,
    Forbidden,
}

impl TargetError {
    fn into_response(self) -> Response {
        match self {
            Self::Collection => {
                (StatusCode::FORBIDDEN, dav_error_body("no-collection-move")).into_response()
            }
            Self::Forbidden => StatusCode::FORBIDDEN.into_response(),
        }
    }
}

fn object_target_or_collection_error(
    target: DavTarget,
) -> Result<(KbSlug, ObjectPath), TargetError> {
    match target {
        DavTarget::Object(kb, path) => Ok((kb, path)),
        DavTarget::Root | DavTarget::KbRoot(_) => Err(TargetError::Collection),
        DavTarget::NonDeclaredKb => Err(TargetError::Forbidden),
    }
}

fn response_with_optional_etag(status: StatusCode, etag: Option<String>) -> Response {
    let mut response = status.into_response();
    if let Some(etag) = etag
        && let Ok(value) = HeaderValue::from_str(&etag)
    {
        response.headers_mut().insert("etag", value);
    }
    response
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
                _cursor: Option<&str>,
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

#[cfg(test)]
mod intercept_options {
    mod tests {
        use super::super::*;
        use axum::{
            Router, body::Body, http::Request as HttpRequest, middleware::from_fn, routing::any,
        };
        use tower::util::ServiceExt;

        fn app() -> Router {
            Router::new()
                .route("/", any(|| async { "inner handler reached" }))
                .layer(from_fn(intercept_options))
        }

        #[tokio::test]
        async fn test_options_returns_204_dav_1() {
            let req = HttpRequest::builder()
                .method("OPTIONS")
                .uri("/")
                .body(Body::empty())
                .unwrap();
            let resp = app().oneshot(req).await.unwrap();

            assert_eq!(resp.status(), StatusCode::NO_CONTENT);
            let dav = resp.headers().get("dav").unwrap();
            assert_eq!(dav.to_str().unwrap(), "1");
            assert!(!dav.to_str().unwrap().contains('2'));
            assert!(!dav.to_str().unwrap().contains('3'));
            assert!(resp.headers().contains_key("allow"));
        }

        #[tokio::test]
        async fn test_options_body_empty() {
            let req = HttpRequest::builder()
                .method("OPTIONS")
                .uri("/")
                .body(Body::empty())
                .unwrap();
            let resp = app().oneshot(req).await.unwrap();

            assert_eq!(resp.status(), StatusCode::NO_CONTENT);
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(body.is_empty());
        }

        #[tokio::test]
        async fn test_non_options_passes_through() {
            let req = HttpRequest::builder()
                .method("GET")
                .uri("/")
                .body(Body::empty())
                .unwrap();
            let resp = app().oneshot(req).await.unwrap();

            assert_eq!(resp.status(), StatusCode::OK);
        }
    }
}

#[cfg(test)]
mod intercept_proppatch {
    mod tests {
        use super::super::*;
        use axum::{
            Router, body::Body, http::Request as HttpRequest, middleware::from_fn, routing::any,
        };
        use tower::util::ServiceExt;

        fn app() -> Router {
            Router::new()
                .route("/", any(|| async { "inner handler reached" }))
                .layer(from_fn(intercept_proppatch))
        }

        #[tokio::test]
        async fn test_proppatch_returns_405() {
            let req = HttpRequest::builder()
                .method("PROPPATCH")
                .uri("/")
                .body(Body::empty())
                .unwrap();
            let resp = app().oneshot(req).await.unwrap();

            assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        }

        #[tokio::test]
        async fn test_proppatch_allow_header_present() {
            let req = HttpRequest::builder()
                .method("PROPPATCH")
                .uri("/")
                .body(Body::empty())
                .unwrap();
            let resp = app().oneshot(req).await.unwrap();

            assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
            assert!(resp.headers().contains_key("allow"));
        }
    }
}

#[cfg(test)]
mod intercept_lock {
    mod tests {
        use super::super::*;
        use axum::{
            Router, body::Body, http::Request as HttpRequest, middleware::from_fn, routing::any,
        };
        use tower::util::ServiceExt;

        fn app() -> Router {
            Router::new()
                .route("/", any(|| async { "inner handler reached" }))
                .layer(from_fn(intercept_lock_unlock))
        }

        #[tokio::test]
        async fn test_lock_returns_405() {
            let req = HttpRequest::builder()
                .method("LOCK")
                .uri("/")
                .body(Body::empty())
                .unwrap();
            let resp = app().oneshot(req).await.unwrap();

            assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        }

        #[tokio::test]
        async fn test_unlock_returns_405() {
            let req = HttpRequest::builder()
                .method("UNLOCK")
                .uri("/")
                .body(Body::empty())
                .unwrap();
            let resp = app().oneshot(req).await.unwrap();

            assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        }
    }
}

#[cfg(test)]
mod intercept_write_methods {
    mod tests {
        use super::super::*;
        use async_trait::async_trait;
        use axum::{
            Router,
            body::{Body, to_bytes},
            http::Request as HttpRequest,
            middleware::from_fn_with_state,
            routing::any,
        };
        use bytes::Bytes;
        use notedthat_core::{
            ByteRange, KbManifest, ListResponse, ObjectMeta, ObjectRead, PutOutcome, Storage,
        };
        use std::{collections::HashMap, sync::Arc, sync::Mutex};
        use tokio::sync::mpsc;
        use tower::util::ServiceExt;

        #[derive(Clone)]
        struct StoredObject {
            bytes: Bytes,
            content_type: Option<String>,
            etag: String,
        }

        #[derive(Default)]
        struct MockStorage {
            objects: Mutex<HashMap<String, StoredObject>>,
            calls: Mutex<Vec<&'static str>>,
            next_etag: Mutex<u64>,
        }

        impl MockStorage {
            fn key(kb: &KbSlug, path: &ObjectPath) -> String {
                format!("{}/{}", kb.as_str(), path.as_str())
            }

            fn record(&self, call: &'static str) {
                self.calls.lock().expect("mutex not poisoned").push(call);
            }

            fn calls(&self) -> Vec<&'static str> {
                self.calls.lock().expect("mutex not poisoned").clone()
            }

            fn insert(&self, kb: &str, path: &str, bytes: impl Into<Bytes>, etag: &str) {
                self.objects.lock().expect("mutex not poisoned").insert(
                    format!("{kb}/{path}"),
                    StoredObject {
                        bytes: bytes.into(),
                        content_type: Some("text/markdown".to_string()),
                        etag: etag.to_string(),
                    },
                );
            }

            fn get_stored(&self, kb: &str, path: &str) -> Option<StoredObject> {
                self.objects
                    .lock()
                    .expect("mutex not poisoned")
                    .get(&format!("{kb}/{path}"))
                    .cloned()
            }
        }

        fn unavailable() -> StorageError {
            StorageError::BackendUnavailable {
                message: "mock storage method is not configured for this test".to_string(),
            }
        }

        fn object_meta(key: String, object: &StoredObject) -> ObjectMeta {
            ObjectMeta {
                key,
                size: object.bytes.len() as u64,
                last_modified: Some(1),
                content_type: object.content_type.clone(),
                etag: Some(object.etag.clone()),
            }
        }

        fn check_if_match(
            conditionals: &ConditionalHeaders,
            object: Option<&StoredObject>,
        ) -> Result<(), StorageError> {
            if let Some(if_match) = conditionals.if_match.as_deref()
                && object.is_none_or(|stored| stored.etag != if_match)
            {
                return Err(StorageError::PreconditionFailed);
            }
            Ok(())
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
                kb: &KbSlug,
                path: &ObjectPath,
                conditionals: ConditionalHeaders,
            ) -> Result<ObjectMeta, StorageError> {
                self.record("head_object");
                let key = Self::key(kb, path);
                let objects = self.objects.lock().expect("mutex not poisoned");
                let object = objects.get(&key);
                check_if_match(&conditionals, object)?;
                object
                    .map(|stored| object_meta(path.as_str().to_string(), stored))
                    .ok_or(StorageError::NotFound { key })
            }

            async fn get_object(
                &self,
                kb: &KbSlug,
                path: &ObjectPath,
                _range: Option<Vec<ByteRange>>,
                conditionals: ConditionalHeaders,
            ) -> Result<ObjectRead, StorageError> {
                self.record("get_object");
                let key = Self::key(kb, path);
                let objects = self.objects.lock().expect("mutex not poisoned");
                let object = objects
                    .get(&key)
                    .ok_or_else(|| StorageError::NotFound { key: key.clone() })?;
                check_if_match(&conditionals, Some(object))?;
                Ok(ObjectRead {
                    bytes: object.bytes.clone(),
                    meta: object_meta(path.as_str().to_string(), object),
                    content_range: None,
                })
            }

            async fn put_object(
                &self,
                kb: &KbSlug,
                path: &ObjectPath,
                bytes: Bytes,
                content_type: Option<&str>,
                conditionals: ConditionalHeaders,
            ) -> Result<PutOutcome, StorageError> {
                self.record("put_object");
                let key = Self::key(kb, path);
                let mut objects = self.objects.lock().expect("mutex not poisoned");
                check_if_match(&conditionals, objects.get(&key))?;

                let mut next_etag = self.next_etag.lock().expect("mutex not poisoned");
                *next_etag += 1;
                let etag = format!("\"etag-{next_etag}\"");
                objects.insert(
                    key,
                    StoredObject {
                        bytes,
                        content_type: content_type.map(str::to_string),
                        etag: etag.clone(),
                    },
                );
                Ok(PutOutcome { etag: Some(etag) })
            }

            async fn delete_object(
                &self,
                kb: &KbSlug,
                path: &ObjectPath,
                conditionals: ConditionalHeaders,
            ) -> Result<(), StorageError> {
                self.record("delete_object");
                let key = Self::key(kb, path);
                let mut objects = self.objects.lock().expect("mutex not poisoned");
                check_if_match(&conditionals, objects.get(&key))?;
                objects.remove(&key);
                Ok(())
            }

            async fn list_objects(
                &self,
                _kb: &KbSlug,
                _prefix: Option<&str>,
                _limit: u32,
                _cursor: Option<&str>,
            ) -> Result<ListResponse, StorageError> {
                Err(unavailable())
            }
        }

        fn kb_slug(value: &str) -> KbSlug {
            KbSlug::try_new(value).expect("valid KB slug")
        }

        fn declared_kbs(values: &[&str]) -> BTreeMap<String, KbSlug> {
            values
                .iter()
                .map(|value| ((*value).to_string(), kb_slug(value)))
                .collect()
        }

        fn test_state(storage: Arc<MockStorage>) -> WebDavState {
            let (indexer_tx, _rx) = mpsc::channel(1024);
            test_state_with_indexer_tx(storage, indexer_tx)
        }

        fn test_state_with_indexer_tx(
            storage: Arc<MockStorage>,
            indexer_tx: mpsc::Sender<notedthat_indexer::IndexEvent>,
        ) -> WebDavState {
            let storage: Arc<dyn Storage> = storage;
            WebDavState {
                username: Arc::new("user".to_string()),
                password: Arc::new("pass".to_string()),
                storage,
                declared_kbs: Arc::new(declared_kbs(&["notes", "scratch"])),
                indexer_tx,
            }
        }

        fn app(storage: Arc<MockStorage>) -> Router {
            Router::new()
                .fallback(any(|| async { "inner handler reached" }))
                .layer(from_fn_with_state(
                    test_state(storage),
                    intercept_write_methods,
                ))
        }

        fn app_with_indexer_tx(
            storage: Arc<MockStorage>,
            indexer_tx: mpsc::Sender<notedthat_indexer::IndexEvent>,
        ) -> Router {
            Router::new()
                .fallback(any(|| async { "inner handler reached" }))
                .layer(from_fn_with_state(
                    test_state_with_indexer_tx(storage, indexer_tx),
                    intercept_write_methods,
                ))
        }

        fn object_path(value: &str) -> ObjectPath {
            ObjectPath::try_from(value).expect("valid object path")
        }

        async fn response_body(resp: Response) -> String {
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            String::from_utf8(body.to_vec()).unwrap()
        }

        #[tokio::test]
        async fn test_get_passes_through() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage)
                .oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(response_body(resp).await, "inner handler reached");
        }

        #[tokio::test]
        async fn test_put_creates_and_returns_201_etag() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/notes/new.md")
                        .header("content-type", "text/markdown")
                        .body(Body::from("# New"))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::CREATED);
            assert_eq!(resp.headers().get("etag").unwrap(), "\"etag-1\"");
            assert_eq!(storage.calls(), vec!["head_object", "put_object"]);
        }

        #[tokio::test]
        async fn test_put_overwrite_returns_204() {
            let storage = Arc::new(MockStorage::default());
            storage.insert("notes", "old.md", Bytes::from_static(b"old"), "\"old\"");
            let resp = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/notes/old.md")
                        .body(Body::from("new"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        }

        #[tokio::test]
        async fn test_put_with_if_match_wrong_etag_returns_412() {
            let storage = Arc::new(MockStorage::default());
            storage.insert("notes", "old.md", Bytes::from_static(b"old"), "\"old\"");
            let resp = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/notes/old.md")
                        .header("if-match", "\"wrong\"")
                        .body(Body::from("new"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
        }

        #[tokio::test]
        async fn test_put_content_length_over_5gib_returns_413_before_reading_body() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/notes/huge.md")
                        .header(
                            "content-length",
                            (notedthat_write::MAX_UPLOAD_BYTES + 1).to_string(),
                        )
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
            assert!(storage.calls().is_empty());
        }

        #[tokio::test]
        async fn test_put_md_with_octet_stream_stored_as_text_markdown() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/notes/sniff.md")
                        .header("content-type", "application/octet-stream")
                        .body(Body::from("# Markdown"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
            let stored = storage.get_stored("notes", "sniff.md").unwrap();
            assert_eq!(stored.content_type.as_deref(), Some("text/markdown"));
        }

        #[tokio::test]
        async fn test_put_to_non_declared_kb_returns_403() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/unknown/file.md")
                        .body(Body::from("x"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        }

        #[tokio::test]
        async fn test_put_returns_503_with_retry_after_when_indexer_backpressure() {
            let storage = Arc::new(MockStorage::default());
            let (indexer_tx, _rx) = mpsc::channel(1);
            indexer_tx
                .try_send(notedthat_indexer::IndexEvent::Upsert {
                    kb: kb_slug("notes"),
                    object_key: object_path("queued.md"),
                    etag: "\"queued\"".to_string(),
                    mtime: 0,
                })
                .expect("queue accepts the prefilled event");

            let resp = app_with_indexer_tx(storage.clone(), indexer_tx)
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/notes/x.md")
                        .body(Body::from("x"))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
            let body = response_body(resp).await;
            assert!(body.contains("backend_unavailable"));
            assert!(body.contains("object stored"));
            assert!(storage.get_stored("notes", "x.md").is_some());
        }

        #[tokio::test]
        async fn test_delete_idempotent_returns_204() {
            let storage = Arc::new(MockStorage::default());
            let first = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("DELETE")
                        .uri("/notes/missing.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let second = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("DELETE")
                        .uri("/notes/missing.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(first.status(), StatusCode::NO_CONTENT);
            assert_eq!(second.status(), StatusCode::NO_CONTENT);
        }

        #[tokio::test]
        async fn test_delete_with_if_match_wrong_etag_returns_412() {
            let storage = Arc::new(MockStorage::default());
            storage.insert("notes", "delete.md", Bytes::from_static(b"old"), "\"old\"");
            let resp = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("DELETE")
                        .uri("/notes/delete.md")
                        .header("if-match", "\"wrong\"")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
        }

        #[tokio::test]
        async fn test_delete_returns_503_with_retry_after_when_indexer_backpressure() {
            let storage = Arc::new(MockStorage::default());
            storage.insert("notes", "y.md", Bytes::from_static(b"y"), "\"old\"");
            let (indexer_tx, _rx) = mpsc::channel(1);
            indexer_tx
                .try_send(notedthat_indexer::IndexEvent::Tombstone {
                    kb: kb_slug("notes"),
                    object_key: object_path("queued.md"),
                })
                .expect("queue accepts the prefilled event");

            let resp = app_with_indexer_tx(storage.clone(), indexer_tx)
                .oneshot(
                    HttpRequest::builder()
                        .method("DELETE")
                        .uri("/notes/y.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
            let body = response_body(resp).await;
            assert!(body.contains("backend_unavailable"));
            assert!(body.contains("deleted from storage; retry to clear from search index"));
            assert!(!body.contains("object stored; indexer queue full — retry to re-enqueue"));
            assert!(storage.get_stored("notes", "y.md").is_none());
        }

        #[tokio::test]
        async fn test_move_missing_destination_returns_400() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("MOVE")
                        .uri("/notes/source.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }

        #[tokio::test]
        async fn test_move_cross_server_returns_502_destination_different_server() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("MOVE")
                        .uri("/notes/source.md")
                        .header("host", "example.test")
                        .header("destination", "http://other.test/notes/dest.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
            assert!(
                response_body(resp)
                    .await
                    .contains("destination-different-server")
            );
        }

        #[tokio::test]
        async fn test_move_cross_kb_returns_403_cannot_modify_source() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("MOVE")
                        .uri("/notes/source.md")
                        .header("destination", "/scratch/dest.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
            assert!(response_body(resp).await.contains("cannot-modify-source"));
        }

        #[tokio::test]
        async fn test_move_single_object_returns_201_and_calls_commit_then_commit_delete() {
            let storage = Arc::new(MockStorage::default());
            storage.insert(
                "notes",
                "source.md",
                Bytes::from_static(b"source"),
                "\"source\"",
            );
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("MOVE")
                        .uri("/notes/source.md")
                        .header("destination", "/notes/dest.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
            assert_eq!(
                storage.calls(),
                vec!["get_object", "head_object", "put_object", "delete_object"]
            );
            assert!(storage.get_stored("notes", "source.md").is_none());
            assert!(storage.get_stored("notes", "dest.md").is_some());
        }

        #[tokio::test]
        async fn test_copy_single_object_returns_201_and_calls_only_commit() {
            let storage = Arc::new(MockStorage::default());
            storage.insert(
                "notes",
                "source.md",
                Bytes::from_static(b"source"),
                "\"source\"",
            );
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("COPY")
                        .uri("/notes/source.md")
                        .header("destination", "/notes/copy.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
            assert_eq!(
                storage.calls(),
                vec!["get_object", "head_object", "put_object"]
            );
            assert!(storage.get_stored("notes", "source.md").is_some());
            assert!(storage.get_stored("notes", "copy.md").is_some());
        }

        #[tokio::test]
        async fn test_copy_or_move_maps_destination_indexer_backpressure_to_503() {
            let storage = Arc::new(MockStorage::default());
            storage.insert("notes", "src.md", Bytes::from_static(b"src"), "\"src\"");
            let (indexer_tx, _rx) = mpsc::channel(1);
            indexer_tx
                .try_send(notedthat_indexer::IndexEvent::Upsert {
                    kb: kb_slug("notes"),
                    object_key: object_path("queued.md"),
                    etag: "\"queued\"".to_string(),
                    mtime: 0,
                })
                .expect("queue accepts the prefilled event");

            let resp = app_with_indexer_tx(storage.clone(), indexer_tx)
                .oneshot(
                    HttpRequest::builder()
                        .method("COPY")
                        .uri("/notes/src.md")
                        .header("destination", "/notes/dst.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
            let body = response_body(resp).await;
            assert!(body.contains("backend_unavailable"));
            assert!(
                body.contains("destination write succeeded but destination index event failed")
            );
            assert!(storage.get_stored("notes", "dst.md").is_some());
            assert!(storage.get_stored("notes", "src.md").is_some());
        }

        #[tokio::test]
        async fn test_move_returns_503_when_destination_upsert_backpressured() {
            let storage = Arc::new(MockStorage::default());
            storage.insert("notes", "src.md", Bytes::from_static(b"src"), "\"src\"");
            let (indexer_tx, _rx) = mpsc::channel(1);
            indexer_tx
                .try_send(notedthat_indexer::IndexEvent::Upsert {
                    kb: kb_slug("notes"),
                    object_key: object_path("queued.md"),
                    etag: "\"queued\"".to_string(),
                    mtime: 0,
                })
                .expect("queue accepts the prefilled event");

            let resp = app_with_indexer_tx(storage.clone(), indexer_tx)
                .oneshot(
                    HttpRequest::builder()
                        .method("MOVE")
                        .uri("/notes/src.md")
                        .header("destination", "/notes/dst.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
            assert!(response_body(resp).await.contains(
                "destination write succeeded but destination index event failed; source unchanged. Retry MOVE to re-enqueue destination index event."
            ));
            assert!(storage.get_stored("notes", "dst.md").is_some());
            assert!(storage.get_stored("notes", "src.md").is_some());
        }

        #[tokio::test]
        async fn test_move_returns_503_when_source_tombstone_backpressured_after_destination_put() {
            let storage = Arc::new(MockStorage::default());
            storage.insert("notes", "src.md", Bytes::from_static(b"src"), "\"src\"");
            let (indexer_tx, _rx) = mpsc::channel(2);
            indexer_tx
                .try_send(notedthat_indexer::IndexEvent::Upsert {
                    kb: kb_slug("notes"),
                    object_key: object_path("queued.md"),
                    etag: "\"queued\"".to_string(),
                    mtime: 0,
                })
                .expect("queue accepts the prefilled event");

            let resp = app_with_indexer_tx(storage.clone(), indexer_tx)
                .oneshot(
                    HttpRequest::builder()
                        .method("MOVE")
                        .uri("/notes/src.md")
                        .header("destination", "/notes/dst.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(resp.headers().get("retry-after").unwrap(), "5");
            assert!(response_body(resp).await.contains(
                "destination write succeeded and source deleted from storage, but source search-index tombstone failed — search may return stale entries for the source path until retry or reindex. Retry MOVE to re-enqueue the source tombstone; the destination write is idempotent."
            ));
            assert!(storage.get_stored("notes", "dst.md").is_some());
            assert!(storage.get_stored("notes", "src.md").is_none());
        }

        #[tokio::test]
        async fn test_source_not_found_returns_404() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("MOVE")
                        .uri("/notes/missing.md")
                        .header("destination", "/notes/dest.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        }

        #[tokio::test]
        async fn test_put_to_root_returns_400() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/")
                        .body(Body::from("x"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }

        #[tokio::test]
        async fn test_delete_to_kb_root_returns_400() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("DELETE")
                        .uri("/notes/")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }

        #[tokio::test]
        async fn test_move_collection_source_returns_403_no_collection_move() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("MOVE")
                        .uri("/notes/")
                        .header("destination", "/notes/dest.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
            assert!(response_body(resp).await.contains("no-collection-move"));
        }

        #[tokio::test]
        async fn encoded_uri_put_stores_decoded_key() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/notes/Untitled%201.canvas")
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::CREATED);
            // The key should be stored DECODED as "Untitled 1.canvas", not encoded as "Untitled%201.canvas"
            assert!(
                storage.get_stored("notes", "Untitled 1.canvas").is_some(),
                "expected decoded key 'Untitled 1.canvas' to be stored"
            );
            assert!(
                storage.get_stored("notes", "Untitled%201.canvas").is_none(),
                "encoded key 'Untitled%201.canvas' must not be stored"
            );
        }

        #[tokio::test]
        async fn encoded_uri_put_multi_segment() {
            // Multi-segment path with percent-encoded directory name proves split-before-decode:
            // raw '/' separates segments, then %20 in "my%20folder" decodes within that segment.
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/notes/my%20folder/notes.md")
                        .header("content-type", "text/markdown")
                        .body(Body::from("# Notes"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
            assert!(
                storage.get_stored("notes", "my folder/notes.md").is_some(),
                "expected decoded multi-segment key 'my folder/notes.md'"
            );
        }

        #[tokio::test]
        async fn encoded_uri_put_literal_percent_round_trips() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/notes/file%25.md")
                        .header("content-type", "text/markdown")
                        .body(Body::from("# Percent"))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert!(resp.status().is_success());
            assert!(
                storage.get_stored("notes", "file%.md").is_some(),
                "expected decoded literal percent key 'file%.md'"
            );
            assert!(
                storage.get_stored("notes", "file%25.md").is_none(),
                "encoded key 'file%25.md' must not be stored"
            );
        }

        #[tokio::test]
        #[allow(clippy::too_many_lines)]
        async fn edge_case_uri_segment_decoding_matrix() {
            struct Case {
                name: &'static str,
                raw_uri: &'static str,
                should_succeed: bool,
                stored_key: Option<&'static str>,
            }

            let cases = [
                Case {
                    name: "reject_empty_middle_segment",
                    raw_uri: "/notes//file.md",
                    should_succeed: false,
                    stored_key: None,
                },
                Case {
                    name: "reject_double_leading_slash",
                    raw_uri: "//notes/file.md",
                    should_succeed: false,
                    stored_key: None,
                },
                Case {
                    name: "reject_decoded_slash_in_segment",
                    raw_uri: "/notes/%2Ffile.md",
                    should_succeed: false,
                    stored_key: None,
                },
                Case {
                    name: "reject_decoded_parent_segment",
                    raw_uri: "/notes/%2E%2E/foo.md",
                    should_succeed: false,
                    stored_key: None,
                },
                Case {
                    name: "allow_leading_dot_filename",
                    raw_uri: "/notes/%2Efoo.md",
                    should_succeed: true,
                    stored_key: Some(".foo.md"),
                },
                Case {
                    name: "reject_decoded_backslash",
                    raw_uri: "/notes/file%5Cbad.md",
                    should_succeed: false,
                    stored_key: None,
                },
                Case {
                    name: "reject_decoded_nul",
                    raw_uri: "/notes/file%00.md",
                    should_succeed: false,
                    stored_key: None,
                },
                Case {
                    name: "allow_literal_percent_filename",
                    raw_uri: "/notes/file%25.md",
                    should_succeed: true,
                    stored_key: Some("file%.md"),
                },
                Case {
                    name: "allow_query_not_part_of_path",
                    raw_uri: "/notes/file%3Fname.md?ignored=1",
                    should_succeed: true,
                    stored_key: Some("file?name.md"),
                },
                Case {
                    name: "reject_decoded_slash_in_middle_segment",
                    raw_uri: "/notes/segment%2Fwith-slash/file.md",
                    should_succeed: false,
                    stored_key: None,
                },
            ];

            for case in cases {
                let storage = Arc::new(MockStorage::default());
                let resp = app(storage.clone())
                    .oneshot(
                        HttpRequest::builder()
                            .method("PUT")
                            .uri(case.raw_uri)
                            .header("content-type", "text/markdown")
                            .body(Body::from(case.name))
                            .unwrap(),
                    )
                    .await
                    .unwrap();

                if case.should_succeed {
                    assert!(
                        resp.status().is_success(),
                        "{} should succeed, got {}",
                        case.name,
                        resp.status()
                    );
                    let stored_key = case.stored_key.expect("success case stores a key");
                    assert!(
                        storage.get_stored("notes", stored_key).is_some(),
                        "{} should store decoded key {stored_key:?}",
                        case.name
                    );
                } else {
                    assert_eq!(
                        resp.status(),
                        StatusCode::BAD_REQUEST,
                        "{} should reject malformed segment",
                        case.name
                    );
                    assert!(
                        storage.calls().is_empty(),
                        "{} must not hit storage",
                        case.name
                    );
                }
            }
        }

        #[tokio::test]
        async fn encoded_uri_put_unicode() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/notes/%E6%97%A5%E6%9C%AC%E8%AA%9E.md")
                        .header("content-type", "text/markdown")
                        .body(Body::from("# Japanese"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
            assert!(
                storage.get_stored("notes", "日本語.md").is_some(),
                "expected decoded unicode key '日本語.md'"
            );
        }

        #[tokio::test]
        async fn encoded_uri_put_reserved_chars() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/notes/file%23with%3Fchars.md")
                        .header("content-type", "text/markdown")
                        .body(Body::from("# Reserved"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
            assert!(
                storage.get_stored("notes", "file#with?chars.md").is_some(),
                "expected decoded key 'file#with?chars.md'"
            );
        }

        #[tokio::test]
        async fn encoded_uri_put_non_utf8_returns_400() {
            let storage = Arc::new(MockStorage::default());
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("PUT")
                        .uri("/notes/%FF%FE.md")
                        .header("content-type", "text/markdown")
                        .body(Body::from("# Bad"))
                        .unwrap(),
                )
                .await
                .unwrap();
            // Non-UTF-8 percent sequences must be rejected with 400
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            // Consistent with existing write-method 400s: no x-request-id header
            assert!(
                resp.headers().get("x-request-id").is_none(),
                "write-method 400 must not include x-request-id"
            );
            // Nothing was stored
            assert!(storage.calls().is_empty());
        }

        #[tokio::test]
        async fn encoded_destination_move_decodes_key() {
            let storage = Arc::new(MockStorage::default());
            storage.insert(
                "notes",
                "source.md",
                Bytes::from_static(b"source content"),
                "\"etag-source\"",
            );
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("MOVE")
                        .uri("/notes/source.md")
                        .header("destination", "/notes/renamed%20file.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
            // Destination key must be decoded
            assert!(
                storage.get_stored("notes", "renamed file.md").is_some(),
                "expected decoded destination key 'renamed file.md'"
            );
            // Source must be gone (MOVE deletes source)
            assert!(
                storage.get_stored("notes", "source.md").is_none(),
                "MOVE source should be deleted"
            );
        }

        #[tokio::test]
        async fn encoded_destination_copy_decodes_key() {
            let storage = Arc::new(MockStorage::default());
            storage.insert(
                "notes",
                "source.md",
                Bytes::from_static(b"source content"),
                "\"etag-source\"",
            );
            let resp = app(storage.clone())
                .oneshot(
                    HttpRequest::builder()
                        .method("COPY")
                        .uri("/notes/source.md")
                        .header("destination", "/notes/renamed%20file.md")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
            // Destination key must be decoded
            assert!(
                storage.get_stored("notes", "renamed file.md").is_some(),
                "expected decoded destination key 'renamed file.md'"
            );
            // Source must still exist (COPY keeps source)
            assert!(
                storage.get_stored("notes", "source.md").is_some(),
                "COPY source should still exist"
            );
        }

        #[tokio::test]
        async fn destination_with_fragment_returns_400_before_uri_parse() {
            let storage = Arc::new(MockStorage::default());
            storage.insert(
                "notes",
                "source.md",
                Bytes::from_static(b"source content"),
                "\"etag-source\"",
            );
            let resp = app(storage)
                .oneshot(
                    HttpRequest::builder()
                        .method("MOVE")
                        .uri("/notes/source.md")
                        .header("host", "localhost")
                        .header("destination", "http://localhost/notes/file.md#fragment")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }
    }
}

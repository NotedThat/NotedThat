//! Axum router builder and HTTP handlers for the `NotedThat` API.

use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::{auth_middleware, extract_request_id};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderName, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use notedthat_core::{
    ConditionalHeaders, Error as CoreError, KbSlug, LineIndex, ObjectPath, StorageError,
    parse_line_range_header, parse_range_header,
};
use serde::Deserialize;
use std::fmt::Write;
use std::time::{Duration, UNIX_EPOCH};
use tower::ServiceBuilder;
use tower_http::request_id::{
    MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

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
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/knowledgebases", get(list_kbs))
        .route("/v1/knowledgebases/{kb_slug}", get(list_objects))
        .route(
            "/v1/knowledgebases/{kb_slug}/search",
            axum::routing::post(crate::search_route::search_kb).layer(
                axum::extract::DefaultBodyLimit::max(crate::search_route::SEARCH_BODY_MAX_BYTES),
            ),
        )
        .route(
            "/v1/knowledgebases/{kb_slug}/{*object_path}",
            get(get_object)
                .head(head_object)
                .put(put_object)
                .delete(delete_object),
        )
        .layer(
            ServiceBuilder::new()
                .layer(DefaultBodyLimit::max(body_limit_usize(MAX_BODY_BYTES)))
                .layer(SetRequestIdLayer::new(
                    request_id_header.clone(),
                    MakeRequestUuidV7,
                ))
                .layer(PropagateRequestIdLayer::new(request_id_header))
                .layer(TraceLayer::new_for_http())
                .layer(from_fn_with_state(state.clone(), auth_middleware)),
        )
        .with_state(state)
}

// ─── Health probes ──────────────────────────────────────────────────────────

/// GET /healthz — liveness probe (no auth required).
async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

/// GET /readyz — readiness probe (no auth required, static 200 in M2).
async fn readyz() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

// ─── KB list ────────────────────────────────────────────────────────────────

/// GET /v1/knowledgebases — list all declared knowledge bases.
async fn list_kbs(State(state): State<AppState>) -> impl IntoResponse {
    let slugs: Vec<&str> = state.declared_kbs.keys().map(String::as_str).collect();
    Json(serde_json::json!({"knowledgebases": slugs}))
}

// ─── Object list ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ListQuery {
    prefix: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

/// `GET /v1/knowledgebases/{kb_slug}` — list objects in a KB.
async fn list_objects(
    State(state): State<AppState>,
    Path(kb_slug): Path<String>,
    Query(q): Query<ListQuery>,
    req: Request,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let kb = lookup_kb(&state, &kb_slug).map_err(&err)?;
    let limit = q.limit.filter(|&limit| limit > 0).unwrap_or(100).min(1000);

    let result = state
        .storage
        .list_objects(&kb, q.prefix.as_deref(), limit, q.cursor.as_deref())
        .await
        .map_err(|error| err(ApiError::Storage(error)))?;

    Ok(Json(serde_json::json!({
        "objects": result.objects,
        "truncated": result.truncated,
        "next_cursor": result.next_cursor,
    })))
}

// ─── Object CRUD ────────────────────────────────────────────────────────────

/// `HEAD /v1/knowledgebases/{kb_slug}/{*object_path}` — return metadata, no body.
async fn head_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let kb = lookup_kb(&state, &kb_slug).map_err(&err)?;
    let path = parse_path(&object_path).map_err(&err)?;

    // Range header intentionally NOT forwarded on HEAD (RFC 7233 §3.1).
    // Scope-OUT: Conditional writes (`If-Match`, `If-None-Match`) that succeed at
    // S3 but return 503 at the indexer queue leave a naive retry in a state
    // where S3 may return 412 because the object now exists or its ETag changed.
    // Clients using conditional headers MUST detect the 503 → 412 sequence and
    // either accept the ghost state or use a stronger consistency mechanism. v1
    // does not provide automatic replay/repair for conditional-write ghost
    // states.
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

    let meta = state
        .storage
        .head_object(&kb, &path, conditionals)
        .await
        .map_err(|e| err(ApiError::from(e)))?;

    let mut builder = Response::builder().status(StatusCode::OK);

    if let Some(ct) = &meta.content_type {
        builder = builder.header("content-type", ct.as_str());
    }
    if let Some(etag) = &meta.etag {
        builder = builder.header("etag", etag.as_str());
    }
    if let Some(last_modified) = meta
        .last_modified
        .and_then(|seconds| u64::try_from(seconds).ok())
        .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds))
    {
        builder = builder.header("last-modified", httpdate::fmt_http_date(last_modified));
    }
    // Content-Length from metadata size, not body length (HEAD has no body).
    builder = builder.header("content-length", meta.size.to_string());

    Ok(builder
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

/// `GET /v1/knowledgebases/{kb_slug}/{*object_path}` — download an object.
async fn get_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let kb = lookup_kb(&state, &kb_slug).map_err(&err)?;
    let path = parse_path(&object_path).map_err(&err)?;
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

    let range = match req.headers().get(axum::http::header::RANGE) {
        None => None,
        Some(raw) => {
            let raw_str = raw
                .to_str()
                .map_err(|_| err(ApiError::MalformedRange("non-UTF-8 Range header".into())))?;
            let parsed = parse_range_header(raw_str)
                .map_err(|_| err(ApiError::MalformedRange(raw_str.to_owned())))?;
            if parsed.unit == "lines" {
                return serve_line_range_read(
                    &state,
                    &kb,
                    &path,
                    raw_str,
                    conditionals,
                    &request_id,
                )
                .await;
            } else if parsed.unit == "bytes" && !parsed.ranges.is_empty() {
                Some(parsed.ranges)
            } else {
                None
            }
        }
    };

    let read = state
        .storage
        .get_object(&kb, &path, range, conditionals)
        .await
        .map_err(|error| match error {
            StorageError::NotFound { .. } => err(ApiError::Core(CoreError::NotFound {
                resource: path.as_str().to_string(),
            })),
            other => err(ApiError::from(other)),
        })?;
    let content_type = read
        .meta
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");

    let status = if read.content_range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut builder = Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(axum::http::header::CONTENT_LENGTH, read.bytes.len());

    if let Some(etag) = &read.meta.etag {
        builder = builder.header(axum::http::header::ETAG, etag.as_str());
    }
    if let Some(last_modified) = read
        .meta
        .last_modified
        .and_then(|seconds| u64::try_from(seconds).ok())
        .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds))
    {
        builder = builder.header(
            axum::http::header::LAST_MODIFIED,
            httpdate::fmt_http_date(last_modified),
        );
    }
    if let Some(content_range) = &read.content_range {
        builder = builder.header(axum::http::header::CONTENT_RANGE, content_range.as_str());
    }

    let resp = builder
        .body(Body::from(read.bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());

    Ok(resp)
}

async fn serve_line_range_read(
    state: &AppState,
    kb: &KbSlug,
    path: &ObjectPath,
    raw_range: &str,
    conditionals: ConditionalHeaders,
    request_id: &str,
) -> Result<Response, ApiErrorResponse> {
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.to_string(),
    };

    let line_range = parse_line_range_header(raw_range)
        .map_err(|_| err(ApiError::MalformedRange(raw_range.to_owned())))?;
    let read = state
        .storage
        .get_object(kb, path, None, conditionals)
        .await
        .map_err(|error| match error {
            StorageError::NotFound { .. } => err(ApiError::Core(CoreError::NotFound {
                resource: path.as_str().to_string(),
            })),
            other => err(ApiError::from(other)),
        })?;

    let idx = LineIndex::from_bytes(&read.bytes);
    let byte_range = idx.byte_range(&line_range).ok_or_else(|| {
        err(ApiError::LineRangeNotSatisfiable {
            line_total: idx.total_lines,
            byte_total: idx.total_bytes,
        })
    })?;
    let sliced = read
        .bytes
        .slice(byte_range.start as usize..byte_range.end as usize);
    let content_type = read
        .meta
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");

    let mut builder = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(axum::http::header::CONTENT_LENGTH, sliced.len());

    if let Some(etag) = &read.meta.etag {
        builder = builder.header(axum::http::header::ETAG, etag.as_str());
    }
    if let Some(last_modified) = read
        .meta
        .last_modified
        .and_then(|seconds| u64::try_from(seconds).ok())
        .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds))
    {
        builder = builder.header(
            axum::http::header::LAST_MODIFIED,
            httpdate::fmt_http_date(last_modified),
        );
    }

    let content_range_value = idx.content_range_string(&line_range);
    builder = builder.header("Content-Range", content_range_value);

    let inclusive_end = byte_range.end.saturating_sub(1);
    let x_content_range_bytes =
        format!("{}-{}/{}", byte_range.start, inclusive_end, idx.total_bytes);
    builder = builder.header("X-Content-Range-Bytes", x_content_range_bytes);

    Ok(builder
        .body(Body::from(sliced))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

/// `PUT /v1/knowledgebases/{kb_slug}/{*object_path}` — upload an object.
async fn put_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let kb = lookup_kb(&state, &kb_slug).map_err(&err)?;
    let path = parse_path(&object_path).map_err(&err)?;

    let content_length = req
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if let Some(content_length) = content_length
        && content_length > state.max_body_size
    {
        return Err(err(ApiError::Core(CoreError::PayloadTooLarge {
            size: content_length,
            limit: state.max_body_size,
        })));
    }

    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

    let body_bytes: Bytes =
        axum::body::to_bytes(req.into_body(), body_limit_usize(state.max_body_size))
            .await
            .map_err(|_| {
                err(ApiError::Core(CoreError::PayloadTooLarge {
                    size: state.max_body_size + 1,
                    limit: state.max_body_size,
                }))
            })?;

    if body_bytes.len() as u64 > state.max_body_size {
        return Err(err(ApiError::Core(CoreError::PayloadTooLarge {
            size: body_bytes.len() as u64,
            limit: state.max_body_size,
        })));
    }

    let outcome = notedthat_write::commit(
        state.storage.as_ref(),
        &state.indexer_tx,
        &kb,
        &path,
        body_bytes,
        content_type.as_deref(),
        conditionals,
    )
    .await
    .map_err(|e| err(ApiError::from(e)))?;

    let location = format!(
        "/v1/knowledgebases/{kb_slug}/{}",
        percent_encode_path(path.as_str())
    );
    let mut builder = Response::builder()
        .status(StatusCode::CREATED)
        .header("location", location);
    if let Some(etag) = &outcome.etag {
        builder = builder.header(axum::http::header::ETAG, etag.as_str());
    }
    let resp = builder
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    Ok(resp)
}

/// `DELETE /v1/knowledgebases/{kb_slug}/{*object_path}` — delete an object (idempotent).
async fn delete_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let kb = lookup_kb(&state, &kb_slug).map_err(&err)?;
    let path = parse_path(&object_path).map_err(&err)?;
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

    notedthat_write::commit_delete(
        state.storage.as_ref(),
        &state.indexer_tx,
        &kb,
        &path,
        conditionals,
    )
    .await
    .map_err(|e| err(ApiError::from(e)))?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Look up a [`KbSlug`] from `state.declared_kbs`, returning 404 if not found.
pub(crate) fn lookup_kb(state: &AppState, slug: &str) -> Result<KbSlug, ApiError> {
    state.declared_kbs.get(slug).cloned().ok_or_else(|| {
        ApiError::Core(CoreError::NotFound {
            resource: format!("KB '{slug}' not declared"),
        })
    })
}

/// Parse an [`ObjectPath`] from the raw path string extracted from the URL.
fn parse_path(raw: &str) -> Result<ObjectPath, ApiError> {
    ObjectPath::try_from_str(raw).map_err(ApiError::Core)
}

fn body_limit_usize(max_body_size: u64) -> usize {
    usize::try_from(max_body_size.saturating_add(1)).unwrap_or(usize::MAX)
}

fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for &byte in path.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                encoded.push(char::from(byte));
            }
            _ => {
                let _ = write!(&mut encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

#[cfg(test)]
mod line_range_get {
    use super::*;
    use axum::body::{Body, to_bytes};
    use notedthat_core::Storage;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tower::util::ServiceExt;

    const KB: &str = "notes";
    const TOKEN: &str = "test-token-abc";

    fn twenty_line_markdown() -> String {
        (1..=20).map(|line| format!("line {line:02}\n")).collect()
    }

    fn markdown_lines(first: u32, last: u32) -> String {
        (first..=last)
            .map(|line| format!("line {line:02}\n"))
            .collect()
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
                    .uri(format!("/v1/knowledgebases/{KB}/ranges.md"))
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
            (1..=10).map(|line| format!("line {line:02}\n")).collect()
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
    use notedthat_core::Storage;
    use notedthat_indexer::IndexEvent;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tower::util::ServiceExt;

    const KB: &str = "notes";
    const TOKEN: &str = "test-token-abc";

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
                    .uri(format!("/v1/knowledgebases/{KB}/cond.md"))
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
                    .uri(format!("/v1/knowledgebases/{KB}/cond.md"))
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
                    .uri(format!("/v1/knowledgebases/{KB}/to-delete.md"))
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

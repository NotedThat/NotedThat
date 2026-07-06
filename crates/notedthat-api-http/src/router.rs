//! Axum router builder and HTTP handlers for the `NotedThat` API.

use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::{auth_middleware, extract_request_id};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::handler::Handler;
use axum::http::{HeaderName, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, Error as CoreError, KbSlug, LineIndex, ObjectPath, StorageError,
    parse_line_range_header, parse_range_header,
};
use notedthat_write::PatchMode;
use serde::{Deserialize, Serialize};
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
const REPLACE_IF_MATCH_ERROR: &str =
    "If-Match is required for POST replace and must be a single strong ETag";

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
                .delete(delete_object)
                .patch(patch_object.layer(DefaultBodyLimit::disable()))
                .post(post_object.layer(DefaultBodyLimit::disable())),
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

#[derive(Deserialize)]
struct ReplaceBody {
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Serialize)]
struct ReplaceResponse {
    etag: String,
    match_count: u64,
    total_bytes: u64,
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
    let range_start = usize::try_from(byte_range.start).map_err(|_| {
        err(ApiError::Core(CoreError::InvalidInput {
            message: "line range start does not fit usize".to_string(),
        }))
    })?;
    let range_end = usize::try_from(byte_range.end).map_err(|_| {
        err(ApiError::Core(CoreError::InvalidInput {
            message: "line range end does not fit usize".to_string(),
        }))
    })?;
    let sliced = read.bytes.slice(range_start..range_end);
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

async fn patch_object(
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
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if let Some(content_length) = content_length
        && content_length > state.max_patchable_size
    {
        return Err(err(ApiError::Core(CoreError::PayloadTooLarge {
            size: content_length,
            limit: state.max_patchable_size,
        })));
    }

    if let Some(if_match) = req
        .headers()
        .get(axum::http::header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        && (if_match == "*" || if_match.contains(','))
    {
        return Err(err(ApiError::Core(CoreError::InvalidInput {
            message: "If-Match: * and multi-value If-Match not supported on PATCH in v1; provide a single strong ETag".into(),
        })));
    }

    let content_range_header = req
        .headers()
        .get(axum::http::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let nt_patch_mode = req
        .headers()
        .get(axum::http::HeaderName::from_static("nt-patch-mode"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_lowercase);
    let content_type = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

    let body_bytes: Bytes =
        axum::body::to_bytes(req.into_body(), body_limit_usize(state.max_patchable_size))
            .await
            .map_err(|_| {
                err(ApiError::Core(CoreError::PayloadTooLarge {
                    size: state.max_patchable_size + 1,
                    limit: state.max_patchable_size,
                }))
            })?;

    if body_bytes.len() as u64 > state.max_patchable_size {
        return Err(err(ApiError::Core(CoreError::PayloadTooLarge {
            size: body_bytes.len() as u64,
            limit: state.max_patchable_size,
        })));
    }

    let patch_mode = patch_mode_from_headers(
        nt_patch_mode.as_deref(),
        content_range_header.as_deref(),
        body_bytes,
    )
    .map_err(|error| err(ApiError::Core(error)))?;

    let outcome = notedthat_write::patch(
        state.storage.as_ref(),
        &state.indexer_tx,
        notedthat_write::patch::PatchRequest {
            kb: &kb,
            path: &path,
            patch_mode,
            caller_conditionals: conditionals,
            max_patchable_size: state.max_patchable_size,
            caller_content_type: content_type.as_deref(),
        },
    )
    .await
    .map_err(|e| err(ApiError::from(e)))?;

    let location = format!(
        "/v1/knowledgebases/{kb_slug}/{}",
        percent_encode_path(path.as_str())
    );
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::LOCATION, location);
    if let Some(etag) = &outcome.etag {
        builder = builder.header(axum::http::header::ETAG, etag.as_str());
    }

    Ok(builder
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

/// Dispatcher for POST on the object catch-all. Only `replace/<target-path>` is a defined
/// action; every other POST returns 404 `not_found`.
async fn post_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let Some(target_path) = object_path.strip_prefix("replace/") else {
        return Err(err(ApiError::Core(CoreError::NotFound {
            resource: format!(
                "POST on '{object_path}' is not a defined action (supported actions: 'replace/<path>')"
            ),
        })));
    };

    replace_object(State(state), kb_slug, target_path.to_string(), req).await
}

async fn replace_object(
    State(state): State<AppState>,
    kb_slug: String,
    object_path: String,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let kb = lookup_kb(&state, &kb_slug).map_err(&err)?;
    let path = parse_path(&object_path).map_err(&err)?;
    let json_cap_u64 = state
        .max_patchable_size
        .saturating_mul(2)
        .saturating_add(4096);

    let content_length = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if let Some(content_length) = content_length
        && content_length > json_cap_u64
    {
        return Err(err(ApiError::Core(CoreError::PayloadTooLarge {
            size: content_length,
            limit: json_cap_u64,
        })));
    }

    let conditionals = replace_conditionals(&req).map_err(&err)?;
    let body_bytes: Bytes = axum::body::to_bytes(req.into_body(), body_limit_usize(json_cap_u64))
        .await
        .map_err(|_| {
            err(ApiError::Core(CoreError::PayloadTooLarge {
                size: json_cap_u64.saturating_add(1),
                limit: json_cap_u64,
            }))
        })?;

    if body_bytes.len() as u64 > json_cap_u64 {
        return Err(err(ApiError::Core(CoreError::PayloadTooLarge {
            size: body_bytes.len() as u64,
            limit: json_cap_u64,
        })));
    }

    let body = serde_json::from_slice::<ReplaceBody>(&body_bytes).map_err(|error| {
        err(ApiError::Core(CoreError::InvalidInput {
            message: format!("malformed replace JSON body: {error}"),
        }))
    })?;
    if body.old_string.is_empty() {
        return Err(err(ApiError::Core(CoreError::InvalidInput {
            message: "old_string must be non-empty".into(),
        })));
    }

    let outcome = notedthat_write::replace(
        state.storage.as_ref(),
        &state.indexer_tx,
        notedthat_write::ReplaceRequest {
            kb: &kb,
            path: &path,
            old_string: &body.old_string,
            new_string: &body.new_string,
            replace_all: body.replace_all,
            caller_conditionals: conditionals,
            max_patchable_size: state.max_patchable_size,
            caller_content_type: None,
        },
    )
    .await
    .map_err(|e| err(ApiError::from(e)))?;

    let meta = state
        .storage
        .head_object(&kb, &path, ConditionalHeaders::default())
        .await
        .map_err(|e| err(ApiError::from(e)))?;
    let etag = outcome.put_outcome.etag.or(meta.etag).unwrap_or_default();
    let response = ReplaceResponse {
        etag: etag.clone(),
        match_count: outcome.match_count,
        total_bytes: meta.size,
    };
    let content_location = format!(
        "/v1/knowledgebases/{kb_slug}/{}",
        percent_encode_path(path.as_str())
    );

    Ok((
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_LOCATION, content_location),
            (axum::http::header::ETAG, etag),
        ],
        Json(response),
    )
        .into_response())
}

fn replace_conditionals(req: &Request) -> Result<ConditionalHeaders, ApiError> {
    let if_match = req
        .headers()
        .get(axum::http::header::IF_MATCH)
        .and_then(|value| value.to_str().ok());
    if if_match.is_none()
        || if_match == Some("*")
        || if_match.is_some_and(|value| value.contains(','))
    {
        return Err(ApiError::Core(CoreError::InvalidInput {
            message: REPLACE_IF_MATCH_ERROR.into(),
        }));
    }

    Ok(ConditionalHeaders::from_header_map(req.headers()))
}

fn patch_mode_from_headers(
    nt_patch_mode: Option<&str>,
    content_range_header: Option<&str>,
    body_bytes: Bytes,
) -> Result<PatchMode, CoreError> {
    match (nt_patch_mode, content_range_header) {
        (Some("append"), None) => Ok(PatchMode::Append { body: body_bytes }),
        (Some("append"), Some(_)) => Err(CoreError::InvalidInput {
            message: "NT-Patch-Mode: append is mutually exclusive with Content-Range".into(),
        }),
        (None, Some(content_range)) => parse_patch_content_range(content_range, body_bytes)
            .map_err(|message| CoreError::InvalidInput { message }),
        (None, None) => Err(CoreError::InvalidInput {
            message: "PATCH requires either Content-Range or NT-Patch-Mode: append".into(),
        }),
        (Some(mode), _) => Err(CoreError::InvalidInput {
            message: format!("Unknown NT-Patch-Mode value: {mode}; only 'append' is supported"),
        }),
    }
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

fn parse_patch_content_range(content_range: &str, body: Bytes) -> Result<PatchMode, String> {
    let (unit, range_part) = content_range
        .split_once(' ')
        .ok_or_else(|| format!("malformed Content-Range: {content_range}"))?;
    let (range_str, _total) = range_part
        .split_once('/')
        .ok_or_else(|| format!("malformed Content-Range: {content_range}"))?;

    match unit {
        "bytes" => {
            let (start, end) = range_str
                .split_once('-')
                .ok_or_else(|| format!("malformed Content-Range bytes range: {range_str}"))?;
            let first = start
                .parse::<u64>()
                .map_err(|_| format!("invalid byte range start: {start}"))?;
            let last = end
                .parse::<u64>()
                .map_err(|_| format!("invalid byte range end: {end}"))?;
            Ok(PatchMode::Bytes {
                range: ByteRange::FromStart { first, last },
                body,
            })
        }
        "lines" => {
            let line_range = parse_line_range_header(&format!("lines={range_str}"))
                .map_err(|_| format!("malformed Content-Range lines range: {range_str}"))?;
            Ok(PatchMode::Lines {
                range: line_range,
                body,
            })
        }
        other => Err(format!(
            "Content-Range unit must be 'bytes' or 'lines', got: {other}"
        )),
    }
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
mod patch_route {
    use super::*;
    use async_trait::async_trait;
    use axum::body::{Body, to_bytes};
    use notedthat_core::Storage;
    use notedthat_core::{KbManifest, ListResponse, ObjectMeta, ObjectRead, PutOutcome};
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
            .uri(format!("/v1/knowledgebases/{KB}/{OBJECT_PATH}"))
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
            &format!("/v1/knowledgebases/{KB}/{OBJECT_PATH}")
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
                    .uri(format!("/v1/knowledgebases/{KB}/missing.md"))
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
    use notedthat_core::Storage;
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
    use notedthat_core::Storage;
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
                    .uri(format!("/v1/knowledgebases/{KB}/{path}"))
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
                    .uri(format!("/v1/knowledgebases/{KB}/{path}"))
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
                    .uri(format!("/v1/knowledgebases/{KB}/replace/{path}"))
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
                    .uri(format!("/v1/knowledgebases/{KB}/replace/bar.md"))
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
                    .uri(format!("/v1/knowledgebases/{KB}/replace/delete.md"))
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
                    .uri(format!("/v1/knowledgebases/{KB}/foo.md"))
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

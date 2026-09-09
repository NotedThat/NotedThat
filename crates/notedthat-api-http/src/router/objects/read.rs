use super::super::helpers::parse_path;
use crate::authz::KbAccess;
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use notedthat_core::{
    ConditionalHeaders, Error as CoreError, KbSlug, LineIndex, ObjectPath, StorageError, Verb,
    parse_line_range_header, parse_range_header,
};
use std::time::{Duration, UNIX_EPOCH};

pub(in crate::router) async fn head_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let path = parse_path(&object_path).map_err(&err)?;
    let access = KbAccess::resolve(&state, &kb_slug, &req).map_err(&err)?;
    // Authorize before any storage call, so a denial costs nothing and reads
    // identically whether or not the object exists.
    access.require(Verb::Read, path.as_str()).map_err(&err)?;
    let kb = access.kb().clone();

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

pub(in crate::router) async fn get_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let path = parse_path(&object_path).map_err(&err)?;
    let access = KbAccess::resolve(&state, &kb_slug, &req).map_err(&err)?;
    // Authorize before any storage call, so a denial costs nothing and reads
    // identically whether or not the object exists.
    access.require(Verb::Read, path.as_str()).map_err(&err)?;
    let kb = access.kb().clone();
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

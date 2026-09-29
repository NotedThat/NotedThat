use super::super::helpers::parse_path;
use crate::authz::KbAccess;
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;
use crate::validators::with_validators;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::{ACCEPT_RANGES, IF_RANGE};
use axum::response::{IntoResponse, Response};
use notedthat_core::{
    ConditionalHeaders, Error as CoreError, KbSlug, LineIndex, ObjectPath, ObjectRead,
    StorageError, Verb, if_range_matches, parse_line_range_header, parse_range_header,
};
use std::borrow::Cow;

fn normalize_content_type(content_type: &str) -> Cow<'_, str> {
    let Ok(media_type) = content_type.parse::<mime::Mime>() else {
        return Cow::Borrowed(content_type);
    };

    let subtype = media_type.subtype().as_str();
    let textual = media_type.type_() == mime::TEXT
        || (media_type.type_() == mime::APPLICATION
            && (subtype == "json"
                || media_type
                    .suffix()
                    .is_some_and(|suffix| suffix.as_str() == "json")
                || subtype == "xml"
                || media_type
                    .suffix()
                    .is_some_and(|suffix| suffix.as_str() == "xml")));

    if textual && media_type.get_param(mime::CHARSET).is_none() {
        Cow::Owned(format!("{content_type}; charset=utf-8"))
    } else {
        Cow::Borrowed(content_type)
    }
}

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
        let content_type = normalize_content_type(ct);
        builder = builder.header("content-type", content_type.as_ref());
    }
    builder = with_validators(builder, meta.etag.as_deref(), meta.last_modified)
        .header(ACCEPT_RANGES, "bytes");
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
    // A non-UTF-8 `If-Range` can match nothing, so it reads as a stale validator.
    let if_range = req
        .headers()
        .get(IF_RANGE)
        .map(|value| value.to_str().unwrap_or_default().to_owned());

    let range = match req.headers().get(axum::http::header::RANGE) {
        None => None,
        Some(raw) => {
            let raw_str = raw
                .to_str()
                .map_err(|_| err(ApiError::MalformedRange("non-UTF-8 Range header".into())))?;
            // The parser's reason rides along: a refused range set must say
            // it was the count of ranges, not the syntax (D57).
            let parsed = parse_range_header(raw_str)
                .map_err(|e| err(ApiError::MalformedRange(format!("{raw_str}: {e}"))))?;
            if parsed.unit == "lines" {
                return serve_line_range_read(
                    &state,
                    &kb,
                    &path,
                    raw_str,
                    conditionals,
                    if_range.as_deref(),
                    &request_id,
                )
                .await;
            }
            parsed.range
        }
    };

    let not_found = |error: StorageError| match error {
        StorageError::NotFound { .. } => err(ApiError::Core(CoreError::NotFound {
            resource: path.as_str().to_string(),
        })),
        other => err(ApiError::from(other)),
    };
    let ranged = range.is_some();
    let read = state
        .storage
        .get_object(&kb, &path, range, conditionals.clone())
        .await;

    // RFC 9110 §13.1.5: `If-Range` makes the range conditional on the validator still
    // describing the object. It is checked against the metadata that came back with the
    // slice — the version the bytes were cut from, so no write can slip in between. On
    // a mismatch the slice is discarded and the whole current object served as a 200.
    let Some(if_range) = if_range.filter(|_| ranged) else {
        return Ok(full_or_partial_response(read.map_err(not_found)?));
    };
    let unsatisfiable = match read {
        Ok(read)
            if if_range_matches(
                &if_range,
                read.meta.etag.as_deref(),
                read.meta.last_modified,
            ) =>
        {
            return Ok(full_or_partial_response(read));
        }
        Ok(_) => None,
        // A 416 stands only if the validator matches; a stale one gets the full body.
        Err(error @ StorageError::RangeNotSatisfiable { .. }) => Some(error),
        Err(error) => return Err(not_found(error)),
    };
    let full = state
        .storage
        .get_object(&kb, &path, None, conditionals)
        .await
        .map_err(not_found)?;
    if let Some(error) = unsatisfiable
        && if_range_matches(
            &if_range,
            full.meta.etag.as_deref(),
            full.meta.last_modified,
        )
    {
        return Err(not_found(error));
    }
    Ok(full_or_partial_response(full))
}

/// The response for a byte read: `206` with `Content-Range` when the backend served a
/// slice, `200` with the whole object otherwise.
fn full_or_partial_response(read: ObjectRead) -> Response {
    let content_type = normalize_content_type(
        read.meta
            .content_type
            .as_deref()
            .unwrap_or("application/octet-stream"),
    );

    let status = if read.content_range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut builder = Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, content_type.as_ref())
        .header(axum::http::header::CONTENT_LENGTH, read.bytes.len())
        .header(ACCEPT_RANGES, "bytes");

    builder = with_validators(builder, read.meta.etag.as_deref(), read.meta.last_modified);
    if let Some(content_range) = &read.content_range {
        builder = builder.header(axum::http::header::CONTENT_RANGE, content_range.as_str());
    }

    builder
        .body(Body::from(read.bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn serve_line_range_read(
    state: &AppState,
    kb: &KbSlug,
    path: &ObjectPath,
    raw_range: &str,
    conditionals: ConditionalHeaders,
    if_range: Option<&str>,
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

    // The whole object is already in hand, so a stale `If-Range` simply serves it.
    if if_range.is_some_and(|if_range| {
        !if_range_matches(if_range, read.meta.etag.as_deref(), read.meta.last_modified)
    }) {
        return Ok(full_or_partial_response(read));
    }

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
    let content_type = normalize_content_type(
        read.meta
            .content_type
            .as_deref()
            .unwrap_or("application/octet-stream"),
    );

    let mut builder = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(axum::http::header::CONTENT_TYPE, content_type.as_ref())
        .header(axum::http::header::CONTENT_LENGTH, sliced.len());

    builder = with_validators(builder, read.meta.etag.as_deref(), read.meta.last_modified);

    let content_range_value = idx.content_range_string(&line_range);
    builder = builder.header("Content-Range", content_range_value);

    // An inclusive end cannot spell an empty slice: an insert point says
    // `*/<total>` and names its offset in `X-Insert-Offset` instead (D74).
    if byte_range.is_empty() {
        builder = builder
            .header("X-Content-Range-Bytes", format!("*/{}", idx.total_bytes))
            .header("X-Insert-Offset", byte_range.start);
    } else {
        let x_content_range_bytes = format!(
            "{}-{}/{}",
            byte_range.start,
            byte_range.end - 1,
            idx.total_bytes
        );
        builder = builder.header("X-Content-Range-Bytes", x_content_range_bytes);
    }

    Ok(builder
        .body(Body::from(sliced))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

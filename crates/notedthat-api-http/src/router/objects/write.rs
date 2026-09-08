use super::super::helpers::{body_limit_usize, lookup_kb, parse_path, percent_encode_path};
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use notedthat_core::{ConditionalHeaders, Error as CoreError};

pub(in crate::router) async fn put_object(
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
        "/api/v1/knowledgebases/{kb_slug}/{}",
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

pub(in crate::router) async fn delete_object(
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

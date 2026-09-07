use super::super::helpers::{
    body_limit_usize, lookup_kb, parse_path, percent_encode_path, replace_conditionals,
};
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use notedthat_core::{ConditionalHeaders, Error as CoreError};
use serde::{Deserialize, Serialize};

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

/// Dispatcher for POST on the object catch-all. Only `replace/<target-path>` is a defined
/// action; every other POST returns 404 `not_found`.
pub(in crate::router) async fn post_object(
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

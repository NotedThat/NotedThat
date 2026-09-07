//! Handler for `POST /v1/okf/{kb_slug}/validate`.

use axum::{
    Json,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use notedthat_core::{Error as CoreError, KbSlug, ObjectPath};
use serde::Deserialize;

use super::{DEFAULT_VALIDATE_LIMIT, MAX_VALIDATE_LIMIT, OKF_BODY_MAX_BYTES, walk};
use crate::{
    error::{ApiError, ApiErrorResponse},
    state::AppState,
};

/// Request body for a validate call.
#[derive(Debug, Default, Deserialize)]
pub struct ValidateRequest {
    /// Validate exactly this object. Mutually exclusive with `prefix`.
    #[serde(default)]
    pub path: Option<String>,
    /// Restrict a bundle walk to this key prefix.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Objects to inspect in this page. Clamped to `MAX_VALIDATE_LIMIT`.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Continuation cursor from a previous report.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Check that in-body links resolve to objects that exist. Off by default,
    /// because it costs a round trip per unseen target.
    #[serde(default)]
    pub check_links: bool,
}

/// Handle `POST /v1/okf/{kb_slug}/validate`.
///
/// # Errors
///
/// 400 for a malformed slug or body, 404 for an undeclared KB, 503 when storage
/// is unavailable.
pub async fn validate_bundle(
    State(state): State<AppState>,
    Path(kb_slug_raw): Path<String>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = crate::middleware::extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    // Slug format before declaration lookup, so a malformed slug is a 400 rather
    // than leaking as a 404.
    let kb_slug = KbSlug::try_new(kb_slug_raw).map_err(|e| err(ApiError::Core(e)))?;
    let kb = crate::router::lookup_kb(&state, kb_slug.as_str()).map_err(err)?;

    let (parts, body) = req.into_parts();
    let body_bytes: Bytes = axum::body::to_bytes(body, OKF_BODY_MAX_BYTES)
        .await
        .map_err(|_| {
            err(ApiError::Core(CoreError::PayloadTooLarge {
                size: OKF_BODY_MAX_BYTES as u64 + 1,
                limit: OKF_BODY_MAX_BYTES as u64,
            }))
        })?;

    let request: ValidateRequest = if body_bytes.is_empty() {
        ValidateRequest::default()
    } else {
        let content_type = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok());
        if content_type.is_none_or(|value| !value.starts_with("application/json")) {
            return Err(err(ApiError::Core(CoreError::InvalidInput {
                message: "Content-Type must be application/json".into(),
            })));
        }
        serde_json::from_slice(&body_bytes).map_err(|e| {
            err(ApiError::Core(CoreError::InvalidInput {
                message: format!("invalid request body: {e}"),
            }))
        })?
    };

    if request.path.is_some() && request.prefix.is_some() {
        return Err(err(ApiError::Core(CoreError::InvalidInput {
            message: "`path` and `prefix` are mutually exclusive".into(),
        })));
    }

    let report = if let Some(raw_path) = &request.path {
        let path = ObjectPath::try_from_str(raw_path).map_err(|e| err(ApiError::Core(e)))?;
        walk::validate_one(&state.storage, &kb, &path, request.check_links)
            .await
            .map_err(|e| err(ApiError::from(e)))?
    } else {
        let params = walk::WalkParams {
            prefix: request.prefix.clone(),
            limit: request
                .limit
                .unwrap_or(DEFAULT_VALIDATE_LIMIT)
                .clamp(1, MAX_VALIDATE_LIMIT),
            cursor: request.cursor.clone(),
            check_links: request.check_links,
        };
        walk::validate_bundle(&state.storage, &kb, &params)
            .await
            .map_err(|e| err(ApiError::from(e)))?
    };

    Ok((StatusCode::OK, Json(report)).into_response())
}

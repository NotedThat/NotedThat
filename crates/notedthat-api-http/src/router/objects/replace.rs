use super::super::extract::{ObjectRequest, Write};
use super::super::helpers::{object_location, read_bounded_body, replace_conditionals};
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use notedthat_core::{ConditionalHeaders, Error as CoreError};
use serde::{Deserialize, Serialize};

use super::super::openapi::{
    BackendUnavailable, BadRequest, Forbidden, InternalError, NotFound, ObjectPathParams,
    PayloadTooLarge, PreconditionFailed, PreconditionRequired, Unauthorized, UnprocessableReplace,
};

/// A text replacement within one object.
#[derive(Deserialize, utoipa::ToSchema)]
struct ReplaceBody {
    /// The text to find. Must not be empty.
    old_string: String,
    /// The text to put in its place.
    new_string: String,
    /// Replace every occurrence. Otherwise exactly one must exist.
    #[serde(default)]
    replace_all: bool,
}

/// The outcome of a replace.
#[derive(Serialize, utoipa::ToSchema)]
struct ReplaceResponse {
    /// The object's new entity tag.
    etag: String,
    /// How many occurrences were replaced.
    match_count: u64,
    /// The object's new size in bytes.
    total_bytes: u64,
}

/// Dispatcher for POST on the object catch-all. Only `replace/<target-path>` is a defined
/// action; every other POST returns 404 `not_found`.
///
/// Documented as its one defined action, `replace/<path>`.
#[utoipa::path(
    post,
    path = "/knowledgebases/{kb_slug}/replace/{object_path}",
    tag = "objects",
    operation_id = "replace_object",
    summary = "Replace text within an object.",
    description = "Finds `old_string` in the object at `object_path` and replaces it with \
        `new_string`, atomically against `If-Match`, which is required and must be a single \
        entity tag. The object must be no larger than the server's patchable size limit.",
    params(
        ObjectPathParams,
        ("If-Match" = String, Header,
            description = "The object's current entity tag. `*` and lists are refused `400`."),
        ("X-NotedThat-Source" = Option<String>, Header,
            description = "`mcp` attributes the change events to the MCP server. Informational."),
    ),
    request_body(content = ReplaceBody, content_type = "application/json"),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Replaced.", body = ReplaceResponse,
            headers(
                ("Content-Location" = String, description = "The object's URL."),
                ("ETag" = String, description = "The new entity tag."),
            )),
        (status = 400, response = BadRequest),
        (status = 401, response = Unauthorized),
        (status = 403, response = Forbidden),
        (status = 404, response = NotFound),
        (status = 412, response = PreconditionFailed),
        (status = 413, response = PayloadTooLarge),
        (status = 422, response = UnprocessableReplace),
        (status = 428, response = PreconditionRequired),
        (status = 500, response = InternalError),
        (status = 503, response = BackendUnavailable),
    ),
)]
pub(in crate::router) async fn post_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(req.extensions());
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

    let object = ObjectRequest::<Write>::resolve(
        &state,
        req.headers(),
        req.extensions(),
        &kb_slug,
        target_path,
    )?;
    replace_object(&state, object, req).await
}

async fn replace_object(
    state: &AppState,
    req: ObjectRequest<Write>,
    body: Request,
) -> Result<Response, ApiErrorResponse> {
    let err = &req.err();
    let kb = req.access.kb();
    let path = &req.path;
    let json_cap_u64 = state
        .max_patchable_size
        .saturating_mul(2)
        .saturating_add(4096);

    let conditionals = replace_conditionals(&body).map_err(err)?;
    let body_bytes = read_bounded_body(body, json_cap_u64).await.map_err(err)?;

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
        &state.sinks(req.source),
        notedthat_write::ReplaceRequest {
            kb,
            path,
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
        .head_object(kb, path, ConditionalHeaders::default())
        .await
        .map_err(|e| err(ApiError::from(e)))?;
    let etag = outcome.put_outcome.etag.or(meta.etag).unwrap_or_default();
    let response = ReplaceResponse {
        etag: etag.clone(),
        match_count: outcome.match_count,
        total_bytes: meta.size,
    };
    let content_location = object_location(kb.as_str(), path.as_str());

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

use super::super::extract::{ObjectRequest, Write};
use super::super::helpers::{object_location, patch_mode_from_headers, read_bounded_body};
use crate::error::{ApiError, ApiErrorResponse};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use notedthat_core::Error as CoreError;

use super::super::openapi::{
    BackendUnavailable, BadRequest, Forbidden, InternalError, NotFound, ObjectPathParams,
    PayloadTooLarge, PreconditionFailed, PreconditionRequired, RangeNotSatisfiable, Unauthorized,
};

/// Change part of an object.
///
/// Either `Content-Range` names the bytes or lines the body replaces, and
/// `If-Match` is required; or `NT-Patch-Mode: append` appends the body, and
/// `If-Match` is optional. The two are mutually exclusive. `If-Match` must be a
/// single entity tag. The object must be no larger than the server's patchable
/// size limit.
#[utoipa::path(
    patch,
    path = "/knowledgebases/{kb_slug}/{object_path}",
    tag = "objects",
    params(
        ObjectPathParams,
        ("Content-Range" = Option<String>, Header,
            description = "`bytes <first>-<last>/*` or `lines <first>-<last>/*`: the span the body replaces."),
        ("NT-Patch-Mode" = Option<String>, Header, description = "`append`: append the body."),
        ("If-Match" = Option<String>, Header,
            description = "The object's current entity tag. Required with `Content-Range`; `*` and lists are refused `400`."),
        ("X-NotedThat-Source" = Option<String>, Header,
            description = "`mcp` attributes the change events to the MCP server. Informational."),
    ),
    request_body(description = "The replacement or appended bytes.", content_type = "*/*"),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Patched. No body.",
            headers(
                ("Location" = String, description = "The object's URL."),
                ("ETag" = String, description = "The new entity tag."),
            )),
        (status = 400, response = BadRequest),
        (status = 401, response = Unauthorized),
        (status = 403, response = Forbidden),
        (status = 404, response = NotFound),
        (status = 412, response = PreconditionFailed),
        (status = 413, response = PayloadTooLarge),
        (status = 416, response = RangeNotSatisfiable),
        (status = 428, response = PreconditionRequired),
        (status = 500, response = InternalError),
        (status = 503, response = BackendUnavailable),
    ),
)]
pub(in crate::router) async fn patch_object(
    State(state): State<AppState>,
    req: ObjectRequest<Write>,
    body: Request,
) -> Result<Response, ApiErrorResponse> {
    let err = &req.err();

    if let Some(if_match) = body
        .headers()
        .get(axum::http::header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        && (if_match == "*" || if_match.contains(','))
    {
        return Err(err(ApiError::Core(CoreError::InvalidInput {
            message: "If-Match: * and multi-value If-Match not supported on PATCH in v1; provide a single strong ETag".into(),
        })));
    }

    let content_range_header = body
        .headers()
        .get(axum::http::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let nt_patch_mode = body
        .headers()
        .get(axum::http::HeaderName::from_static("nt-patch-mode"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_lowercase);
    let content_type = body
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body_bytes = read_bounded_body(body, state.max_patchable_size)
        .await
        .map_err(err)?;

    let patch_mode = patch_mode_from_headers(
        nt_patch_mode.as_deref(),
        content_range_header.as_deref(),
        body_bytes,
    )
    .map_err(|error| err(ApiError::Core(error)))?;

    let outcome = notedthat_write::patch(
        state.storage.as_ref(),
        &state.sinks(req.source),
        notedthat_write::patch::PatchRequest {
            kb: req.access.kb(),
            path: &req.path,
            patch_mode,
            caller_conditionals: req.conditionals,
            max_patchable_size: state.max_patchable_size,
            caller_content_type: content_type.as_deref(),
        },
    )
    .await
    .map_err(|e| err(ApiError::from(e)))?;

    let location = object_location(req.access.kb().as_str(), req.path.as_str());
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

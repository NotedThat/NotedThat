use super::super::helpers::{body_limit_usize, event_source, object_location, parse_path};
use crate::authz::KbAccess;
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use notedthat_core::{ConditionalHeaders, Error as CoreError, Verb};

use super::super::openapi::{
    BackendUnavailable, BadRequest, Forbidden, InternalError, NotFound, ObjectPathParams,
    PayloadTooLarge, PreconditionFailed, Unauthorized, WriteConditions,
};

/// Create or replace an object.
///
/// The body is stored as is, with its `Content-Type`. Writing
/// `.notedthat/manifest.json` is validated as a manifest first.
#[utoipa::path(
    put,
    path = "/knowledgebases/{kb_slug}/{object_path}",
    tag = "objects",
    params(ObjectPathParams, WriteConditions,
        ("X-NotedThat-Source" = Option<String>, Header,
            description = "`mcp` attributes the change events to the MCP server. Informational."),
    ),
    request_body(description = "The object's bytes; at most the server's body limit (16 MiB by default).",
        content_type = "*/*"),
    security(("bearer" = [])),
    responses(
        (status = 201, description = "Created.",
            headers(
                ("Location" = String, description = "The object's URL."),
                ("ETag" = String, description = "The new entity tag."),
            )),
        (status = 204, description = "Replaced.",
            headers(("ETag" = String, description = "The new entity tag."))),
        (status = 400, response = BadRequest),
        (status = 401, response = Unauthorized),
        (status = 403, response = Forbidden),
        (status = 404, response = NotFound),
        (status = 412, response = PreconditionFailed),
        (status = 413, response = PayloadTooLarge),
        (status = 500, response = InternalError),
        (status = 503, response = BackendUnavailable),
    ),
)]
pub(in crate::router) async fn put_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let source = event_source(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let path = parse_path(&object_path).map_err(&err)?;
    let access = KbAccess::resolve(&state, &kb_slug, &req).map_err(&err)?;
    // Authorized before the body is read or staged, so a denied write
    // never spools bytes to memory or disk.
    access.require(Verb::Write, path.as_str()).map_err(&err)?;
    let kb = access.kb().clone();

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

    // A body for `.notedthat/manifest.json` is checked against what startup
    // would accept inside `notedthat_write::commit`, where every surface's
    // write ends (#98, §3.1); the `400` here is `WriteError::InvalidManifest`
    // mapped like any other write refusal.
    let outcome = notedthat_write::commit(
        state.storage.as_ref(),
        &state.sinks(source),
        &kb,
        &path,
        body_bytes,
        content_type.as_deref(),
        conditionals,
    )
    .await
    .map_err(|e| err(ApiError::from(e)))?;

    // RFC 9110 §9.3.4: 201 with a Location for a new object, 204 for a replacement.
    let mut builder = if outcome.created {
        Response::builder()
            .status(StatusCode::CREATED)
            .header("location", object_location(&kb_slug, path.as_str()))
    } else {
        Response::builder().status(StatusCode::NO_CONTENT)
    };
    if let Some(etag) = &outcome.etag {
        builder = builder.header(axum::http::header::ETAG, etag.as_str());
    }
    let resp = builder
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    Ok(resp)
}

/// Delete an object.
#[utoipa::path(
    delete,
    path = "/knowledgebases/{kb_slug}/{object_path}",
    tag = "objects",
    params(ObjectPathParams, WriteConditions,
        ("X-NotedThat-Source" = Option<String>, Header,
            description = "`mcp` attributes the change events to the MCP server. Informational."),
    ),
    security(("bearer" = [])),
    responses(
        (status = 204, description = "Deleted."),
        (status = 400, response = BadRequest),
        (status = 401, response = Unauthorized),
        (status = 403, response = Forbidden),
        (status = 404, response = NotFound),
        (status = 412, response = PreconditionFailed),
        (status = 500, response = InternalError),
        (status = 503, response = BackendUnavailable),
    ),
)]
pub(in crate::router) async fn delete_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let source = event_source(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let path = parse_path(&object_path).map_err(&err)?;
    let access = KbAccess::resolve(&state, &kb_slug, &req).map_err(&err)?;
    // Authorized before the body is read or staged, so a denied write
    // never spools bytes to memory or disk.
    access.require(Verb::Delete, path.as_str()).map_err(&err)?;
    let kb = access.kb().clone();
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

    notedthat_write::commit_delete(
        state.storage.as_ref(),
        &state.sinks(source),
        &kb,
        &path,
        conditionals,
    )
    .await
    .map_err(|e| err(ApiError::from(e)))?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

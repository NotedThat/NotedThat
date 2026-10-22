use super::super::extract::{Delete, ObjectRequest, Write};
use super::super::helpers::{object_location, read_bounded_body};
use crate::error::{ApiError, ApiErrorResponse};
use crate::state::AppState;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

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
    req: ObjectRequest<Write>,
    body: Request,
) -> Result<Response, ApiErrorResponse> {
    let err = &req.err();
    let content_type = body
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body_bytes = read_bounded_body(body, state.max_body_size)
        .await
        .map_err(err)?;

    // A body for `.notedthat/manifest.json` is checked against what startup
    // would accept inside `notedthat_write::commit`, where every surface's
    // write ends (#98, §3.1); the `400` here is `WriteError::InvalidManifest`
    // mapped like any other write refusal.
    let outcome = notedthat_write::commit(
        state.storage.as_ref(),
        &state.sinks(req.source),
        req.access.kb(),
        &req.path,
        body_bytes,
        content_type.as_deref(),
        req.conditionals,
    )
    .await
    .map_err(|e| err(ApiError::from(e)))?;

    // RFC 9110 §9.3.4: 201 with a Location for a new object, 204 for a replacement.
    let mut builder = if outcome.created {
        Response::builder().status(StatusCode::CREATED).header(
            "location",
            object_location(req.access.kb().as_str(), req.path.as_str()),
        )
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
    req: ObjectRequest<Delete>,
) -> Result<Response, ApiErrorResponse> {
    let err = &req.err();
    notedthat_write::commit_delete(
        state.storage.as_ref(),
        &state.sinks(req.source),
        req.access.kb(),
        &req.path,
        req.conditionals,
    )
    .await
    .map_err(|e| err(ApiError::from(e)))?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

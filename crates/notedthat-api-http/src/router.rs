//! Axum router builder and HTTP handlers for the `NotedThat` API.

use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::{auth_middleware, extract_request_id};
use crate::state::AppState;
use crate::write_path::commit;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use notedthat_core::{Error as CoreError, KbSlug, ObjectPath};
use serde::Deserialize;
use std::fmt::Write;
use tower::ServiceBuilder;
use tower_http::request_id::{MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

/// Maximum body size for PUT requests: 16 MiB (D35).
pub const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

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
            "/v1/knowledgebases/{kb_slug}/{*object_path}",
            get(get_object).head(head_object).put(put_object).delete(delete_object),
        )
        .layer(
            ServiceBuilder::new()
                .layer(DefaultBodyLimit::max(body_limit_usize(MAX_BODY_BYTES)))
                .layer(SetRequestIdLayer::new(request_id_header.clone(), MakeRequestUuidV7))
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
}

/// `GET /v1/knowledgebases/{kb_slug}` — list objects in a KB.
async fn list_objects(
    State(state): State<AppState>,
    Path(kb_slug): Path<String>,
    Query(q): Query<ListQuery>,
    req: Request,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse { error, request_id: request_id.clone() };

    let kb = lookup_kb(&state, &kb_slug).map_err(&err)?;
    let limit = q.limit.filter(|&limit| limit > 0).unwrap_or(100).min(1000);

    let result = state
        .storage
        .list_objects(&kb, q.prefix.as_deref(), limit)
        .await
        .map_err(|error| err(ApiError::Storage(error)))?;

    Ok(Json(serde_json::json!({
        "objects": result.objects,
        "truncated": result.truncated,
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
    let err = |error: ApiError| ApiErrorResponse { error, request_id: request_id.clone() };

    let kb = lookup_kb(&state, &kb_slug).map_err(&err)?;
    let path = parse_path(&object_path).map_err(&err)?;

    let meta = state
        .storage
        .head_object(&kb, &path)
        .await
        .map_err(|error| err(ApiError::Storage(error)))?;

    let mut resp = StatusCode::OK.into_response();
    let content_length = meta.size.to_string().parse().unwrap_or(HeaderValue::from_static("0"));
    resp.headers_mut().insert("content-length", content_length);
    if let Some(content_type) = meta.content_type
        && let Ok(header_value) = content_type.parse::<HeaderValue>()
    {
        resp.headers_mut().insert("content-type", header_value);
    }
    if let Some(last_modified) = meta.last_modified
        && let Ok(header_value) = last_modified.to_string().parse::<HeaderValue>()
    {
        resp.headers_mut().insert("last-modified", header_value);
    }
    Ok(resp)
}

/// `GET /v1/knowledgebases/{kb_slug}/{*object_path}` — download an object.
async fn get_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse { error, request_id: request_id.clone() };

    let kb = lookup_kb(&state, &kb_slug).map_err(&err)?;
    let path = parse_path(&object_path).map_err(&err)?;

    let read = state
        .storage
        .get_object(&kb, &path)
        .await
        .map_err(|error| err(ApiError::Storage(error)))?;
    let content_type = read.meta.content_type.unwrap_or_else(|| "application/octet-stream".to_string());

    let resp = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("content-length", read.meta.size.to_string())
        .body(Body::from(read.bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());

    Ok(resp)
}

/// `PUT /v1/knowledgebases/{kb_slug}/{*object_path}` — upload an object.
async fn put_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse { error, request_id: request_id.clone() };

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

    let body_bytes: Bytes = axum::body::to_bytes(req.into_body(), body_limit_usize(state.max_body_size))
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

    commit(state.storage.as_ref(), &kb, &path, body_bytes, content_type.as_deref())
        .await
        .map_err(&err)?;

    let location = format!("/v1/knowledgebases/{kb_slug}/{}", percent_encode_path(path.as_str()));
    let resp = Response::builder()
        .status(StatusCode::CREATED)
        .header("location", location)
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    Ok(resp)
}

/// `DELETE /v1/knowledgebases/{kb_slug}/{*object_path}` — delete an object (idempotent).
async fn delete_object(
    State(state): State<AppState>,
    Path((kb_slug, object_path)): Path<(String, String)>,
    req: Request,
) -> Result<StatusCode, ApiErrorResponse> {
    let request_id = extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse { error, request_id: request_id.clone() };

    let kb = lookup_kb(&state, &kb_slug).map_err(&err)?;
    let path = parse_path(&object_path).map_err(&err)?;

    state
        .storage
        .delete_object(&kb, &path)
        .await
        .map_err(|error| err(ApiError::Storage(error)))?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Look up a [`KbSlug`] from `state.declared_kbs`, returning 404 if not found.
fn lookup_kb(state: &AppState, slug: &str) -> Result<KbSlug, ApiError> {
    state.declared_kbs.get(slug).cloned().ok_or_else(|| {
        ApiError::Core(CoreError::NotFound { resource: format!("KB '{slug}' not declared") })
    })
}

/// Parse an [`ObjectPath`] from the raw path string extracted from the URL.
fn parse_path(raw: &str) -> Result<ObjectPath, ApiError> {
    ObjectPath::try_from_str(raw).map_err(ApiError::Core)
}

fn body_limit_usize(max_body_size: u64) -> usize {
    usize::try_from(max_body_size.saturating_add(1)).unwrap_or(usize::MAX)
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

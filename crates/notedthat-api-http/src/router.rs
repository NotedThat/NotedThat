//! Axum router builder and HTTP handlers for the `NotedThat` API.

use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::{auth_middleware, extract_request_id};
use crate::state::AppState;
use crate::write_path::commit;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderName, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use notedthat_core::{
    ConditionalHeaders, Error as CoreError, KbSlug, ObjectPath, StorageError, parse_range_header,
};
use notedthat_indexer::IndexEvent;
use serde::Deserialize;
use std::fmt::Write;
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::mpsc::error::TrySendError;
use tower::ServiceBuilder;
use tower_http::request_id::{
    MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
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
            "/v1/knowledgebases/{kb_slug}/search",
            axum::routing::post(crate::search_route::search_kb)
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::search_route::SEARCH_BODY_MAX_BYTES
                )),
        )
        .route(
            "/v1/knowledgebases/{kb_slug}/{*object_path}",
            get(get_object)
                .head(head_object)
                .put(put_object)
                .delete(delete_object),
        )
        .layer(
            ServiceBuilder::new()
                .layer(DefaultBodyLimit::max(body_limit_usize(MAX_BODY_BYTES)))
                .layer(SetRequestIdLayer::new(
                    request_id_header.clone(),
                    MakeRequestUuidV7,
                ))
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
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

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
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let kb = lookup_kb(&state, &kb_slug).map_err(&err)?;
    let path = parse_path(&object_path).map_err(&err)?;

    // Range header intentionally NOT forwarded on HEAD (RFC 7233 §3.1).
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

/// `GET /v1/knowledgebases/{kb_slug}/{*object_path}` — download an object.
async fn get_object(
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

    let range = match req.headers().get(axum::http::header::RANGE) {
        None => None,
        Some(raw) => {
            let raw_str = raw
                .to_str()
                .map_err(|_| err(ApiError::MalformedRange("non-UTF-8 Range header".into())))?;
            let parsed = parse_range_header(raw_str)
                .map_err(|_| err(ApiError::MalformedRange(raw_str.to_owned())))?;
            if parsed.unit == "bytes" && !parsed.ranges.is_empty() {
                Some(parsed.ranges)
            } else {
                None
            }
        }
    };
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

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

/// `PUT /v1/knowledgebases/{kb_slug}/{*object_path}` — upload an object.
async fn put_object(
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

    let outcome = commit(
        state.storage.as_ref(),
        &state.indexer_tx,
        &kb,
        &path,
        body_bytes,
        content_type.as_deref(),
        conditionals,
    )
    .await
    .map_err(&err)?;

    let location = format!(
        "/v1/knowledgebases/{kb_slug}/{}",
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

/// `DELETE /v1/knowledgebases/{kb_slug}/{*object_path}` — delete an object (idempotent).
async fn delete_object(
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

    match state.storage.delete_object(&kb, &path, conditionals).await {
        Ok(()) | Err(StorageError::NotFound { .. }) => {
            // Enqueue Tombstone — both branches return 204
            // delete_points with empty match is idempotent on Qdrant
            let event = IndexEvent::Tombstone {
                kb: kb.clone(),
                object_key: path.clone(),
            };
            match state.indexer_tx.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(ev)) => {
                    tracing::warn!(
                        target: "notedthat::indexing",
                        kb = %ev.kb().as_str(),
                        path = %ev.object_key().as_str(),
                        "INDEX_QUEUE_FULL"
                    );
                }
                Err(TrySendError::Closed(ev)) => {
                    tracing::error!(
                        target: "notedthat::indexing",
                        kb = %ev.kb().as_str(),
                        path = %ev.object_key().as_str(),
                        "INDEX_QUEUE_CLOSED"
                    );
                }
            }
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Err(StorageError::PreconditionFailed) => Err(err(ApiError::PreconditionFailed)),
        Err(e) => Err(err(ApiError::Storage(e))),
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Look up a [`KbSlug`] from `state.declared_kbs`, returning 404 if not found.
pub(crate) fn lookup_kb(state: &AppState, slug: &str) -> Result<KbSlug, ApiError> {
    state.declared_kbs.get(slug).cloned().ok_or_else(|| {
        ApiError::Core(CoreError::NotFound {
            resource: format!("KB '{slug}' not declared"),
        })
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

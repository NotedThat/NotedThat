//! Knowledgebase-level routes: list KBs and list objects within a KB.

use super::helpers::lookup_kb;
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::response::IntoResponse;
use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct ListQuery {
    prefix: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

pub(super) async fn list_kbs(State(state): State<AppState>) -> impl IntoResponse {
    let slugs: Vec<&str> = state.declared_kbs.keys().map(String::as_str).collect();
    Json(serde_json::json!({"knowledgebases": slugs}))
}

pub(super) async fn list_objects(
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
        .list_objects(&kb, q.prefix.as_deref(), limit, q.cursor.as_deref())
        .await
        .map_err(|error| err(ApiError::Storage(error)))?;

    Ok(Json(serde_json::json!({
        "objects": result.objects,
        "truncated": result.truncated,
        "next_cursor": result.next_cursor,
    })))
}

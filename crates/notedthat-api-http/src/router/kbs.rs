//! Knowledgebase-level routes: list KBs and list objects within a KB.

use super::helpers::lookup_kb;
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::response::IntoResponse;
use notedthat_core::{ListResponse, PublicReadCapability, StorageError};
use serde::Deserialize;

#[derive(Deserialize)]
pub(super) struct ListQuery {
    prefix: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

pub(super) async fn list_kbs(State(state): State<AppState>, req: Request) -> impl IntoResponse {
    let anonymous = crate::middleware::auth_context(&req).is_anonymous();
    let slugs: Vec<&str> = state
        .declared_kbs
        .keys()
        .filter(|slug| {
            !anonymous
                || state
                    .public_read_policies
                    .get(*slug)
                    .is_some_and(|policy| policy.allows(PublicReadCapability::Discover))
        })
        .map(String::as_str)
        .collect();
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

    let result = if crate::middleware::auth_context(&req).is_anonymous() {
        list_public_objects(&state, &kb, q.prefix.as_deref(), limit, q.cursor.as_deref())
            .await
            .map_err(|error| err(ApiError::Storage(error)))?
    } else {
        state
            .storage
            .list_objects(&kb, q.prefix.as_deref(), limit, q.cursor.as_deref())
            .await
            .map_err(|error| err(ApiError::Storage(error)))?
    };

    Ok(Json(serde_json::json!({
        "objects": result.objects,
        "truncated": result.truncated,
        "next_cursor": result.next_cursor,
    })))
}

async fn list_public_objects(
    state: &AppState,
    kb: &notedthat_core::KbSlug,
    prefix: Option<&str>,
    limit: u32,
    initial_cursor: Option<&str>,
) -> Result<ListResponse, StorageError> {
    if prefix.is_some_and(crate::middleware::is_internal_path) {
        return Ok(ListResponse {
            objects: Vec::new(),
            truncated: false,
            next_cursor: None,
        });
    }

    let mut objects = Vec::new();
    let mut cursor = initial_cursor.map(str::to_string);
    loop {
        let remaining =
            u32::try_from(objects.len()).map_or(limit, |visible| limit.saturating_sub(visible));
        let mut page = state
            .storage
            .list_objects(kb, prefix, remaining, cursor.as_deref())
            .await?;
        page.objects
            .retain(|object| !crate::middleware::is_internal_path(&object.key));
        objects.extend(page.objects);

        if objects.len() >= limit as usize {
            return Ok(ListResponse {
                objects,
                truncated: page.truncated,
                next_cursor: page.next_cursor,
            });
        }

        let Some(next_cursor) = page.next_cursor else {
            return Ok(ListResponse {
                objects,
                truncated: false,
                next_cursor: None,
            });
        };
        if cursor.as_deref() == Some(next_cursor.as_str()) {
            return Err(StorageError::BackendUnavailable {
                message: "storage returned a non-advancing cursor".into(),
            });
        }
        cursor = Some(next_cursor);
    }
}

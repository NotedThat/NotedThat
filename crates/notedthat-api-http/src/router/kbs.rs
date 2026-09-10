//! Knowledgebase-level routes: list KBs and list objects within a KB.

use crate::authz::{KbAccess, ScanScope, effective_prefix, visible_in_listing};
use crate::error::{ApiError, ApiErrorResponse};
use crate::middleware::extract_request_id;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::response::IntoResponse;
use notedthat_core::{KbSlug, KeyFilter, ListResponse, Principal, Storage, StorageError, Verb};
use serde::Deserialize;

/// Backend rows examined per storage call while refilling a filtered page.
///
/// A fixed page, not a shrinking remainder: shrinking makes each successive
/// call fetch fewer rows, which turns a sparse grant into a long tail of tiny
/// round-trips.
const LIST_SCAN_PAGE: u32 = 1000;

/// Storage calls spent filling one API page before returning it short.
///
/// This is what makes a narrow grant over a large knowledge base terminate: the
/// page comes back short with a cursor, and the client pages again.
const LIST_SCAN_MAX_CALLS: usize = 20;

#[derive(Deserialize)]
pub(super) struct ListQuery {
    prefix: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

pub(super) async fn list_kbs(
    State(state): State<AppState>,
    req: Request,
) -> Result<impl IntoResponse, ApiErrorResponse> {
    // Visibility is derived from holding a grant rather than declared by a
    // capability of its own, and it applies to both principals now: a
    // credential no longer implies reach into every declared knowledge base.
    let principal = crate::middleware::principal(&req);
    let slugs: Vec<&str> = state
        .declared_kbs
        .keys()
        .filter(|slug| visible_in_listing(&state, slug, principal))
        .map(String::as_str)
        .collect();

    // An anonymous caller who can see nothing is refused rather than handed an
    // empty array. Both leak the same amount — nothing — but `401` is the
    // truthful answer to "may I look at this deployment": credentials would
    // change it. This preserves the pre-D50 contract for the discovery route.
    if slugs.is_empty() && principal == Principal::Anyone {
        return Err(ApiErrorResponse {
            error: ApiError::Unauthorized,
            request_id: extract_request_id(&req),
        });
    }

    Ok(Json(serde_json::json!({"knowledgebases": slugs})))
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

    let access = KbAccess::resolve(&state, &kb_slug, &req).map_err(&err)?;
    access.require_any(Verb::List).map_err(&err)?;
    let limit = q.limit.filter(|&limit| limit > 0).unwrap_or(100).min(1000);

    let result = list_filtered_objects(
        state.storage.as_ref(),
        access.kb(),
        q.prefix.as_deref(),
        limit,
        q.cursor.as_deref(),
        &access.filter(Verb::List),
    )
    .await
    .map_err(|error| err(ApiError::Storage(error)))?;

    Ok(Json(serde_json::json!({
        "objects": result.objects,
        "truncated": result.truncated,
        "next_cursor": result.next_cursor,
    })))
}

/// List objects the principal may see, refilling across backend pages.
///
/// Storage listing is flat and unaware of access rules, so a filtered page has
/// to be assembled by scanning and discarding. Three things follow, and each is
/// handled below: the unfiltered case must stay free, a scoped grant must not
/// scan what it cannot return, and the cursor handed back must remain exact.
async fn list_filtered_objects(
    storage: &dyn Storage,
    kb: &KbSlug,
    prefix: Option<&str>,
    limit: u32,
    cursor: Option<&str>,
    filter: &KeyFilter<'_>,
) -> Result<ListResponse, StorageError> {
    // The credential holder with a whole-knowledge-base grant needs no filtering
    // at all, so this is one backend call and byte-identical to the behaviour
    // before access rules existed. Under the upgrade default that is every
    // existing authenticated client.
    if filter.is_allow_all() {
        return storage.list_objects(kb, prefix, limit, cursor).await;
    }
    if filter.is_deny_all() {
        return Ok(empty_page());
    }

    let ScanScope::From(scan_prefix) = effective_prefix(prefix, filter.literal_prefix_hint())
    else {
        // The caller's prefix and the grant's scope do not overlap, so no key
        // can match and there is nothing to ask storage for.
        return Ok(empty_page());
    };

    let mut objects = Vec::new();
    let mut page_cursor = cursor.map(str::to_string);

    for _ in 0..LIST_SCAN_MAX_CALLS {
        let page = storage
            .list_objects(
                kb,
                scan_prefix.as_deref(),
                LIST_SCAN_PAGE,
                page_cursor.as_deref(),
            )
            .await?;

        let mut consumed = 0_usize;
        let mut filled = false;
        for object in &page.objects {
            consumed += 1;
            if filter.allows(&object.key) {
                objects.push(object.clone());
                if objects.len() >= limit as usize {
                    filled = true;
                    break;
                }
            }
        }

        if filled {
            // The backend only issues cursors at *its* page boundaries, so
            // returning this page's cursor would skip every row after the one
            // that filled us up. Re-ask for exactly the rows we consumed and
            // hand back that response's cursor instead.
            //
            // This assumes re-listing the same prefix from the same cursor with
            // a smaller limit yields the same rows in the same order — true of
            // S3 ListObjectsV2, the filesystem backend and the in-memory one,
            // absent concurrent mutation, which D41 already declares
            // approximate for pagination.
            if consumed < page.objects.len() {
                let exact = storage
                    .list_objects(
                        kb,
                        scan_prefix.as_deref(),
                        u32::try_from(consumed).unwrap_or(LIST_SCAN_PAGE),
                        page_cursor.as_deref(),
                    )
                    .await?;
                return Ok(ListResponse {
                    objects,
                    truncated: exact.truncated,
                    next_cursor: exact.next_cursor,
                });
            }
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
        if page_cursor.as_deref() == Some(next_cursor.as_str()) {
            return Err(StorageError::BackendUnavailable {
                message: "storage returned a non-advancing cursor".into(),
            });
        }
        page_cursor = Some(next_cursor);
    }

    // The scan budget is spent. Returning short with a live cursor is what keeps
    // a sparse grant over a huge knowledge base bounded; the client pages again.
    Ok(ListResponse {
        objects,
        truncated: true,
        next_cursor: page_cursor,
    })
}

fn empty_page() -> ListResponse {
    ListResponse {
        objects: Vec::new(),
        truncated: false,
        next_cursor: None,
    }
}

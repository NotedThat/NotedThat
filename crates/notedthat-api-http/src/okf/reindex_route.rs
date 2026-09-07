//! Handler for `POST /v1/okf/{kb_slug}/reindex`.
//!
//! Rebuilds one directory's `index.md` from the concepts actually present. This
//! is the **explicit** counterpart to background index maintenance: it is always
//! available regardless of whether auto-maintenance is switched on, and it
//! defaults to `dry_run`, so seeing the proposal costs nothing.

use axum::{
    Json,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use notedthat_core::{
    ConditionalHeaders, Error as CoreError, KbSlug, ObjectPath, Storage, StorageError,
};
use notedthat_okf::{IndexEntry, is_reserved};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::OKF_BODY_MAX_BYTES;
use crate::{
    error::{ApiError, ApiErrorResponse},
    state::AppState,
};

/// Objects considered per reindex call.
const REINDEX_LIMIT: u32 = 1_000;

/// Request body for a reindex call.
#[derive(Debug, Default, Deserialize)]
pub struct ReindexRequest {
    /// The directory to rebuild. Defaults to the knowledge base root.
    #[serde(default)]
    pub dir: Option<String>,
    /// When `false`, write the result. Defaults to `true`.
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

fn default_dry_run() -> bool {
    true
}

/// The proposed or applied result of a reindex.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReindexResponse {
    /// The directory rebuilt.
    pub dir: String,
    /// Whether the result was written.
    pub applied: bool,
    /// Whether the proposal differs from what is stored.
    pub changed: bool,
    /// Concepts listed in the proposal.
    pub entries: usize,
    /// One line per entry added or updated.
    pub changes: Vec<String>,
    /// The proposed `index.md` in full.
    pub index_md: String,
}

/// Handle `POST /v1/okf/{kb_slug}/reindex`.
///
/// # Errors
///
/// 400 for a malformed slug, directory or body; 404 for an undeclared KB; 503
/// when storage is unavailable or the indexing queue is full.
pub async fn reindex_directory(
    State(state): State<AppState>,
    Path(kb_slug_raw): Path<String>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = crate::middleware::extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

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

    let request: ReindexRequest = if body_bytes.is_empty() {
        ReindexRequest {
            dir: None,
            dry_run: true,
        }
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

    let dir = super::browse_route::normalise_dir_public(request.dir.as_deref());
    let index_key = format!("{dir}{}", notedthat_okf::INDEX_FILE);
    let index_path = ObjectPath::try_from_str(&index_key).map_err(|e| err(ApiError::Core(e)))?;

    let existing = read_text(&state.storage, &kb, &index_path)
        .await
        .map_err(|e| err(ApiError::from(e)))?;
    let existing_text = existing.unwrap_or_default();

    let concepts = collect_concepts(&state.storage, &kb, &dir)
        .await
        .map_err(|e| err(ApiError::from(e)))?;

    let mut proposed = existing_text.clone();
    let mut changes = Vec::new();
    for (entry, section) in &concepts {
        let before = proposed.clone();
        proposed = notedthat_okf::upsert_entry(&proposed, section, entry);
        if proposed != before {
            changes.push(format!("{}: {}", section, entry.url));
        }
    }

    let differs = proposed != existing_text;
    let applied = differs && !request.dry_run;

    if applied {
        notedthat_write::commit(
            state.storage.as_ref(),
            &state.indexer_tx,
            &kb,
            &index_path,
            Bytes::from(proposed.clone()),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .map_err(|e| err(ApiError::from(e)))?;
    }

    Ok((
        StatusCode::OK,
        Json(ReindexResponse {
            dir,
            applied,
            changed: differs,
            entries: concepts.len(),
            changes,
            index_md: proposed,
        }),
    )
        .into_response())
}

/// Every OKF concept directly inside `dir`, with the section it belongs under.
///
/// Only direct children are considered — a subdirectory has its own `index.md` —
/// and the reserved files are never listed as concepts.
async fn collect_concepts(
    storage: &Arc<dyn Storage>,
    kb: &KbSlug,
    dir: &str,
) -> Result<Vec<(IndexEntry, String)>, StorageError> {
    let listing = storage
        .list_objects(kb, Some(dir), REINDEX_LIMIT, None)
        .await?;
    let mut out = Vec::new();

    for meta in listing.objects {
        let Some(name) = meta.key.strip_prefix(dir) else {
            continue;
        };
        if name.contains('/') || is_reserved(&meta.key) || !super::walk::is_markdown(name) {
            continue;
        }
        let Ok(path) = ObjectPath::try_from_str(&meta.key) else {
            continue;
        };
        let Some(text) = read_text(storage, kb, &path).await? else {
            continue;
        };
        let Ok((concept, _)) = notedthat_okf::parse_document(&text) else {
            continue;
        };

        let title = concept
            .title
            .clone()
            .unwrap_or_else(|| humanise(name.trim_end_matches(".md")));
        out.push((
            IndexEntry {
                title,
                url: format!("./{name}"),
                description: concept.description.clone(),
                is_directory: false,
                line: 0,
            },
            concept.concept_type.clone(),
        ));
    }

    out.sort_by(|a, b| a.0.title.cmp(&b.0.title));
    Ok(out)
}

async fn read_text(
    storage: &Arc<dyn Storage>,
    kb: &KbSlug,
    path: &ObjectPath,
) -> Result<Option<String>, StorageError> {
    match storage
        .get_object(kb, path, None, ConditionalHeaders::default())
        .await
    {
        Ok(read) => Ok(String::from_utf8(read.bytes.to_vec()).ok()),
        Err(StorageError::NotFound { .. }) => Ok(None),
        Err(err) => Err(err),
    }
}

/// A file stem turned into a display title: `daily-revenue` → `Daily revenue`.
fn humanise(stem: &str) -> String {
    let spaced = stem.replace(['-', '_'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanise_replaces_separators_and_capitalises() {
        assert_eq!(humanise("daily-revenue"), "Daily revenue");
        assert_eq!(humanise("active_users"), "Active users");
        assert_eq!(humanise("x"), "X");
        assert_eq!(humanise(""), "");
    }

    #[test]
    fn dry_run_defaults_to_true() {
        let request: ReindexRequest = serde_json::from_str("{}").unwrap();
        assert!(request.dry_run, "reindex must not write unless asked");
    }

    #[test]
    fn dry_run_can_be_turned_off_explicitly() {
        let request: ReindexRequest = serde_json::from_str(r#"{"dry_run":false}"#).unwrap();
        assert!(!request.dry_run);
    }
}

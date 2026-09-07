//! Handler for `GET /v1/okf/{kb_slug}/index?dir=…`.

use axum::{
    Json,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, StorageError};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, ApiErrorResponse},
    state::AppState,
};

/// Query parameters for a browse call.
#[derive(Debug, Default, Deserialize)]
pub struct BrowseQuery {
    /// Directory to browse, with or without a trailing `/`. Defaults to the root.
    #[serde(default)]
    pub dir: Option<String>,
}

/// One entry of a browse response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BrowseEntry {
    /// Link text.
    pub title: String,
    /// Link target exactly as written in `index.md`.
    pub url: String,
    /// The target resolved against the knowledge base root.
    ///
    /// This is the whole reason the route exists: resolving `./x.md` from a
    /// subdirectory is precisely where agents get it wrong.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<String>,
    /// The trailing description, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the target names a directory.
    pub is_directory: bool,
}

/// One section of a browse response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BrowseSection {
    /// The section heading.
    pub heading: String,
    /// Entries under it.
    pub entries: Vec<BrowseEntry>,
}

/// A parsed `index.md`, or a listing standing in for one.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BrowseResponse {
    /// The directory browsed, with a trailing `/` unless it is the root.
    pub dir: String,
    /// `"index.md"` when the bundle supplied one, `"listing"` when it did not.
    pub source: &'static str,
    /// The bundle's declared OKF version, from the root `index.md`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub okf_version: Option<String>,
    /// Sections, in document order.
    pub sections: Vec<BrowseSection>,
    /// Whether a fallback listing was truncated.
    pub truncated: bool,
    /// Tolerated departures from the conventional `index.md` shape.
    pub deviations: Vec<String>,
}

/// Objects listed when standing in for a missing `index.md`.
const LISTING_LIMIT: u32 = 100;

/// Handle `GET /v1/okf/{kb_slug}/index`.
///
/// # Errors
///
/// 400 for a malformed slug or directory, 404 for an undeclared KB, 503 when
/// storage is unavailable.
pub async fn browse_index(
    State(state): State<AppState>,
    Path(kb_slug_raw): Path<String>,
    Query(query): Query<BrowseQuery>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = crate::middleware::extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };

    let kb_slug = KbSlug::try_new(kb_slug_raw).map_err(|e| err(ApiError::Core(e)))?;
    let kb = crate::router::lookup_kb(&state, kb_slug.as_str()).map_err(err)?;

    let dir = normalise_dir(query.dir.as_deref());
    let index_key = format!("{dir}{}", notedthat_okf::INDEX_FILE);
    let index_path = ObjectPath::try_from_str(&index_key).map_err(|e| err(ApiError::Core(e)))?;

    let index_bytes = match state
        .storage
        .get_object(&kb, &index_path, None, ConditionalHeaders::default())
        .await
    {
        Ok(read) => Some(read.bytes),
        Err(StorageError::NotFound { .. }) => None,
        Err(e) => return Err(err(ApiError::from(e))),
    };

    let response =
        if let Some(text) = index_bytes.and_then(|bytes| String::from_utf8(bytes.to_vec()).ok()) {
            from_index(&dir, &text)
        } else {
            // No `index.md` here: stand in with a listing, and say so, so a caller
            // knows the bundle did not supply one (OKF §11 tolerates its absence).
            let listing = state
                .storage
                .list_objects(&kb, Some(&dir), LISTING_LIMIT, None)
                .await
                .map_err(|e| err(ApiError::from(e)))?;
            from_listing(&dir, &listing)
        };

    Ok((StatusCode::OK, Json(response)).into_response())
}

/// A directory key with a trailing `/`, or `""` for the root.
pub(crate) fn normalise_dir_public(raw: Option<&str>) -> String {
    normalise_dir(raw)
}

/// A directory key with a trailing `/`, or `""` for the root.
fn normalise_dir(raw: Option<&str>) -> String {
    let trimmed = raw.unwrap_or("").trim().trim_start_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.ends_with('/') {
        trimmed.to_string()
    } else {
        format!("{trimmed}/")
    }
}

fn from_index(dir: &str, text: &str) -> BrowseResponse {
    let doc = notedthat_okf::parse_index(text);
    let base_dir = dir.to_string();
    BrowseResponse {
        dir: dir.to_string(),
        source: "index.md",
        okf_version: doc.okf_version,
        sections: doc
            .sections
            .into_iter()
            .map(|section| BrowseSection {
                heading: section.heading,
                entries: section
                    .entries
                    .into_iter()
                    .map(|entry| {
                        let resolved = match notedthat_okf::resolve_link(&base_dir, &entry.url) {
                            notedthat_okf::LinkTarget::Internal(path) => {
                                Some(path.as_str().to_string())
                            }
                            notedthat_okf::LinkTarget::Directory(prefix) => Some(prefix),
                            // An absolute URL or an unresolvable target has no
                            // in-bundle path, and is never dereferenced.
                            _ => None,
                        };
                        BrowseEntry {
                            title: entry.title,
                            url: entry.url,
                            resolved_path: resolved,
                            description: entry.description,
                            is_directory: entry.is_directory,
                        }
                    })
                    .collect(),
            })
            .collect(),
        truncated: false,
        deviations: doc
            .deviations
            .into_iter()
            .map(|d| format!("line {}: {}", d.line, d.message))
            .collect(),
    }
}

fn from_listing(dir: &str, listing: &notedthat_core::storage::ListResponse) -> BrowseResponse {
    let mut entries: Vec<BrowseEntry> = Vec::new();
    let mut seen_dirs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for meta in &listing.objects {
        let Some(rest) = meta.key.strip_prefix(dir) else {
            continue;
        };
        match rest.split_once('/') {
            // Collapse everything below the next `/` into one directory entry.
            Some((subdir, _)) => {
                if seen_dirs.insert(subdir.to_string()) {
                    entries.push(BrowseEntry {
                        title: subdir.to_string(),
                        url: format!("{subdir}/"),
                        resolved_path: Some(format!("{dir}{subdir}/")),
                        description: None,
                        is_directory: true,
                    });
                }
            }
            None => entries.push(BrowseEntry {
                title: rest.to_string(),
                url: format!("./{rest}"),
                resolved_path: Some(meta.key.clone()),
                description: None,
                is_directory: false,
            }),
        }
    }

    BrowseResponse {
        dir: dir.to_string(),
        source: "listing",
        okf_version: None,
        sections: vec![BrowseSection {
            heading: "Contents".to_string(),
            entries,
        }],
        truncated: listing.truncated,
        deviations: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_normalisation_adds_a_trailing_slash() {
        assert_eq!(normalise_dir(Some("tables")), "tables/");
        assert_eq!(normalise_dir(Some("tables/")), "tables/");
    }

    #[test]
    fn dir_normalisation_strips_a_leading_slash() {
        assert_eq!(normalise_dir(Some("/tables")), "tables/");
    }

    #[test]
    fn absent_or_empty_dir_is_the_root() {
        assert_eq!(normalise_dir(None), "");
        assert_eq!(normalise_dir(Some("")), "");
        assert_eq!(normalise_dir(Some("/")), "");
    }

    #[test]
    fn index_entries_get_resolved_paths() {
        let response = from_index("tables/", "# Tables\n\n* [Customers](./customers.md) - x\n");
        let entry = &response.sections[0].entries[0];
        assert_eq!(entry.resolved_path.as_deref(), Some("tables/customers.md"));
        assert_eq!(entry.url, "./customers.md");
        assert_eq!(response.source, "index.md");
    }

    #[test]
    fn a_bundle_relative_entry_resolves_from_the_root() {
        let response = from_index("deep/nested/", "# T\n\n* [A](/a.md)\n");
        assert_eq!(
            response.sections[0].entries[0].resolved_path.as_deref(),
            Some("a.md")
        );
    }

    #[test]
    fn an_absolute_url_entry_has_no_resolved_path() {
        let response = from_index("", "# T\n\n* [Docs](https://example.com/x)\n");
        assert!(response.sections[0].entries[0].resolved_path.is_none());
    }

    #[test]
    fn root_okf_version_is_surfaced() {
        let response = from_index("", "---\nokf_version: \"0.2\"\n---\n# T\n");
        assert_eq!(response.okf_version.as_deref(), Some("0.2"));
    }

    #[test]
    fn deviations_are_reported_with_line_numbers() {
        let response = from_index("", "# T\n\n* not a link\n");
        assert_eq!(response.deviations.len(), 1);
        assert!(response.deviations[0].starts_with("line 3:"));
    }
}

//! A server-rendered HTML view of what a caller may read (#100, D52).
//!
//! Deliberately thin: it renders directory listings over the same storage and
//! the same access rules as every other surface, and it links objects at their
//! existing `/api/v1` representation rather than growing a second download path.
//! No JavaScript, no accounts, no editing, no search.
//!
//! # Denials are `404`, not `403`
//!
//! With allow-only, glob-scoped, private-by-default rules, "you may not see
//! this" and "this does not exist" should be the same answer to an anonymous
//! caller — otherwise the difference between the two statuses is an oracle for
//! enumerating private prefixes. The cost is diagnosability, which the request
//! id on the error page hands back to whoever can read the logs. A credentialed
//! caller gets `403`, which tells them something true and reveals nothing they
//! could not already enumerate.

mod format;
mod links;
mod listing;
mod render;

use crate::authz::KbAccess;
use crate::middleware::{extract_request_id, resolve_principal};
use crate::state::AppState;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use format::{ABSENT, civil_date, http_date, human_size};
use listing::{BROWSE_MAX_KEYS, DirectoryListing, is_present, read_directory};
use notedthat_core::{ObjectMeta, ObjectPath, Principal, Verb};
use render::{Crumb, PageView, RowKind, RowView, display_text, escape_html, page};

/// The name shown at the root of every breadcrumb.
///
/// Hardcoded, like the `/llms.txt` body: a public page should carry no
/// deployment-specific detail it was not asked to publish.
const SITE_NAME: &str = "notedthat";

/// `GET|HEAD /browse` and `/browse/` — the index of knowledge bases.
pub(super) async fn browse_root(State(state): State<AppState>, mut req: Request) -> Response {
    if !req.uri().path().ends_with('/') {
        // A statement about the route, not about the data: permanent.
        return redirect(StatusCode::PERMANENT_REDIRECT, &links::root_href());
    }

    let request_id = extract_request_id(&req);
    let Ok(principal) = resolve_principal(req.headers(), &state.bearer_token) else {
        return unauthorized(&request_id);
    };
    req.extensions_mut().insert(principal);

    let visible: Vec<&String> = state
        .declared_kbs
        .keys()
        .filter(|slug| crate::authz::visible_in_listing(&state, slug, principal))
        .collect();

    let rows: Vec<RowView> = visible
        .iter()
        .map(|slug| {
            // A knowledge base can be visible without being listable — a
            // `search`-only grant, say. Linking it would promise a page that
            // answers 404, so the row stays plain text.
            let listable = state
                .access_policies
                .get(slug.as_str())
                .is_some_and(|policy| policy.grants_any(principal, Verb::List));
            RowView {
                label: format!("{}/", display_text(slug)),
                href: listable.then(|| escape_html(&links::directory_href(slug, ""))),
                size: ABSENT.to_string(),
                modified: ABSENT.to_string(),
                modified_title: None,
                kind: RowKind::Folder,
            }
        })
        .collect();

    // A person who lands on `/browse` and can see nothing gets a page saying so,
    // where `GET /api/v1/knowledgebases` answers `401` for the same fact. The
    // divergence is deliberate and specific to this route: a browser cannot act
    // on `401` — it has no way to offer a Bearer token — so the status would
    // read as a dead end rather than an invitation. Neither answer discloses
    // anything, because neither names a knowledge base. Every route that *does*
    // name one agrees on `404` for an anonymous denial; see `KbAccess::denial`.
    let summary = if rows.is_empty() {
        "Nothing is published here.".to_string()
    } else {
        format!(
            "{} {}",
            rows.len(),
            plural(rows.len(), "knowledge base", "knowledge bases")
        )
    };

    html_ok(&page(&PageView {
        title: SITE_NAME.to_string(),
        crumbs: vec![Crumb {
            label: escape_html(SITE_NAME),
            href: escape_html(&links::root_href()),
        }],
        rows,
        summary,
        notice: None,
        footnote: None,
    }))
}

/// `GET|HEAD /browse/{*path}` — a knowledge base, a folder inside one, or an object.
pub(super) async fn browse_path(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    mut req: Request,
) -> Response {
    let request_id = extract_request_id(&req);

    let Ok(Path(captured)) = path else {
        // Invalid percent-escapes. Without taking the `Result` form this would
        // be axum's own plain-text 400 rather than a page.
        return not_found(&request_id);
    };
    let Ok(principal) = resolve_principal(req.headers(), &state.bearer_token) else {
        return unauthorized(&request_id);
    };
    // `KbAccess` reads the principal from the request, as it does for every
    // `/api/v1` handler. This surface resolves its own — it is mounted outside
    // `auth_middleware` — so it has to record the answer in the same place, or
    // `KbAccess` would fall back to anonymous and every credential would be
    // ignored here.
    req.extensions_mut().insert(principal);

    // Read the trailing slash from the raw URI rather than the capture, so the
    // decision does not depend on how the router treats a catch-all.
    let has_trailing_slash = req.uri().path().ends_with('/');

    let (kb_slug, remainder) = match captured.split_once('/') {
        Some((slug, rest)) => (slug, rest),
        None => (captured.as_str(), ""),
    };

    let Ok(access) = KbAccess::resolve(&state, kb_slug, &req) else {
        return not_found(&request_id);
    };
    if !crate::authz::visible_in_listing(&state, kb_slug, principal) {
        return denied(principal, &request_id);
    }

    if !has_trailing_slash && !remainder.is_empty() {
        return resolve_ambiguous(&state, &access, kb_slug, remainder, &request_id).await;
    }
    if !has_trailing_slash {
        // `/browse/{kb}` — a statement about the data, so temporary.
        return redirect(
            StatusCode::TEMPORARY_REDIRECT,
            &links::directory_href(kb_slug, ""),
        );
    }

    let prefix = remainder;
    if !prefix.is_empty() {
        let trimmed = prefix.trim_end_matches('/');
        if ObjectPath::try_from_str(trimmed).is_err() {
            return not_found(&request_id);
        }
        // Browse shows the internal namespace to nobody, so `404` is the
        // truthful answer for every principal — it is not that the caller may
        // not see it, it is that this surface does not render it.
        if notedthat_core::is_internal_path(trimmed) {
            return not_found(&request_id);
        }
    }
    if !access.policy_grants_any(Verb::List) {
        return denied(principal, &request_id);
    }

    render_directory(&state, &access, kb_slug, prefix, principal, &request_id).await
}

/// `/browse/{kb}/{path}` with no trailing slash: an object, a folder, or neither.
async fn resolve_ambiguous(
    state: &AppState,
    access: &KbAccess,
    kb_slug: &str,
    key: &str,
    request_id: &str,
) -> Response {
    let Ok(path) = ObjectPath::try_from_str(key) else {
        return not_found(request_id);
    };
    if notedthat_core::is_internal_path(path.as_str()) {
        return not_found(request_id);
    }

    // Probe as an object only when the caller may read one, so a read-denied
    // object and an absent object are indistinguishable here.
    if access.allows(Verb::Read, path.as_str())
        && state
            .storage
            .head_object(
                access.kb(),
                &path,
                notedthat_core::ConditionalHeaders::default(),
            )
            .await
            .is_ok()
    {
        return redirect(
            StatusCode::SEE_OTHER,
            &links::object_href(kb_slug, path.as_str()),
        );
    }

    // Otherwise it may be a folder: does any visible key sit under it?
    let folder_prefix = format!("{}/", path.as_str());
    if access.policy_grants_any(Verb::List) {
        match read_directory(state.storage.as_ref(), access.kb(), &folder_prefix, access).await {
            Ok(listing) if is_present(&listing) => {
                return redirect(
                    StatusCode::TEMPORARY_REDIRECT,
                    &links::directory_href(kb_slug, &folder_prefix),
                );
            }
            Ok(_) => {}
            Err(_) => return server_error(request_id),
        }
    }

    not_found(request_id)
}

async fn render_directory(
    state: &AppState,
    access: &KbAccess,
    kb_slug: &str,
    prefix: &str,
    principal: Principal,
    request_id: &str,
) -> Response {
    let Ok(listing) = read_directory(state.storage.as_ref(), access.kb(), prefix, access).await
    else {
        return server_error(request_id);
    };

    // A folder is synthesised from keys, so a folder with no visible keys does
    // not exist. A knowledge-base root is different: it is declared, and a
    // granted-but-empty one should not 404 at its own front door.
    if !prefix.is_empty() && !is_present(&listing) {
        return denied(principal, request_id);
    }

    let rows = directory_rows(access, kb_slug, prefix, &listing);
    let restricted = rows
        .iter()
        .filter(|row| row.kind == RowKind::RestrictedObject)
        .count();

    let folders = listing.rollup.folders.len();
    let objects = listing.rollup.files.len();

    html_ok(&page(&PageView {
        title: format!("{kb_slug}/{prefix}"),
        crumbs: crumbs(kb_slug, prefix),
        rows,
        summary: format!(
            "{folders} {}, {objects} {}",
            plural(folders, "folder", "folders"),
            plural(objects, "object", "objects")
        ),
        notice: listing.truncated.then(|| truncation_notice(&listing)),
        footnote: (restricted > 0).then(|| {
            format!(
                "{restricted} {} listed but not readable.",
                plural(restricted, "object is", "objects are")
            )
        }),
    }))
}

fn directory_rows(
    access: &KbAccess,
    kb_slug: &str,
    prefix: &str,
    listing: &DirectoryListing,
) -> Vec<RowView> {
    let mut rows =
        Vec::with_capacity(listing.rollup.folders.len() + listing.rollup.files.len() + 1);

    rows.push(RowView {
        label: "../".to_string(),
        href: Some(escape_html(&links::parent_href(kb_slug, prefix))),
        size: String::new(),
        modified: String::new(),
        modified_title: None,
        kind: RowKind::Parent,
    });

    for folder in &listing.rollup.folders {
        rows.push(RowView {
            label: format!("{}/", display_text(folder)),
            href: Some(escape_html(&links::directory_href(
                kb_slug,
                &format!("{prefix}{folder}/"),
            ))),
            size: ABSENT.to_string(),
            modified: ABSENT.to_string(),
            modified_title: None,
            kind: RowKind::Folder,
        });
    }

    for (name, meta) in &listing.rollup.files {
        rows.push(object_row(access, kb_slug, name, meta));
    }

    rows
}

fn object_row(access: &KbAccess, kb_slug: &str, name: &str, meta: &ObjectMeta) -> RowView {
    // Per row, not per page: with glob scoping a directory can genuinely be
    // listable while only part of it is readable.
    let readable = access.allows(Verb::Read, &meta.key);
    let (modified, modified_title) = match meta.last_modified.and_then(civil_date) {
        Some(date) => (
            date,
            meta.last_modified
                .and_then(http_date)
                .map(|full| escape_html(&full)),
        ),
        None => (ABSENT.to_string(), None),
    };

    RowView {
        label: display_text(name),
        href: readable.then(|| escape_html(&links::object_href(kb_slug, &meta.key))),
        size: human_size(meta.size),
        modified,
        modified_title,
        kind: if readable {
            RowKind::Object
        } else {
            RowKind::RestrictedObject
        },
    }
}

fn crumbs(kb_slug: &str, prefix: &str) -> Vec<Crumb> {
    let mut crumbs = vec![
        Crumb {
            label: escape_html(SITE_NAME),
            href: escape_html(&links::root_href()),
        },
        Crumb {
            label: display_text(kb_slug),
            href: escape_html(&links::directory_href(kb_slug, "")),
        },
    ];

    let mut walked = String::new();
    for segment in prefix.split('/').filter(|segment| !segment.is_empty()) {
        walked.push_str(segment);
        walked.push('/');
        crumbs.push(Crumb {
            label: display_text(segment),
            href: escape_html(&links::directory_href(kb_slug, &walked)),
        });
    }
    crumbs
}

fn truncation_notice(listing: &DirectoryListing) -> String {
    // `WebDAV` answers 507 at its cap because its consumer is a sync client that
    // would read a short listing as a complete one and delete the difference. A
    // person reading a page has no such failure mode, and keys arrive in
    // lexicographic order — so what is shown is a correct prefix of the truth,
    // and the notice says exactly where the truth stops.
    match &listing.last_key {
        Some(last) => format!(
            "Listing truncated at {BROWSE_MAX_KEYS} objects. Entries after \
             <code>{}</code> are not shown — open a subfolder to reach them.",
            display_text(last)
        ),
        None => format!("Listing truncated at {BROWSE_MAX_KEYS} objects."),
    }
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

fn redirect(status: StatusCode, location: &str) -> Response {
    let mut response = status.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    browse_headers(&mut response);
    response
}

fn html_ok(body: &str) -> Response {
    let mut response = (StatusCode::OK, body.to_string()).into_response();
    browse_headers(&mut response);
    response
}

fn html_error(status: StatusCode, heading: &str, detail: &str, request_id: &str) -> Response {
    let mut response = (status, render::error_page(heading, detail, request_id)).into_response();
    browse_headers(&mut response);
    response
}

fn not_found(request_id: &str) -> Response {
    html_error(
        StatusCode::NOT_FOUND,
        "Not found",
        "There is nothing to show at this address.",
        request_id,
    )
}

fn unauthorized(request_id: &str) -> Response {
    html_error(
        StatusCode::UNAUTHORIZED,
        "Unauthorized",
        "The credentials supplied with this request were not accepted.",
        request_id,
    )
}

fn server_error(request_id: &str) -> Response {
    html_error(
        StatusCode::BAD_GATEWAY,
        "Unavailable",
        "The storage backend could not be reached.",
        request_id,
    )
}

/// The answer for a caller who may not see something.
fn denied(principal: Principal, request_id: &str) -> Response {
    match principal {
        Principal::Anyone => not_found(request_id),
        Principal::SignedIn => html_error(
            StatusCode::FORBIDDEN,
            "Forbidden",
            "This knowledge base's access rules do not grant you this listing.",
            request_id,
        ),
    }
}

fn browse_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    // Anonymous and credentialed callers share one URL and see different pages,
    // so an intermediary caching one and replaying it to the other would be a
    // real disclosure.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::VARY, HeaderValue::from_static("authorization"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // The page has one inline style block and nothing else, so this costs
    // nothing and forecloses the injection class even if the escaping were wrong.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; \
             form-action 'none'; frame-ancestors 'none'",
        ),
    );
}

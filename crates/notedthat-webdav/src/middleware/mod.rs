//! `WebDAV` middleware: basic-auth and method-interception layers.

mod access;
mod backpressure;
mod handlers;
mod helpers;
mod path_validation;

use crate::state::WebDavState;
use crate::{
    filesystem::{DavTarget, PROPFIND_TOO_LARGE_DAV_XML},
    propfind::{
        PropfindDepth, depth_infinity_response, parse_propfind_depth, prepare_propfind_listing,
    },
};
pub(crate) use access::DavAllow;
pub use access::basic_auth_middleware;
use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use handlers::{handle_copy, handle_delete, handle_move, handle_proppatch, handle_put};
use notedthat_core::ConditionalHeaders;
pub(crate) use path_validation::WEBDAV_PREFIX;
use path_validation::{parse_webdav_uri_path, validate_webdav_read_uri_path};

/// Intercept OPTIONS requests and return DAV Class 1 response before dav-server.
///
/// dav-server v0.11 hardcodes `DAV: 1,2,3,sabredav-partialupdate` which violates
/// issue #22 which requires `DAV: 1` only. Interception is the only way to override.
pub async fn intercept_options(req: Request, next: Next) -> Response {
    if req.method() == axum::http::Method::OPTIONS {
        let mut response = (StatusCode::NO_CONTENT, "").into_response();
        let headers = response.headers_mut();
        headers.insert("dav", HeaderValue::from_static("1"));
        headers.insert("ms-author-via", HeaderValue::from_static("DAV"));
        // `DavAllow` is set by the auth middleware for every request now, not
        // only anonymous ones: access rules bind the credential holder too, so
        // `Allow` has to describe what *this* caller may do rather than what the
        // protocol supports.
        let allow = req.extensions().get::<DavAllow>().map_or_else(
            || "OPTIONS, GET, HEAD, PROPFIND, PUT, DELETE, MKCOL, MOVE, COPY".to_string(),
            |access| {
                let mut methods = vec!["OPTIONS"];
                if access.content {
                    methods.extend(["GET", "HEAD"]);
                }
                if access.propfind {
                    methods.push("PROPFIND");
                }
                methods.join(", ")
            },
        );
        if let Ok(value) = HeaderValue::from_str(&allow) {
            headers.insert("allow", value);
        }
        return response;
    }
    next.run(req).await
}

/// Intercept LOCK/UNLOCK requests and return 405 before dav-server.
///
/// dav-server v0.11 already returns 405 when no `LockSystem` is registered, but we
/// add belt-and-braces interception so future dav-server default changes cannot
/// silently enable LOCK/UNLOCK (D17: no `LockSystem`, ever, in v1).
pub async fn intercept_lock_unlock(req: Request, next: Next) -> Response {
    let method = req.method().as_str();
    if method == "LOCK" || method == "UNLOCK" {
        let mut response = (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
        response.headers_mut().insert(
            "allow",
            HeaderValue::from_static(
                "OPTIONS, GET, HEAD, PROPFIND, PUT, DELETE, MKCOL, MOVE, COPY",
            ),
        );
        return response;
    }
    next.run(req).await
}

/// Intercept over-large PROPFIND requests before dav-server swallows `read_dir` errors.
pub async fn intercept_propfind_too_large(
    State(state): State<WebDavState>,
    mut req: Request,
    next: Next,
) -> Response {
    if req.method().as_str() != "PROPFIND" {
        return next.run(req).await;
    }

    let uri_path = req.uri().path();
    let target_path = uri_path
        .strip_suffix('/')
        .filter(|path| !path.is_empty())
        .unwrap_or(uri_path);
    let target = parse_webdav_uri_path(target_path, &state.declared_kbs);

    // A missing Depth means infinity (RFC 4918 §9.1), and an infinite walk of a
    // knowledge base is refused rather than listed without the size cap. Only a
    // collection has members to walk: anything else ignores its Depth (§10.2) and
    // answers as `Depth: 0` — a file with its properties, a missing path with `404`.
    match parse_propfind_depth(&req) {
        PropfindDepth::Default | PropfindDepth::Infinity => {
            if let Ok(DavTarget::Object(kb, path)) = &target
                && !is_virtual_folder(&state, kb, path).await
            {
                req.headers_mut()
                    .insert("depth", HeaderValue::from_static("0"));
                return next.run(req).await;
            }
            return depth_infinity_response();
        }
        PropfindDepth::Invalid => return StatusCode::BAD_REQUEST.into_response(),
        PropfindDepth::Zero => return next.run(req).await,
        PropfindDepth::One => {}
    }

    let Ok(target) = target else {
        return next.run(req).await;
    };

    let principal = principal_of(&req);
    match prepare_propfind_listing(&state, &target, &principal).await {
        Err(dav_server::fs::FsError::InsufficientStorage) => (
            StatusCode::INSUFFICIENT_STORAGE,
            [(
                axum::http::header::CONTENT_TYPE,
                "application/xml; charset=utf-8",
            )],
            PROPFIND_TOO_LARGE_DAV_XML,
        )
            .into_response(),
        Ok(Some(listing)) => {
            req.extensions_mut().insert(listing);
            next.run(req).await
        }
        Ok(None) | Err(_) => next.run(req).await,
    }
}

/// Whether `path` is a folder: no object of its own, and at least one beneath it.
/// A backend error reads as "not a folder", so the request goes on at `Depth: 0`
/// and reports the error there rather than as a depth refusal.
async fn is_virtual_folder(
    state: &WebDavState,
    kb: &notedthat_core::KbSlug,
    path: &notedthat_core::ObjectPath,
) -> bool {
    match state
        .storage
        .head_object(kb, path, ConditionalHeaders::default())
        .await
    {
        Err(error) if error.is_not_found() => {
            let prefix = format!("{}/", path.as_str());
            state
                .storage
                .list_objects(kb, Some(&prefix), 1, None)
                .await
                .is_ok_and(|listing| !listing.objects.is_empty())
        }
        Ok(_) | Err(_) => false,
    }
}

/// Intercept `WebDAV` write methods before `dav-server` so raw HTTP headers remain available.
pub async fn intercept_write_methods(
    State(state): State<WebDavState>,
    req: Request,
    next: Next,
) -> Response {
    match req.method().as_str() {
        "PUT" => handle_put(state, req).await,
        "DELETE" => handle_delete(state, req).await,
        "MOVE" => handle_move(state, req).await,
        "COPY" => handle_copy(state, req).await,
        "PROPPATCH" => handle_proppatch(state, req).await,
        _ => next.run(req).await,
    }
}

/// Intercept `WebDAV` READ methods before `dav-server` so raw HTTP URIs remain available.
///
/// Enforces D40 strict per-segment path normalization on GET, HEAD, and PROPFIND —
/// the same rules already applied to write methods by `intercept_write_methods`.
/// Rejects requests whose URI contains `.`, `..`, empty middle segments, `/`, `\`,
/// or `\0` segments (raw or percent-encoded) with 400 Bad Request, matching the
/// write-path 400 shape. A single trailing `/` on a legitimate collection path is
/// permitted via `validate_read_uri_path` so `PROPFIND /notes/folder/` still lists.
pub async fn intercept_read_methods(
    State(state): State<WebDavState>,
    req: Request,
    next: Next,
) -> Response {
    match req.method().as_str() {
        "GET" | "HEAD" | "PROPFIND" => {
            if validate_webdav_read_uri_path(req.uri().path(), &state.declared_kbs).is_err() {
                return StatusCode::BAD_REQUEST.into_response();
            }
            next.run(req).await
        }
        _ => next.run(req).await,
    }
}

/// The principal the auth middleware established for this request.
///
/// Defaults to [`Principal::Anyone`] so a request that somehow bypassed the auth
/// layer is treated as having no credential rather than as having every one.
pub(crate) fn principal_of<B>(req: &axum::http::Request<B>) -> notedthat_core::Principal {
    req.extensions()
        .get::<notedthat_core::Principal>()
        .cloned()
        .unwrap_or(notedthat_core::Principal::Anyone)
}

#[cfg(test)]
mod tests;

//! Per-method `WebDAV` write handlers (PUT / DELETE / MOVE / COPY / PROPPATCH).

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use notedthat_core::ConditionalHeaders;

use super::errors::{Retry, storage_status, write_error_response};
use super::path_validation::parse_webdav_write_target;
use crate::dav_xml;
use crate::filesystem::{DavTarget, Named, name_in_storage};
use crate::state::WebDavState;

mod copy_move;
mod proppatch;
mod put;

pub(super) use copy_move::{handle_copy, handle_move};
pub(super) use proppatch::handle_proppatch;
pub(super) use put::handle_put;

pub(super) async fn handle_delete(state: WebDavState, req: Request) -> Response {
    let uri_path = req.uri().path().to_string();
    let Ok((target, as_collection)) = parse_webdav_write_target(&uri_path, &state.declared_kbs)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (kb, path) = match target {
        DavTarget::Object(kb, path) => (kb, path),
        DavTarget::Root | DavTarget::KbRoot(_) => return StatusCode::BAD_REQUEST.into_response(),
        DavTarget::NonDeclaredKb => return StatusCode::FORBIDDEN.into_response(),
    };
    // Folders are virtual (a key prefix), so a folder delete would be a bulk
    // delete: refused (#281). A missing key spelled as an object still goes
    // through `commit_delete`: idempotent, and the documented retry path.
    match name_in_storage(state.storage.as_ref(), &kb, &path, as_collection).await {
        Ok(Named::Folder) => {
            return dav_xml::error(StatusCode::FORBIDDEN, "no-collection-delete");
        }
        Ok(Named::Nothing) if as_collection => return StatusCode::NOT_FOUND.into_response(),
        Ok(Named::Object(_) | Named::Nothing) => {}
        Err(error) => return storage_status(&error).into_response(),
    }
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

    match notedthat_write::commit_delete(
        state.storage.as_ref(),
        &state.sinks(),
        &kb,
        &path,
        conditionals,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => write_error_response(
            error,
            Retry {
                queue_full: "deleted from storage; retry to clear from search index",
                publish_failed: "deleted from storage; change event not published. Retry DELETE to publish.",
            },
        ),
    }
}

//! Per-method `WebDAV` write handlers (PUT / DELETE / MOVE / COPY).

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use notedthat_core::{ConditionalHeaders, StorageError};

use super::backpressure::delete_backpressure_response;
use super::path_validation::parse_webdav_uri_path;
use crate::filesystem::DavTarget;
use crate::state::WebDavState;

mod copy_move;
mod put;

pub(super) use copy_move::{handle_copy, handle_move};
pub(super) use put::handle_put;

pub(super) async fn handle_delete(state: WebDavState, req: Request) -> Response {
    let uri_path = req.uri().path().to_string();
    let Ok(target) = parse_webdav_uri_path(&uri_path, &state.declared_kbs) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (kb, path) = match target {
        DavTarget::Object(kb, path) => (kb, path),
        DavTarget::Root | DavTarget::KbRoot(_) => return StatusCode::BAD_REQUEST.into_response(),
        DavTarget::NonDeclaredKb => return StatusCode::FORBIDDEN.into_response(),
    };
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

    match notedthat_write::commit_delete(
        state.storage.as_ref(),
        &state.indexer_tx,
        &kb,
        &path,
        conditionals,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(notedthat_write::WriteError::Storage(StorageError::PreconditionFailed)) => {
            StatusCode::PRECONDITION_FAILED.into_response()
        }
        Err(notedthat_write::WriteError::IndexerBackpressureTombstone) => {
            delete_backpressure_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

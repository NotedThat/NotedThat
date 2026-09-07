//! Per-method `WebDAV` write handlers (PUT / DELETE / MOVE / COPY).

use axum::body::to_bytes;
use axum::extract::Request;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use notedthat_core::{ConditionalHeaders, StorageError};

use super::backpressure::{
    backpressure_response, copy_destination_backpressure_response, dav_error_body,
    delete_backpressure_response, move_destination_backpressure_response,
    move_source_tombstone_backpressure_response,
};
use super::helpers::{object_target_or_collection_error, response_with_optional_etag};
use super::path_validation::parse_uri_path;
use crate::filesystem::DavTarget;
use crate::state::WebDavState;

pub(super) async fn handle_put(state: WebDavState, req: Request) -> Response {
    let uri_path = req.uri().path().to_string();
    let Ok(target) = parse_uri_path(&uri_path, &state.declared_kbs) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (kb, path) = match target {
        DavTarget::Object(kb, path) => (kb, path),
        DavTarget::Root | DavTarget::KbRoot(_) => return StatusCode::BAD_REQUEST.into_response(),
        DavTarget::NonDeclaredKb => return StatusCode::FORBIDDEN.into_response(),
    };

    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let conditionals = ConditionalHeaders::from_header_map(req.headers());

    if let Some(content_length) = req
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        && content_length > notedthat_write::MAX_UPLOAD_BYTES
    {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }

    let exists = state
        .storage
        .head_object(&kb, &path, ConditionalHeaders::default())
        .await
        .is_ok();

    let limit = usize::try_from(notedthat_write::MAX_UPLOAD_BYTES)
        .unwrap_or(usize::MAX)
        .saturating_add(1);
    let Ok(body_bytes) = to_bytes(req.into_body(), limit).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };

    match notedthat_write::commit(
        state.storage.as_ref(),
        &state.indexer_tx,
        &kb,
        &path,
        body_bytes,
        content_type.as_deref(),
        conditionals,
    )
    .await
    {
        Ok(outcome) => response_with_optional_etag(
            if exists {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::CREATED
            },
            outcome.etag,
        ),
        Err(notedthat_write::WriteError::TooLarge { .. }) => {
            StatusCode::PAYLOAD_TOO_LARGE.into_response()
        }
        Err(notedthat_write::WriteError::Storage(StorageError::PreconditionFailed)) => {
            StatusCode::PRECONDITION_FAILED.into_response()
        }
        Err(notedthat_write::WriteError::IndexerBackpressureUpsert) => backpressure_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub(super) async fn handle_delete(state: WebDavState, req: Request) -> Response {
    let uri_path = req.uri().path().to_string();
    let Ok(target) = parse_uri_path(&uri_path, &state.declared_kbs) else {
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

pub(super) async fn handle_move(state: WebDavState, req: Request) -> Response {
    handle_copy_or_move(state, req, true).await
}

pub(super) async fn handle_copy(state: WebDavState, req: Request) -> Response {
    handle_copy_or_move(state, req, false).await
}

#[allow(clippy::too_many_lines)]
async fn handle_copy_or_move(state: WebDavState, req: Request, delete_source: bool) -> Response {
    let dest_header = match req
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
    {
        Some(destination) => destination.to_string(),
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    if dest_header.contains('#') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(dest_uri) = dest_header.parse::<Uri>() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let req_host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if let Some(dest_authority) = dest_uri.authority() {
        let req_host_bare = req_host.split(':').next().unwrap_or(req_host);
        let dest_host = dest_authority.as_str();
        let dest_host_bare = dest_host.split(':').next().unwrap_or(dest_host);
        if !req_host_bare.eq_ignore_ascii_case(dest_host_bare) {
            return (
                StatusCode::BAD_GATEWAY,
                dav_error_body("destination-different-server"),
            )
                .into_response();
        }
    }
    let Ok(src_target) = parse_uri_path(req.uri().path(), &state.declared_kbs) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(dst_target) = parse_uri_path(dest_uri.path(), &state.declared_kbs) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (src_kb, src_obj) = match object_target_or_collection_error(src_target) {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    let (dst_kb, dst_obj) = match object_target_or_collection_error(dst_target) {
        Ok(target) => target,
        Err(err) => return err.into_response(),
    };
    if src_kb.as_str() != dst_kb.as_str() {
        return (
            StatusCode::FORBIDDEN,
            dav_error_body("cannot-modify-source"),
        )
            .into_response();
    }
    let src_data = match state
        .storage
        .get_object(&src_kb, &src_obj, None, ConditionalHeaders::default())
        .await
    {
        Ok(object) => object,
        Err(StorageError::NotFound { .. } | StorageError::BucketNotFound { .. }) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let dst_exists = state
        .storage
        .head_object(&dst_kb, &dst_obj, ConditionalHeaders::default())
        .await
        .is_ok();
    let content_type = src_data.meta.content_type.clone();
    let outcome = match notedthat_write::commit(
        state.storage.as_ref(),
        &state.indexer_tx,
        &dst_kb,
        &dst_obj,
        src_data.bytes,
        content_type.as_deref(),
        ConditionalHeaders::default(),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(notedthat_write::WriteError::IndexerBackpressureUpsert) => {
            return if delete_source {
                move_destination_backpressure_response("")
            } else {
                copy_destination_backpressure_response()
            };
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if delete_source {
        // NOTE: Intentional fail-visible partial-completion semantics. Destination write already
        // succeeded and source storage delete already succeeded, but the source search tombstone
        // is missing. 503 tells the client the search index may contain a stale source entry
        // until retry/reindex. Since v1 has no reindex endpoint and retrying the whole MOVE
        // will 404 on GET(src), this propagates the failure visibly.
        match notedthat_write::commit_delete(
            state.storage.as_ref(),
            &state.indexer_tx,
            &src_kb,
            &src_obj,
            ConditionalHeaders::default(),
        )
        .await
        {
            Ok(()) => {}
            Err(notedthat_write::WriteError::IndexerBackpressureTombstone) => {
                return move_source_tombstone_backpressure_response("");
            }
            Err(_) => {
                // Other commit_delete errors after a successful destination write: the source
                // object is already deleted from storage. Return 500 to signal an unexpected
                // failure in the source-tombstone step.
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    }
    response_with_optional_etag(
        if dst_exists {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::CREATED
        },
        outcome.etag,
    )
}

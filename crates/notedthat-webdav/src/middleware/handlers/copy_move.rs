use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use notedthat_core::{ConditionalHeaders, CopyObjectOptions, StorageError};

use crate::state::WebDavState;

use super::super::backpressure::{
    copy_destination_backpressure_response, dav_error_body, move_destination_backpressure_response,
    move_source_tombstone_backpressure_response,
};
use super::super::helpers::{object_target_or_collection_error, response_with_optional_etag};
use super::super::path_validation::parse_uri_path;

fn single_header(headers: &HeaderMap, name: &'static str) -> Result<Option<String>, ()> {
    let mut values = headers.get_all(name).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    first
        .map(|value| value.to_str().map(str::to_owned).map_err(|_| ()))
        .transpose()
}

pub(crate) async fn handle_move(state: WebDavState, req: Request) -> Response {
    handle_copy_or_move(state, req, true).await
}

pub(crate) async fn handle_copy(state: WebDavState, req: Request) -> Response {
    handle_copy_or_move(state, req, false).await
}

#[allow(clippy::too_many_lines)]
async fn handle_copy_or_move(state: WebDavState, req: Request, delete_source: bool) -> Response {
    let Ok(Some(dest_header)) = single_header(req.headers(), "destination") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let overwrite = match single_header(req.headers(), "overwrite") {
        Ok(None) => true,
        Ok(Some(value)) if value == "T" => true,
        Ok(Some(value)) if value == "F" => false,
        Ok(Some(_)) | Err(()) => return StatusCode::BAD_REQUEST.into_response(),
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
        .and_then(|value| value.to_str().ok())
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
        Err(error) => return error.into_response(),
    };
    let (dst_kb, dst_obj) = match object_target_or_collection_error(dst_target) {
        Ok(target) => target,
        Err(error) => return error.into_response(),
    };
    if src_kb != dst_kb {
        return (
            StatusCode::FORBIDDEN,
            dav_error_body("cannot-modify-source"),
        )
            .into_response();
    }
    if src_obj == dst_obj {
        let condition = if delete_source {
            "cannot-move-resource"
        } else {
            "cannot-copy-resource"
        };
        return (StatusCode::FORBIDDEN, dav_error_body(condition)).into_response();
    }
    let src_meta = match state
        .storage
        .head_object(&src_kb, &src_obj, ConditionalHeaders::default())
        .await
    {
        Ok(meta) => meta,
        Err(StorageError::NotFound { .. } | StorageError::BucketNotFound { .. }) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let Some(source_etag) = src_meta.etag else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let dst_exists = overwrite
        && state
            .storage
            .head_object(&dst_kb, &dst_obj, ConditionalHeaders::default())
            .await
            .is_ok();
    let outcome = match notedthat_write::commit_copy(
        state.storage.as_ref(),
        &state.indexer_tx,
        &src_kb,
        &src_obj,
        &dst_obj,
        CopyObjectOptions {
            source_if_match: Some(source_etag.clone()),
            destination_if_none_match: (!overwrite).then(|| "*".to_string()),
            content_type: src_meta.content_type,
        },
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
        Err(notedthat_write::WriteError::Storage(StorageError::PreconditionFailed)) => {
            return StatusCode::PRECONDITION_FAILED.into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if delete_source {
        match notedthat_write::commit_delete(
            state.storage.as_ref(),
            &state.indexer_tx,
            &src_kb,
            &src_obj,
            ConditionalHeaders {
                if_match: Some(source_etag),
                ..ConditionalHeaders::default()
            },
        )
        .await
        {
            Ok(()) => {}
            Err(notedthat_write::WriteError::IndexerBackpressureTombstone) => {
                return move_source_tombstone_backpressure_response("");
            }
            Err(notedthat_write::WriteError::Storage(StorageError::PreconditionFailed)) => {
                return (
                    StatusCode::PRECONDITION_FAILED,
                    "MOVE partially completed: destination copied and queued for indexing; source changed before deletion and was not deleted.",
                )
                    .into_response();
            }
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
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

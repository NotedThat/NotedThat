use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use notedthat_core::{
    ConditionalHeaders, CopyObjectOptions, ObjectState, StorageError, evaluate_write_preconditions,
};
use notedthat_write::sniff_content_type;

use crate::if_header::IfHeader;
use crate::state::WebDavState;

use super::super::backpressure::{
    copy_destination_backpressure_response, dav_error_body, event_publish_failed_response,
    move_destination_backpressure_response, move_source_tombstone_backpressure_response,
};
use super::super::helpers::{object_target_or_collection_error, response_with_optional_etag};
use super::super::path_validation::parse_webdav_uri_path;

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

/// Whether the `If` resource tag `tag` names `kb`/`object`. The tag is an absolute URI
/// or an absolute path, spelled like the `Destination` header.
fn same_object(
    tag: &str,
    state: &WebDavState,
    kb: &notedthat_core::KbSlug,
    object: &notedthat_core::ObjectPath,
) -> bool {
    let Ok(uri) = tag.parse::<Uri>() else {
        return false;
    };
    parse_webdav_uri_path(uri.path(), &state.declared_kbs)
        .ok()
        .and_then(|target| object_target_or_collection_error(target).ok())
        .is_some_and(|(tag_kb, tag_object)| &tag_kb == kb && &tag_object == object)
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
    let Ok(src_target) = parse_webdav_uri_path(req.uri().path(), &state.declared_kbs) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(dst_target) = parse_webdav_uri_path(dest_uri.path(), &state.declared_kbs) else {
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
    // Only the destination: an object already stored under a reserved key
    // (before #279) can still be copied or moved away from it.
    if dst_obj.is_reserved() {
        return StatusCode::BAD_REQUEST.into_response();
    }
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
    // The client's preconditions on the source: If-Match / If-None-Match (RFC 9110
    // §13.2.1) and the WebDAV If header (RFC 4918 §10.4). Both are judged against the
    // ETag the HEAD below reports, and the copy and the MOVE's delete are then pinned to
    // that same ETag, so a write landing after the check still fails with 412.
    let conditionals = ConditionalHeaders::from_header_map(req.headers());
    let Ok(if_header) = IfHeader::from_headers(req.headers()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let src_meta = match state
        .storage
        .head_object(&src_kb, &src_obj, ConditionalHeaders::default())
        .await
    {
        Ok(meta) => meta,
        // Without its preconditions this request would be a 404, so they are ignored
        // (RFC 9110 §13.2.1) and a missing source stays 404 even under If-Match.
        Err(StorageError::NotFound { .. } | StorageError::BucketNotFound { .. }) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let Some(source_etag) = src_meta.etag else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let source_state = ObjectState {
        etag: &source_etag,
        // Write preconditions never read the dates (see `evaluate_write_preconditions`).
        last_modified: std::time::UNIX_EPOCH,
    };
    if evaluate_write_preconditions(Some(source_state), &conditionals).is_err() {
        return StatusCode::PRECONDITION_FAILED.into_response();
    }
    // The destination is looked up only when a tagged `If` list names it and so needs
    // its ETag. Whether the copy created it comes from the backend (`outcome.created`).
    let names_destination = if_header.as_ref().is_some_and(|header| {
        header
            .resources()
            .any(|resource| same_object(resource, &state, &dst_kb, &dst_obj))
    });
    let dst_etag = if names_destination {
        state
            .storage
            .head_object(&dst_kb, &dst_obj, ConditionalHeaders::default())
            .await
            .ok()
            .map(|meta| meta.etag.unwrap_or_default())
    } else {
        None
    };
    if let Some(header) = &if_header {
        let holds = header.evaluate(|resource| match resource {
            None => Some(source_etag.as_str()),
            Some(tag) if same_object(tag, &state, &src_kb, &src_obj) => Some(source_etag.as_str()),
            Some(tag) if same_object(tag, &state, &dst_kb, &dst_obj) => dst_etag.as_deref(),
            // Any other resource is none this request touches; treat it as absent.
            Some(_) => None,
        });
        if !holds {
            return StatusCode::PRECONDITION_FAILED.into_response();
        }
    }
    let outcome = match notedthat_write::commit_copy(
        state.storage.as_ref(),
        &state.sinks(),
        &src_kb,
        &src_obj,
        &dst_obj,
        CopyObjectOptions {
            source_if_match: Some(source_etag.clone()),
            destination_if_none_match: (!overwrite).then(|| "*".to_string()),
            content_type: Some(sniff_content_type(
                src_meta.content_type.as_deref(),
                &dst_obj,
            )),
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
        Err(notedthat_write::WriteError::EventPublishFailed { .. }) => {
            return event_publish_failed_response(if delete_source {
                "destination copied; its change event not published; source unchanged. Retry MOVE to publish."
            } else {
                "destination copied; its change event not published. Retry COPY to publish."
            });
        }
        Err(notedthat_write::WriteError::Storage(StorageError::PreconditionFailed)) => {
            return StatusCode::PRECONDITION_FAILED.into_response();
        }
        // The source is not a manifest startup would accept; nothing was
        // copied and, on MOVE, the source is untouched (#98).
        Err(notedthat_write::WriteError::InvalidManifest { message }) => {
            return (StatusCode::BAD_REQUEST, message).into_response();
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if delete_source {
        match notedthat_write::commit_delete(
            state.storage.as_ref(),
            &state.sinks(),
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
            Err(notedthat_write::WriteError::EventPublishFailed { .. }) => {
                return event_publish_failed_response(
                    "destination copied and source deleted; the source's change event not published. Send DELETE for the source to publish it — DELETE of a missing key is idempotent and still publishes, whereas a retried MOVE would find no source.",
                );
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
        if outcome.created {
            StatusCode::CREATED
        } else {
            StatusCode::NO_CONTENT
        },
        outcome.etag,
    )
}

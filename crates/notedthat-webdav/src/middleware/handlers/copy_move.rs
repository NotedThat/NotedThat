use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use notedthat_core::{
    ConditionalHeaders, CopyObjectOptions, KbSlug, ObjectPath, ObjectState, PutOutcome,
    StorageError, evaluate_write_preconditions,
};
use notedthat_write::sniff_content_type;

use crate::if_header::IfHeader;
use crate::state::WebDavState;

use super::super::backpressure::{
    copy_destination_backpressure_response, dav_error_body, event_publish_failed_response,
    move_destination_backpressure_response, move_source_tombstone_backpressure_response,
};
use super::super::helpers::{
    TargetError, object_target_or_collection_error, response_with_optional_etag,
};
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
fn same_object(tag: &str, state: &WebDavState, kb: &KbSlug, object: &ObjectPath) -> bool {
    let Ok(uri) = tag.parse::<Uri>() else {
        return false;
    };
    parse_webdav_uri_path(uri.path(), &state.declared_kbs)
        .ok()
        .and_then(|target| object_target_or_collection_error(target).ok())
        .is_some_and(|(tag_kb, tag_object)| &tag_kb == kb && &tag_object == object)
}

/// The two methods this handler serves: a MOVE is a COPY that then deletes its source.
#[derive(Clone, Copy)]
enum Transfer {
    Copy,
    Move,
}

impl Transfer {
    /// The DAV precondition a request fails when its destination is its source.
    fn self_target_condition(self) -> &'static str {
        match self {
            Self::Copy => "cannot-copy-resource",
            Self::Move => "cannot-move-resource",
        }
    }

    /// The 503 for a destination whose index event could not be enqueued.
    fn destination_backpressure_response(self) -> Response {
        match self {
            Self::Copy => copy_destination_backpressure_response(),
            Self::Move => move_destination_backpressure_response(),
        }
    }

    /// The 503 message for a destination whose change event was not published.
    fn destination_event_failed_message(self) -> &'static str {
        match self {
            Self::Copy => {
                "destination copied; its change event not published. Retry COPY to publish."
            }
            Self::Move => {
                "destination copied; its change event not published; source unchanged. Retry MOVE to publish."
            }
        }
    }
}

/// The objects a COPY or MOVE reads and writes, once its headers and paths are valid.
struct Targets {
    /// The knowledge base holding both objects; a transfer across KBs is refused.
    kb: KbSlug,
    source: ObjectPath,
    destination: ObjectPath,
    /// Whether an existing destination may be replaced (`Overwrite: T`, the default).
    overwrite: bool,
}

/// The source as its preconditions were judged against it. The copy and the MOVE's
/// delete are pinned to `etag`, so a write landing after the check fails with 412.
struct Source {
    etag: String,
    content_type: Option<String>,
}

pub(crate) async fn handle_move(state: WebDavState, req: Request) -> Response {
    handle_transfer(state, req, Transfer::Move).await
}

pub(crate) async fn handle_copy(state: WebDavState, req: Request) -> Response {
    handle_transfer(state, req, Transfer::Copy).await
}

async fn handle_transfer(state: WebDavState, req: Request, transfer: Transfer) -> Response {
    match transfer_object(&state, req.headers(), req.uri(), transfer).await {
        Ok(response) | Err(response) => response,
    }
}

/// Each step returns the response for its first failing check as `Err`.
async fn transfer_object(
    state: &WebDavState,
    headers: &HeaderMap,
    uri: &Uri,
    transfer: Transfer,
) -> Result<Response, Response> {
    let targets =
        resolve_targets(state, headers, uri.path(), transfer).map_err(Rejection::into_response)?;
    let source = check_source_preconditions(state, headers, &targets).await?;
    let outcome = copy_to_destination(state, &targets, &source, transfer).await?;
    if let Transfer::Move = transfer {
        delete_source(state, &targets, source.etag).await?;
    }
    Ok(response_with_optional_etag(
        if outcome.created {
            StatusCode::CREATED
        } else {
            StatusCode::NO_CONTENT
        },
        outcome.etag,
    ))
}

/// Why a COPY or MOVE was refused before touching storage.
enum Rejection {
    /// A missing, repeated or malformed header or path, or a reserved destination.
    BadRequest,
    /// The `Destination` names another host than the request's.
    DifferentServer,
    /// The source or destination is not an object in a declared KB.
    NotAnObject(TargetError),
    /// The destination is in another KB than the source.
    CrossKb,
    /// The destination is the source.
    SelfTarget(Transfer),
}

impl Rejection {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest => StatusCode::BAD_REQUEST.into_response(),
            Self::DifferentServer => (
                StatusCode::BAD_GATEWAY,
                dav_error_body("destination-different-server"),
            )
                .into_response(),
            Self::NotAnObject(error) => error.into_response(),
            Self::CrossKb => (
                StatusCode::FORBIDDEN,
                dav_error_body("cannot-modify-source"),
            )
                .into_response(),
            Self::SelfTarget(transfer) => (
                StatusCode::FORBIDDEN,
                dav_error_body(transfer.self_target_condition()),
            )
                .into_response(),
        }
    }
}

/// Reads the `Destination` and `Overwrite` headers and refuses a destination on
/// another host than the request's.
fn parse_destination_headers(headers: &HeaderMap) -> Result<(Uri, bool), Rejection> {
    let Ok(Some(dest_header)) = single_header(headers, "destination") else {
        return Err(Rejection::BadRequest);
    };
    let overwrite = match single_header(headers, "overwrite") {
        Ok(None) => true,
        Ok(Some(value)) if value == "T" => true,
        Ok(Some(value)) if value == "F" => false,
        Ok(Some(_)) | Err(()) => return Err(Rejection::BadRequest),
    };
    if dest_header.contains('#') {
        return Err(Rejection::BadRequest);
    }
    let Ok(dest_uri) = dest_header.parse::<Uri>() else {
        return Err(Rejection::BadRequest);
    };
    let req_host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if let Some(dest_authority) = dest_uri.authority() {
        let req_host_bare = req_host.split(':').next().unwrap_or(req_host);
        let dest_host = dest_authority.as_str();
        let dest_host_bare = dest_host.split(':').next().unwrap_or(dest_host);
        if !req_host_bare.eq_ignore_ascii_case(dest_host_bare) {
            return Err(Rejection::DifferentServer);
        }
    }
    Ok((dest_uri, overwrite))
}

/// Resolves the request path and `Destination` to two distinct objects in one KB.
fn resolve_targets(
    state: &WebDavState,
    headers: &HeaderMap,
    source_path: &str,
    transfer: Transfer,
) -> Result<Targets, Rejection> {
    let (dest_uri, overwrite) = parse_destination_headers(headers)?;
    let Ok(src_target) = parse_webdav_uri_path(source_path, &state.declared_kbs) else {
        return Err(Rejection::BadRequest);
    };
    let Ok(dst_target) = parse_webdav_uri_path(dest_uri.path(), &state.declared_kbs) else {
        return Err(Rejection::BadRequest);
    };
    let (src_kb, src_obj) =
        object_target_or_collection_error(src_target).map_err(Rejection::NotAnObject)?;
    let (dst_kb, dst_obj) =
        object_target_or_collection_error(dst_target).map_err(Rejection::NotAnObject)?;
    // Only the destination: an object already stored under a reserved key
    // (before #279) can still be copied or moved away from it.
    if dst_obj.is_reserved() {
        return Err(Rejection::BadRequest);
    }
    if src_kb != dst_kb {
        return Err(Rejection::CrossKb);
    }
    if src_obj == dst_obj {
        return Err(Rejection::SelfTarget(transfer));
    }
    Ok(Targets {
        kb: src_kb,
        source: src_obj,
        destination: dst_obj,
        overwrite,
    })
}

/// Judges the client's preconditions on the source: `If-Match` / `If-None-Match`
/// (RFC 9110 §13.2.1) and the `WebDAV` `If` header (RFC 4918 §10.4), both against the
/// `ETag` its HEAD reports.
async fn check_source_preconditions(
    state: &WebDavState,
    headers: &HeaderMap,
    targets: &Targets,
) -> Result<Source, Response> {
    let conditionals = ConditionalHeaders::from_header_map(headers);
    let Ok(if_header) = IfHeader::from_headers(headers) else {
        return Err(StatusCode::BAD_REQUEST.into_response());
    };
    let src_meta = match state
        .storage
        .head_object(&targets.kb, &targets.source, ConditionalHeaders::default())
        .await
    {
        Ok(meta) => meta,
        // Without its preconditions this request would be a 404, so they are ignored
        // (RFC 9110 §13.2.1) and a missing source stays 404 even under If-Match.
        Err(StorageError::NotFound { .. } | StorageError::BucketNotFound { .. }) => {
            return Err(StatusCode::NOT_FOUND.into_response());
        }
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    };
    let Some(source_etag) = src_meta.etag else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
    };
    let source_state = ObjectState {
        etag: &source_etag,
        // Write preconditions never read the dates (see `evaluate_write_preconditions`).
        last_modified: std::time::UNIX_EPOCH,
    };
    if evaluate_write_preconditions(Some(source_state), &conditionals).is_err() {
        return Err(StatusCode::PRECONDITION_FAILED.into_response());
    }
    // The destination is looked up only when a tagged `If` list names it and so needs
    // its ETag. Whether the copy created it comes from the backend (`outcome.created`).
    let names_destination = if_header.as_ref().is_some_and(|header| {
        header
            .resources()
            .any(|resource| same_object(resource, state, &targets.kb, &targets.destination))
    });
    let dst_etag = if names_destination {
        state
            .storage
            .head_object(
                &targets.kb,
                &targets.destination,
                ConditionalHeaders::default(),
            )
            .await
            .ok()
            .map(|meta| meta.etag.unwrap_or_default())
    } else {
        None
    };
    if let Some(header) = &if_header {
        let holds = header.evaluate(|resource| match resource {
            None => Some(source_etag.as_str()),
            Some(tag) if same_object(tag, state, &targets.kb, &targets.source) => {
                Some(source_etag.as_str())
            }
            Some(tag) if same_object(tag, state, &targets.kb, &targets.destination) => {
                dst_etag.as_deref()
            }
            // Any other resource is none this request touches; treat it as absent.
            Some(_) => None,
        });
        if !holds {
            return Err(StatusCode::PRECONDITION_FAILED.into_response());
        }
    }
    Ok(Source {
        etag: source_etag,
        content_type: src_meta.content_type,
    })
}

async fn copy_to_destination(
    state: &WebDavState,
    targets: &Targets,
    source: &Source,
    transfer: Transfer,
) -> Result<PutOutcome, Response> {
    match notedthat_write::commit_copy(
        state.storage.as_ref(),
        &state.sinks(),
        &targets.kb,
        &targets.source,
        &targets.destination,
        CopyObjectOptions {
            source_if_match: Some(source.etag.clone()),
            destination_if_none_match: (!targets.overwrite).then(|| "*".to_string()),
            content_type: Some(sniff_content_type(
                source.content_type.as_deref(),
                &targets.destination,
            )),
        },
    )
    .await
    {
        Ok(outcome) => Ok(outcome),
        Err(notedthat_write::WriteError::IndexerBackpressureUpsert) => {
            Err(transfer.destination_backpressure_response())
        }
        Err(notedthat_write::WriteError::EventPublishFailed { .. }) => Err(
            event_publish_failed_response(transfer.destination_event_failed_message()),
        ),
        Err(notedthat_write::WriteError::Storage(StorageError::PreconditionFailed)) => {
            Err(StatusCode::PRECONDITION_FAILED.into_response())
        }
        // The source is not a manifest startup would accept; nothing was
        // copied and, on MOVE, the source is untouched (#98).
        Err(notedthat_write::WriteError::InvalidManifest { message }) => {
            Err((StatusCode::BAD_REQUEST, message).into_response())
        }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    }
}

/// The second half of a MOVE, run once the destination holds the copy.
async fn delete_source(
    state: &WebDavState,
    targets: &Targets,
    source_etag: String,
) -> Result<(), Response> {
    match notedthat_write::commit_delete(
        state.storage.as_ref(),
        &state.sinks(),
        &targets.kb,
        &targets.source,
        ConditionalHeaders {
            if_match: Some(source_etag),
            ..ConditionalHeaders::default()
        },
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(notedthat_write::WriteError::IndexerBackpressureTombstone) => {
            Err(move_source_tombstone_backpressure_response())
        }
        Err(notedthat_write::WriteError::EventPublishFailed { .. }) => {
            Err(event_publish_failed_response(
                "destination copied and source deleted; the source's change event not published. Send DELETE for the source to publish it — DELETE of a missing key is idempotent and still publishes, whereas a retried MOVE would find no source.",
            ))
        }
        Err(notedthat_write::WriteError::Storage(StorageError::PreconditionFailed)) => Err((
            StatusCode::PRECONDITION_FAILED,
            "MOVE partially completed: destination copied and queued for indexing; source changed before deletion and was not deleted.",
        )
            .into_response()),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    }
}

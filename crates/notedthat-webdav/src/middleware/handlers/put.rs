use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use notedthat_core::{ConditionalHeaders, StageError, StagedBody, StorageError};

use crate::filesystem::DavTarget;
use crate::state::WebDavState;

use super::super::backpressure::backpressure_response;
use super::super::helpers::response_with_optional_etag;
use super::super::path_validation::parse_webdav_uri_path;

fn content_length(headers: &HeaderMap) -> Result<Option<u64>, ()> {
    let mut values = headers.get_all("content-length").iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    first
        .map(|value| {
            value
                .to_str()
                .map_err(|_| ())?
                .parse::<u64>()
                .map_err(|_| ())
        })
        .transpose()
}

pub(crate) async fn handle_put(state: WebDavState, req: Request) -> Response {
    let uri_path = req.uri().path().to_string();
    let Ok(target) = parse_webdav_uri_path(&uri_path, &state.declared_kbs) else {
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
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let conditionals = ConditionalHeaders::from_header_map(req.headers());
    let Ok(expected_len) = content_length(req.headers()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if expected_len.is_some_and(|length| length > notedthat_write::MAX_UPLOAD_BYTES) {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let exists = state
        .storage
        .head_object(&kb, &path, ConditionalHeaders::default())
        .await
        .is_ok();
    let body = match StagedBody::stage_stream(
        req.into_body().into_data_stream(),
        expected_len,
        notedthat_write::MAX_UPLOAD_BYTES,
        &state.staging_config,
    )
    .await
    {
        Ok(body) => body,
        Err(StageError::TooLarge { .. }) => {
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        Err(StageError::LengthMismatch { .. } | StageError::Read { .. }) => {
            return StatusCode::BAD_REQUEST.into_response();
        }
        Err(StageError::Write { .. } | StageError::Config { .. }) => {
            return StatusCode::INSUFFICIENT_STORAGE.into_response();
        }
    };
    match notedthat_write::commit(
        state.storage.as_ref(),
        &state.indexer_tx,
        &kb,
        &path,
        body,
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

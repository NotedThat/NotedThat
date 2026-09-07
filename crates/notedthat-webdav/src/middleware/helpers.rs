//! Small helpers shared by the write-method handlers.

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use notedthat_core::{KbSlug, ObjectPath};

use super::backpressure::dav_error_body;
use crate::filesystem::DavTarget;

pub(super) enum TargetError {
    Collection,
    Forbidden,
}

impl TargetError {
    pub(super) fn into_response(self) -> Response {
        match self {
            Self::Collection => {
                (StatusCode::FORBIDDEN, dav_error_body("no-collection-move")).into_response()
            }
            Self::Forbidden => StatusCode::FORBIDDEN.into_response(),
        }
    }
}

pub(super) fn object_target_or_collection_error(
    target: DavTarget,
) -> Result<(KbSlug, ObjectPath), TargetError> {
    match target {
        DavTarget::Object(kb, path) => Ok((kb, path)),
        DavTarget::Root | DavTarget::KbRoot(_) => Err(TargetError::Collection),
        DavTarget::NonDeclaredKb => Err(TargetError::Forbidden),
    }
}

pub(super) fn response_with_optional_etag(status: StatusCode, etag: Option<String>) -> Response {
    let mut response = status.into_response();
    if let Some(etag) = etag
        && let Ok(value) = HeaderValue::from_str(&etag)
    {
        response.headers_mut().insert("etag", value);
    }
    response
}

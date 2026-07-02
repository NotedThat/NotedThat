//! The single write path primitive — all PUT operations funnel through [`commit`].
//!
//! In M5, `WebDAV` will call this path. For now, it is
//! a thin wrapper around [`Storage::put_object`].

use crate::error::ApiError;
use bytes::Bytes;
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, PutOutcome, Storage};

/// Store an object, replacing any existing content at the same path.
///
/// This is the canonical write path. All PUT handlers must call this function
/// rather than calling [`Storage::put_object`] directly, so future cross-cutting
/// concerns (`ETag` generation, audit events) can be added in one place.
pub async fn commit(
    storage: &dyn Storage,
    kb: &KbSlug,
    path: &ObjectPath,
    bytes: Bytes,
    content_type: Option<&str>,
    conditionals: ConditionalHeaders,
) -> Result<PutOutcome, ApiError> {
    storage
        .put_object(kb, path, bytes, content_type, conditionals)
        .await
        .map_err(Into::into)
}

//! `Storage` trait — object store abstraction.

use crate::conditional::ConditionalHeaders;
use crate::error::StorageError;
use crate::kb::{KbManifest, ObjectMeta};
use crate::object_path::ObjectPath;
use crate::range::ByteRange;
use crate::slug::KbSlug;
use async_trait::async_trait;
use bytes::Bytes;

/// The bytes and metadata returned by a GET or HEAD operation.
pub struct ObjectRead {
    /// The raw object bytes.
    pub bytes: Bytes,
    /// Associated metadata (size, content-type, last-modified).
    pub meta: ObjectMeta,
    /// `Content-Range: bytes start-end/total` header value from the backend
    /// when responding to a range request, or `None` for full-body reads.
    /// Passed through to HTTP clients on 206 responses.
    pub content_range: Option<String>,
}

/// The result of a LIST operation.
///
/// # Invariant
///
/// `truncated == next_cursor.is_some()`. A response where `truncated=true` MUST supply a
/// `next_cursor`; a response where `next_cursor=Some(_)` MUST also set `truncated=true`.
#[derive(Debug)]
pub struct ListResponse {
    /// The matching objects, up to the requested `limit`.
    pub objects: Vec<ObjectMeta>,
    /// `true` if the backend indicated more objects exist beyond `limit`.
    pub truncated: bool,
    /// Opaque backend continuation token. Present exactly when `truncated=true`.
    ///
    /// Pass this value unchanged as `cursor` on the next `list_objects` call to retrieve the
    /// next page. Clients MUST NOT parse, validate, or store this value beyond the immediate
    /// next request. Invalid or expired tokens cause the backend to return
    /// `StorageError::BackendUnavailable`.
    pub next_cursor: Option<String>,
}

/// Return value from [`Storage::put_object`]. Carries the `ETag` of the stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutOutcome {
    /// `ETag` from the backend (opaque, quoted per RFC 7232 §2.3), or `None` if not returned.
    pub etag: Option<String>,
}

/// The storage abstraction shared by all `NotedThat` components.
///
/// Implementations include `notedthat_storage_s3::S3Storage` (production)
/// and `notedthat_api_http::testing::InMemoryStorage` (tests).
///
/// # Object safety
///
/// This trait is designed to be used as `Arc<dyn Storage>` from axum handlers.
/// All methods take `&self` (not `&mut self`) to allow sharing across threads.
#[async_trait]
pub trait Storage: Send + Sync {
    /// Idempotently create the S3 bucket for the given KB.
    ///
    /// Returns `Ok(())` if the bucket already exists (owned by this account).
    async fn ensure_bucket(&self, kb: &KbSlug) -> Result<(), StorageError>;

    /// Read the KB manifest from `.notedthat/manifest.json` in the KB's bucket.
    ///
    /// Returns `Err(StorageError::NotFound)` if no manifest exists yet.
    async fn read_manifest(&self, kb: &KbSlug) -> Result<KbManifest, StorageError>;

    /// Write (overwrite) the KB manifest in the KB's bucket.
    async fn write_manifest(&self, kb: &KbSlug, manifest: &KbManifest) -> Result<(), StorageError>;

    /// Return metadata for an object without fetching its body.
    ///
    /// `conditionals` carries raw HTTP conditional headers for the backend to evaluate.
    /// Returns `Err(StorageError::NotFound)` if the object does not exist.
    async fn head_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectMeta, StorageError>;

    /// Fetch an object's bytes and metadata.
    ///
    /// `range` carries parsed byte ranges for the backend, and `conditionals`
    /// carries raw HTTP conditional headers for the backend to evaluate.
    /// Returns `Err(StorageError::NotFound)` if the object does not exist.
    async fn get_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<Vec<ByteRange>>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError>;

    /// Store an object, overwriting any existing object at the same path.
    ///
    /// The `content_type` is stored with the object and echoed on GET/HEAD.
    /// `conditionals` carries raw HTTP conditional headers for the backend to evaluate.
    async fn put_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        bytes: Bytes,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError>;

    /// Delete an object.
    ///
    /// `conditionals` carries raw HTTP conditional headers for the backend to evaluate.
    /// This operation is **idempotent** — deleting a non-existent object returns
    /// `Ok(())` (matching S3 semantics per Metis directive).
    async fn delete_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<(), StorageError>;

    /// List objects in the KB, optionally filtered by a prefix.
    ///
    /// Results are capped at `limit` (default 100, max 1000).
    ///
    /// Pass `cursor = None` on the first call. On subsequent calls, pass the opaque
    /// `next_cursor` value from the previous [`ListResponse`] unchanged. An invalid or
    /// expired cursor causes the backend to return `StorageError::BackendUnavailable`.
    ///
    /// # Invariant
    ///
    /// Every returned `ListResponse` satisfies `truncated == next_cursor.is_some()`.
    async fn list_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<ListResponse, StorageError>;
}

#[cfg(test)]
mod tests {
    fn assert_send_sync<T: Send + Sync + ?Sized>() {}

    #[test]
    fn storage_is_dyn_compatible_and_send_sync() {
        assert_send_sync::<dyn crate::storage::Storage>();
    }
}

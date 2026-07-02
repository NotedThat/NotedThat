//! `Storage` trait — object store abstraction.

use crate::error::StorageError;
use crate::kb::{KbManifest, ObjectMeta};
use crate::object_path::ObjectPath;
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
pub struct ListResponse {
    /// The matching objects, up to the requested `limit`.
    pub objects: Vec<ObjectMeta>,
    /// `true` if the backend indicated more objects exist beyond `limit`.
    /// Cursor/continuation is deferred to M3.
    pub truncated: bool,
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
    /// Returns `Err(StorageError::NotFound)` if the object does not exist.
    async fn head_object(&self, kb: &KbSlug, path: &ObjectPath)
    -> Result<ObjectMeta, StorageError>;

    /// Fetch an object's bytes and metadata.
    ///
    /// Returns `Err(StorageError::NotFound)` if the object does not exist.
    /// Range support is deferred to M3.
    async fn get_object(&self, kb: &KbSlug, path: &ObjectPath) -> Result<ObjectRead, StorageError>;

    /// Store an object, overwriting any existing object at the same path.
    ///
    /// The `content_type` is stored with the object and echoed on GET/HEAD.
    async fn put_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        bytes: Bytes,
        content_type: Option<&str>,
    ) -> Result<(), StorageError>;

    /// Delete an object.
    ///
    /// This operation is **idempotent** — deleting a non-existent object returns
    /// `Ok(())` (matching S3 semantics per Metis directive).
    async fn delete_object(&self, kb: &KbSlug, path: &ObjectPath) -> Result<(), StorageError>;

    /// List objects in the KB, optionally filtered by a prefix.
    ///
    /// Results are capped at `limit` (default 100, max 1000).
    /// Cursor/continuation tokens are deferred to M3.
    async fn list_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
        limit: u32,
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

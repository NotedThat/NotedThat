//! `Storage` trait — object store abstraction.

use crate::conditional::ConditionalHeaders;
use crate::error::StorageError;
use crate::kb::{KbManifest, ObjectMeta};
use crate::object_path::ObjectPath;
use crate::range::ByteRange;
use crate::slug::KbSlug;
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use std::pin::Pin;

use crate::staging::StagedBody;

/// Ordered chunks returned by a streaming object read.
pub type ObjectChunkStream = Pin<Box<dyn Stream<Item = Result<Bytes, StorageError>> + Send>>;

/// Metadata and ordered chunks returned without materializing the object.
pub struct ObjectStream {
    /// Ordered object body chunks.
    pub chunks: ObjectChunkStream,
    /// Associated metadata.
    pub meta: ObjectMeta,
    /// Backend `Content-Range` value for range reads.
    pub content_range: Option<String>,
}

/// Preconditions and metadata for a server-side object copy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopyObjectOptions {
    /// Required source `ETag` match when present.
    pub source_if_match: Option<String>,
    /// Required destination non-match condition when present.
    pub destination_if_none_match: Option<String>,
    /// Content type stored on the destination.
    pub content_type: Option<String>,
}

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
    /// next request.
    ///
    /// What an *invalid* token does is backend-defined and must not be relied on. A backend
    /// that detects one returns `StorageError::BackendUnavailable`; some S3 implementations
    /// accept an unrecognised token and answer with a page instead. Since the token is
    /// opaque and backend-issued, a client has no legitimate reason to send one that did
    /// not come from this API.
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

    /// Fetch metadata and an ordered body stream without whole-object buffering.
    async fn get_object_stream(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<Vec<ByteRange>>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectStream, StorageError>;

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

    /// Store an owned staged body without loading file-backed bodies into memory.
    async fn put_staged_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        body: StagedBody,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError>;

    /// Copy an object within one KB using backend-native conditional copy.
    async fn copy_object(
        &self,
        kb: &KbSlug,
        source: &ObjectPath,
        destination: &ObjectPath,
        options: CopyObjectOptions,
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
    /// `next_cursor` value from the previous [`ListResponse`] unchanged. What an invalid
    /// cursor does is backend-defined — see [`ListResponse::next_cursor`].
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

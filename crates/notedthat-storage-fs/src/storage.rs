//! [`Storage`] over a local directory tree.

use std::fs::Metadata;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use notedthat_core::{
    ByteRange, ConditionalHeaders, CopyObjectOptions, EtagHasher, KbManifest, KbSlug, ListResponse,
    ObjectMeta, ObjectPath, ObjectRead, ObjectState, ObjectStream, PutOutcome, StagedBody, Storage,
    StorageError, TenantSlug, evaluate_read_preconditions, evaluate_write_preconditions,
    matches_if_match, resolve_range, unix_seconds_i64,
};

use crate::commit;
use crate::config::FsConfig;
use crate::errors;
use crate::layout::{Layout, bucket_name, key_of};
use crate::listing::{OrderedWalk, decode_cursor, encode_cursor};
use crate::locks::KeyLocks;
use crate::meta::{MetaStore, ObjectAttrs};

/// Key the KB manifest lives at, matching the S3 adapter exactly.
const MANIFEST_KEY: &str = ".notedthat/manifest.json";

/// Bytes read per chunk when streaming an object body.
const STREAM_CHUNK: usize = 64 * 1024;

/// Largest page `list_objects` will return, matching S3's `MaxKeys` ceiling.
const MAX_LIST_LIMIT: u32 = 1000;

/// [`Storage`] backed by a local directory tree.
///
/// Objects are stored at their key path under a per-KB directory, so the tree can be
/// read, edited, grepped and backed up with ordinary tools. See [`crate::layout`] for
/// the shape and [`crate::meta`] for how an out-of-band edit is noticed.
#[derive(Debug, Clone)]
pub struct FsStorage {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    layout: Layout,
    meta: MetaStore,
    locks: KeyLocks,
    tenant: TenantSlug,
}

impl FsStorage {
    /// Build a backend over an already-validated root.
    ///
    /// Construction does no I/O. Call [`crate::open_root`] first: it proves the root is
    /// usable and claims it for this process.
    #[must_use]
    pub fn new(config: &FsConfig, root: PathBuf, tenant: TenantSlug) -> Self {
        Self {
            inner: Arc::new(Inner {
                layout: Layout::new(root, config.file_mode, config.dir_mode),
                meta: MetaStore::new(config.metadata),
                locks: KeyLocks::new(),
                tenant,
            }),
        }
    }

    fn bucket(&self, kb: &KbSlug) -> String {
        bucket_name(&self.inner.tenant, kb)
    }

    /// Run blocking filesystem work off the async runtime.
    ///
    /// One `spawn_blocking` per operation rather than per syscall: `tokio::fs` is itself
    /// a `spawn_blocking` per call, so a write built from it would cost eight or ten
    /// hops where this costs one.
    async fn blocking<T, F>(&self, work: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(&Inner) -> Result<T, StorageError> + Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || work(&inner))
            .await
            .map_err(|error| StorageError::Other {
                source: Box::new(std::io::Error::other(error)),
            })?
    }
}

impl Inner {
    /// Stat an object, treating anything that is not a regular file as absent.
    ///
    /// A directory is not an object. `WebDAV` depends on this: it distinguishes a resource
    /// from a collection by a `head_object` that reports `NotFound`, then a prefix list.
    fn stat(path: &Path, key: &str) -> Result<Metadata, StorageError> {
        let metadata =
            std::fs::symlink_metadata(path).map_err(|error| errors::object(key, error))?;
        if !metadata.is_file() {
            return Err(StorageError::NotFound {
                key: key.to_string(),
            });
        }
        Ok(metadata)
    }

    /// Stat plus resolved metadata, rehashing only when the file has moved on.
    fn resolve(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(PathBuf, Metadata, ObjectAttrs), StorageError> {
        let path = self.layout.object_path(bucket, key)?;
        let metadata = Self::stat(&path, key)?;
        let hash_path = path.clone();
        let attrs = self
            .meta
            .resolve(&self.layout, bucket, key, &metadata, || {
                hash_file(&hash_path).map_err(|error| errors::object(key, error))
            })?;
        Ok((path, metadata, attrs))
    }

    fn object_meta(key: &str, size: u64, metadata: &Metadata, attrs: &ObjectAttrs) -> ObjectMeta {
        ObjectMeta {
            key: key.to_string(),
            size,
            last_modified: metadata.modified().ok().map(unix_seconds_i64),
            content_type: attrs.content_type.clone(),
            etag: Some(attrs.etag.clone()),
        }
    }

    fn state<'a>(attrs: &'a ObjectAttrs, metadata: &Metadata) -> ObjectState<'a> {
        ObjectState {
            etag: &attrs.etag,
            last_modified: metadata
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
        }
    }

    /// Current state of a key, for a write precondition. `None` when absent.
    fn current(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(Metadata, ObjectAttrs)>, StorageError> {
        match self.resolve(bucket, key) {
            Ok((_, metadata, attrs)) => Ok(Some((metadata, attrs))),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Commit `write` as the new content of `key`, under already-checked preconditions.
    fn store(
        &self,
        bucket: &str,
        key: &str,
        content_type: Option<&str>,
        write: impl FnOnce(&mut std::fs::File, &mut EtagHasher) -> std::io::Result<()>,
    ) -> Result<PutOutcome, StorageError> {
        let path = self.layout.object_path(bucket, key)?;
        let parent = path
            .parent()
            .ok_or_else(|| crate::layout::unsupported_key(format!("key '{key}' has no parent")))?;
        commit::create_dir_all(parent, self.layout.dir_mode())
            .map_err(|error| errors::object(key, error))?;

        let mut staged = commit::temp_in(parent).map_err(|error| errors::object(key, error))?;
        let mut hasher = EtagHasher::new();
        write(staged.as_file_mut(), &mut hasher).map_err(|error| errors::object(key, error))?;
        let etag = hasher.finish();

        // The stamp comes from the file we just wrote, taken before the rename, so it can
        // only ever describe these bytes. Stat-ing `path` afterwards would instead
        // describe whatever sits there at that instant — an editor saving over the object
        // in that window would leave our `ETag` recorded against their content, matching
        // `is_fresh_for` and reading as fresh forever after.
        let metadata = commit::finish(staged, &path, self.layout.file_mode())
            .map_err(|error| errors::object(key, error))?;
        let attrs = ObjectAttrs::new(etag.clone(), content_type.map(str::to_string), &metadata);
        self.meta.write(&self.layout, bucket, key, &attrs)?;

        Ok(PutOutcome { etag: Some(etag) })
    }
}

/// Hash a file's current content without holding it in memory.
fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = EtagHasher::new();
    let mut buffer = vec![0_u8; STREAM_CHUNK];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finish())
}

/// Read `len` bytes starting at `start`.
fn read_slice(path: &Path, start: u64, len: u64) -> std::io::Result<Bytes> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let capacity = usize::try_from(len).unwrap_or(usize::MAX);
    let mut buffer = BytesMut::zeroed(capacity);
    file.read_exact(&mut buffer)?;
    Ok(buffer.freeze())
}

#[async_trait]
impl Storage for FsStorage {
    async fn ensure_bucket(&self, kb: &KbSlug) -> Result<(), StorageError> {
        let bucket = self.bucket(kb);
        self.blocking(move |inner| {
            commit::create_dir_all(&inner.layout.bucket_dir(&bucket), inner.layout.dir_mode())
                .and_then(|()| {
                    commit::create_dir_all(
                        &inner.layout.meta_bucket_dir(&bucket),
                        inner.layout.dir_mode(),
                    )
                })
                .map_err(|error| errors::backend_at(&format!("creating {bucket}"), &error))?;
            commit::sync_dir(Some(inner.layout.root()));
            Ok(())
        })
        .await
    }

    async fn read_manifest(&self, kb: &KbSlug) -> Result<KbManifest, StorageError> {
        let bucket = self.bucket(kb);
        self.blocking(move |inner| {
            let path = inner.layout.object_path(&bucket, MANIFEST_KEY)?;
            let bytes = std::fs::read(&path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    StorageError::NotFound {
                        key: MANIFEST_KEY.to_string(),
                    }
                } else {
                    // Every non-missing manifest failure is `BackendUnavailable`, matching
                    // the S3 adapter — including a corrupt one, below.
                    errors::backend_at(&format!("reading the manifest for {bucket}"), &error)
                }
            })?;
            let manifest: KbManifest = serde_json::from_slice(&bytes).map_err(|error| {
                StorageError::BackendUnavailable {
                    message: format!("deserializing the manifest for {bucket}: {error}"),
                }
            })?;
            manifest
                .validate()
                .map_err(|error| StorageError::BackendUnavailable {
                    message: format!("manifest validation failed for {bucket}: {error}"),
                })?;
            Ok(manifest)
        })
        .await
    }

    async fn write_manifest(&self, kb: &KbSlug, manifest: &KbManifest) -> Result<(), StorageError> {
        let bucket = self.bucket(kb);
        let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
            StorageError::BackendUnavailable {
                message: format!("serializing the manifest: {error}"),
            }
        })?;
        // Unconditional last-writer-wins, as on S3, but under the manifest key's lock like
        // every other writer. The atomic rename already stops a browser catching a
        // half-written manifest; the lock is what stops two writers interleaving their
        // rename and their sidecar write, which would leave one writer's `ETag` recorded
        // against the other's file — a stamp that matches, and so reads as fresh.
        let _guard = self.inner.locks.key(&bucket, MANIFEST_KEY).await;
        self.blocking(move |inner| {
            inner
                .store(
                    &bucket,
                    MANIFEST_KEY,
                    Some("application/json"),
                    |file, hasher| {
                        hasher.update(&bytes);
                        file.write_all(&bytes)
                    },
                )
                .map_err(|error| match error {
                    StorageError::NotFound { .. } | StorageError::Other { .. } => {
                        StorageError::BackendUnavailable {
                            message: format!("writing the manifest for {bucket} failed"),
                        }
                    }
                    other => other,
                })?;
            Ok(())
        })
        .await
    }

    async fn head_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectMeta, StorageError> {
        let bucket = self.bucket(kb);
        let key = key_of(path).to_string();
        self.blocking(move |inner| {
            let (_, metadata, attrs) = inner.resolve(&bucket, &key)?;
            evaluate_read_preconditions(Inner::state(&attrs, &metadata), &conditionals)?;
            Ok(Inner::object_meta(&key, metadata.len(), &metadata, &attrs))
        })
        .await
    }

    async fn get_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<Vec<ByteRange>>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        let bucket = self.bucket(kb);
        let key = key_of(path).to_string();
        self.blocking(move |inner| {
            let (file_path, metadata, attrs) = inner.resolve(&bucket, &key)?;
            evaluate_read_preconditions(Inner::state(&attrs, &metadata), &conditionals)?;

            let total = metadata.len();
            let (bytes, content_range) = match resolve_range(total, range.as_deref())? {
                Some((exclusive, content_range)) => (
                    read_slice(&file_path, exclusive.start, exclusive.end - exclusive.start)
                        .map_err(|error| errors::object(&key, error))?,
                    Some(content_range),
                ),
                None => (
                    Bytes::from(
                        std::fs::read(&file_path).map_err(|error| errors::object(&key, error))?,
                    ),
                    None,
                ),
            };

            // As on S3, `size` reports the length served, not the object length.
            let meta = Inner::object_meta(&key, bytes.len() as u64, &metadata, &attrs);
            Ok(ObjectRead {
                bytes,
                meta,
                content_range,
            })
        })
        .await
    }

    async fn get_object_stream(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<Vec<ByteRange>>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectStream, StorageError> {
        let bucket = self.bucket(kb);
        let key = key_of(path).to_string();

        // Resolve and open under the lock, then stream from the open handle. The handle
        // pins the inode, so a concurrent overwrite leaves this reader on the bytes whose
        // ETag we just reported rather than splicing two versions together.
        let (file, meta, content_range) = self
            .blocking(move |inner| {
                let (file_path, metadata, attrs) = inner.resolve(&bucket, &key)?;
                evaluate_read_preconditions(Inner::state(&attrs, &metadata), &conditionals)?;

                let total = metadata.len();
                let resolved = resolve_range(total, range.as_deref())?;
                let mut file =
                    std::fs::File::open(&file_path).map_err(|error| errors::object(&key, error))?;

                let (length, content_range) = match resolved {
                    Some((exclusive, content_range)) => {
                        file.seek(SeekFrom::Start(exclusive.start))
                            .map_err(|error| errors::object(&key, error))?;
                        (exclusive.end - exclusive.start, Some(content_range))
                    }
                    None => (total, None),
                };

                let meta = Inner::object_meta(&key, length, &metadata, &attrs);
                let reader = tokio::io::AsyncReadExt::take(tokio::fs::File::from_std(file), length);
                Ok((reader, meta, content_range))
            })
            .await?;

        let chunks = tokio_util::io::ReaderStream::with_capacity(file, STREAM_CHUNK);
        Ok(ObjectStream {
            chunks: Box::pin(futures::StreamExt::map(chunks, |chunk| {
                chunk.map_err(|error| StorageError::Other {
                    source: Box::new(error),
                })
            })),
            meta,
            content_range,
        })
    }

    async fn put_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        bytes: Bytes,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        let bucket = self.bucket(kb);
        let key = key_of(path).to_string();
        let content_type = content_type.map(str::to_string);
        log_ignored_write_dates(&conditionals);

        let _guard = self.inner.locks.key(&bucket, &key).await;
        self.blocking(move |inner| {
            let current = inner.current(&bucket, &key)?;
            evaluate_write_preconditions(
                current
                    .as_ref()
                    .map(|(metadata, attrs)| Inner::state(attrs, metadata)),
                &conditionals,
            )?;
            inner.store(&bucket, &key, content_type.as_deref(), |file, hasher| {
                hasher.update(&bytes);
                file.write_all(&bytes)
            })
        })
        .await
    }

    async fn put_staged_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        body: StagedBody,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        let bucket = self.bucket(kb);
        let key = key_of(path).to_string();
        let content_type = content_type.map(str::to_string);

        let _guard = self.inner.locks.key(&bucket, &key).await;
        self.blocking(move |inner| {
            let current = inner.current(&bucket, &key)?;
            evaluate_write_preconditions(
                current
                    .as_ref()
                    .map(|(metadata, attrs)| Inner::state(attrs, metadata)),
                &conditionals,
            )?;

            inner.store(&bucket, &key, content_type.as_deref(), |file, hasher| {
                // Copied rather than renamed into place, deliberately. StagedBody owns a
                // TempPath that unlinks on drop and offers no way to take it, so a
                // rename would delete the object we just committed; the staging directory
                // is usually a different mount anyway, and a staged file carries 0600.
                if let Some(bytes) = body.memory_bytes() {
                    hasher.update(bytes);
                    return file.write_all(bytes);
                }
                let mut reader = body.open_blocking()?;
                let mut buffer = vec![0_u8; STREAM_CHUNK];
                loop {
                    let read = reader.read(&mut buffer)?;
                    if read == 0 {
                        return Ok(());
                    }
                    hasher.update(&buffer[..read]);
                    file.write_all(&buffer[..read])?;
                }
            })
        })
        .await
    }

    async fn copy_object(
        &self,
        kb: &KbSlug,
        source: &ObjectPath,
        destination: &ObjectPath,
        options: CopyObjectOptions,
    ) -> Result<PutOutcome, StorageError> {
        let bucket = self.bucket(kb);
        let source_key = key_of(source).to_string();
        let destination_key = key_of(destination).to_string();

        let _guard = self
            .inner
            .locks
            .pair(&bucket, &source_key, &destination_key)
            .await;
        self.blocking(move |inner| {
            let (source_path, _, source_attrs) = inner.resolve(&bucket, &source_key)?;

            if let Some(expected) = &options.source_if_match
                && !matches_if_match(&source_attrs.etag, expected)
            {
                return Err(StorageError::PreconditionFailed);
            }

            let destination_conditions = ConditionalHeaders {
                if_none_match: options.destination_if_none_match.clone(),
                ..ConditionalHeaders::default()
            };
            let current = inner.current(&bucket, &destination_key)?;
            evaluate_write_preconditions(
                current
                    .as_ref()
                    .map(|(metadata, attrs)| Inner::state(attrs, metadata)),
                &destination_conditions,
            )?;

            let content_type = options
                .content_type
                .clone()
                .or_else(|| source_attrs.content_type.clone());

            let outcome = inner.store(
                &bucket,
                &destination_key,
                content_type.as_deref(),
                |file, hasher| {
                    let mut reader = std::fs::File::open(&source_path)?;
                    let mut buffer = vec![0_u8; STREAM_CHUNK];
                    loop {
                        let read = reader.read(&mut buffer)?;
                        if read == 0 {
                            return Ok(());
                        }
                        hasher.update(&buffer[..read]);
                        file.write_all(&buffer[..read])?;
                    }
                },
            )?;
            Ok(outcome)
        })
        .await
    }

    async fn delete_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<(), StorageError> {
        let bucket = self.bucket(kb);
        let key = key_of(path).to_string();
        log_ignored_delete_conditions(&conditionals);

        let _guard = self.inner.locks.key(&bucket, &key).await;
        self.blocking(move |inner| {
            let Some((metadata, attrs)) = inner.current(&bucket, &key)? else {
                // Idempotent, as on S3.
                return Ok(());
            };

            if let Some(expected) = &conditionals.if_match
                && !matches_if_match(&attrs.etag, expected)
            {
                let _ = metadata;
                return Err(StorageError::PreconditionFailed);
            }

            let object_path = inner.layout.object_path(&bucket, &key)?;
            match std::fs::remove_file(&object_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(errors::object(&key, error)),
            }
            inner.meta.remove(&inner.layout, &bucket, &key)?;

            if let Some(parent) = object_path.parent() {
                commit::prune_empty_dirs(parent, &inner.layout.bucket_dir(&bucket));
            }
            Ok(())
        })
        .await
    }

    async fn list_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<ListResponse, StorageError> {
        let bucket = self.bucket(kb);
        let prefix = prefix.map(str::to_string);
        let cursor = cursor.map(str::to_string);

        self.blocking(move |inner| {
            let after = match &cursor {
                Some(token) => Some(decode_cursor(token, prefix.as_deref())?),
                None => None,
            };

            let bucket_dir = inner.layout.bucket_dir(&bucket);
            if !bucket_dir.is_dir() {
                // Matches S3, where a missing bucket is a blanket list failure.
                return Err(StorageError::BackendUnavailable {
                    message: format!("no storage directory for {bucket}"),
                });
            }

            let capped = limit.min(MAX_LIST_LIMIT);
            let mut walk = OrderedWalk::new(bucket_dir);
            let mut objects = Vec::new();
            let mut truncated = false;

            // One past the page, so `truncated` is observed rather than guessed.
            while let Some(found) = walk.next_match(after.as_deref(), prefix.as_deref()) {
                if u32::try_from(objects.len()).unwrap_or(u32::MAX) >= capped {
                    truncated = true;
                    break;
                }
                objects.push(ObjectMeta {
                    key: found.key,
                    size: found.size,
                    last_modified: found.last_modified,
                    // S3's ListObjectsV2 mapping drops both, and resolving them here would
                    // turn a listing into up to 1000 metadata reads and rehashes.
                    content_type: None,
                    etag: None,
                });
            }

            let next_cursor = if truncated {
                objects
                    .last()
                    .map(|object| encode_cursor(&object.key, prefix.as_deref()))
            } else {
                None
            };
            // Upholds the documented invariant even at limit = 0, where the page is empty
            // and there is no last key to resume from.
            let truncated = next_cursor.is_some();

            Ok(ListResponse {
                objects,
                truncated,
                next_cursor,
            })
        })
        .await
    }
}

/// S3 cannot express either date header on a write, so neither backend honours them.
fn log_ignored_write_dates(conditionals: &ConditionalHeaders) {
    if conditionals.if_modified_since.is_some() {
        tracing::debug!("if_modified_since ignored on PUT (no equivalent on the S3 API)");
    }
    if conditionals.if_unmodified_since.is_some() {
        tracing::debug!("if_unmodified_since ignored on PUT (no equivalent on the S3 API)");
    }
}

/// DELETE carries only `If-Match` on S3.
fn log_ignored_delete_conditions(conditionals: &ConditionalHeaders) {
    if conditionals.if_none_match.is_some() {
        tracing::debug!("if_none_match ignored on DELETE (no equivalent on the S3 API)");
    }
    if conditionals.if_modified_since.is_some() {
        tracing::debug!("if_modified_since ignored on DELETE (no equivalent on the S3 API)");
    }
    if conditionals.if_unmodified_since.is_some() {
        tracing::debug!("if_unmodified_since ignored on DELETE (no equivalent on the S3 API)");
    }
}

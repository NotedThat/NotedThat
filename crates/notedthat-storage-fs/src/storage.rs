//! [`Storage`] over a local directory tree.

use std::fs::{File, Metadata};
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

/// Times `Inner::open` looks again at a path swapped out from under it before giving up.
const OPEN_ATTEMPTS: usize = 3;

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

    /// Directory holding one knowledge base's objects.
    ///
    /// The one place outside this module that needs a real path: watching a knowledge base
    /// starts from its directory, and nothing else should be re-deriving that name.
    pub(crate) fn bucket_dir(&self, kb: &KbSlug) -> PathBuf {
        self.inner.layout.bucket_dir(&self.bucket(kb))
    }

    /// Every object under `prefix`, with the `ETag` a read would report, in one pass.
    ///
    /// Keys come back in the byte-lexicographic order `list_objects` uses, so a caller
    /// comparing this against an equally sorted list of indexed keys can merge the two in
    /// step rather than building a lookup table.
    ///
    /// Cheap by design: [`crate::meta`]'s freshness stamp means each object costs a stat
    /// and a sidecar read, and content is hashed only where the recorded stamp no longer
    /// describes the file. Confirming an unchanged knowledge base therefore reads none of
    /// its content.
    pub(crate) async fn walk_etags(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, String)>, StorageError> {
        let bucket = self.bucket(kb);
        let prefix = prefix.map(str::to_string);
        self.blocking(move |inner| {
            let mut walk = OrderedWalk::new(inner.layout.bucket_dir(&bucket));
            let mut found = Vec::new();
            while let Some(entry) = walk.next_match(None, prefix.as_deref()) {
                match inner.describe(&bucket, &entry.key) {
                    Ok((_, attrs)) => found.push((entry.key, attrs.etag)),
                    // Vanished between the walk and the stat, or unreadable. Skipping it
                    // means the pass reports nothing about that key, which leaves the index
                    // as it was — the safe direction, and the next event covers it.
                    Err(error) if error.is_not_found() => {}
                    Err(error) => {
                        tracing::warn!(
                            key = %entry.key,
                            %error,
                            "skipping an unreadable object while walking for reconciliation"
                        );
                    }
                }
            }
            Ok(found)
        })
        .await
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
    /// The knowledge base's directory, or [`StorageError::BucketNotFound`] when it is
    /// gone.
    ///
    /// Every operation but `ensure_bucket` starts here, so a directory removed after
    /// provisioning is a `404` on every surface (D43) rather than a `5xx` from `list`,
    /// a `NotFound` for the object from a read, or — worst — a write that quietly
    /// recreates the directory. One `stat`, next to the object's own that follows.
    ///
    /// Only *absent* is gone. A directory the process cannot stat — `EACCES` after a
    /// `chown` gone wrong, a mount back under the wrong user — is
    /// [`StorageError::BackendUnavailable`] (`errors::bucket`), so the operator is sent
    /// to the store and not to look for a deleted directory that is right there.
    ///
    /// Best-effort, not a lock: the `stat` and the write that follows are two
    /// filesystem operations, and a directory removed between them is recreated by
    /// `store` as before, in a window of microseconds. A write that *finds* the
    /// directory gone is refused; that is the operator scenario (`rm -rf` by hand)
    /// and enough for it.
    fn require_bucket(&self, bucket: &str) -> Result<PathBuf, StorageError> {
        let dir = self.layout.bucket_dir(bucket);
        match std::fs::metadata(&dir) {
            Ok(meta) if meta.is_dir() => Ok(dir),
            Ok(_) => Err(StorageError::BucketNotFound {
                bucket: bucket.to_string(),
            }),
            Err(error) => Err(errors::bucket(bucket, &error)),
        }
    }

    /// Stat an object, treating anything that is not a regular file as absent.
    ///
    /// A directory is not an object. `WebDAV` depends on this: it distinguishes a resource
    /// from a collection by a `head_object` that reports `NotFound`, then a prefix list.
    /// Nor is a symlink, so this is an `lstat`.
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

    /// Open an object, treating anything that is not a regular file as absent, by the
    /// same rules as [`Self::stat`].
    ///
    /// A symlink is not an object, and `open` follows one, so the path is `lstat`ed first and the
    /// handle's own `stat` must name the same file. When it does not, the path was
    /// swapped between the two — an editor's save, or a symlink put in its place — and
    /// the path is looked at again. The `lstat` also keeps a FIFO from blocking `open`.
    /// Without an inode to compare (non-unix) the handle only has to be a regular file.
    fn open(path: &Path, key: &str) -> Result<(File, Metadata), StorageError> {
        for _ in 0..OPEN_ATTEMPTS {
            let linked =
                std::fs::symlink_metadata(path).map_err(|error| errors::object(key, error))?;
            if !linked.is_file() {
                break;
            }
            let file = File::open(path).map_err(|error| errors::object(key, error))?;
            let metadata = file
                .metadata()
                .map_err(|error| errors::object(key, error))?;
            if metadata.is_file() && same_file(&linked, &metadata) {
                return Ok((file, metadata));
            }
        }
        Err(StorageError::NotFound {
            key: key.to_string(),
        })
    }

    /// An open object plus its resolved metadata, rehashing only when the file has moved
    /// on.
    ///
    /// The stamp, the `ETag` and any bytes the caller goes on to read all come from the
    /// one handle returned, so they describe one version of the object even when the
    /// path is renamed over in between: the handle stays on the inode it opened.
    fn resolve(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(File, Metadata, ObjectAttrs), StorageError> {
        let path = self.layout.object_path(bucket, key)?;
        let (file, metadata) = Self::open(&path, key)?;
        let attrs = self
            .meta
            .resolve(&self.layout, bucket, key, &metadata, || {
                hash_file(&file).map_err(|error| errors::object(key, error))
            })?;
        Ok((file, metadata, attrs))
    }

    /// An object's metadata, for callers that serve none of its bytes: `head_object`,
    /// write preconditions and `walk_etags`.
    ///
    /// Only a stale or missing record opens the file, through [`Self::resolve`], so the
    /// repaired stamp and `ETag` still come from one handle. A fresh record needs just
    /// the `lstat`, so an object the process may not read (`chmod 000`) can still be
    /// described, replaced and deleted, as a rename or unlink needs no read permission.
    /// With no bytes returned, #294's mislabel cannot happen here.
    fn describe(&self, bucket: &str, key: &str) -> Result<(Metadata, ObjectAttrs), StorageError> {
        let path = self.layout.object_path(bucket, key)?;
        let metadata = Self::stat(&path, key)?;
        if let Some(attrs) = self.meta.fresh(&self.layout, bucket, key, &metadata) {
            return Ok((metadata, attrs));
        }
        let (_, metadata, attrs) = self.resolve(bucket, key)?;
        Ok((metadata, attrs))
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
        match self.describe(bucket, key) {
            Ok(found) => Ok(Some(found)),
            // Only the object's own absence: a missing bucket must never read as
            // "nothing there, go ahead and write".
            Err(StorageError::NotFound { .. }) => Ok(None),
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
    ) -> Result<String, StorageError> {
        let path = self.layout.object_path(bucket, key)?;
        let parent = path
            .parent()
            .ok_or_else(|| crate::layout::unsupported_key(format!("key '{key}' has no parent")))?;

        let mut staged = commit::stage_in(parent, self.layout.dir_mode())
            .map_err(|error| errors::object(key, error))?;
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

        Ok(etag)
    }
}

/// Whether two stats describe the same file.
#[cfg(unix)]
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_a: &Metadata, _b: &Metadata) -> bool {
    true
}

/// Hash an open file's whole content without holding it in memory.
fn hash_file(mut file: &File) -> std::io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
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

/// Read `len` bytes of an open file starting at `start`.
fn read_slice(mut file: &File, start: u64, len: u64) -> std::io::Result<Bytes> {
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

    async fn probe(&self, kb: &KbSlug) -> Result<(), StorageError> {
        let bucket = self.bucket(kb);
        // The same lookup every operation starts with, so the readiness answer and
        // the request-path answer for one directory can never disagree.
        self.blocking(move |inner| inner.require_bucket(&bucket).map(drop))
            .await
    }

    async fn read_manifest(&self, kb: &KbSlug) -> Result<KbManifest, StorageError> {
        let bucket = self.bucket(kb);
        self.blocking(move |inner| {
            inner.require_bucket(&bucket)?;
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
            inner.require_bucket(&bucket)?;
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
            inner.require_bucket(&bucket)?;
            let (metadata, attrs) = inner.describe(&bucket, &key)?;
            evaluate_read_preconditions(Inner::state(&attrs, &metadata), &conditionals)?;
            Ok(Inner::object_meta(&key, metadata.len(), &metadata, &attrs))
        })
        .await
    }

    async fn get_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<ByteRange>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        let bucket = self.bucket(kb);
        let key = key_of(path).to_string();
        self.blocking(move |inner| {
            inner.require_bucket(&bucket)?;
            let (file, metadata, attrs) = inner.resolve(&bucket, &key)?;
            evaluate_read_preconditions(Inner::state(&attrs, &metadata), &conditionals)?;

            let total = metadata.len();
            let (start, length, content_range) = match resolve_range(total, range.as_ref())? {
                Some((exclusive, content_range)) => (
                    exclusive.start,
                    exclusive.end - exclusive.start,
                    Some(content_range),
                ),
                None => (0, total, None),
            };
            // From the handle `resolve` stamped, so the bytes are the ones the `ETag`
            // describes.
            let bytes =
                read_slice(&file, start, length).map_err(|error| errors::object(&key, error))?;

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
        range: Option<ByteRange>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectStream, StorageError> {
        let bucket = self.bucket(kb);
        let key = key_of(path).to_string();

        // No lock: stream from the handle `resolve` opened and stamped. A rename over the
        // path after that leaves this reader on the inode whose `ETag` we report, so it
        // never serves one version's bytes under another's label, nor splices the two. An
        // in-place rewrite of that same inode is not guarded against.
        let (file, meta, content_range) = self
            .blocking(move |inner| {
                inner.require_bucket(&bucket)?;
                let (mut file, metadata, attrs) = inner.resolve(&bucket, &key)?;
                evaluate_read_preconditions(Inner::state(&attrs, &metadata), &conditionals)?;

                let total = metadata.len();
                let (start, length, content_range) = match resolve_range(total, range.as_ref())? {
                    Some((exclusive, content_range)) => (
                        exclusive.start,
                        exclusive.end - exclusive.start,
                        Some(content_range),
                    ),
                    None => (0, total, None),
                };
                // Always seek: a rehash in `resolve` leaves the cursor at the end.
                file.seek(SeekFrom::Start(start))
                    .map_err(|error| errors::object(&key, error))?;

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
            inner.require_bucket(&bucket)?;
            let current = inner.current(&bucket, &key)?;
            evaluate_write_preconditions(
                current
                    .as_ref()
                    .map(|(metadata, attrs)| Inner::state(attrs, metadata)),
                &conditionals,
            )?;
            let etag = inner.store(&bucket, &key, content_type.as_deref(), |file, hasher| {
                hasher.update(&bytes);
                file.write_all(&bytes)
            })?;
            Ok(PutOutcome {
                etag: Some(etag),
                created: current.is_none(),
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
            inner.require_bucket(&bucket)?;
            let current = inner.current(&bucket, &key)?;
            evaluate_write_preconditions(
                current
                    .as_ref()
                    .map(|(metadata, attrs)| Inner::state(attrs, metadata)),
                &conditionals,
            )?;

            let etag = inner.store(&bucket, &key, content_type.as_deref(), |file, hasher| {
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
            })?;
            Ok(PutOutcome {
                etag: Some(etag),
                created: current.is_none(),
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
            inner.require_bucket(&bucket)?;
            let (mut source, _, source_attrs) = inner.resolve(&bucket, &source_key)?;

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

            let etag = inner.store(
                &bucket,
                &destination_key,
                content_type.as_deref(),
                |file, hasher| {
                    // The handle `source_if_match` was checked against, not a fresh open
                    // that an out-of-band save could have swapped underneath.
                    source.seek(SeekFrom::Start(0))?;
                    let mut buffer = vec![0_u8; STREAM_CHUNK];
                    loop {
                        let read = source.read(&mut buffer)?;
                        if read == 0 {
                            return Ok(());
                        }
                        hasher.update(&buffer[..read]);
                        file.write_all(&buffer[..read])?;
                    }
                },
            )?;
            Ok(PutOutcome {
                etag: Some(etag),
                created: current.is_none(),
            })
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
            inner.require_bucket(&bucket)?;
            let Some((_, attrs)) = inner.current(&bucket, &key)? else {
                // Idempotent, as on S3.
                return Ok(());
            };

            if let Some(expected) = &conditionals.if_match
                && !matches_if_match(&attrs.etag, expected)
            {
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

            let bucket_dir = inner.require_bucket(&bucket)?;

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
                    // S3's `ListObjectsV2` carries no content type; it does carry the
                    // `ETag`, but resolving one here would turn a listing into up to 1000
                    // sidecar reads and rehashes, so this backend leaves it absent (a
                    // pinned divergence; `walk_etags` is the reconciliation walk's source).
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

#[cfg(test)]
mod tests {
    use super::{FsStorage, hash_file, read_slice};
    use crate::config::FsConfig;
    use bytes::Bytes;
    use notedthat_core::{
        ConditionalHeaders, KbSlug, ObjectPath, Storage, StorageError, TenantSlug, compute_etag,
    };

    struct Env {
        _dir: tempfile::TempDir,
        storage: FsStorage,
        kb: KbSlug,
    }

    async fn env() -> Env {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = FsConfig::new(dir.path().to_path_buf());
        let storage = FsStorage::new(&config, dir.path().to_path_buf(), TenantSlug::default());
        let kb = KbSlug::try_new("notes").expect("slug");
        storage.ensure_bucket(&kb).await.expect("bucket");
        Env {
            _dir: dir,
            storage,
            kb,
        }
    }

    fn path(key: &str) -> ObjectPath {
        ObjectPath::try_from_str(key).expect("valid key")
    }

    async fn put(env: &Env, key: &str, body: &'static str) {
        env.storage
            .put_object(
                &env.kb,
                &path(key),
                Bytes::from_static(body.as_bytes()),
                None,
                ConditionalHeaders::default(),
            )
            .await
            .expect("put");
    }

    /// What an editor's save does: write a sibling, rename it over the object.
    fn rename_over(target: &std::path::Path, body: &[u8]) {
        let staged = target.with_extension("swap");
        std::fs::write(&staged, body).expect("stage");
        std::fs::rename(&staged, target).expect("rename");
    }

    /// The handle `resolve` returns is the version its `ETag` describes, whatever lands
    /// on the path afterwards — the race a read takes no lock against.
    #[tokio::test]
    async fn a_resolved_handle_keeps_the_version_it_labels() {
        let env = env().await;
        put(&env, "a.md", "first").await;
        let bucket = env.storage.bucket(&env.kb);
        let inner = &env.storage.inner;
        let object = inner.layout.object_path(&bucket, "a.md").expect("path");

        let (file, metadata, attrs) = inner.resolve(&bucket, "a.md").expect("resolve");
        rename_over(&object, b"second, and longer");

        assert_eq!(attrs.etag, compute_etag(b"first"));
        let bytes = read_slice(&file, 0, metadata.len()).expect("read");
        assert_eq!(&bytes[..], b"first");
    }

    /// A rehash reads the handle it was given, not whatever the path names by then.
    #[test]
    fn a_rehash_hashes_the_open_handle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let object = dir.path().join("a.md");
        std::fs::write(&object, b"first").expect("write");

        let file = std::fs::File::open(&object).expect("open");
        rename_over(&object, b"second");

        assert_eq!(hash_file(&file).expect("hash"), compute_etag(b"first"));
        // And again from the end, where a first pass leaves the cursor.
        assert_eq!(hash_file(&file).expect("hash"), compute_etag(b"first"));
    }

    /// A symlink is not an object on any read path, even though `open` would follow it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_is_not_an_object_to_read() {
        let env = env().await;
        put(&env, "real.md", "content").await;
        let bucket = env.storage.bucket(&env.kb);
        let layout = &env.storage.inner.layout;
        std::os::unix::fs::symlink(
            layout.object_path(&bucket, "real.md").expect("path"),
            layout.object_path(&bucket, "link.md").expect("path"),
        )
        .expect("symlink");

        let link = path("link.md");
        let head = env
            .storage
            .head_object(&env.kb, &link, ConditionalHeaders::default())
            .await;
        assert!(
            matches!(head, Err(StorageError::NotFound { .. })),
            "{head:?}"
        );
        let get = env
            .storage
            .get_object(&env.kb, &link, None, ConditionalHeaders::default())
            .await;
        assert!(
            matches!(get, Err(StorageError::NotFound { .. })),
            "get should be NotFound"
        );
        let stream = env
            .storage
            .get_object_stream(&env.kb, &link, None, ConditionalHeaders::default())
            .await;
        assert!(
            matches!(stream, Err(StorageError::NotFound { .. })),
            "stream should be NotFound"
        );
    }

    /// Only the paths that serve bytes need to read the object. With a fresh record,
    /// `HEAD`, the `ETag` walk, an unconditional `PUT` and a `DELETE` need no more than
    /// the `lstat`, as before #294, so a `chmod 000` object can still be replaced and
    /// removed. A `GET` still needs to read it, and says the store is unavailable.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_object_with_a_fresh_record_can_be_replaced_and_deleted() {
        use std::os::unix::fs::PermissionsExt;

        let env = env().await;
        put(&env, "a.md", "first").await;
        put(&env, "b.md", "other").await;
        let bucket = env.storage.bucket(&env.kb);
        let layout = &env.storage.inner.layout;
        let closed = std::fs::Permissions::from_mode(0o000);
        for key in ["a.md", "b.md"] {
            let object = layout.object_path(&bucket, key).expect("path");
            std::fs::set_permissions(&object, closed.clone()).expect("chmod 000");
        }
        if std::fs::File::open(layout.object_path(&bucket, "a.md").expect("path")).is_ok() {
            eprintln!("skipped: this process ignores permission bits (root)");
            return;
        }

        let head = env
            .storage
            .head_object(&env.kb, &path("a.md"), ConditionalHeaders::default())
            .await
            .expect("head needs no read");
        assert_eq!(head.etag, Some(compute_etag(b"first")));
        let walked = env.storage.walk_etags(&env.kb, None).await.expect("walk");
        assert!(
            walked.contains(&("a.md".to_string(), compute_etag(b"first"))),
            "{walked:?}"
        );
        let get = env
            .storage
            .get_object(&env.kb, &path("a.md"), None, ConditionalHeaders::default())
            .await;
        assert!(
            matches!(get, Err(StorageError::BackendUnavailable { .. })),
            "get must read the object"
        );

        let outcome = env
            .storage
            .put_object(
                &env.kb,
                &path("a.md"),
                Bytes::from_static(b"second"),
                None,
                ConditionalHeaders::default(),
            )
            .await
            .expect("an unconditional put renames over the object");
        assert!(!outcome.created);
        assert_eq!(outcome.etag, Some(compute_etag(b"second")));

        env.storage
            .delete_object(&env.kb, &path("b.md"), ConditionalHeaders::default())
            .await
            .expect("delete unlinks the object");
        assert!(!layout.object_path(&bucket, "b.md").expect("path").exists());
    }
}

//! [`ScriptedStorage`]: an [`InMemoryStorage`] that records every call and can be
//! told to fail, or to change the store underneath the caller, at a chosen operation.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use tokio::io::AsyncReadExt;

use super::InMemoryStorage;
use crate::{
    ByteRange, ConditionalHeaders, CopyObjectOptions, KbManifest, KbSlug, ListResponse, ObjectMeta,
    ObjectPath, ObjectRead, ObjectStream, PutOutcome, StagedBody, Storage, StorageError,
};

/// One [`Storage`] method, for scripting and counting calls to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs)] // one variant per trait method, named after it
pub enum StorageOp {
    EnsureBucket,
    Probe,
    ReadManifest,
    WriteManifest,
    HeadObject,
    GetObject,
    GetObjectStream,
    PutObject,
    PutStagedObject,
    CopyObject,
    DeleteObject,
    ListObjects,
}

/// A call [`ScriptedStorage`] received, with the arguments tests assert on.
///
/// Recorded before any scripted failure, so a call that was made to fail still shows.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(missing_docs)] // fields mirror the trait method's parameters
pub enum StorageCall {
    EnsureBucket {
        kb: String,
    },
    Probe {
        kb: String,
    },
    ReadManifest {
        kb: String,
    },
    WriteManifest {
        kb: String,
    },
    HeadObject {
        kb: String,
        path: String,
        conditionals: ConditionalHeaders,
    },
    GetObject {
        kb: String,
        path: String,
        range: Option<ByteRange>,
        conditionals: ConditionalHeaders,
    },
    GetObjectStream {
        kb: String,
        path: String,
        range: Option<ByteRange>,
        conditionals: ConditionalHeaders,
    },
    PutObject {
        kb: String,
        path: String,
        len: u64,
        content_type: Option<String>,
        conditionals: ConditionalHeaders,
    },
    PutStagedObject {
        kb: String,
        path: String,
        /// The spill file, for a body staged to disk; `None` for one held in memory.
        staged_file: Option<PathBuf>,
        len: u64,
        content_type: Option<String>,
        conditionals: ConditionalHeaders,
    },
    CopyObject {
        kb: String,
        source: String,
        destination: String,
        options: CopyObjectOptions,
    },
    DeleteObject {
        kb: String,
        path: String,
        conditionals: ConditionalHeaders,
    },
    ListObjects {
        kb: String,
        prefix: Option<String>,
        limit: u32,
        cursor: Option<String>,
    },
}

impl StorageCall {
    /// The method this call was to.
    #[must_use]
    pub const fn op(&self) -> StorageOp {
        match self {
            Self::EnsureBucket { .. } => StorageOp::EnsureBucket,
            Self::Probe { .. } => StorageOp::Probe,
            Self::ReadManifest { .. } => StorageOp::ReadManifest,
            Self::WriteManifest { .. } => StorageOp::WriteManifest,
            Self::HeadObject { .. } => StorageOp::HeadObject,
            Self::GetObject { .. } => StorageOp::GetObject,
            Self::GetObjectStream { .. } => StorageOp::GetObjectStream,
            Self::PutObject { .. } => StorageOp::PutObject,
            Self::PutStagedObject { .. } => StorageOp::PutStagedObject,
            Self::CopyObject { .. } => StorageOp::CopyObject,
            Self::DeleteObject { .. } => StorageOp::DeleteObject,
            Self::ListObjects { .. } => StorageOp::ListObjects,
        }
    }
}

type ErrorFactory = Arc<dyn Fn() -> StorageError + Send + Sync>;
type Hook = Arc<dyn Fn(InMemoryStorage) -> BoxFuture<'static, ()> + Send + Sync>;

struct Failure {
    /// `None` fails every call; `Some(n)` fails the next `n`.
    remaining: Option<u32>,
    error: ErrorFactory,
}

#[derive(Default)]
struct Script {
    failures: HashMap<StorageOp, Failure>,
    before: HashMap<StorageOp, Vec<Hook>>,
    after: HashMap<StorageOp, Vec<Hook>>,
    discard_staged_bodies: bool,
    max_page: Option<u32>,
}

/// An [`InMemoryStorage`] with a script: every call is recorded, and any operation can
/// be made to fail or to run a hook against the store before or after it.
///
/// Per call, in order: the call is recorded, `before` hooks run, a scripted failure
/// (if any is due) is returned, the call is delegated to the inner store, and on
/// success `after` hooks run. Clones share the store, the script and the record.
#[derive(Clone, Default)]
pub struct ScriptedStorage {
    inner: InMemoryStorage,
    script: Arc<Mutex<Script>>,
    calls: Arc<Mutex<Vec<StorageCall>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .expect("scripted storage state is not poisoned")
}

impl ScriptedStorage {
    /// A store with these knowledge bases provisioned, as [`InMemoryStorage::with_kbs`].
    #[must_use]
    pub fn with_kbs<'a>(kbs: impl IntoIterator<Item = &'a KbSlug>) -> Self {
        Self {
            inner: InMemoryStorage::with_kbs(kbs),
            script: Arc::default(),
            calls: Arc::default(),
        }
    }

    /// The store underneath, for seeding and inspecting without recording a call.
    #[must_use]
    pub const fn inner(&self) -> &InMemoryStorage {
        &self.inner
    }

    /// Fail the next `times` calls to `op` with the error `error` builds.
    pub fn fail_next(
        &self,
        op: StorageOp,
        times: u32,
        error: impl Fn() -> StorageError + Send + Sync + 'static,
    ) {
        lock(&self.script).failures.insert(
            op,
            Failure {
                remaining: Some(times),
                error: Arc::new(error),
            },
        );
    }

    /// Fail every call to `op` with the error `error` builds.
    pub fn fail_always(
        &self,
        op: StorageOp,
        error: impl Fn() -> StorageError + Send + Sync + 'static,
    ) {
        lock(&self.script).failures.insert(
            op,
            Failure {
                remaining: None,
                error: Arc::new(error),
            },
        );
    }

    /// Run `hook` against the store at the start of every call to `op`, before any
    /// scripted failure — a concurrent writer landing just ahead of the caller.
    pub fn before<F, Fut>(&self, op: StorageOp, hook: F)
    where
        F: Fn(InMemoryStorage) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let hook: Hook = Arc::new(move |store| Box::pin(hook(store)));
        lock(&self.script).before.entry(op).or_default().push(hook);
    }

    /// Run `hook` against the store after every successful call to `op` — a
    /// concurrent writer landing just behind the caller.
    pub fn after<F, Fut>(&self, op: StorageOp, hook: F)
    where
        F: Fn(InMemoryStorage) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let hook: Hook = Arc::new(move |store| Box::pin(hook(store)));
        lock(&self.script).after.entry(op).or_default().push(hook);
    }

    /// Drain a disk-staged body and store it empty instead of reading it into memory,
    /// so a multi-gigabyte upload test does not need the memory to hold it.
    pub fn discard_staged_bodies(&self) {
        lock(&self.script).discard_staged_bodies = true;
    }

    /// Cap every listing page at `max` objects, below whatever limit the caller asks for.
    pub fn max_page(&self, max: u32) {
        lock(&self.script).max_page = Some(max);
    }

    /// Every call so far, oldest first.
    #[must_use]
    pub fn calls(&self) -> Vec<StorageCall> {
        lock(&self.calls).clone()
    }

    /// The operation of every call so far, oldest first.
    #[must_use]
    pub fn ops(&self) -> Vec<StorageOp> {
        lock(&self.calls).iter().map(StorageCall::op).collect()
    }

    /// How many calls to `op` there have been.
    #[must_use]
    pub fn count(&self, op: StorageOp) -> usize {
        lock(&self.calls)
            .iter()
            .filter(|call| call.op() == op)
            .count()
    }

    /// Record `call`, run its `before` hooks and return a scripted failure if one is due.
    async fn enter(&self, call: StorageCall) -> Result<(), StorageError> {
        let op = call.op();
        lock(&self.calls).push(call);
        let hooks = lock(&self.script)
            .before
            .get(&op)
            .cloned()
            .unwrap_or_default();
        for hook in hooks {
            hook(self.inner.clone()).await;
        }
        let mut script = lock(&self.script);
        let Some(failure) = script.failures.get_mut(&op) else {
            return Ok(());
        };
        match &mut failure.remaining {
            None => Err((failure.error)()),
            Some(0) => Ok(()),
            Some(remaining) => {
                *remaining -= 1;
                Err((failure.error)())
            }
        }
    }

    /// Run `op`'s `after` hooks if `result` succeeded, then hand it back.
    async fn leave<T>(
        &self,
        op: StorageOp,
        result: Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        if result.is_ok() {
            let hooks = lock(&self.script)
                .after
                .get(&op)
                .cloned()
                .unwrap_or_default();
            for hook in hooks {
                hook(self.inner.clone()).await;
            }
        }
        result
    }
}

fn io_error(source: std::io::Error) -> StorageError {
    StorageError::Other {
        source: Box::new(source),
    }
}

/// Read a disk-staged body to its end without keeping it, checking its length.
async fn drain(body: &StagedBody) -> Result<(), StorageError> {
    let mut reader = body.open().await.map_err(io_error)?;
    let mut buffer = vec![0; 64 * 1024];
    let mut read = 0_u64;
    loop {
        let n = reader.read(&mut buffer).await.map_err(io_error)?;
        if n == 0 {
            break;
        }
        read += n as u64;
    }
    assert_eq!(read, body.len(), "a staged body reads back at its length");
    Ok(())
}

fn owned(value: Option<&str>) -> Option<String> {
    value.map(str::to_string)
}

#[async_trait]
impl Storage for ScriptedStorage {
    async fn ensure_bucket(&self, kb: &KbSlug) -> Result<(), StorageError> {
        let kb_name = kb.as_str().to_string();
        self.enter(StorageCall::EnsureBucket { kb: kb_name })
            .await?;
        let result = self.inner.ensure_bucket(kb).await;
        self.leave(StorageOp::EnsureBucket, result).await
    }

    async fn probe(&self, kb: &KbSlug) -> Result<(), StorageError> {
        let kb_name = kb.as_str().to_string();
        self.enter(StorageCall::Probe { kb: kb_name }).await?;
        let result = self.inner.probe(kb).await;
        self.leave(StorageOp::Probe, result).await
    }

    async fn read_manifest(&self, kb: &KbSlug) -> Result<KbManifest, StorageError> {
        let kb_name = kb.as_str().to_string();
        self.enter(StorageCall::ReadManifest { kb: kb_name })
            .await?;
        let result = self.inner.read_manifest(kb).await;
        self.leave(StorageOp::ReadManifest, result).await
    }

    async fn write_manifest(&self, kb: &KbSlug, manifest: &KbManifest) -> Result<(), StorageError> {
        let kb_name = kb.as_str().to_string();
        self.enter(StorageCall::WriteManifest { kb: kb_name })
            .await?;
        let result = self.inner.write_manifest(kb, manifest).await;
        self.leave(StorageOp::WriteManifest, result).await
    }

    async fn head_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectMeta, StorageError> {
        self.enter(StorageCall::HeadObject {
            kb: kb.as_str().to_string(),
            path: path.as_str().to_string(),
            conditionals: conditionals.clone(),
        })
        .await?;
        let result = self.inner.head_object(kb, path, conditionals).await;
        self.leave(StorageOp::HeadObject, result).await
    }

    async fn get_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<ByteRange>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        self.enter(StorageCall::GetObject {
            kb: kb.as_str().to_string(),
            path: path.as_str().to_string(),
            range: range.clone(),
            conditionals: conditionals.clone(),
        })
        .await?;
        let result = self.inner.get_object(kb, path, range, conditionals).await;
        self.leave(StorageOp::GetObject, result).await
    }

    async fn get_object_stream(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<ByteRange>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectStream, StorageError> {
        self.enter(StorageCall::GetObjectStream {
            kb: kb.as_str().to_string(),
            path: path.as_str().to_string(),
            range: range.clone(),
            conditionals: conditionals.clone(),
        })
        .await?;
        let result = self
            .inner
            .get_object_stream(kb, path, range, conditionals)
            .await;
        self.leave(StorageOp::GetObjectStream, result).await
    }

    async fn put_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        bytes: Bytes,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        self.enter(StorageCall::PutObject {
            kb: kb.as_str().to_string(),
            path: path.as_str().to_string(),
            len: bytes.len() as u64,
            content_type: owned(content_type),
            conditionals: conditionals.clone(),
        })
        .await?;
        let result = self
            .inner
            .put_object(kb, path, bytes, content_type, conditionals)
            .await;
        self.leave(StorageOp::PutObject, result).await
    }

    async fn put_staged_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        body: StagedBody,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        self.enter(StorageCall::PutStagedObject {
            kb: kb.as_str().to_string(),
            path: path.as_str().to_string(),
            staged_file: body.file_path().map(std::path::Path::to_path_buf),
            len: body.len(),
            content_type: owned(content_type),
            conditionals: conditionals.clone(),
        })
        .await?;
        let discard = body.is_file() && lock(&self.script).discard_staged_bodies;
        let result = if discard {
            match drain(&body).await {
                Ok(()) => {
                    self.inner
                        .put_object(kb, path, Bytes::new(), content_type, conditionals)
                        .await
                }
                Err(error) => Err(error),
            }
        } else {
            self.inner
                .put_staged_object(kb, path, body, content_type, conditionals)
                .await
        };
        self.leave(StorageOp::PutStagedObject, result).await
    }

    async fn copy_object(
        &self,
        kb: &KbSlug,
        source: &ObjectPath,
        destination: &ObjectPath,
        options: CopyObjectOptions,
    ) -> Result<PutOutcome, StorageError> {
        self.enter(StorageCall::CopyObject {
            kb: kb.as_str().to_string(),
            source: source.as_str().to_string(),
            destination: destination.as_str().to_string(),
            options: options.clone(),
        })
        .await?;
        let result = self
            .inner
            .copy_object(kb, source, destination, options)
            .await;
        self.leave(StorageOp::CopyObject, result).await
    }

    async fn delete_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<(), StorageError> {
        self.enter(StorageCall::DeleteObject {
            kb: kb.as_str().to_string(),
            path: path.as_str().to_string(),
            conditionals: conditionals.clone(),
        })
        .await?;
        let result = self.inner.delete_object(kb, path, conditionals).await;
        self.leave(StorageOp::DeleteObject, result).await
    }

    async fn list_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<ListResponse, StorageError> {
        self.enter(StorageCall::ListObjects {
            kb: kb.as_str().to_string(),
            prefix: owned(prefix),
            limit,
            cursor: owned(cursor),
        })
        .await?;
        let page = lock(&self.script)
            .max_page
            .map_or(limit, |max| max.min(limit));
        let result = self.inner.list_objects(kb, prefix, page, cursor).await;
        self.leave(StorageOp::ListObjects, result).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn kb() -> KbSlug {
        KbSlug::try_new("test-kb").unwrap()
    }

    fn path(s: &str) -> ObjectPath {
        ObjectPath::try_from_str(s).unwrap()
    }

    async fn seeded() -> ScriptedStorage {
        let storage = ScriptedStorage::with_kbs([&kb()]);
        storage
            .inner()
            .seed("test-kb", "a.md", "a", Some("text/markdown"), Some("\"a\""))
            .await;
        storage
    }

    async fn head(storage: &ScriptedStorage, key: &str) -> Result<ObjectMeta, StorageError> {
        storage
            .head_object(&kb(), &path(key), ConditionalHeaders::default())
            .await
    }

    #[tokio::test]
    async fn delegates_to_the_inner_store() {
        let storage = seeded().await;
        let meta = head(&storage, "a.md").await.unwrap();
        assert_eq!(meta.etag.as_deref(), Some("\"a\""));
        assert!(matches!(
            head(&storage, "missing.md").await,
            Err(StorageError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn fail_next_fails_that_many_calls_then_recovers() {
        let storage = seeded().await;
        storage.fail_next(StorageOp::HeadObject, 2, || {
            StorageError::PreconditionFailed
        });
        for _ in 0..2 {
            assert!(matches!(
                head(&storage, "a.md").await,
                Err(StorageError::PreconditionFailed)
            ));
        }
        assert!(head(&storage, "a.md").await.is_ok());
        assert_eq!(
            storage.count(StorageOp::HeadObject),
            3,
            "failed calls count"
        );
    }

    #[tokio::test]
    async fn fail_always_never_recovers_and_leaves_other_ops_alone() {
        let storage = seeded().await;
        storage.fail_always(StorageOp::DeleteObject, || {
            StorageError::BackendUnavailable {
                message: "down".into(),
            }
        });
        for _ in 0..3 {
            assert!(
                storage
                    .delete_object(&kb(), &path("a.md"), ConditionalHeaders::default())
                    .await
                    .is_err()
            );
        }
        assert!(head(&storage, "a.md").await.is_ok());
    }

    #[tokio::test]
    async fn before_hook_runs_ahead_of_the_call_and_of_a_scripted_failure() {
        let storage = seeded().await;
        storage.before(StorageOp::PutObject, |store| async move {
            store
                .seed("test-kb", "a.md", "raced", None, Some("\"raced\""))
                .await;
        });
        storage.fail_next(StorageOp::PutObject, 1, || StorageError::PreconditionFailed);
        let (kb, key) = (kb(), path("a.md"));
        let put = |if_match: &str| {
            storage.put_object(
                &kb,
                &key,
                Bytes::from_static(b"mine"),
                None,
                ConditionalHeaders {
                    if_match: Some(if_match.to_string()),
                    ..ConditionalHeaders::default()
                },
            )
        };
        assert!(put("\"a\"").await.is_err(), "the scripted failure");
        let stored = storage.inner().object("test-kb", "a.md").await.unwrap();
        assert_eq!(
            stored.bytes, "raced",
            "the hook ran although the call failed"
        );
        assert!(
            matches!(put("\"a\"").await, Err(StorageError::PreconditionFailed)),
            "the hook moved the ETag under the caller"
        );
    }

    #[tokio::test]
    async fn after_hook_runs_only_on_success() {
        let storage = seeded().await;
        let runs = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&runs);
        storage.after(StorageOp::HeadObject, move |_| {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });
        head(&storage, "a.md").await.unwrap();
        head(&storage, "missing.md").await.unwrap_err();
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn records_arguments_in_call_order() {
        let storage = seeded().await;
        let options = CopyObjectOptions {
            destination_if_none_match: Some("*".into()),
            ..CopyObjectOptions::default()
        };
        storage
            .copy_object(&kb(), &path("a.md"), &path("b.md"), options.clone())
            .await
            .unwrap();
        storage
            .list_objects(&kb(), Some("a"), 10, None)
            .await
            .unwrap();
        assert_eq!(
            storage.calls(),
            vec![
                StorageCall::CopyObject {
                    kb: "test-kb".into(),
                    source: "a.md".into(),
                    destination: "b.md".into(),
                    options,
                },
                StorageCall::ListObjects {
                    kb: "test-kb".into(),
                    prefix: Some("a".into()),
                    limit: 10,
                    cursor: None,
                },
            ]
        );
        assert_eq!(
            storage.ops(),
            vec![StorageOp::CopyObject, StorageOp::ListObjects]
        );
    }

    #[tokio::test]
    async fn clones_share_the_store_the_script_and_the_record() {
        let storage = seeded().await;
        let clone = storage.clone();
        clone.fail_next(StorageOp::HeadObject, 1, || {
            StorageError::PreconditionFailed
        });
        assert!(head(&storage, "a.md").await.is_err());
        assert_eq!(clone.ops(), vec![StorageOp::HeadObject]);
    }

    #[tokio::test]
    async fn max_page_caps_the_listing_below_the_requested_limit() {
        let storage = seeded().await;
        storage
            .inner()
            .seed("test-kb", "b.md", "b", None, None)
            .await;
        storage.max_page(1);
        let page = storage.list_objects(&kb(), None, 1000, None).await.unwrap();
        assert_eq!(page.objects.len(), 1);
        assert!(page.truncated);
        assert_eq!(
            storage.calls(),
            vec![StorageCall::ListObjects {
                kb: "test-kb".into(),
                prefix: None,
                limit: 1000,
                cursor: None,
            }],
            "the caller's limit is what is recorded"
        );
    }

    #[tokio::test]
    async fn discarded_staged_body_is_stored_empty_at_its_recorded_length() {
        let storage = ScriptedStorage::with_kbs([&kb()]);
        storage.discard_staged_bodies();
        let body = StagedBody::stage_stream_to_file(
            futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"on disk"))]),
            Some(7),
            64,
            &crate::StagingConfig::default(),
        )
        .await
        .unwrap();
        storage
            .put_staged_object(
                &kb(),
                &path("big.bin"),
                body,
                None,
                ConditionalHeaders::default(),
            )
            .await
            .unwrap();
        let calls = storage.calls();
        let [
            StorageCall::PutStagedObject {
                staged_file, len, ..
            },
        ] = calls.as_slice()
        else {
            panic!("one staged put recorded: {calls:?}");
        };
        assert!(staged_file.is_some());
        assert_eq!(*len, 7);
        let stored = storage.inner().object("test-kb", "big.bin").await.unwrap();
        assert!(stored.bytes.is_empty());
    }
}

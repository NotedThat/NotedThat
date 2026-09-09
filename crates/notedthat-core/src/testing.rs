//! In-memory [`Storage`] implementation and related test helpers.
//!
//! Gated behind `#[cfg(any(test, feature = "test-support"))]` — consumers wire this via
//! `notedthat-core = { workspace = true, features = ["test-support"] }` in
//! `[dev-dependencies]`. **Never enable `test-support` in production builds.**

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::io::AsyncReadExt;
use tokio::sync::RwLock;

use crate::preconditions::{
    ObjectState, evaluate_read_preconditions, evaluate_write_preconditions, matches_if_match,
    resolve_range, unix_seconds_i64,
};

use crate::{
    ByteRange, ConditionalHeaders, CopyObjectOptions, KbManifest, KbSlug, ListResponse, ObjectMeta,
    ObjectPath, ObjectRead, ObjectStream, PutOutcome, StagedBody, Storage, StorageError,
};

#[derive(Clone)]
struct StoredObject {
    bytes: Bytes,
    content_type: Option<String>,
    etag: String,
    last_modified: SystemTime,
}

/// In-memory storage implementation for use in integration tests.
///
/// Mirrors the semantics of `notedthat_storage_s3::S3Storage`:
/// - `ensure_bucket` is idempotent
/// - `delete_object` is idempotent (returns `Ok` if the object does not exist)
/// - `list_objects` returns a hard-capped subset, sorted lexicographically by key
#[derive(Default, Clone)]
pub struct InMemoryStorage {
    inner: Arc<RwLock<InMemoryInner>>,
}

#[derive(Default)]
struct InMemoryInner {
    /// (`kb_slug`, `object_key`) → stored object
    objects: HashMap<(String, String), StoredObject>,
    manifests: HashMap<String, KbManifest>,
    buckets: HashSet<String>,
}

pub use crate::etag::compute_etag;

fn to_slice_index(value: u64) -> Result<usize, StorageError> {
    usize::try_from(value).map_err(|e| StorageError::Other {
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("range index {value} does not fit usize: {e}"),
        )),
    })
}

fn object_state(stored: &StoredObject) -> ObjectState<'_> {
    ObjectState {
        etag: &stored.etag,
        last_modified: stored.last_modified,
    }
}

fn object_meta(path: &ObjectPath, stored: &StoredObject, size: u64) -> ObjectMeta {
    ObjectMeta {
        key: path.as_str().to_string(),
        size,
        last_modified: Some(unix_seconds_i64(stored.last_modified)),
        content_type: stored.content_type.clone(),
        etag: Some(stored.etag.clone()),
    }
}

#[async_trait]
impl Storage for InMemoryStorage {
    async fn ensure_bucket(&self, kb: &KbSlug) -> Result<(), StorageError> {
        let mut inner = self.inner.write().await;
        inner.buckets.insert(kb.as_str().to_string());
        Ok(())
    }

    async fn read_manifest(&self, kb: &KbSlug) -> Result<KbManifest, StorageError> {
        let inner = self.inner.read().await;
        inner
            .manifests
            .get(kb.as_str())
            .cloned()
            .ok_or_else(|| StorageError::NotFound {
                key: ".notedthat/manifest.json".into(),
            })
    }

    async fn write_manifest(&self, kb: &KbSlug, manifest: &KbManifest) -> Result<(), StorageError> {
        let mut inner = self.inner.write().await;
        inner
            .manifests
            .insert(kb.as_str().to_string(), manifest.clone());
        Ok(())
    }

    async fn head_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectMeta, StorageError> {
        let inner = self.inner.read().await;
        let key = (kb.as_str().to_string(), path.as_str().to_string());
        let stored = inner
            .objects
            .get(&key)
            .ok_or_else(|| StorageError::NotFound {
                key: path.as_str().to_string(),
            })?;
        evaluate_read_preconditions(object_state(stored), &conditionals)?;
        Ok(object_meta(path, stored, stored.bytes.len() as u64))
    }

    async fn get_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<Vec<ByteRange>>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        let inner = self.inner.read().await;
        let key = (kb.as_str().to_string(), path.as_str().to_string());
        let stored = inner
            .objects
            .get(&key)
            .ok_or_else(|| StorageError::NotFound {
                key: path.as_str().to_string(),
            })?;
        evaluate_read_preconditions(object_state(stored), &conditionals)?;

        let total_size = stored.bytes.len() as u64;
        let (bytes, content_range) = match resolve_range(total_size, range.as_deref())? {
            Some((exclusive, content_range)) => (
                stored
                    .bytes
                    .slice(to_slice_index(exclusive.start)?..to_slice_index(exclusive.end)?),
                Some(content_range),
            ),
            None => (stored.bytes.clone(), None),
        };

        Ok(ObjectRead {
            meta: object_meta(path, stored, bytes.len() as u64),
            bytes,
            content_range,
        })
    }

    async fn get_object_stream(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<Vec<ByteRange>>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectStream, StorageError> {
        let read = self.get_object(kb, path, range, conditionals).await?;
        Ok(ObjectStream {
            chunks: Box::pin(futures::stream::once(async move { Ok(read.bytes) })),
            meta: read.meta,
            content_range: read.content_range,
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
        let mut inner = self.inner.write().await;
        let key = (kb.as_str().to_string(), path.as_str().to_string());
        evaluate_write_preconditions(inner.objects.get(&key).map(object_state), &conditionals)?;

        let etag = compute_etag(&bytes);
        inner.objects.insert(
            key,
            StoredObject {
                bytes,
                content_type: content_type.map(str::to_string),
                etag: etag.clone(),
                last_modified: SystemTime::now(),
            },
        );
        Ok(PutOutcome { etag: Some(etag) })
    }

    async fn put_staged_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        body: StagedBody,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        let bytes = if let Some(bytes) = body.memory_bytes() {
            bytes.clone()
        } else {
            let capacity = usize::try_from(body.len()).map_err(|source| StorageError::Other {
                source: Box::new(source),
            })?;
            let mut reader = body.open().await.map_err(|source| StorageError::Other {
                source: Box::new(source),
            })?;
            let mut bytes = Vec::with_capacity(capacity);
            reader
                .read_to_end(&mut bytes)
                .await
                .map_err(|source| StorageError::Other {
                    source: Box::new(source),
                })?;
            Bytes::from(bytes)
        };
        self.put_object(kb, path, bytes, content_type, conditionals)
            .await
    }

    async fn copy_object(
        &self,
        kb: &KbSlug,
        source: &ObjectPath,
        destination: &ObjectPath,
        options: CopyObjectOptions,
    ) -> Result<PutOutcome, StorageError> {
        let mut inner = self.inner.write().await;
        let source_key = (kb.as_str().to_string(), source.as_str().to_string());
        let destination_key = (kb.as_str().to_string(), destination.as_str().to_string());
        let source_object =
            inner
                .objects
                .get(&source_key)
                .cloned()
                .ok_or_else(|| StorageError::NotFound {
                    key: source.as_str().to_string(),
                })?;
        if options
            .source_if_match
            .as_ref()
            .is_some_and(|etag| !matches_if_match(&source_object.etag, etag))
        {
            return Err(StorageError::PreconditionFailed);
        }
        let destination_conditions = ConditionalHeaders {
            if_none_match: options.destination_if_none_match,
            ..ConditionalHeaders::default()
        };
        evaluate_write_preconditions(
            inner.objects.get(&destination_key).map(object_state),
            &destination_conditions,
        )?;
        let etag = source_object.etag.clone();
        inner.objects.insert(
            destination_key,
            StoredObject {
                bytes: source_object.bytes,
                content_type: options.content_type.or(source_object.content_type),
                etag: etag.clone(),
                last_modified: SystemTime::now(),
            },
        );
        Ok(PutOutcome { etag: Some(etag) })
    }

    async fn delete_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<(), StorageError> {
        let mut inner = self.inner.write().await;
        let key = (kb.as_str().to_string(), path.as_str().to_string());
        if let Some(if_match) = &conditionals.if_match
            && !inner
                .objects
                .get(&key)
                .is_some_and(|object| matches_if_match(&object.etag, if_match))
        {
            return Err(StorageError::PreconditionFailed);
        }

        inner.objects.remove(&key);
        Ok(())
    }

    async fn list_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<ListResponse, StorageError> {
        let inner = self.inner.read().await;
        let kb_str = kb.as_str();
        let mut matching: Vec<ObjectMeta> = inner
            .objects
            .iter()
            .filter(|((kb_key, obj_key), _)| {
                kb_key == kb_str && prefix.is_none_or(|p| obj_key.starts_with(p))
            })
            .map(|((_, obj_key), stored)| ObjectMeta {
                key: obj_key.clone(),
                size: stored.bytes.len() as u64,
                last_modified: Some(unix_seconds_i64(stored.last_modified)),
                content_type: stored.content_type.clone(),
                etag: Some(stored.etag.clone()),
            })
            .collect();
        matching.sort_by(|a, b| a.key.cmp(&b.key));

        // Apply cursor: cursor is the last returned key; start after it.
        if let Some(cursor_key) = cursor {
            // Validate: cursor_key must exist in the KB (it was a real key we returned)
            let key_exists = matching.iter().any(|obj| obj.key == cursor_key);
            if !key_exists {
                return Err(StorageError::BackendUnavailable {
                    message: "invalid or expired cursor".into(),
                });
            }
            // Skip everything up to and including the cursor key
            let cursor_pos = matching
                .iter()
                .position(|obj| obj.key == cursor_key)
                .unwrap();
            matching = matching.split_off(cursor_pos + 1);
        }

        let limit = limit.min(1000) as usize;
        let truncated = matching.len() > limit;
        matching.truncate(limit);

        // Compute next_cursor: the last key in the returned page, only when truncated
        let next_cursor = if truncated {
            matching.last().map(|obj| obj.key.clone())
        } else {
            None
        };

        Ok(ListResponse {
            objects: matching,
            truncated,
            next_cursor,
        })
    }
}

/// Reserve a loopback address that no other caller in this process will be given.
///
/// The obvious version of this — bind port 0, read the port back, drop the
/// listener — is a race. The port is free again the moment the probe listener
/// drops, so the kernel is entitled to hand the same one to the next probe, and
/// with a dozen tests in a binary each claiming several ports it does. Whichever
/// server binds second then fails with `EADDRINUSE`, and the suite reports a
/// bind error or a readiness timeout instead of the thing it was testing.
///
/// Remembering what has already been handed out closes the intra-process half of
/// that race, which is the half a `cargo test` run actually hits: test binaries
/// get separate port ranges from the kernel far more reliably than parallel
/// threads inside one binary do.
///
/// Still a probe, so a process outside this one can always steal the port
/// between the probe and the real bind. Nothing short of handing the bound
/// listener to the server fixes that, and it is not what makes these suites
/// flaky.
///
/// # Panics
///
/// Panics if no ephemeral port can be bound, or if the reservation set has been
/// poisoned by another test panicking while holding it.
#[must_use]
pub fn reserve_addr() -> std::net::SocketAddr {
    use std::sync::{Mutex, OnceLock};

    static TAKEN: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
    let taken = TAKEN.get_or_init(|| Mutex::new(HashSet::new()));

    // Hold every probe open until a fresh port turns up, so this loop cannot be
    // handed the same rejected port over and over.
    let mut probes = Vec::new();
    loop {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral loopback port");
        let addr = listener
            .local_addr()
            .expect("probe listener has a local addr");
        let fresh = taken
            .lock()
            .expect("port reservations are not poisoned")
            .insert(addr.port());
        if fresh {
            return addr;
        }
        probes.push(listener);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> KbSlug {
        KbSlug::try_new("test-kb").unwrap()
    }

    fn path(s: &str) -> ObjectPath {
        ObjectPath::try_from_str(s).unwrap()
    }

    #[tokio::test]
    async fn test_round_trip_put_get() {
        let storage = InMemoryStorage::default();
        let kb = kb();
        storage
            .put_object(
                &kb,
                &path("hello.md"),
                Bytes::from_static(b"# Hello"),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .unwrap();
        let read = storage
            .get_object(&kb, &path("hello.md"), None, ConditionalHeaders::default())
            .await
            .unwrap();
        assert_eq!(&read.bytes[..], b"# Hello");
        assert_eq!(read.meta.content_type.as_deref(), Some("text/markdown"));
    }

    #[tokio::test]
    async fn test_delete_idempotent() {
        let storage = InMemoryStorage::default();
        let kb = kb();
        assert!(
            storage
                .delete_object(&kb, &path("no-such-file.md"), ConditionalHeaders::default())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_list_sorted_and_truncated() {
        let storage = InMemoryStorage::default();
        let kb = kb();
        for i in 0..5 {
            storage
                .put_object(
                    &kb,
                    &path(&format!("{i}.md")),
                    Bytes::new(),
                    None,
                    ConditionalHeaders::default(),
                )
                .await
                .unwrap();
        }
        let result = storage.list_objects(&kb, None, 2, None).await.unwrap();
        assert_eq!(result.objects.len(), 2);
        assert!(result.truncated);
        assert_eq!(result.objects[0].key, "0.md");
        assert_eq!(result.objects[1].key, "1.md");
    }

    #[tokio::test]
    async fn test_list_invalid_or_expired_cursor_is_backend_unavailable() {
        let storage = InMemoryStorage::default();
        let kb = KbSlug::try_new("test").expect("valid slug");
        // Seed a few objects
        for i in 0..5u32 {
            let path = ObjectPath::try_from_str(&format!("doc-{i:04}.md")).expect("valid path");
            storage
                .put_object(
                    &kb,
                    &path,
                    bytes::Bytes::from_static(b"content"),
                    Some("text/markdown"),
                    ConditionalHeaders::default(),
                )
                .await
                .expect("put succeeded");
        }
        // Pass a garbage cursor (not a real key)
        let result = storage
            .list_objects(&kb, None, 10, Some("nonexistent-key.md"))
            .await;
        match result {
            Err(StorageError::BackendUnavailable { message }) => {
                assert!(
                    message.contains("invalid or expired cursor"),
                    "expected invalid cursor message, got: {message}"
                );
            }
            other => panic!("expected BackendUnavailable, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_list_cursor_collects_1500_without_duplicates() {
        use std::collections::HashSet;
        let storage = InMemoryStorage::default();
        let kb = KbSlug::try_new("test").expect("valid slug");
        // Seed 1500 objects with lexicographically-sortable keys
        for i in 0..1500u32 {
            let path = ObjectPath::try_from_str(&format!("doc-{i:04}.md")).expect("valid path");
            storage
                .put_object(
                    &kb,
                    &path,
                    bytes::Bytes::from_static(b"content"),
                    Some("text/markdown"),
                    ConditionalHeaders::default(),
                )
                .await
                .expect("put succeeded");
        }
        // Loop using cursor API until exhausted
        let mut all_keys: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut call_count = 0usize;
        loop {
            let resp = storage
                .list_objects(&kb, None, 100, cursor.as_deref())
                .await
                .expect("list succeeded");
            call_count += 1;
            all_keys.extend(resp.objects.iter().map(|o| o.key.clone()));
            cursor = resp.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        // AC1: total unique keys == 1500 and call count == 15
        assert_eq!(call_count, 15, "expected exactly 15 paginated calls");
        assert_eq!(all_keys.len(), 1500, "expected 1500 total keys collected");
        // AC2: collected order equals sorted order
        let mut sorted_keys = all_keys.clone();
        sorted_keys.sort();
        assert_eq!(all_keys, sorted_keys, "keys must be in lexicographic order");
        // AC3: no duplicates
        let unique: HashSet<_> = all_keys.iter().collect();
        assert_eq!(unique.len(), 1500, "no duplicate keys across pages");
        // AC4: the loop exited because next_cursor was None (not truncated=false workaround)
        // (call_count == 15 with 1500/100 pages satisfies this)
    }

    #[tokio::test]
    async fn etag_deterministic() {
        let storage = InMemoryStorage::default();
        let kb = kb();
        let expected = compute_etag(b"hello world");

        let put = storage
            .put_object(
                &kb,
                &path("etag.md"),
                Bytes::from_static(b"hello world"),
                None,
                ConditionalHeaders::default(),
            )
            .await
            .unwrap();
        let read = storage
            .get_object(&kb, &path("etag.md"), None, ConditionalHeaders::default())
            .await
            .unwrap();

        assert_eq!(put.etag.as_deref(), Some(expected.as_str()));
        assert_eq!(read.meta.etag.as_deref(), Some(expected.as_str()));
    }

    #[tokio::test]
    async fn if_match_multi() {
        let storage = InMemoryStorage::default();
        let kb = kb();
        let object_path = path("conditional.md");
        let etag = storage
            .put_object(
                &kb,
                &object_path,
                Bytes::from_static(b"initial"),
                None,
                ConditionalHeaders::default(),
            )
            .await
            .unwrap()
            .etag
            .unwrap();

        let ok = storage
            .put_object(
                &kb,
                &object_path,
                Bytes::from_static(b"updated"),
                None,
                ConditionalHeaders {
                    if_match: Some(format!("\"other\", {etag}, \"another\"")),
                    ..ConditionalHeaders::default()
                },
            )
            .await;
        assert!(ok.is_ok());

        let err = storage
            .put_object(
                &kb,
                &object_path,
                Bytes::from_static(b"rejected"),
                None,
                ConditionalHeaders {
                    if_match: Some("\"nope\"".to_string()),
                    ..ConditionalHeaders::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::PreconditionFailed));
    }

    #[tokio::test]
    async fn if_none_match_get_304() {
        let storage = InMemoryStorage::default();
        let kb = kb();
        let object_path = path("not-modified.md");
        let etag = storage
            .put_object(
                &kb,
                &object_path,
                Bytes::from_static(b"cached"),
                None,
                ConditionalHeaders::default(),
            )
            .await
            .unwrap()
            .etag
            .unwrap();

        let Err(err) = storage
            .get_object(
                &kb,
                &object_path,
                None,
                ConditionalHeaders {
                    if_none_match: Some(etag),
                    ..ConditionalHeaders::default()
                },
            )
            .await
        else {
            panic!("If-None-Match should return NotModified");
        };
        assert!(matches!(err, StorageError::NotModified));
    }

    #[tokio::test]
    async fn range_slice() {
        let storage = InMemoryStorage::default();
        let kb = kb();
        let object_path = path("range.bin");
        storage
            .put_object(
                &kb,
                &object_path,
                Bytes::from((0_u8..100).collect::<Vec<_>>()),
                None,
                ConditionalHeaders::default(),
            )
            .await
            .unwrap();

        let read = storage
            .get_object(
                &kb,
                &object_path,
                Some(vec![ByteRange::FromStart {
                    first: 10,
                    last: 19,
                }]),
                ConditionalHeaders::default(),
            )
            .await
            .unwrap();

        assert_eq!(read.bytes.len(), 10);
        let expected = (10_u8..20).collect::<Vec<_>>();
        assert_eq!(read.bytes.as_ref(), expected.as_slice());
        assert_eq!(read.meta.size, 10);
        assert_eq!(read.content_range.as_deref(), Some("bytes 10-19/100"));
    }

    #[tokio::test]
    async fn range_416() {
        let storage = InMemoryStorage::default();
        let kb = kb();
        let object_path = path("range-416.bin");
        storage
            .put_object(
                &kb,
                &object_path,
                Bytes::from(vec![0_u8; 50]),
                None,
                ConditionalHeaders::default(),
            )
            .await
            .unwrap();

        let Err(err) = storage
            .get_object(
                &kb,
                &object_path,
                Some(vec![ByteRange::FromStart {
                    first: 100,
                    last: 200,
                }]),
                ConditionalHeaders::default(),
            )
            .await
        else {
            panic!("unsatisfiable range should return RangeNotSatisfiable");
        };
        assert!(matches!(
            err,
            StorageError::RangeNotSatisfiable {
                complete_length: 50
            }
        ));
    }
}

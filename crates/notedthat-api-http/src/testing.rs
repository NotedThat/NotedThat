//! In-memory [`Storage`] implementation for testing.
//!
//! This module is only available when the `test-support` feature is enabled
//! or when running `cfg(test)`. **Never enable `test-support` in production builds.**

use async_trait::async_trait;
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, KbManifest, KbSlug, ListResponse, ObjectMeta, ObjectPath,
    ObjectRead, PutOutcome, Storage, StorageError,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

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

/// SHA-256 `ETag` for `bytes` in `"<hex>"` form (public for integration tests).
pub fn compute_etag(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("\"{}\"", hex::encode(Sha256::digest(bytes)))
}

fn parse_http_date_or_err(s: &str) -> Result<SystemTime, StorageError> {
    httpdate::parse_http_date(s).map_err(|e| StorageError::Other {
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid HTTP-date '{s}': {e}"),
        )),
    })
}

/// Strong `ETag` comparison: `If-Match` semantics.
fn matches_if_match(current_etag: &str, if_match_value: &str) -> bool {
    if if_match_value.trim() == "*" {
        return true;
    }

    if_match_value
        .split(',')
        .map(str::trim)
        .any(|tag| tag == current_etag)
}

/// Returns true if `If-None-Match` matches the current object.
fn matches_if_none_match(current_etag: Option<&str>, if_none_match_value: &str) -> bool {
    let Some(current) = current_etag else {
        return false;
    };

    if if_none_match_value.trim() == "*" {
        return true;
    }

    if_none_match_value.split(',').map(str::trim).any(|tag| {
        let tag = tag.trim_start_matches("W/");
        let current = current.trim_start_matches("W/");
        tag == current
    })
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map_or(0, |duration| duration.as_secs())
}

fn unix_seconds_i64(time: SystemTime) -> i64 {
    i64::try_from(unix_seconds(time)).unwrap_or(i64::MAX)
}

fn to_slice_index(value: u64) -> Result<usize, StorageError> {
    usize::try_from(value).map_err(|e| StorageError::Other {
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("range index {value} does not fit usize: {e}"),
        )),
    })
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

fn evaluate_write_preconditions(
    stored: Option<&StoredObject>,
    conditionals: &ConditionalHeaders,
) -> Result<(), StorageError> {
    if let Some(if_match) = &conditionals.if_match
        && !stored.is_some_and(|object| matches_if_match(&object.etag, if_match))
    {
        return Err(StorageError::PreconditionFailed);
    }

    if let Some(if_unmodified_since) = &conditionals.if_unmodified_since {
        let threshold = parse_http_date_or_err(if_unmodified_since)?;
        if stored.is_some_and(|object| unix_seconds(object.last_modified) > unix_seconds(threshold))
        {
            return Err(StorageError::PreconditionFailed);
        }
    }

    if let Some(if_none_match) = &conditionals.if_none_match
        && matches_if_none_match(stored.map(|object| object.etag.as_str()), if_none_match)
    {
        return Err(StorageError::PreconditionFailed);
    }

    Ok(())
}

fn evaluate_read_preconditions(
    stored: &StoredObject,
    conditionals: &ConditionalHeaders,
) -> Result<(), StorageError> {
    if let Some(if_match) = &conditionals.if_match
        && !matches_if_match(&stored.etag, if_match)
    {
        return Err(StorageError::PreconditionFailed);
    }

    if let Some(if_unmodified_since) = &conditionals.if_unmodified_since {
        let threshold = parse_http_date_or_err(if_unmodified_since)?;
        if unix_seconds(stored.last_modified) > unix_seconds(threshold) {
            return Err(StorageError::PreconditionFailed);
        }
    }

    if let Some(if_none_match) = &conditionals.if_none_match
        && matches_if_none_match(Some(&stored.etag), if_none_match)
    {
        return Err(StorageError::NotModified);
    }

    if let Some(if_modified_since) = &conditionals.if_modified_since {
        let threshold = parse_http_date_or_err(if_modified_since)?;
        if unix_seconds(stored.last_modified) <= unix_seconds(threshold) {
            return Err(StorageError::NotModified);
        }
    }

    Ok(())
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
        evaluate_read_preconditions(stored, &conditionals)?;
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
        evaluate_read_preconditions(stored, &conditionals)?;

        let total_size = stored.bytes.len() as u64;
        let first_range = range.as_ref().and_then(|ranges| ranges.first());
        let (bytes, content_range) = if let Some(byte_range) = first_range {
            let exclusive = byte_range.to_exclusive_range(total_size).ok_or(
                StorageError::RangeNotSatisfiable {
                    complete_length: total_size,
                },
            )?;
            let start = exclusive.start;
            let end = exclusive.end;
            (
                stored
                    .bytes
                    .slice(to_slice_index(start)?..to_slice_index(end)?),
                Some(format!("bytes {}-{}/{}", start, end - 1, total_size)),
            )
        } else {
            (stored.bytes.clone(), None)
        };

        Ok(ObjectRead {
            meta: object_meta(path, stored, bytes.len() as u64),
            bytes,
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
        let mut inner = self.inner.write().await;
        let key = (kb.as_str().to_string(), path.as_str().to_string());
        evaluate_write_preconditions(inner.objects.get(&key), &conditionals)?;

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
            let cursor_pos = matching.iter().position(|obj| obj.key == cursor_key).unwrap();
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

/// A `Searcher` that always returns an empty `SearchResponse`.
/// Used as the default searcher in test `AppState` instances so existing tests
/// don't need to mock the search path.
pub struct NoopSearcher;

#[async_trait]
impl notedthat_indexer::Searcher for NoopSearcher {
    async fn search(
        &self,
        _kb: &notedthat_core::KbSlug,
        _request: notedthat_core::search::ValidatedRequest,
    ) -> Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError> {
        Ok(notedthat_core::search::SearchResponse::empty())
    }
}

/// A scriptable `Searcher` for unit tests. Pre-load responses via `push_response`.
#[cfg(feature = "test-support")]
pub struct MockSearcher {
    responses: std::sync::Mutex<
        std::collections::VecDeque<
            Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError>,
        >,
    >,
}

#[cfg(feature = "test-support")]
impl MockSearcher {
    /// Create a new empty `MockSearcher`.
    pub fn new() -> Self {
        Self {
            responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// Push a response to the queue. Responses are returned in FIFO order.
    pub fn push_response(
        &self,
        r: Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError>,
    ) {
        self.responses.lock().unwrap().push_back(r);
    }

    /// Alias for `push_response` — matches the plan's specified API.
    pub fn set_response(
        &self,
        r: Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError>,
    ) {
        self.push_response(r);
    }
}

#[cfg(feature = "test-support")]
#[async_trait]
impl notedthat_indexer::Searcher for MockSearcher {
    async fn search(
        &self,
        _kb: &notedthat_core::KbSlug,
        _request: notedthat_core::search::ValidatedRequest,
    ) -> Result<notedthat_core::search::SearchResponse, notedthat_core::search::SearchError> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(notedthat_core::search::SearchResponse::empty()))
    }
}

#[cfg(feature = "test-support")]
impl Default for MockSearcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a test [`crate::state::AppState`] discarding the indexer receiver.
pub fn test_app_state_with_default_channel(
    storage: Arc<dyn Storage>,
    declared_kbs: Arc<BTreeMap<String, KbSlug>>,
    bearer_token: Arc<String>,
    max_body_size: u64,
) -> crate::state::AppState {
    let (indexer_tx, _) = tokio::sync::mpsc::channel(1024);
    crate::state::AppState {
        storage,
        declared_kbs,
        bearer_token,
        max_body_size,
        indexer_tx,
        searcher: Arc::new(NoopSearcher),
    }
}

/// Build a test [`crate::state::AppState`] returning both the state and the indexer receiver.
pub fn test_app_state_with_channel(
    storage: Arc<dyn Storage>,
    declared_kbs: Arc<BTreeMap<String, KbSlug>>,
    bearer_token: Arc<String>,
    max_body_size: u64,
) -> (
    crate::state::AppState,
    tokio::sync::mpsc::Receiver<notedthat_indexer::IndexEvent>,
) {
    let (indexer_tx, rx) = tokio::sync::mpsc::channel(1024);
    (
        crate::state::AppState {
            storage,
            declared_kbs,
            bearer_token,
            max_body_size,
            indexer_tx,
            searcher: Arc::new(NoopSearcher),
        },
        rx,
    )
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
        use std::collections::HashSet;
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

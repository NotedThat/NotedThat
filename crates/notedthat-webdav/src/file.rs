//! `dav-server` file adapter backed by range reads from `notedthat-core::Storage`.

use std::fmt;
use std::io::SeekFrom;
use std::sync::Arc;

use bytes::{Buf, Bytes};
use dav_server::fs::{DavFile, DavMetaData, FsError, FsFuture};
use notedthat_core::{ByteRange, ConditionalHeaders, KbSlug, ObjectPath};

use crate::metadata::WebDavMetaData;
use crate::state::WebDavState;

/// Read-only `WebDAV` file handle for a single stored object.
pub struct WebDavFile {
    state: Arc<WebDavState>,
    kb: KbSlug,
    path: ObjectPath,
    read_offset: u64,
    size_hint: Option<u64>,
}

#[allow(clippy::missing_fields_in_debug)]
impl fmt::Debug for WebDavFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebDavFile")
            .field("kb", &self.kb.as_str())
            .field("path", &self.path.as_str())
            .field("read_offset", &self.read_offset)
            .finish()
    }
}

impl WebDavFile {
    /// Create a new read-only file handle for `path` in `kb`.
    pub fn new(state: Arc<WebDavState>, kb: KbSlug, path: ObjectPath) -> Self {
        Self {
            state,
            kb,
            path,
            read_offset: 0,
            size_hint: None,
        }
    }
}

impl DavFile for WebDavFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let meta = self
                .state
                .storage
                .head_object(&self.kb, &self.path, ConditionalHeaders::default())
                .await
                .map_err(|_err| FsError::NotFound)?;
            self.size_hint = Some(meta.size);
            Ok(WebDavMetaData::object(meta))
        })
    }

    fn write_buf(&'_ mut self, _buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async { Err(FsError::NotImplemented) })
    }

    fn write_bytes(&'_ mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async { Err(FsError::NotImplemented) })
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            if count == 0 {
                return Ok(Bytes::new());
            }

            let first = self.read_offset;
            let last = first.saturating_add(count as u64).saturating_sub(1);
            let range = vec![ByteRange::FromStart { first, last }];
            let result = self
                .state
                .storage
                .get_object(
                    &self.kb,
                    &self.path,
                    Some(range),
                    ConditionalHeaders::default(),
                )
                .await
                .map_err(|_err| FsError::NotFound)?;
            let len = result.bytes.len() as u64;
            self.read_offset = self.read_offset.saturating_add(len);
            Ok(result.bytes)
        })
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            match pos {
                SeekFrom::Start(n) => {
                    self.read_offset = n;
                }
                SeekFrom::Current(n) => {
                    if n >= 0 {
                        self.read_offset = self.read_offset.saturating_add(n.unsigned_abs());
                    } else {
                        self.read_offset = self.read_offset.saturating_sub(n.unsigned_abs());
                    }
                }
                SeekFrom::End(n) => {
                    let size = if let Some(size) = self.size_hint {
                        size
                    } else {
                        let meta = self
                            .state
                            .storage
                            .head_object(&self.kb, &self.path, ConditionalHeaders::default())
                            .await
                            .map_err(|_err| FsError::NotFound)?;
                        self.size_hint = Some(meta.size);
                        meta.size
                    };

                    if n >= 0 {
                        self.read_offset = size.saturating_add(n.unsigned_abs());
                    } else {
                        self.read_offset = size.saturating_sub(n.unsigned_abs());
                    }
                }
            }

            Ok(self.read_offset)
        })
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(async { Err(FsError::NotImplemented) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use notedthat_core::{
        KbManifest, ListResponse, ObjectMeta, ObjectRead, PutOutcome, Storage, StorageError,
    };
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum StorageCall {
        HeadObject,
        GetObject { range: Option<Vec<ByteRange>> },
    }

    #[derive(Default)]
    struct MockStorage {
        calls: Mutex<Vec<StorageCall>>,
    }

    impl MockStorage {
        fn calls(&self) -> Vec<StorageCall> {
            self.calls.lock().expect("mutex not poisoned").clone()
        }
    }

    fn unavailable() -> StorageError {
        StorageError::BackendUnavailable {
            message: "mock storage method is not configured for this test".to_string(),
        }
    }

    fn object_meta() -> ObjectMeta {
        ObjectMeta {
            key: "test.md".to_string(),
            size: 100,
            last_modified: Some(1_700_000_000),
            content_type: Some("text/markdown".to_string()),
            etag: Some("\"test-etag\"".to_string()),
        }
    }

    #[async_trait]
    impl Storage for MockStorage {
        async fn ensure_bucket(&self, _kb: &KbSlug) -> Result<(), StorageError> {
            Err(unavailable())
        }

        async fn read_manifest(&self, _kb: &KbSlug) -> Result<KbManifest, StorageError> {
            Err(unavailable())
        }

        async fn write_manifest(
            &self,
            _kb: &KbSlug,
            _manifest: &KbManifest,
        ) -> Result<(), StorageError> {
            Err(unavailable())
        }

        async fn head_object(
            &self,
            _kb: &KbSlug,
            _path: &ObjectPath,
            _conditionals: ConditionalHeaders,
        ) -> Result<ObjectMeta, StorageError> {
            self.calls
                .lock()
                .expect("mutex not poisoned")
                .push(StorageCall::HeadObject);
            Ok(object_meta())
        }

        async fn get_object(
            &self,
            _kb: &KbSlug,
            _path: &ObjectPath,
            range: Option<Vec<ByteRange>>,
            _conditionals: ConditionalHeaders,
        ) -> Result<ObjectRead, StorageError> {
            self.calls
                .lock()
                .expect("mutex not poisoned")
                .push(StorageCall::GetObject {
                    range: range.clone(),
                });
            let len =
                range
                    .as_ref()
                    .and_then(|ranges| ranges.first())
                    .map_or(0, |range| match range {
                        ByteRange::FromStart { first, last } => last.saturating_sub(*first) + 1,
                        ByteRange::FromStartOpen { .. } | ByteRange::Suffix { .. } => 0,
                    });
            let len = usize::try_from(len).map_err(|_err| unavailable())?;

            Ok(ObjectRead {
                bytes: Bytes::from(vec![0; len]),
                meta: object_meta(),
                content_range: None,
            })
        }

        async fn put_object(
            &self,
            _kb: &KbSlug,
            _path: &ObjectPath,
            _bytes: Bytes,
            _content_type: Option<&str>,
            _conditionals: ConditionalHeaders,
        ) -> Result<PutOutcome, StorageError> {
            Err(unavailable())
        }

        async fn delete_object(
            &self,
            _kb: &KbSlug,
            _path: &ObjectPath,
            _conditionals: ConditionalHeaders,
        ) -> Result<(), StorageError> {
            Err(unavailable())
        }

        async fn list_objects(
            &self,
            _kb: &KbSlug,
            _prefix: Option<&str>,
            _limit: u32,
        ) -> Result<ListResponse, StorageError> {
            Err(unavailable())
        }
    }

    fn kb_slug() -> KbSlug {
        KbSlug::try_new("notes").expect("valid KB slug")
    }

    fn object_path() -> ObjectPath {
        ObjectPath::try_from_str("test.md").expect("valid object path")
    }

    fn test_state(storage: Arc<dyn Storage>) -> Arc<WebDavState> {
        let (indexer_tx, _rx) = mpsc::channel(16);
        Arc::new(WebDavState {
            username: Arc::new("user".to_string()),
            password: Arc::new("pass".to_string()),
            storage,
            declared_kbs: Arc::new(BTreeMap::new()),
            indexer_tx,
        })
    }

    fn test_file(storage: Arc<dyn Storage>) -> WebDavFile {
        WebDavFile::new(test_state(storage), kb_slug(), object_path())
    }

    #[tokio::test]
    async fn test_metadata_delegates_to_head_object_and_caches_size() {
        let storage = Arc::new(MockStorage::default());
        let mut file = test_file(storage.clone());

        let meta = file.metadata().await.expect("metadata should load");

        assert_eq!(meta.len(), 100);
        assert_eq!(file.size_hint, Some(100));
        assert_eq!(storage.calls(), vec![StorageCall::HeadObject]);
    }

    #[tokio::test]
    async fn test_read_bytes_translates_to_storage_range_request() {
        let storage = Arc::new(MockStorage::default());
        let mut file = test_file(storage.clone());

        let bytes = file.read_bytes(10).await.expect("read should succeed");

        assert_eq!(bytes.len(), 10);
        assert_eq!(
            storage.calls(),
            vec![StorageCall::GetObject {
                range: Some(vec![ByteRange::FromStart { first: 0, last: 9 }]),
            }]
        );
    }

    #[tokio::test]
    async fn test_read_bytes_advances_offset() {
        let storage = Arc::new(MockStorage::default());
        let mut file = test_file(storage);

        let _bytes = file.read_bytes(5).await.expect("read should succeed");

        assert_eq!(file.read_offset, 5);
    }

    #[tokio::test]
    async fn test_seek_from_start() {
        let storage = Arc::new(MockStorage::default());
        let mut file = test_file(storage);

        let offset = file
            .seek(SeekFrom::Start(42))
            .await
            .expect("seek should succeed");

        assert_eq!(offset, 42);
        assert_eq!(file.read_offset, 42);
    }

    #[tokio::test]
    async fn test_seek_from_end_uses_cached_size() {
        let storage = Arc::new(MockStorage::default());
        let mut file = test_file(storage.clone());
        let _meta = file.metadata().await.expect("metadata should load");

        let offset = file
            .seek(SeekFrom::End(0))
            .await
            .expect("seek should succeed");

        assert_eq!(offset, 100);
        assert_eq!(file.read_offset, 100);
        assert_eq!(storage.calls(), vec![StorageCall::HeadObject]);
    }

    #[tokio::test]
    async fn test_seek_from_current() {
        let storage = Arc::new(MockStorage::default());
        let mut file = test_file(storage);
        file.read_offset = 5;

        let offset = file
            .seek(SeekFrom::Current(10))
            .await
            .expect("seek should succeed");

        assert_eq!(offset, 15);
        assert_eq!(file.read_offset, 15);
    }

    #[tokio::test]
    async fn test_write_bytes_returns_not_implemented() {
        let storage = Arc::new(MockStorage::default());
        let mut file = test_file(storage);

        let result = file.write_bytes(Bytes::new()).await;

        assert!(matches!(result, Err(FsError::NotImplemented)));
    }

    #[tokio::test]
    async fn test_write_buf_returns_not_implemented() {
        let storage = Arc::new(MockStorage::default());
        let mut file = test_file(storage);

        let result = file.write_buf(Box::new(Bytes::new())).await;

        assert!(matches!(result, Err(FsError::NotImplemented)));
    }
}

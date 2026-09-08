use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, CopyObjectOptions, KbManifest, KbSlug, ListResponse, ObjectMeta,
    ObjectPath, ObjectRead, ObjectStream, PutOutcome, StagedBody, Storage, StorageError,
};

#[derive(Default)]
pub(super) struct MemoryStorage {
    objects: Mutex<BTreeMap<String, Vec<ObjectMeta>>>,
    calls: Mutex<Vec<String>>,
    page_size: Option<usize>,
}

impl MemoryStorage {
    pub(super) fn with_objects(
        objects: impl IntoIterator<Item = (&'static str, &'static str)>,
    ) -> Self {
        let mut by_kb = BTreeMap::<String, Vec<ObjectMeta>>::new();
        for (kb, key) in objects {
            by_kb.entry(kb.to_string()).or_default().push(ObjectMeta {
                key: key.to_string(),
                size: 7,
                last_modified: Some(1_700_000_000),
                content_type: Some("text/plain".to_string()),
                etag: Some(format!("\"{key}\"")),
            });
        }
        Self {
            objects: Mutex::new(by_kb),
            calls: Mutex::new(Vec::new()),
            page_size: None,
        }
    }

    pub(super) fn with_page_size(mut self, page_size: usize) -> Self {
        self.page_size = Some(page_size);
        self
    }

    pub(super) fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("calls mutex must not be poisoned")
            .clone()
    }

    fn record(&self, call: &str) {
        self.calls
            .lock()
            .expect("calls mutex must not be poisoned")
            .push(call.to_string());
    }
}

fn unavailable() -> StorageError {
    StorageError::BackendUnavailable {
        message: "operation is outside this public-read test".to_string(),
    }
}

#[async_trait]
impl Storage for MemoryStorage {
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
        kb: &KbSlug,
        path: &ObjectPath,
        _conditionals: ConditionalHeaders,
    ) -> Result<ObjectMeta, StorageError> {
        self.record("head");
        self.objects
            .lock()
            .expect("objects mutex must not be poisoned")
            .get(kb.as_str())
            .and_then(|objects| objects.iter().find(|meta| meta.key == path.as_str()))
            .cloned()
            .ok_or_else(|| StorageError::NotFound {
                key: path.as_str().to_string(),
            })
    }

    async fn get_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        _range: Option<Vec<ByteRange>>,
        _conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        self.record("get");
        let meta = self
            .objects
            .lock()
            .expect("objects mutex must not be poisoned")
            .get(kb.as_str())
            .and_then(|objects| objects.iter().find(|meta| meta.key == path.as_str()))
            .cloned()
            .ok_or_else(|| StorageError::NotFound {
                key: path.as_str().to_string(),
            })?;
        Ok(ObjectRead {
            bytes: Bytes::from_static(b"content"),
            meta,
            content_range: None,
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
        _kb: &KbSlug,
        _path: &ObjectPath,
        _bytes: Bytes,
        _content_type: Option<&str>,
        _conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        self.record("put");
        Err(unavailable())
    }

    async fn put_staged_object(
        &self,
        _kb: &KbSlug,
        _path: &ObjectPath,
        _body: StagedBody,
        _content_type: Option<&str>,
        _conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        self.record("put_staged");
        Err(unavailable())
    }

    async fn copy_object(
        &self,
        _kb: &KbSlug,
        _source: &ObjectPath,
        _destination: &ObjectPath,
        _options: CopyObjectOptions,
    ) -> Result<PutOutcome, StorageError> {
        self.record("copy");
        Err(unavailable())
    }

    async fn delete_object(
        &self,
        _kb: &KbSlug,
        _path: &ObjectPath,
        _conditionals: ConditionalHeaders,
    ) -> Result<(), StorageError> {
        self.record("delete");
        Err(unavailable())
    }

    async fn list_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
        _limit: u32,
        cursor: Option<&str>,
    ) -> Result<ListResponse, StorageError> {
        self.record("list");
        let objects = self
            .objects
            .lock()
            .expect("objects mutex must not be poisoned")
            .get(kb.as_str())
            .into_iter()
            .flatten()
            .filter(|meta| prefix.is_none_or(|prefix| meta.key.starts_with(prefix)))
            .cloned()
            .collect::<Vec<_>>();
        let start = cursor
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let page_size = self.page_size.unwrap_or(objects.len().max(1));
        let end = start.saturating_add(page_size).min(objects.len());
        let truncated = end < objects.len();
        Ok(ListResponse {
            objects: objects[start..end].to_vec(),
            truncated,
            next_cursor: truncated.then(|| end.to_string()),
        })
    }
}

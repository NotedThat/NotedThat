//! `dav-server` filesystem adapter backed by `notedthat-core::Storage`.

use bytes::{Buf, Bytes};
use dav_server::{
    davpath::DavPath,
    fs::{
        DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
        OpenOptions, ReadDirMeta,
    },
};
use futures::StreamExt as _;
use notedthat_core::{
    ByteRange, ConditionalHeaders, KbSlug, ListResponse, ObjectMeta, ObjectPath, StorageError,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::{self, Future},
    io::SeekFrom,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{metadata::WebDavMetaData, state::WebDavState};

/// Parsed target represented by a `DavPath`.
#[derive(Debug, PartialEq)]
pub(crate) enum DavTarget {
    /// The virtual `WebDAV` root (`/`).
    Root,
    /// The virtual root of a declared knowledge base (`/{kb}`).
    KbRoot(KbSlug),
    /// An object or object-prefix inside a declared knowledge base.
    Object(KbSlug, ObjectPath),
    /// A syntactically valid knowledge base slug that was not declared at startup.
    NonDeclaredKb,
}

/// Parse a `dav-server` path into a `NotedThat` `WebDAV` target.
pub(crate) fn parse_dav_path(
    path: &DavPath,
    declared_kbs: &BTreeMap<String, KbSlug>,
) -> Result<DavTarget, FsError> {
    let path_str = std::str::from_utf8(path.as_bytes()).map_err(|_| FsError::NotFound)?;
    let stripped = path_str.trim_start_matches('/');

    if stripped.is_empty() {
        return Ok(DavTarget::Root);
    }

    let (kb_part, rest) = stripped.split_once('/').unwrap_or((stripped, ""));

    let Ok(kb_slug) = KbSlug::try_new(kb_part) else {
        return Err(FsError::NotFound);
    };

    if !declared_kbs.contains_key(kb_part) {
        return Ok(DavTarget::NonDeclaredKb);
    }

    if rest.is_empty() {
        return Ok(DavTarget::KbRoot(kb_slug));
    }

    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() {
        return Ok(DavTarget::KbRoot(kb_slug));
    }

    match ObjectPath::try_from_str(rest) {
        Ok(path) => Ok(DavTarget::Object(kb_slug, path)),
        Err(_) => Err(FsError::NotFound),
    }
}

/// `WebDAV` filesystem implementation backed by the shared `NotedThat` state.
#[derive(Clone)]
pub struct WebDavStorage {
    /// Shared `WebDAV` state containing storage and declared knowledge bases.
    pub(crate) state: Arc<WebDavState>,
}

impl WebDavStorage {
    /// Create a new `WebDAV` filesystem wrapper around shared state.
    #[must_use]
    pub fn new(state: Arc<WebDavState>) -> Self {
        Self { state }
    }
}

impl DavFileSystem for WebDavStorage {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        if options.write
            || options.append
            || options.create
            || options.create_new
            || options.truncate
        {
            return Box::pin(future::ready(Err(FsError::NotImplemented)));
        }

        let state = Arc::clone(&self.state);
        let result = match parse_dav_path(path, state.declared_kbs.as_ref()) {
            Ok(DavTarget::Object(kb, path)) => {
                Ok(Box::new(StorageReadFile::new(state, kb, path)) as Box<dyn DavFile>)
            }
            Ok(DavTarget::Root | DavTarget::KbRoot(_) | DavTarget::NonDeclaredKb) => {
                Err(FsError::Forbidden)
            }
            Err(err) => Err(err),
        };

        Box::pin(future::ready(result))
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        let state = Arc::clone(&self.state);

        Box::pin(async move {
            match parse_dav_path(path, state.declared_kbs.as_ref())? {
                DavTarget::Root => Ok(stream_entries(root_entries(&state))),
                DavTarget::KbRoot(kb) => list_entries(state, kb, None).await,
                DavTarget::Object(kb, path) => {
                    let prefix = format!("{}/", path.as_str());
                    list_entries(state, kb, Some(prefix)).await
                }
                DavTarget::NonDeclaredKb => Err(FsError::Forbidden),
            }
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        let state = Arc::clone(&self.state);

        Box::pin(async move {
            match parse_dav_path(path, state.declared_kbs.as_ref())? {
                DavTarget::Root => Ok(WebDavMetaData::root(virtual_mtime())),
                DavTarget::KbRoot(kb) => {
                    Ok(WebDavMetaData::kb(kb.as_str().to_string(), virtual_mtime()))
                }
                DavTarget::Object(kb, path) => metadata_for_object_or_prefix(state, kb, path).await,
                DavTarget::NonDeclaredKb => Err(FsError::Forbidden),
            }
        })
    }

    fn create_dir<'a>(&'a self, _path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(future::ready(Ok(())))
    }

    fn remove_dir<'a>(&'a self, _path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(future::ready(Err(FsError::Forbidden)))
    }

    fn remove_file<'a>(&'a self, _path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(future::ready(Err(FsError::NotImplemented)))
    }

    fn rename<'a>(&'a self, _from: &'a DavPath, _to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(future::ready(Err(FsError::NotImplemented)))
    }

    fn copy<'a>(&'a self, _from: &'a DavPath, _to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(future::ready(Err(FsError::NotImplemented)))
    }

    fn have_props<'a>(
        &'a self,
        _path: &'a DavPath,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(future::ready(false))
    }

    fn get_quota(&'_ self) -> FsFuture<'_, (u64, Option<u64>)> {
        Box::pin(future::ready(Err(FsError::NotImplemented)))
    }
}

async fn list_entries(
    state: Arc<WebDavState>,
    kb: KbSlug,
    prefix: Option<String>,
) -> FsResult<FsStream<Box<dyn DavDirEntry>>> {
    let response = state
        .storage
        .list_objects(&kb, prefix.as_deref(), 1000, None)
        .await
        .map_err(|err| storage_error_to_fs(&err))?;

    if response.truncated {
        tracing::warn!(
            kb = %kb,
            prefix = prefix.as_deref().unwrap_or(""),
            "PROPFIND_TRUNCATED"
        );
    }

    Ok(stream_entries(entries_from_list(
        response,
        prefix.as_deref(),
    )))
}

async fn metadata_for_object_or_prefix(
    state: Arc<WebDavState>,
    kb: KbSlug,
    path: ObjectPath,
) -> FsResult<Box<dyn DavMetaData>> {
    match state
        .storage
        .head_object(&kb, &path, ConditionalHeaders::default())
        .await
    {
        Ok(meta) => Ok(WebDavMetaData::object(meta)),
        Err(err) if err.is_not_found() => {
            let prefix = format!("{}/", path.as_str());
            let response = state
                .storage
                .list_objects(&kb, Some(&prefix), 1, None)
                .await
                .map_err(|err| storage_error_to_fs(&err))?;
            if response.objects.is_empty() {
                Err(FsError::NotFound)
            } else {
                Ok(WebDavMetaData::kb(
                    format!("virtual-{}", path.as_str()),
                    virtual_mtime(),
                ))
            }
        }
        Err(err) => Err(storage_error_to_fs(&err)),
    }
}

fn root_entries(state: &WebDavState) -> Vec<Box<dyn DavDirEntry>> {
    state
        .declared_kbs
        .iter()
        .map(|(name, kb)| Box::new(KbDirEntry::new(name.clone(), kb)) as Box<dyn DavDirEntry>)
        .collect()
}

fn entries_from_list(response: ListResponse, prefix: Option<&str>) -> Vec<Box<dyn DavDirEntry>> {
    let mut virtual_dirs = BTreeSet::new();
    let mut files = BTreeMap::new();
    let prefix = prefix.unwrap_or("");

    for meta in response.objects {
        let Some(relative) = meta.key.strip_prefix(prefix) else {
            continue;
        };
        if relative.is_empty() {
            continue;
        }

        if let Some((dir_name, _)) = relative.split_once('/') {
            virtual_dirs.insert(dir_name.to_string());
        } else {
            files.insert(relative.to_string(), meta);
        }
    }

    let mut entries = Vec::new();
    for name in virtual_dirs {
        entries.push(Box::new(VirtualDirEntry::new(name.clone())) as Box<dyn DavDirEntry>);
        files.remove(&name);
    }
    entries.extend(
        files
            .into_iter()
            .map(|(name, meta)| Box::new(ObjectDirEntry::new(name, meta)) as Box<dyn DavDirEntry>),
    );
    entries
}

fn stream_entries(entries: Vec<Box<dyn DavDirEntry>>) -> FsStream<Box<dyn DavDirEntry>> {
    Box::pin(futures::stream::iter(entries).map(Ok))
}

fn storage_error_to_fs(err: &StorageError) -> FsError {
    match err {
        StorageError::NotFound { .. } | StorageError::BucketNotFound { .. } => FsError::NotFound,
        StorageError::PreconditionFailed | StorageError::RangeNotSatisfiable { .. } => {
            FsError::Forbidden
        }
        StorageError::BackendUnavailable { .. } | StorageError::Other { .. } => {
            FsError::GeneralFailure
        }
        StorageError::NotModified => FsError::GeneralFailure,
    }
}

fn virtual_mtime() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1)
}

#[derive(Debug, Clone)]
struct KbDirEntry {
    name: Vec<u8>,
    kb: String,
}

impl KbDirEntry {
    fn new(name: String, kb: &KbSlug) -> Self {
        Self {
            name: name.into_bytes(),
            kb: kb.as_str().to_string(),
        }
    }
}

impl DavDirEntry for KbDirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }

    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(future::ready(Ok(WebDavMetaData::kb(
            self.kb.clone(),
            virtual_mtime(),
        ))))
    }

    fn is_dir(&'_ self) -> FsFuture<'_, bool> {
        Box::pin(future::ready(Ok(true)))
    }

    fn is_file(&'_ self) -> FsFuture<'_, bool> {
        Box::pin(future::ready(Ok(false)))
    }
}

#[derive(Debug, Clone)]
struct ObjectDirEntry {
    name: Vec<u8>,
    meta: ObjectMeta,
}

impl ObjectDirEntry {
    fn new(name: String, meta: ObjectMeta) -> Self {
        Self {
            name: name.into_bytes(),
            meta,
        }
    }
}

impl DavDirEntry for ObjectDirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }

    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(future::ready(Ok(WebDavMetaData::object(self.meta.clone()))))
    }

    fn is_dir(&'_ self) -> FsFuture<'_, bool> {
        Box::pin(future::ready(Ok(false)))
    }

    fn is_file(&'_ self) -> FsFuture<'_, bool> {
        Box::pin(future::ready(Ok(true)))
    }
}

#[derive(Debug, Clone)]
struct VirtualDirEntry {
    name: Vec<u8>,
    metadata_slug: String,
}

impl VirtualDirEntry {
    fn new(name: String) -> Self {
        let metadata_slug = format!("virtual-{name}");
        Self {
            name: name.into_bytes(),
            metadata_slug,
        }
    }
}

impl DavDirEntry for VirtualDirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }

    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(future::ready(Ok(WebDavMetaData::kb(
            self.metadata_slug.clone(),
            virtual_mtime(),
        ))))
    }

    fn is_dir(&'_ self) -> FsFuture<'_, bool> {
        Box::pin(future::ready(Ok(true)))
    }

    fn is_file(&'_ self) -> FsFuture<'_, bool> {
        Box::pin(future::ready(Ok(false)))
    }
}

struct StorageReadFile {
    state: Arc<WebDavState>,
    kb: KbSlug,
    path: ObjectPath,
    read_offset: u64,
    size_hint: Option<u64>,
}

impl StorageReadFile {
    fn new(state: Arc<WebDavState>, kb: KbSlug, path: ObjectPath) -> Self {
        Self {
            state,
            kb,
            path,
            read_offset: 0,
            size_hint: None,
        }
    }
}

impl std::fmt::Debug for StorageReadFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageReadFile")
            .field("kb", &self.kb)
            .field("path", &self.path)
            .field("read_offset", &self.read_offset)
            .field("size_hint", &self.size_hint)
            .finish_non_exhaustive()
    }
}

impl DavFile for StorageReadFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let meta = self
                .state
                .storage
                .head_object(&self.kb, &self.path, ConditionalHeaders::default())
                .await
                .map_err(|err| storage_error_to_fs(&err))?;
            self.size_hint = Some(meta.size);
            Ok(WebDavMetaData::object(meta))
        })
    }

    fn write_buf(&'_ mut self, _buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(future::ready(Err(FsError::NotImplemented)))
    }

    fn write_bytes(&'_ mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(future::ready(Err(FsError::NotImplemented)))
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            if count == 0 {
                return Ok(Bytes::new());
            }

            let count = u64::try_from(count).map_err(|_| FsError::TooLarge)?;
            let last = self
                .read_offset
                .checked_add(count)
                .and_then(|end| end.checked_sub(1))
                .ok_or(FsError::TooLarge)?;
            let ranges = vec![ByteRange::FromStart {
                first: self.read_offset,
                last,
            }];

            let object = self
                .state
                .storage
                .get_object(
                    &self.kb,
                    &self.path,
                    Some(ranges),
                    ConditionalHeaders::default(),
                )
                .await
                .or_else(|err| match err {
                    StorageError::RangeNotSatisfiable { .. } => Ok(notedthat_core::ObjectRead {
                        bytes: Bytes::new(),
                        meta: ObjectMeta {
                            key: self.path.as_str().to_string(),
                            size: 0,
                            last_modified: None,
                            content_type: None,
                            etag: None,
                        },
                        content_range: None,
                    }),
                    other => Err(other),
                })
                .map_err(|err| storage_error_to_fs(&err))?;
            let read_len = u64::try_from(object.bytes.len()).map_err(|_| FsError::TooLarge)?;
            self.read_offset = self
                .read_offset
                .checked_add(read_len)
                .ok_or(FsError::TooLarge)?;
            Ok(object.bytes)
        })
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            let next_offset = match pos {
                SeekFrom::Start(offset) => offset,
                SeekFrom::Current(delta) => apply_seek_delta(self.read_offset, delta)?,
                SeekFrom::End(delta) => {
                    let size = if let Some(size) = self.size_hint {
                        size
                    } else {
                        let meta = self
                            .state
                            .storage
                            .head_object(&self.kb, &self.path, ConditionalHeaders::default())
                            .await
                            .map_err(|err| storage_error_to_fs(&err))?;
                        self.size_hint = Some(meta.size);
                        meta.size
                    };
                    apply_seek_delta(size, delta)?
                }
            };
            self.read_offset = next_offset;
            Ok(self.read_offset)
        })
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(future::ready(Err(FsError::NotImplemented)))
    }
}

fn apply_seek_delta(base: u64, delta: i64) -> FsResult<u64> {
    if delta >= 0 {
        base.checked_add(delta.unsigned_abs())
            .ok_or(FsError::TooLarge)
    } else {
        base.checked_sub(delta.unsigned_abs())
            .ok_or(FsError::Forbidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::TryStreamExt as _;
    use notedthat_core::{
        KbManifest, ObjectRead, PutOutcome, Storage, StorageError, storage::ListResponse,
    };
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct ListCall {
        kb: String,
        prefix: Option<String>,
        limit: u32,
        cursor: Option<String>,
    }

    #[derive(Default)]
    struct MockStorage {
        calls: Mutex<Vec<&'static str>>,
        list_calls: Mutex<Vec<ListCall>>,
        objects: Mutex<Vec<ObjectMeta>>,
        truncated: bool,
        next_cursor: Option<String>,
        head_not_found: bool,
        get_range_not_satisfiable: bool,
    }

    impl MockStorage {
        fn with_objects(objects: Vec<ObjectMeta>) -> Self {
            Self {
                objects: Mutex::new(objects),
                ..Self::default()
            }
        }

        fn record(&self, method: &'static str) {
            self.calls.lock().expect("mutex not poisoned").push(method);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().expect("mutex not poisoned").clone()
        }

        fn list_calls(&self) -> Vec<ListCall> {
            self.list_calls.lock().expect("mutex not poisoned").clone()
        }
    }

    fn unavailable() -> StorageError {
        StorageError::BackendUnavailable {
            message: "mock storage method is not configured for this test".to_string(),
        }
    }

    #[async_trait]
    impl Storage for MockStorage {
        async fn ensure_bucket(&self, _kb: &KbSlug) -> Result<(), StorageError> {
            self.record("ensure_bucket");
            Err(unavailable())
        }

        async fn read_manifest(&self, _kb: &KbSlug) -> Result<KbManifest, StorageError> {
            self.record("read_manifest");
            Err(unavailable())
        }

        async fn write_manifest(
            &self,
            _kb: &KbSlug,
            _manifest: &KbManifest,
        ) -> Result<(), StorageError> {
            self.record("write_manifest");
            Err(unavailable())
        }

        async fn head_object(
            &self,
            _kb: &KbSlug,
            path: &ObjectPath,
            _conditionals: ConditionalHeaders,
        ) -> Result<ObjectMeta, StorageError> {
            self.record("head_object");
            if self.head_not_found {
                return Err(StorageError::NotFound {
                    key: path.as_str().to_string(),
                });
            }
            Err(unavailable())
        }

        async fn get_object(
            &self,
            _kb: &KbSlug,
            _path: &ObjectPath,
            _range: Option<Vec<ByteRange>>,
            _conditionals: ConditionalHeaders,
        ) -> Result<ObjectRead, StorageError> {
            self.record("get_object");
            if self.get_range_not_satisfiable {
                return Err(StorageError::RangeNotSatisfiable { complete_length: 0 });
            }
            Err(unavailable())
        }

        async fn put_object(
            &self,
            _kb: &KbSlug,
            _path: &ObjectPath,
            _bytes: Bytes,
            _content_type: Option<&str>,
            _conditionals: ConditionalHeaders,
        ) -> Result<PutOutcome, StorageError> {
            self.record("put_object");
            Err(unavailable())
        }

        async fn delete_object(
            &self,
            _kb: &KbSlug,
            _path: &ObjectPath,
            _conditionals: ConditionalHeaders,
        ) -> Result<(), StorageError> {
            self.record("delete_object");
            Err(unavailable())
        }

        async fn list_objects(
            &self,
            kb: &KbSlug,
            prefix: Option<&str>,
            limit: u32,
            cursor: Option<&str>,
        ) -> Result<ListResponse, StorageError> {
            self.record("list_objects");
            self.list_calls
                .lock()
                .expect("mutex not poisoned")
                .push(ListCall {
                    kb: kb.as_str().to_string(),
                    prefix: prefix.map(str::to_string),
                    limit,
                    cursor: cursor.map(str::to_string),
                });
            Ok(ListResponse {
                objects: self.objects.lock().expect("mutex not poisoned").clone(),
                truncated: self.truncated,
                next_cursor: self.next_cursor.clone(),
            })
        }
    }

    fn kb_slug(value: &str) -> KbSlug {
        KbSlug::try_new(value).expect("valid KB slug")
    }

    fn object_path(value: &str) -> ObjectPath {
        ObjectPath::try_from_str(value).expect("valid object path")
    }

    fn declared_kbs(values: &[&str]) -> BTreeMap<String, KbSlug> {
        values
            .iter()
            .map(|value| ((*value).to_string(), kb_slug(value)))
            .collect()
    }

    fn object_meta(key: &str) -> ObjectMeta {
        ObjectMeta {
            key: key.to_string(),
            size: 42,
            last_modified: Some(1),
            content_type: Some("text/markdown".to_string()),
            etag: Some(format!("\"etag-{key}\"")),
        }
    }

    fn test_state(
        storage: Arc<dyn Storage>,
        declared_kbs: BTreeMap<String, KbSlug>,
    ) -> Arc<WebDavState> {
        let (indexer_tx, _rx) = mpsc::channel(16);
        Arc::new(WebDavState {
            username: Arc::new("user".to_string()),
            password: Arc::new("pass".to_string()),
            storage,
            declared_kbs: Arc::new(declared_kbs),
            indexer_tx,
        })
    }

    fn test_filesystem(storage: Arc<dyn Storage>, declared_kbs: &[&str]) -> WebDavStorage {
        WebDavStorage::new(test_state(storage, self::declared_kbs(declared_kbs)))
    }

    fn dav_path(value: &str) -> DavPath {
        DavPath::new(value).expect("valid DAV path")
    }

    fn entry_names(entries: Vec<Box<dyn DavDirEntry>>) -> Vec<String> {
        entries
            .into_iter()
            .map(|entry| String::from_utf8(entry.name()).expect("UTF-8 entry name"))
            .collect()
    }

    #[test]
    fn test_parse_dav_path_root() {
        let declared = declared_kbs(&["notes"]);
        let target = parse_dav_path(&dav_path("/"), &declared).expect("root parses");

        assert_eq!(target, DavTarget::Root);
    }

    #[test]
    fn test_parse_dav_path_kb_root() {
        let declared = declared_kbs(&["notes"]);
        let target = parse_dav_path(&dav_path("/notes"), &declared).expect("KB root parses");

        assert_eq!(target, DavTarget::KbRoot(kb_slug("notes")));
    }

    #[test]
    fn test_parse_dav_path_object() {
        let declared = declared_kbs(&["notes"]);
        let target =
            parse_dav_path(&dav_path("/notes/folder/file.md"), &declared).expect("object parses");

        assert_eq!(
            target,
            DavTarget::Object(kb_slug("notes"), object_path("folder/file.md"))
        );
    }

    #[test]
    fn test_parse_dav_path_decodes_percent_encoded_object_path() {
        let declared = declared_kbs(&["notes"]);
        let target = parse_dav_path(&dav_path("/notes/hello%20world%25.md"), &declared)
            .expect("encoded object path parses");

        assert_eq!(
            target,
            DavTarget::Object(kb_slug("notes"), object_path("hello world%.md"))
        );
    }

    #[tokio::test]
    async fn test_parse_dav_path_non_declared_kb_forbidden() {
        let storage = Arc::new(MockStorage::default());
        let fs = test_filesystem(storage.clone(), &["notes"]);
        let path = dav_path("/scratch/file.md");

        let target = parse_dav_path(&path, fs.state.declared_kbs.as_ref()).expect("path parses");
        assert_eq!(target, DavTarget::NonDeclaredKb);

        let err = fs
            .metadata(&path)
            .await
            .expect_err("non-declared KB is forbidden");
        assert_eq!(err, FsError::Forbidden);
        assert!(storage.calls().is_empty());
    }

    #[test]
    fn test_parse_dav_path_invalid_slug_returns_notfound() {
        let declared = declared_kbs(&["notes"]);
        let err = parse_dav_path(&dav_path("/Invalid/file.md"), &declared)
            .expect_err("invalid slug returns not found");

        assert_eq!(err, FsError::NotFound);
    }

    #[tokio::test]
    async fn test_read_dir_root_lists_all_declared_kbs() {
        let storage = Arc::new(MockStorage::default());
        let fs = test_filesystem(storage.clone(), &["notes", "scratch"]);

        let entries = fs
            .read_dir(&dav_path("/"), ReadDirMeta::None)
            .await
            .expect("root read_dir succeeds")
            .try_collect::<Vec<_>>()
            .await
            .expect("stream succeeds");

        for entry in &entries {
            assert!(entry.is_dir().await.expect("entry dir check succeeds"));
        }
        assert_eq!(entry_names(entries), vec!["notes", "scratch"]);
        assert!(storage.calls().is_empty());
    }

    #[tokio::test]
    async fn test_read_dir_kb_root_calls_list_objects() {
        let storage = Arc::new(MockStorage::with_objects(vec![
            object_meta("folder/bravo.md"),
            object_meta("alpha.md"),
        ]));
        let fs = test_filesystem(storage.clone(), &["notes"]);

        let entries = fs
            .read_dir(&dav_path("/notes"), ReadDirMeta::None)
            .await
            .expect("KB root read_dir succeeds")
            .try_collect::<Vec<_>>()
            .await
            .expect("stream succeeds");

        assert_eq!(
            storage.list_calls(),
            vec![ListCall {
                kb: "notes".to_string(),
                prefix: None,
                limit: 1000,
                cursor: None,
            }]
        );
        assert_eq!(entry_names(entries), vec!["folder", "alpha.md"]);
    }

    #[tokio::test]
    async fn test_read_dir_virtual_folder_uses_decoded_prefix() {
        let storage = Arc::new(MockStorage::with_objects(vec![object_meta(
            "hello world/bravo.md",
        )]));
        let fs = test_filesystem(storage.clone(), &["notes"]);

        let entries = fs
            .read_dir(&dav_path("/notes/hello%20world/"), ReadDirMeta::None)
            .await
            .expect("virtual folder read_dir succeeds")
            .try_collect::<Vec<_>>()
            .await
            .expect("stream succeeds");

        assert_eq!(
            storage.list_calls(),
            vec![ListCall {
                kb: "notes".to_string(),
                prefix: Some("hello world/".to_string()),
                limit: 1000,
                cursor: None,
            }]
        );
        assert_eq!(entry_names(entries), vec!["bravo.md"]);
    }

    #[tokio::test]
    async fn test_metadata_virtual_folder_falls_back_to_prefix_listing() {
        let storage = Arc::new(MockStorage {
            head_not_found: true,
            ..MockStorage::with_objects(vec![object_meta("folder/bravo.md")])
        });
        let fs = test_filesystem(storage.clone(), &["notes"]);

        let metadata = fs
            .metadata(&dav_path("/notes/folder"))
            .await
            .expect("virtual folder metadata succeeds");

        assert!(metadata.is_dir());
        assert_eq!(
            storage.list_calls(),
            vec![ListCall {
                kb: "notes".to_string(),
                prefix: Some("folder/".to_string()),
                limit: 1,
                cursor: None,
            }]
        );
    }

    #[tokio::test]
    async fn test_read_bytes_range_not_satisfiable_returns_empty_at_eof() {
        let storage = Arc::new(MockStorage {
            get_range_not_satisfiable: true,
            ..MockStorage::default()
        });
        let fs = test_filesystem(storage, &["notes"]);
        let mut file = fs
            .open(&dav_path("/notes/file.md"), OpenOptions::default())
            .await
            .expect("file opens");

        let bytes = file.read_bytes(1).await.expect("EOF read succeeds");

        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn test_create_dir_returns_ok_without_storage_call() {
        let storage = Arc::new(MockStorage::default());
        let fs = test_filesystem(storage.clone(), &["notes"]);

        fs.create_dir(&dav_path("/notes/new-folder"))
            .await
            .expect("MKCOL succeeds as a no-op");

        assert!(storage.calls().is_empty());
    }

    #[tokio::test]
    async fn test_have_props_returns_false() {
        let storage = Arc::new(MockStorage::default());
        let fs = test_filesystem(storage, &["notes"]);

        assert!(!fs.have_props(&dav_path("/")).await);
    }
}

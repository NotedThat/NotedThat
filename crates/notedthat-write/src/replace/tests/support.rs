use super::super::{ReplaceRequest, replace};
use async_trait::async_trait;
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, KbManifest, KbSlug, ListResponse, ObjectMeta, ObjectPath,
    ObjectRead, PutOutcome, Storage, StorageError,
};
use notedthat_indexer::IndexEvent;
use std::{collections::HashMap, sync::Mutex};
use tokio::sync::mpsc;

#[derive(Clone)]
struct StoredObject {
    body: Bytes,
    etag: String,
    content_type: Option<String>,
}

#[derive(Default)]
pub(super) struct TestStorage {
    objects: Mutex<HashMap<String, StoredObject>>,
    calls: Mutex<Calls>,
    script: Mutex<Script>,
}

#[derive(Default)]
pub(super) struct Calls {
    pub(super) head: u32,
    pub(super) get: u32,
    pub(super) put: u32,
}

#[derive(Default)]
pub(super) struct Script {
    pub(super) get_failures_remaining: u32,
    pub(super) put_failures_remaining: u32,
}

#[async_trait]
impl Storage for TestStorage {
    async fn ensure_bucket(&self, _kb: &KbSlug) -> Result<(), StorageError> {
        unimplemented!()
    }

    async fn read_manifest(&self, _kb: &KbSlug) -> Result<KbManifest, StorageError> {
        unimplemented!()
    }

    async fn write_manifest(
        &self,
        _kb: &KbSlug,
        _manifest: &KbManifest,
    ) -> Result<(), StorageError> {
        unimplemented!()
    }

    async fn head_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectMeta, StorageError> {
        self.calls.lock().expect("mutex not poisoned").head += 1;
        let object = self.object(kb, path)?;
        check_if_match(&conditionals, &object.etag)?;
        Ok(meta(path, &object))
    }

    async fn get_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        _range: Option<Vec<ByteRange>>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        self.calls.lock().expect("mutex not poisoned").get += 1;
        let object = self.object(kb, path)?;
        check_if_match(&conditionals, &object.etag)?;
        let mut script = self.script.lock().expect("mutex not poisoned");
        if script.get_failures_remaining > 0 {
            script.get_failures_remaining -= 1;
            return Err(StorageError::PreconditionFailed);
        }
        drop(script);
        Ok(ObjectRead {
            bytes: object.body.clone(),
            meta: meta(path, &object),
            content_range: None,
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
        self.calls.lock().expect("mutex not poisoned").put += 1;
        let key = key(kb, path);
        let mut objects = self.objects.lock().expect("mutex not poisoned");
        let current = objects.get(&key).map(|object| object.etag.as_str());
        if conditionals
            .if_match
            .as_deref()
            .is_some_and(|etag| current != Some(etag))
        {
            return Err(StorageError::PreconditionFailed);
        }
        let mut script = self.script.lock().expect("mutex not poisoned");
        if script.put_failures_remaining > 0 {
            script.put_failures_remaining -= 1;
            return Err(StorageError::PreconditionFailed);
        }
        drop(script);
        let etag = "etag2".to_string();
        objects.insert(
            key,
            StoredObject {
                body: bytes,
                etag: etag.clone(),
                content_type: content_type.map(str::to_string),
            },
        );
        Ok(PutOutcome { etag: Some(etag) })
    }

    async fn delete_object(
        &self,
        _kb: &KbSlug,
        _path: &ObjectPath,
        _conditionals: ConditionalHeaders,
    ) -> Result<(), StorageError> {
        unimplemented!()
    }

    async fn list_objects(
        &self,
        _kb: &KbSlug,
        _prefix: Option<&str>,
        _limit: u32,
        _cursor: Option<&str>,
    ) -> Result<ListResponse, StorageError> {
        unimplemented!()
    }
}

impl TestStorage {
    pub(super) fn with_body(body: &'static [u8]) -> Self {
        let storage = Self::default();
        storage.objects.lock().expect("mutex not poisoned").insert(
            key(&kb(), &path()),
            StoredObject {
                body: Bytes::from_static(body),
                etag: "etag1".to_string(),
                content_type: Some("text/plain".to_string()),
            },
        );
        storage
    }

    pub(super) fn with_script(body: &'static [u8], script: Script) -> Self {
        let storage = Self {
            objects: Mutex::new(HashMap::new()),
            calls: Mutex::new(Calls::default()),
            script: Mutex::new(script),
        };
        storage.objects.lock().expect("mutex not poisoned").insert(
            key(&kb(), &path()),
            StoredObject {
                body: Bytes::from_static(body),
                etag: "etag1".to_string(),
                content_type: Some("text/plain".to_string()),
            },
        );
        storage
    }

    pub(super) async fn read(&self) -> ObjectRead {
        self.get_object(&kb(), &path(), None, ConditionalHeaders::default())
            .await
            .expect("object read succeeds")
    }

    pub(super) fn body(&self) -> Bytes {
        self.objects
            .lock()
            .expect("mutex not poisoned")
            .get(&key(&kb(), &path()))
            .expect("object exists")
            .body
            .clone()
    }

    pub(super) fn calls(&self) -> Calls {
        let calls = self.calls.lock().expect("mutex not poisoned");
        Calls {
            head: calls.head,
            get: calls.get,
            put: calls.put,
        }
    }

    fn object(&self, kb: &KbSlug, path: &ObjectPath) -> Result<StoredObject, StorageError> {
        self.objects
            .lock()
            .expect("mutex not poisoned")
            .get(&key(kb, path))
            .cloned()
            .ok_or_else(|| StorageError::NotFound { key: key(kb, path) })
    }
}

pub(super) struct ReplaceArgs<'a> {
    old: &'a str,
    new: &'a str,
    replace_all: bool,
}

impl<'a> ReplaceArgs<'a> {
    pub(super) fn one(old: &'a str, new: &'a str) -> Self {
        Self {
            old,
            new,
            replace_all: false,
        }
    }

    pub(super) fn all(old: &'a str, new: &'a str) -> Self {
        Self {
            old,
            new,
            replace_all: true,
        }
    }
}

pub(super) async fn run_replace(
    storage: &TestStorage,
    args: ReplaceArgs<'_>,
) -> Result<crate::ReplaceOutcome, crate::WriteError> {
    run_replace_with(storage, args, conditionals(Some("etag1")), 1024, None).await
}

pub(super) async fn run_replace_with(
    storage: &TestStorage,
    args: ReplaceArgs<'_>,
    caller_conditionals: ConditionalHeaders,
    max_patchable_size: u64,
    supplied_indexer_tx: Option<mpsc::Sender<IndexEvent>>,
) -> Result<crate::ReplaceOutcome, crate::WriteError> {
    let (indexer_tx, _rx) = mpsc::channel::<IndexEvent>(8);
    let indexer_tx = supplied_indexer_tx.unwrap_or(indexer_tx);
    let kb = kb();
    let path = path();
    replace(
        storage,
        &indexer_tx,
        ReplaceRequest {
            kb: &kb,
            path: &path,
            old_string: args.old,
            new_string: args.new,
            replace_all: args.replace_all,
            caller_conditionals,
            max_patchable_size,
            caller_content_type: None,
        },
    )
    .await
}

pub(super) fn expect_replace_err(
    result: Result<crate::ReplaceOutcome, crate::WriteError>,
) -> crate::WriteError {
    match result {
        Ok(_) => panic!("replace should fail"),
        Err(err) => err,
    }
}

fn check_if_match(
    conditionals: &ConditionalHeaders,
    current_etag: &str,
) -> Result<(), StorageError> {
    if conditionals
        .if_match
        .as_deref()
        .is_some_and(|etag| etag != current_etag)
    {
        return Err(StorageError::PreconditionFailed);
    }
    Ok(())
}

fn meta(path: &ObjectPath, object: &StoredObject) -> ObjectMeta {
    ObjectMeta {
        key: path.as_str().to_string(),
        size: object.body.len() as u64,
        last_modified: None,
        content_type: object.content_type.clone(),
        etag: Some(object.etag.clone()),
    }
}

pub(super) fn conditionals(etag: Option<&str>) -> ConditionalHeaders {
    ConditionalHeaders {
        if_match: etag.map(str::to_string),
        ..ConditionalHeaders::default()
    }
}

pub(super) fn kb() -> KbSlug {
    KbSlug::try_new("test-kb").expect("valid kb slug")
}

pub(super) fn path() -> ObjectPath {
    ObjectPath::try_from_str("test.md").expect("valid path")
}

fn key(kb: &KbSlug, path: &ObjectPath) -> String {
    format!("{}/{}", kb.as_str(), path.as_str())
}

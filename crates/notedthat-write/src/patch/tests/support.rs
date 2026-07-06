use super::super::*;
use async_trait::async_trait;
use notedthat_core::{KbManifest, ListResponse, ObjectMeta, ObjectRead};
use std::collections::HashMap;
use std::sync::Mutex;
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
    pub(super) advance_etag_on_put_failure: bool,
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
        Self::check_if_match(&conditionals, &object.etag)?;
        Ok(Self::meta(path, &object))
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
        Self::check_if_match(&conditionals, &object.etag)?;
        let mut script = self.script.lock().expect("mutex not poisoned");
        if script.get_failures_remaining > 0 {
            script.get_failures_remaining -= 1;
            return Err(StorageError::PreconditionFailed);
        }
        drop(script);
        Ok(ObjectRead {
            bytes: object.body.clone(),
            meta: Self::meta(path, &object),
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
            if script.advance_etag_on_put_failure {
                let object = objects.get_mut(&key).expect("object exists");
                object.etag = "etag2".to_string();
            }
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
        storage.insert(Bytes::from_static(body), "etag1");
        storage
    }

    pub(super) fn with_script(body: &'static [u8], script: Script) -> Self {
        let storage = Self {
            objects: Mutex::new(HashMap::new()),
            calls: Mutex::new(Calls::default()),
            script: Mutex::new(script),
        };
        storage.insert(Bytes::from_static(body), "etag1");
        storage
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

    fn insert(&self, body: Bytes, etag: &str) {
        self.objects.lock().expect("mutex not poisoned").insert(
            key(&kb(), &path()),
            StoredObject {
                body,
                etag: etag.to_string(),
                content_type: Some("text/plain".to_string()),
            },
        );
    }

    fn object(&self, kb: &KbSlug, path: &ObjectPath) -> Result<StoredObject, StorageError> {
        self.objects
            .lock()
            .expect("mutex not poisoned")
            .get(&key(kb, path))
            .cloned()
            .ok_or_else(|| StorageError::NotFound { key: key(kb, path) })
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
}

pub(super) fn conditionals(etag: Option<&str>) -> ConditionalHeaders {
    ConditionalHeaders {
        if_match: etag.map(str::to_string),
        ..ConditionalHeaders::default()
    }
}

pub(super) async fn run_patch(
    storage: &TestStorage,
    patch_mode: PatchMode,
    caller_conditionals: ConditionalHeaders,
    max_size: u64,
) -> Result<(PutOutcome, mpsc::Receiver<IndexEvent>), WriteError> {
    let (indexer_tx, rx) = mpsc::channel(8);
    let kb = kb();
    let path = path();
    let outcome = patch(
        storage,
        &indexer_tx,
        PatchRequest {
            kb: &kb,
            path: &path,
            patch_mode,
            caller_conditionals,
            max_patchable_size: max_size,
            caller_content_type: None,
        },
    )
    .await?;
    Ok((outcome, rx))
}

fn kb() -> KbSlug {
    KbSlug::try_new("test-kb").expect("valid kb slug")
}

fn path() -> ObjectPath {
    ObjectPath::try_from_str("test.md").expect("valid path")
}

fn key(kb: &KbSlug, path: &ObjectPath) -> String {
    format!("{}/{}", kb.as_str(), path.as_str())
}

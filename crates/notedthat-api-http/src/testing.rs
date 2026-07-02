//! In-memory [`Storage`] implementation for testing.
//!
//! This module is only available when the `test-support` feature is enabled
//! or when running `cfg(test)`. **Never enable `test-support` in production builds.**

use async_trait::async_trait;
use bytes::Bytes;
use notedthat_core::{
    KbManifest, KbSlug, ListResponse, ObjectMeta, ObjectPath, ObjectRead, Storage, StorageError,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

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
    /// (`kb_slug`, `object_key`) → (bytes, `content_type`)
    objects: HashMap<(String, String), (Bytes, Option<String>)>,
    manifests: HashMap<String, KbManifest>,
    buckets: HashSet<String>,
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
    ) -> Result<ObjectMeta, StorageError> {
        let inner = self.inner.read().await;
        let key = (kb.as_str().to_string(), path.as_str().to_string());
        inner
            .objects
            .get(&key)
            .map(|(bytes, content_type)| ObjectMeta {
                key: path.as_str().to_string(),
                size: bytes.len() as u64,
                last_modified: None,
                content_type: content_type.clone(),
            })
            .ok_or_else(|| StorageError::NotFound {
                key: path.as_str().to_string(),
            })
    }

    async fn get_object(&self, kb: &KbSlug, path: &ObjectPath) -> Result<ObjectRead, StorageError> {
        let inner = self.inner.read().await;
        let key = (kb.as_str().to_string(), path.as_str().to_string());
        inner
            .objects
            .get(&key)
            .map(|(bytes, content_type)| ObjectRead {
                bytes: bytes.clone(),
                meta: ObjectMeta {
                    key: path.as_str().to_string(),
                    size: bytes.len() as u64,
                    last_modified: None,
                    content_type: content_type.clone(),
                },
            })
            .ok_or_else(|| StorageError::NotFound {
                key: path.as_str().to_string(),
            })
    }

    async fn put_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        bytes: Bytes,
        content_type: Option<&str>,
    ) -> Result<(), StorageError> {
        let mut inner = self.inner.write().await;
        inner.objects.insert(
            (kb.as_str().to_string(), path.as_str().to_string()),
            (bytes, content_type.map(str::to_string)),
        );
        Ok(())
    }

    async fn delete_object(&self, kb: &KbSlug, path: &ObjectPath) -> Result<(), StorageError> {
        let mut inner = self.inner.write().await;
        inner
            .objects
            .remove(&(kb.as_str().to_string(), path.as_str().to_string()));
        Ok(())
    }

    async fn list_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
        limit: u32,
    ) -> Result<ListResponse, StorageError> {
        let inner = self.inner.read().await;
        let kb_str = kb.as_str();
        let mut matching: Vec<ObjectMeta> = inner
            .objects
            .iter()
            .filter(|((kb_key, obj_key), _)| {
                kb_key == kb_str && prefix.is_none_or(|p| obj_key.starts_with(p))
            })
            .map(|((_, obj_key), (bytes, content_type))| ObjectMeta {
                key: obj_key.clone(),
                size: bytes.len() as u64,
                last_modified: None,
                content_type: content_type.clone(),
            })
            .collect();
        matching.sort_by(|a, b| a.key.cmp(&b.key));

        let limit = limit.min(1000) as usize;
        let truncated = matching.len() > limit;
        matching.truncate(limit);
        Ok(ListResponse {
            objects: matching,
            truncated,
        })
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
            )
            .await
            .unwrap();
        let read = storage.get_object(&kb, &path("hello.md")).await.unwrap();
        assert_eq!(&read.bytes[..], b"# Hello");
        assert_eq!(read.meta.content_type.as_deref(), Some("text/markdown"));
    }

    #[tokio::test]
    async fn test_delete_idempotent() {
        let storage = InMemoryStorage::default();
        let kb = kb();
        assert!(
            storage
                .delete_object(&kb, &path("no-such-file.md"))
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
                .put_object(&kb, &path(&format!("{i}.md")), Bytes::new(), None)
                .await
                .unwrap();
        }
        let result = storage.list_objects(&kb, None, 2).await.unwrap();
        assert_eq!(result.objects.len(), 2);
        assert!(result.truncated);
        assert_eq!(result.objects[0].key, "0.md");
        assert_eq!(result.objects[1].key, "1.md");
    }
}

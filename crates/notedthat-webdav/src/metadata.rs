//! `WebDAV` metadata implementation for dav-server integration.
//!
//! Provides `WebDavMetaData` enum implementing `dav_server::fs::DavMetaData` trait
//! with S3 `ETag` override for proper conditional header handling.

use dav_server::fs::{DavMetaData, FsResult};
use notedthat_core::kb::ObjectMeta;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `WebDAV` metadata for virtual (Root, Kb) and real (Object) resources.
///
/// # `ETag` Handling
///
/// The `etag()` method returns UNQUOTED `ETag` values. dav-server's `davheaders.rs::ETag::from_meta()`
/// wraps our return value in double-quotes. S3 returns `ETags` already quoted (e.g., `"abc123"`).
/// We MUST strip the surrounding quotes before returning, or clients see `""abc123""` (double-quoted),
/// which breaks conditional header round-trips.
#[derive(Debug, Clone)]
pub enum WebDavMetaData {
    /// Root collection (knowledge base list).
    Root {
        /// Server start time for stable `ETag` generation.
        server_start: SystemTime,
    },
    /// Knowledge base collection.
    Kb {
        /// Knowledge base slug.
        slug: String,
        /// Server start time for stable `ETag` generation.
        server_start: SystemTime,
    },
    /// Actual stored object (file).
    Object {
        /// Object metadata from storage backend.
        meta: ObjectMeta,
    },
}

impl WebDavMetaData {
    /// Construct Root metadata, boxed for trait object.
    pub fn root(server_start: SystemTime) -> Box<dyn DavMetaData> {
        Box::new(Self::Root { server_start })
    }

    /// Construct Kb metadata, boxed for trait object.
    pub fn kb(slug: String, server_start: SystemTime) -> Box<dyn DavMetaData> {
        Box::new(Self::Kb { slug, server_start })
    }

    /// Construct Object metadata, boxed for trait object.
    pub fn object(meta: ObjectMeta) -> Box<dyn DavMetaData> {
        Box::new(Self::Object { meta })
    }
}

impl DavMetaData for WebDavMetaData {
    fn len(&self) -> u64 {
        match self {
            Self::Root { .. } | Self::Kb { .. } => 0,
            Self::Object { meta } => meta.size,
        }
    }

    fn modified(&self) -> FsResult<SystemTime> {
        match self {
            Self::Root { server_start } | Self::Kb { server_start, .. } => Ok(*server_start),
            Self::Object { meta } => {
                let secs = u64::try_from(meta.last_modified.unwrap_or(0).max(0)).unwrap_or(0);
                Ok(UNIX_EPOCH + Duration::from_secs(secs))
            }
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self, Self::Root { .. } | Self::Kb { .. })
    }

    /// Return UNQUOTED `ETag` value.
    ///
    /// CRITICAL: dav-server v0.11's `davheaders.rs::ETag::from_meta()` wraps our
    /// return value in `"..."`. S3 returns `ETags` already quoted as `"abc123"`.
    /// We MUST strip the surrounding quotes before returning, or clients see `""abc123""`.
    fn etag(&self) -> Option<String> {
        match self {
            Self::Root { server_start } => {
                let secs = server_start
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Some(format!("nt-root-{secs:x}"))
            }
            Self::Kb { slug, server_start } => {
                let secs = server_start
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Some(format!("nt-kb-{slug}-{secs:x}"))
            }
            Self::Object { meta } => {
                // Strip surrounding quotes from S3 ETag (e.g., "abc123" -> abc123)
                meta.etag
                    .as_deref()
                    .map(|e| e.trim_matches('"').to_string())
            }
        }
    }

    fn is_file(&self) -> bool {
        !self.is_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_root_is_dir() {
        let now = SystemTime::now();
        let meta = WebDavMetaData::Root { server_start: now };
        assert!(meta.is_dir());
        assert!(!meta.is_file());
        assert_eq!(meta.len(), 0);
    }

    #[test]
    fn test_kb_is_dir() {
        let now = SystemTime::now();
        let meta = WebDavMetaData::Kb {
            slug: "my-kb".to_string(),
            server_start: now,
        };
        assert!(meta.is_dir());
        assert!(!meta.is_file());
        assert_eq!(meta.len(), 0);
    }

    #[test]
    fn test_object_is_file() {
        let meta = WebDavMetaData::Object {
            meta: ObjectMeta {
                key: "test.md".to_string(),
                size: 1024,
                last_modified: Some(1_700_000_000),
                content_type: Some("text/markdown".to_string()),
                etag: Some("\"abc123\"".to_string()),
            },
        };
        assert!(!meta.is_dir());
        assert!(meta.is_file());
        assert_eq!(meta.len(), 1024);
    }

    #[test]
    fn test_etag_strips_s3_quotes() {
        let meta = WebDavMetaData::Object {
            meta: ObjectMeta {
                key: "test.md".to_string(),
                size: 100,
                last_modified: Some(1_700_000_000),
                content_type: None,
                etag: Some("\"abc123\"".to_string()),
            },
        };
        assert_eq!(meta.etag(), Some("abc123".to_string()));
    }

    #[test]
    fn test_etag_idempotent_on_unquoted() {
        let meta = WebDavMetaData::Object {
            meta: ObjectMeta {
                key: "test.md".to_string(),
                size: 100,
                last_modified: Some(1_700_000_000),
                content_type: None,
                etag: Some("abc123".to_string()),
            },
        };
        assert_eq!(meta.etag(), Some("abc123".to_string()));
    }

    #[test]
    fn test_etag_root_is_stable() {
        let now = SystemTime::now();
        let meta = WebDavMetaData::Root { server_start: now };
        let etag = meta.etag();
        assert!(etag.is_some());
        assert!(etag.unwrap().starts_with("nt-root-"));
    }

    #[test]
    fn test_modified_uses_server_start_for_virtual() {
        let now = SystemTime::now();
        let meta = WebDavMetaData::Root { server_start: now };
        assert_eq!(meta.modified().unwrap(), now);

        let meta_kb = WebDavMetaData::Kb {
            slug: "test".to_string(),
            server_start: now,
        };
        assert_eq!(meta_kb.modified().unwrap(), now);
    }

    #[test]
    fn test_modified_object_from_last_modified() {
        let meta = WebDavMetaData::Object {
            meta: ObjectMeta {
                key: "test.md".to_string(),
                size: 100,
                last_modified: Some(1_700_000_000),
                content_type: None,
                etag: None,
            },
        };
        let expected = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(meta.modified().unwrap(), expected);
    }

    #[test]
    fn test_etag_none_when_no_etag() {
        let meta = WebDavMetaData::Object {
            meta: ObjectMeta {
                key: "test.md".to_string(),
                size: 100,
                last_modified: Some(1_700_000_000),
                content_type: None,
                etag: None,
            },
        };
        assert_eq!(meta.etag(), None);
    }
}

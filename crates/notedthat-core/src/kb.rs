//! Knowledge base domain structs: `KbManifest`, `Kb`, `ObjectMeta`.

use crate::error::Error;
use crate::slug::{KbSlug, TenantSlug};
use serde::{Deserialize, Serialize};

/// The KB manifest stored as `.notedthat/manifest.json` inside each KB's S3 bucket.
/// See SPECIFICATIONS.md §6.7 for the full schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KbManifest {
    /// Semver of the notedthat server that wrote this manifest.
    pub notedthat_version: String,
    /// Manifest schema version. Must equal [`KbManifest::CURRENT_VERSION`] for M2.
    pub manifest_version: u32,
    /// The tenant this KB belongs to.
    pub tenant_slug: TenantSlug,
    /// The knowledge-base slug.
    pub kb_slug: KbSlug,
    /// Human-readable name for the KB.
    pub display_name: String,
    /// Unix timestamp (seconds) of when this KB was first provisioned.
    pub created_at: i64,
    /// Qdrant collection name (reserved for M4 indexer; optional in M2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qdrant_collection: Option<String>,
}

impl KbManifest {
    /// The only supported manifest schema version in M2.
    pub const CURRENT_VERSION: u32 = 1;

    /// Construct a v1 manifest for a newly provisioned KB.
    pub fn new_v1(tenant: &TenantSlug, kb: &KbSlug, display_name: &str, created_at: i64) -> Self {
        Self {
            notedthat_version: env!("CARGO_PKG_VERSION").to_string(),
            manifest_version: Self::CURRENT_VERSION,
            tenant_slug: tenant.clone(),
            kb_slug: kb.clone(),
            display_name: display_name.to_string(),
            created_at,
            qdrant_collection: None,
        }
    }

    /// Validate that this manifest's `manifest_version` is supported.
    pub fn validate(&self) -> Result<(), Error> {
        if self.manifest_version != Self::CURRENT_VERSION {
            return Err(Error::InvalidInput {
                message: format!(
                    "unsupported manifest_version {}; expected {}",
                    self.manifest_version,
                    Self::CURRENT_VERSION
                ),
            });
        }
        Ok(())
    }
}

/// A knowledge base as seen by the API layer.
#[derive(Debug, Clone, PartialEq)]
pub struct Kb {
    /// The unique slug identifying this KB.
    pub slug: KbSlug,
    /// Human-readable name (≤ 128 Unicode code points per §6.8).
    pub display_name: String,
}

impl Kb {
    /// Maximum length of `display_name` in Unicode code points (§6.8).
    pub const DISPLAY_NAME_MAX: usize = 128;

    /// Construct a [`Kb`], validating the display name.
    pub fn new(slug: KbSlug, display_name: impl Into<String>) -> Result<Self, Error> {
        let display_name = display_name.into();
        if display_name.is_empty() {
            return Err(Error::InvalidInput {
                message: "display_name must not be empty".into(),
            });
        }
        if display_name.chars().count() > Self::DISPLAY_NAME_MAX {
            return Err(Error::InvalidInput {
                message: format!("display_name exceeds {} characters", Self::DISPLAY_NAME_MAX),
            });
        }
        Ok(Self { slug, display_name })
    }
}

/// Metadata about a single stored object, returned by HEAD and LIST responses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObjectMeta {
    /// The object's key (path within the KB bucket, without leading slash).
    pub key: String,
    /// Size in bytes.
    pub size: u64,
    /// Last-modified Unix timestamp (seconds), if the backend provides one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<i64>,
    /// Content-Type as stored in S3 (echoed from the original PUT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Opaque `ETag` from the backend, wrapped in quotes per RFC 7232 §2.3
    /// (e.g., `"\"abc123\""`). Emitted verbatim in HTTP responses.
    /// Never generated locally except by the `InMemoryStorage` mock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slug::{KbSlug, TenantSlug};

    #[test]
    fn test_kb_manifest_serde_round_trip() {
        let manifest = KbManifest {
            notedthat_version: "0.1.0".to_string(),
            manifest_version: 1,
            tenant_slug: TenantSlug::try_new("default").unwrap(),
            kb_slug: KbSlug::try_new("my-notes").unwrap(),
            display_name: "My Notes".to_string(),
            created_at: 1_700_000_000_i64,
            qdrant_collection: None,
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let restored: KbManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.manifest_version, manifest.manifest_version);
        assert_eq!(restored.display_name, manifest.display_name);
        assert_eq!(restored.notedthat_version, manifest.notedthat_version);
    }

    #[test]
    fn test_kb_manifest_json_fixture_all_required_keys() {
        let json = serde_json::json!({
            "notedthat_version": "0.1.0",
            "manifest_version": 1,
            "tenant_slug": "default",
            "kb_slug": "my-notes",
            "display_name": "My Notes",
            "created_at": 1_700_000_000_i64
        });
        let manifest: KbManifest = serde_json::from_value(json).unwrap();
        assert_eq!(manifest.manifest_version, 1);
        assert_eq!(manifest.display_name, "My Notes");
    }

    #[test]
    fn test_kb_manifest_validate_wrong_version() {
        let manifest = KbManifest {
            notedthat_version: "0.1.0".to_string(),
            manifest_version: 99,
            tenant_slug: TenantSlug::try_new("default").unwrap(),
            kb_slug: KbSlug::try_new("notes").unwrap(),
            display_name: "Notes".to_string(),
            created_at: 1_700_000_000_i64,
            qdrant_collection: None,
        };
        assert!(
            manifest.validate().is_err(),
            "manifest_version != 1 should fail validate()"
        );
    }

    #[test]
    fn test_kb_new_valid() {
        let slug = KbSlug::try_new("my-kb").unwrap();
        assert!(Kb::new(slug, "My KB").is_ok());
    }

    #[test]
    fn test_kb_new_empty_display_name() {
        let slug = KbSlug::try_new("my-kb").unwrap();
        assert!(Kb::new(slug, "").is_err());
    }

    #[test]
    fn test_kb_new_display_name_exactly_128_chars() {
        let slug = KbSlug::try_new("my-kb").unwrap();
        let name = "a".repeat(128);
        assert!(Kb::new(slug, name).is_ok());
    }

    #[test]
    fn test_kb_new_display_name_129_chars() {
        let slug = KbSlug::try_new("my-kb").unwrap();
        let name = "a".repeat(129);
        assert!(Kb::new(slug, name).is_err());
    }

    #[test]
    fn test_object_meta_serde_round_trip() {
        let meta = ObjectMeta {
            key: "foo/bar.md".to_string(),
            size: 1024_u64,
            last_modified: Some(1_700_000_000_i64),
            content_type: Some("text/markdown".to_string()),
            etag: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let restored: ObjectMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.key, meta.key);
        assert_eq!(restored.size, meta.size);
        assert_eq!(restored.last_modified, meta.last_modified);
        assert_eq!(restored.content_type, meta.content_type);
        assert_eq!(restored.etag, meta.etag);
    }

    #[test]
    fn test_object_meta_optional_fields_none() {
        let meta = ObjectMeta {
            key: "foo.md".to_string(),
            size: 0_u64,
            last_modified: None,
            content_type: None,
            etag: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let restored: ObjectMeta = serde_json::from_str(&json).unwrap();
        assert!(restored.last_modified.is_none());
        assert!(restored.content_type.is_none());
        assert!(restored.etag.is_none());
    }
}

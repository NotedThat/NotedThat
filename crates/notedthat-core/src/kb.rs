//! Knowledge base domain structs: `KbManifest`, `Kb`, `ObjectMeta`.

use crate::access::AccessPolicy;
use crate::error::Error;
use crate::slug::{KbSlug, TenantSlug};
use serde::{Deserialize, Serialize};

/// Embedding configuration for a knowledge base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEmbedding {
    /// The embedding model name (e.g., "text-embedding-3-small").
    pub model: String,
    /// Dimensionality of the embedding vectors.
    pub dimensions: u32,
    /// Optional hint for the embedding endpoint URL (e.g., for provisioner cross-check).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_url_hint: Option<String>,
}

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
    /// What this knowledge base is for, in one line, for agents choosing
    /// where to search (#98). Plain metadata written by an operator, never
    /// derived from the objects; validated by [`KbManifest::validate`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Unix timestamp (seconds) of when this KB was first provisioned.
    pub created_at: i64,
    /// Qdrant collection name (reserved for M4 indexer; optional in M2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qdrant_collection: Option<String>,
    /// Embedding configuration (optional, for provisioner cross-check).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<ManifestEmbedding>,
    /// Who may do what in this knowledge base, and where (D51).
    ///
    /// An absent field means the pre-access-rules default: the credential
    /// holder may do everything, anonymous callers nothing. Every manifest
    /// written before this model existed is in that state, so upgrading leaves
    /// credentialed access exactly as it was.
    #[serde(default = "AccessPolicy::signed_in_full")]
    pub access: AccessPolicy,
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
            description: None,
            created_at,
            qdrant_collection: None,
            embedding: None,
            access: AccessPolicy::signed_in_full(),
        }
    }

    /// Maximum length of `description` in Unicode code points.
    pub const DESCRIPTION_MAX_CHARS: usize = 500;

    /// The object key the manifest lives under, in every knowledge base.
    pub const KEY: &'static str = ".notedthat/manifest.json";

    /// Validate that this manifest's `manifest_version` is supported, its
    /// `description` fits the limits, and its access rules are sound.
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
        if let Some(description) = &self.description {
            validate_description(description)?;
        }
        self.access.validate()?;
        Ok(())
    }

    /// The caller-facing facts about this knowledge base: what listings show.
    #[must_use]
    pub fn details(&self) -> KbDetails {
        KbDetails {
            display_name: self.display_name.clone(),
            description: self.description.clone(),
        }
    }
}

/// A `description` is an inline label an agent reads before choosing where to
/// search, so it is one line, non-empty, and bounded. Control characters —
/// newlines and tabs included — are refused rather than normalised, so the
/// manifest an operator wrote is exactly what clients see.
fn validate_description(description: &str) -> Result<(), Error> {
    if description.trim().is_empty() {
        return Err(Error::InvalidInput {
            message: "description must not be empty; omit the field instead".into(),
        });
    }
    if description.chars().count() > KbManifest::DESCRIPTION_MAX_CHARS {
        return Err(Error::InvalidInput {
            message: format!(
                "description exceeds {} characters",
                KbManifest::DESCRIPTION_MAX_CHARS
            ),
        });
    }
    if description.chars().any(char::is_control) {
        return Err(Error::InvalidInput {
            message: "description must be a single line without control characters".into(),
        });
    }
    Ok(())
}

/// What a listing shows about a knowledge base besides its slug: the
/// manifest's `display_name` and, when set, its `description`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbDetails {
    /// Human-readable name for the KB.
    pub display_name: String,
    /// One-line statement of what the knowledge base is for, when the
    /// manifest carries one.
    pub description: Option<String>,
}

impl KbDetails {
    /// The details of a knowledge base whose manifest says nothing beyond its
    /// slug: the slug doubles as the display name, as `provision_kbs` writes it.
    #[must_use]
    pub fn from_slug(slug: &str) -> Self {
        Self {
            display_name: slug.to_string(),
            description: None,
        }
    }
}

/// The details a deployment gets when no manifest says more than the slug:
/// what startup provisioning writes for a fresh knowledge base, and the
/// sensible starting point anywhere a details map has to be built without one.
#[must_use]
pub fn slug_kb_details(
    declared: &std::collections::BTreeMap<String, KbSlug>,
) -> std::collections::BTreeMap<String, KbDetails> {
    declared
        .keys()
        .map(|slug| (slug.clone(), KbDetails::from_slug(slug)))
        .collect()
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
            description: None,
            created_at: 1_700_000_000_i64,
            qdrant_collection: None,
            embedding: None,
            access: AccessPolicy::signed_in_full(),
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
            description: None,
            created_at: 1_700_000_000_i64,
            qdrant_collection: None,
            embedding: None,
            access: AccessPolicy::signed_in_full(),
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

    #[test]
    fn test_kb_manifest_backwards_compat_no_embedding() {
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
        assert!(
            manifest.embedding.is_none(),
            "embedding should default to None"
        );
    }

    #[test]
    fn test_kb_manifest_embedding_round_trip() {
        let embedding = ManifestEmbedding {
            model: "text-embedding-3-small".to_string(),
            dimensions: 1536,
            endpoint_url_hint: Some("https://api.openai.com/v1/embeddings".to_string()),
        };
        let manifest = KbManifest {
            notedthat_version: "0.1.0".to_string(),
            manifest_version: 1,
            tenant_slug: TenantSlug::try_new("default").unwrap(),
            kb_slug: KbSlug::try_new("my-notes").unwrap(),
            display_name: "My Notes".to_string(),
            description: None,
            created_at: 1_700_000_000_i64,
            qdrant_collection: None,
            embedding: Some(embedding.clone()),
            access: AccessPolicy::signed_in_full(),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let restored: KbManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.embedding, Some(embedding));
    }

    #[test]
    fn test_kb_manifest_embedding_endpoint_url_hint_omitted() {
        let embedding = ManifestEmbedding {
            model: "text-embedding-3-small".to_string(),
            dimensions: 1536,
            endpoint_url_hint: None,
        };
        let manifest = KbManifest {
            notedthat_version: "0.1.0".to_string(),
            manifest_version: 1,
            tenant_slug: TenantSlug::try_new("default").unwrap(),
            kb_slug: KbSlug::try_new("my-notes").unwrap(),
            display_name: "My Notes".to_string(),
            description: None,
            created_at: 1_700_000_000_i64,
            qdrant_collection: None,
            embedding: Some(embedding),
            access: AccessPolicy::signed_in_full(),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(
            !json.contains("endpoint_url_hint"),
            "endpoint_url_hint should not be serialized when None"
        );
        let restored: KbManifest = serde_json::from_str(&json).unwrap();
        assert!(restored.embedding.is_some());
        assert!(
            restored
                .embedding
                .as_ref()
                .unwrap()
                .endpoint_url_hint
                .is_none()
        );
    }

    fn manifest_with_description(description: Option<&str>) -> KbManifest {
        let mut manifest = KbManifest::new_v1(
            &TenantSlug::try_new("default").unwrap(),
            &KbSlug::try_new("notes").unwrap(),
            "Notes",
            1_700_000_000_i64,
        );
        manifest.description = description.map(str::to_string);
        manifest
    }

    #[test]
    fn description_round_trips_and_is_omitted_when_absent() {
        let with = manifest_with_description(Some("Engineering notes, ADRs and minutes."));
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(json["description"], "Engineering notes, ADRs and minutes.");
        let restored: KbManifest = serde_json::from_value(json).unwrap();
        assert_eq!(
            restored.description.as_deref(),
            Some("Engineering notes, ADRs and minutes.")
        );
        assert_eq!(
            restored.details(),
            KbDetails {
                display_name: "Notes".into(),
                description: Some("Engineering notes, ADRs and minutes.".into()),
            }
        );

        let without = manifest_with_description(None);
        let json = serde_json::to_value(&without).unwrap();
        assert!(
            json.get("description").is_none(),
            "absent description is not serialised as null"
        );
        assert!(without.validate().is_ok());
        assert_eq!(without.details(), KbDetails::from_slug("Notes"));
    }

    #[test]
    fn description_within_limits_is_valid() {
        let at_limit = "x".repeat(KbManifest::DESCRIPTION_MAX_CHARS);
        assert!(
            manifest_with_description(Some(&at_limit))
                .validate()
                .is_ok()
        );
        // Code points, not bytes: 500 multi-byte characters still fit.
        let unicode = "é".repeat(KbManifest::DESCRIPTION_MAX_CHARS);
        assert!(manifest_with_description(Some(&unicode)).validate().is_ok());
    }

    #[test]
    fn description_outside_limits_is_refused() {
        let too_long = "x".repeat(KbManifest::DESCRIPTION_MAX_CHARS + 1);
        for (case, description) in [
            ("empty", String::new()),
            ("whitespace only", "   ".into()),
            ("too long", too_long),
            ("newline", "two\nlines".into()),
            ("tab", "a\tb".into()),
        ] {
            let err = manifest_with_description(Some(&description))
                .validate()
                .expect_err(case);
            assert!(matches!(err, Error::InvalidInput { .. }), "{case}: {err:?}");
        }
    }
}

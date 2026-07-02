//! Knowledge base domain structs: `KbManifest`, `Kb`, `ObjectMeta`.

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
        assert!(manifest.validate().is_err(), "manifest_version != 1 should fail validate()");
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
        };
        let json = serde_json::to_string(&meta).unwrap();
        let restored: ObjectMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.key, meta.key);
        assert_eq!(restored.size, meta.size);
        assert_eq!(restored.last_modified, meta.last_modified);
        assert_eq!(restored.content_type, meta.content_type);
    }

    #[test]
    fn test_object_meta_optional_fields_none() {
        let meta = ObjectMeta {
            key: "foo.md".to_string(),
            size: 0_u64,
            last_modified: None,
            content_type: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let restored: ObjectMeta = serde_json::from_str(&json).unwrap();
        assert!(restored.last_modified.is_none());
        assert!(restored.content_type.is_none());
    }
}

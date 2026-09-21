//! Bucket name derivation and validation.

use crate::error::Error;
use crate::slug::{KbSlug, TenantSlug};

/// Maximum S3 bucket name length (DNS constraint). See §6.6 and D20.
pub const BUCKET_NAME_MAX: usize = 63;

/// The prefix prepended to every `NotedThat` bucket name.
pub const BUCKET_NAME_PREFIX: &str = "nt-";

/// Derive the S3 bucket name for a KB in a deterministic, infallible way.
///
/// The format is `nt-{tenant_slug}-{kb_slug}`. Call [`validate_bucket_name`]
/// once at startup to confirm the name fits within 63 characters.
///
/// The encoding is injective: a [`TenantSlug`] contains no hyphen, so the first
/// hyphen after the prefix always ends the tenant and the rest is the knowledge
/// base. Two distinct `(tenant, kb)` pairs therefore never share a bucket — or,
/// under the `fs` backend, a directory (D49, D57).
#[must_use]
pub fn derive_bucket_name(tenant: &TenantSlug, kb: &KbSlug) -> String {
    format!("{BUCKET_NAME_PREFIX}{}-{}", tenant.as_str(), kb.as_str())
}

/// Validate that the derived bucket name fits within the S3 DNS-label length
/// limit (63 characters). See §6.6 and D39 (fail-fast startup).
///
/// Call this once per declared KB during server startup, before any storage
/// operation happens.
pub fn validate_bucket_name(tenant: &TenantSlug, kb: &KbSlug) -> Result<(), Error> {
    let name = derive_bucket_name(tenant, kb);
    if name.len() > BUCKET_NAME_MAX {
        let len = name.len();
        return Err(Error::BucketNameTooLong { name, len });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slug::{KbSlug, TenantSlug};

    #[test]
    fn test_derive_bucket_name_default_notes() {
        let tenant = TenantSlug::try_new("default").unwrap();
        let kb = KbSlug::try_new("notes").unwrap();
        let name = derive_bucket_name(&tenant, &kb);
        assert_eq!(name, "nt-default-notes");
    }

    #[test]
    fn test_derive_bucket_name_single_chars() {
        let tenant = TenantSlug::try_new("a").unwrap();
        let kb = KbSlug::try_new("b").unwrap();
        let name = derive_bucket_name(&tenant, &kb);
        assert_eq!(name, "nt-a-b");
    }

    #[test]
    fn test_derive_bucket_name_is_string_not_result() {
        let tenant = TenantSlug::try_new("default").unwrap();
        let kb = KbSlug::try_new("notes").unwrap();
        let name: String = derive_bucket_name(&tenant, &kb);
        assert!(!name.is_empty());
    }

    #[test]
    fn test_derive_bucket_name_is_deterministic() {
        let tenant = TenantSlug::try_new("default").unwrap();
        let kb = KbSlug::try_new("notes").unwrap();
        let first = derive_bucket_name(&tenant, &kb);
        let second = derive_bucket_name(&tenant, &kb);
        assert_eq!(first, second);
    }

    /// The collision from #70: `(acme, my-notes)` and `(acme-my, notes)` would both
    /// derive `nt-acme-my-notes`. The second pair cannot be constructed, and every
    /// pair that can be gets a name of its own.
    #[test]
    fn test_derive_bucket_name_is_injective() {
        assert!(
            TenantSlug::try_new("acme-my").is_err(),
            "a hyphenated tenant slug is what would make the encoding ambiguous"
        );

        let tenants = ["a", "ab", "acme", "default"];
        let kbs = ["b", "a-b", "ab-c", "b-a", "notes", "my-notes", "my--notes"];
        let mut seen = std::collections::HashSet::new();
        for tenant in tenants {
            let tenant = TenantSlug::try_new(tenant).unwrap();
            for kb in kbs {
                let kb = KbSlug::try_new(kb).unwrap();
                assert!(
                    seen.insert(derive_bucket_name(&tenant, &kb)),
                    "({tenant}, {kb}) collides with an earlier pair"
                );
            }
        }
        assert_eq!(seen.len(), tenants.len() * kbs.len());
    }

    #[test]
    fn test_validate_bucket_name_ok_at_63_chars() {
        let tenant = TenantSlug::try_new("a".repeat(20)).unwrap();
        let kb = KbSlug::try_new("b".repeat(39)).unwrap();
        let result = validate_bucket_name(&tenant, &kb);
        assert!(
            result.is_ok(),
            "63-char bucket name should be Ok, got: {result:?}"
        );
    }

    #[test]
    fn test_validate_bucket_name_err_at_64_chars() {
        let tenant = TenantSlug::try_new("a".repeat(20)).unwrap();
        let kb = KbSlug::try_new("b".repeat(40)).unwrap();
        let result = validate_bucket_name(&tenant, &kb);
        assert!(result.is_err(), "64-char bucket name should be Err");
    }

    #[test]
    fn test_validate_bucket_name_err_has_name_and_len() {
        use crate::error::Error;
        let tenant = TenantSlug::try_new("a".repeat(20)).unwrap();
        let kb = KbSlug::try_new("b".repeat(40)).unwrap();
        match validate_bucket_name(&tenant, &kb) {
            Err(Error::BucketNameTooLong { name, len }) => {
                assert_eq!(len, 64);
                assert_eq!(name, format!("nt-{}-{}", "a".repeat(20), "b".repeat(40)));
            }
            other => panic!("Expected BucketNameTooLong, got: {other:?}"),
        }
    }

    #[test]
    fn test_validate_bucket_name_ok_short() {
        let tenant = TenantSlug::try_new("a").unwrap();
        let kb = KbSlug::try_new("b").unwrap();
        assert!(validate_bucket_name(&tenant, &kb).is_ok());
    }
}

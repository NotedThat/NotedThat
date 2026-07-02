//! Bucket name derivation and validation.

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

    #[test]
    fn test_validate_bucket_name_ok_at_63_chars() {
        let tenant = TenantSlug::try_new(&"a".repeat(20)).unwrap();
        let kb = KbSlug::try_new(&"b".repeat(39)).unwrap();
        let result = validate_bucket_name(&tenant, &kb);
        assert!(result.is_ok(), "63-char bucket name should be Ok, got: {result:?}");
    }

    #[test]
    fn test_validate_bucket_name_err_at_64_chars() {
        let tenant = TenantSlug::try_new(&"a".repeat(20)).unwrap();
        let kb = KbSlug::try_new(&"b".repeat(40)).unwrap();
        let result = validate_bucket_name(&tenant, &kb);
        assert!(result.is_err(), "64-char bucket name should be Err");
    }

    #[test]
    fn test_validate_bucket_name_err_has_name_and_len() {
        use crate::error::Error;
        let tenant = TenantSlug::try_new(&"a".repeat(20)).unwrap();
        let kb = KbSlug::try_new(&"b".repeat(40)).unwrap();
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

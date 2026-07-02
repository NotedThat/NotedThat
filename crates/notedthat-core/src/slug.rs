//! `KbSlug` and `TenantSlug` — validated lowercase-alphanumeric-hyphen identifiers.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kb_slug_valid_simple() {
        assert!(KbSlug::try_new("my-notes").is_ok());
    }

    #[test]
    fn test_kb_slug_valid_alphanumeric_hyphen() {
        assert!(KbSlug::try_new("valid-1").is_ok());
    }

    #[test]
    fn test_kb_slug_valid_single_char() {
        assert!(KbSlug::try_new("a").is_ok());
    }

    #[test]
    fn test_kb_slug_valid_max_length_40() {
        let s = "a".repeat(40);
        assert!(KbSlug::try_new(s.as_str()).is_ok());
    }

    #[test]
    fn test_kb_slug_err_empty() {
        assert!(KbSlug::try_new("").is_err());
    }

    #[test]
    fn test_kb_slug_err_leading_hyphen() {
        assert!(KbSlug::try_new("-leading").is_err());
    }

    #[test]
    fn test_kb_slug_err_trailing_hyphen() {
        assert!(KbSlug::try_new("trailing-").is_err());
    }

    #[test]
    fn test_kb_slug_err_uppercase() {
        assert!(KbSlug::try_new("UPPER").is_err());
    }

    #[test]
    fn test_kb_slug_err_space() {
        assert!(KbSlug::try_new("with space").is_err());
    }

    #[test]
    fn test_kb_slug_err_too_long_41() {
        let s = "a".repeat(41);
        assert!(KbSlug::try_new(s.as_str()).is_err());
    }

    #[test]
    fn test_kb_slug_err_underscore() {
        assert!(KbSlug::try_new("with_underscore").is_err());
    }

    #[test]
    fn test_tenant_slug_valid_default() {
        assert!(TenantSlug::try_new("default").is_ok());
    }

    #[test]
    fn test_tenant_slug_valid_max_length_20() {
        let s = "a".repeat(20);
        assert!(TenantSlug::try_new(s.as_str()).is_ok());
    }

    #[test]
    fn test_tenant_slug_err_too_long_21() {
        let s = "a".repeat(21);
        assert!(TenantSlug::try_new(s.as_str()).is_err());
    }

    #[test]
    fn test_tenant_slug_err_empty() {
        assert!(TenantSlug::try_new("").is_err());
    }

    #[test]
    fn test_kb_slug_as_ref_str() {
        let slug = KbSlug::try_new("my-notes").unwrap();
        assert_eq!(slug.as_ref(), "my-notes");
    }

    #[test]
    fn test_kb_slug_display() {
        let slug = KbSlug::try_new("my-notes").unwrap();
        assert_eq!(slug.to_string(), "my-notes");
    }

    #[test]
    fn test_tenant_slug_as_ref_str() {
        let slug = TenantSlug::try_new("default").unwrap();
        assert_eq!(slug.as_ref(), "default");
    }

    #[test]
    fn test_tenant_slug_display() {
        let slug = TenantSlug::try_new("default").unwrap();
        assert_eq!(slug.to_string(), "default");
    }

    #[test]
    fn test_kb_slug_serde_round_trip() {
        let slug = KbSlug::try_new("my-notes").unwrap();
        let json = serde_json::to_string(&slug).unwrap();
        let restored: KbSlug = serde_json::from_str(&json).unwrap();
        assert_eq!(slug, restored);
    }

    #[test]
    fn test_tenant_slug_serde_round_trip() {
        let slug = TenantSlug::try_new("default").unwrap();
        let json = serde_json::to_string(&slug).unwrap();
        let restored: TenantSlug = serde_json::from_str(&json).unwrap();
        assert_eq!(slug, restored);
    }

    #[test]
    fn test_kb_slug_deserialize_rejects_uppercase() {
        let result = serde_json::from_str::<KbSlug>("\"UPPER\"");
        assert!(result.is_err(), "Deserializing uppercase slug should fail");
    }

    #[test]
    fn test_tenant_slug_deserialize_rejects_too_long() {
        let s = format!("\"{}\"", "a".repeat(21));
        let result = serde_json::from_str::<TenantSlug>(&s);
        assert!(result.is_err(), "Deserializing 21-char tenant slug should fail");
    }

    #[test]
    fn test_kb_slug_equality() {
        let a = KbSlug::try_new("notes").unwrap();
        let b = KbSlug::try_new("notes").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn test_kb_slug_inequality() {
        let a = KbSlug::try_new("notes").unwrap();
        let b = KbSlug::try_new("other").unwrap();
        assert_ne!(a, b);
    }
}

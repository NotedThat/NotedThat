//! `KbSlug` and `TenantSlug` — validated lowercase-alphanumeric-hyphen identifiers.

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use crate::error::Error;

/// A knowledge-base slug: `[a-z0-9-]{1,40}` (ASCII only, no leading/trailing hyphen).
/// See SPECIFICATIONS.md §6.8 and decision D24.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct KbSlug(String);

impl KbSlug {
    /// Maximum length of a `KbSlug` (40 characters per §6.8).
    pub const MAX_LEN: usize = 40;

    /// Validate and construct a `KbSlug`.
    pub fn try_new(input: impl Into<String>) -> Result<Self, Error> {
        let s = input.into();
        validate_slug(&s, Self::MAX_LEN)?;
        Ok(Self(s))
    }

    /// Returns the slug as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for KbSlug {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for KbSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<&str> for KbSlug {
    type Error = Error;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl TryFrom<String> for KbSlug {
    type Error = Error;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl<'de> Deserialize<'de> for KbSlug {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::try_new(s).map_err(serde::de::Error::custom)
    }
}

/// A tenant slug: `[a-z0-9-]{1,20}` (ASCII only, no leading/trailing hyphen).
/// See SPECIFICATIONS.md §6.6 and decision D24.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct TenantSlug(String);

impl TenantSlug {
    /// Maximum length of a `TenantSlug` (20 characters per §6.6).
    pub const MAX_LEN: usize = 20;

    /// Validate and construct a `TenantSlug`.
    pub fn try_new(input: impl Into<String>) -> Result<Self, Error> {
        let s = input.into();
        validate_slug(&s, Self::MAX_LEN)?;
        Ok(Self(s))
    }

    /// Returns the slug as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TenantSlug {
    /// Returns `TenantSlug("default")`. This is the hardcoded tenant for M2 per Metis directive.
    ///
    /// # Panics
    ///
    /// Never panics — `"default"` is always valid per §6.6. This invariant is
    /// verified by the `default_never_panics` unit test.
    fn default() -> Self {
        Self::try_new("default").expect("'default' is always a valid TenantSlug")
    }
}

impl AsRef<str> for TenantSlug {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<&str> for TenantSlug {
    type Error = Error;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl TryFrom<String> for TenantSlug {
    type Error = Error;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl<'de> Deserialize<'de> for TenantSlug {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::try_new(s).map_err(serde::de::Error::custom)
    }
}

fn validate_slug(s: &str, max_len: usize) -> Result<(), Error> {
    if s.is_empty() {
        return Err(Error::InvalidInput { message: "slug must not be empty".into() });
    }
    if s.len() > max_len {
        return Err(Error::InvalidInput {
            message: format!("slug must be at most {max_len} characters, got {}", s.len()),
        });
    }
    if s.starts_with('-') {
        return Err(Error::InvalidInput { message: "slug must not start with a hyphen".into() });
    }
    if s.ends_with('-') {
        return Err(Error::InvalidInput { message: "slug must not end with a hyphen".into() });
    }
    for ch in s.chars() {
        match ch {
            'a'..='z' | '0'..='9' | '-' => {}
            _ => {
                return Err(Error::InvalidInput {
                    message: format!(
                        "slug contains invalid character '{ch}'; only [a-z0-9-] allowed"
                    ),
                })
            }
        }
    }
    Ok(())
}

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

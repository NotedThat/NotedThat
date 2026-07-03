//! `ObjectKey` — validated S3 object key stored in Qdrant payload.

use crate::error::Error;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// A validated S3 object key stored in Qdrant payload.
///
/// Rules: non-empty, no leading `/`, no NUL bytes, no `..` or `.` path segments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ObjectKey(String);

impl ObjectKey {
    /// Validate and construct an `ObjectKey`.
    pub fn try_new(input: impl Into<String>) -> Result<Self, Error> {
        let s = input.into();
        if s.is_empty() {
            return Err(Error::InvalidInput {
                message: "object key must not be empty".into(),
            });
        }
        if s.starts_with('/') {
            return Err(Error::InvalidInput {
                message: "object key must not start with '/'".into(),
            });
        }
        if s.contains('\0') {
            return Err(Error::InvalidInput {
                message: "object key must not contain NUL bytes".into(),
            });
        }
        // Reject path traversal segments: ".." and "."
        for segment in s.split('/') {
            if segment == ".." || segment == "." {
                return Err(Error::InvalidInput {
                    message: format!("object key contains invalid path segment: '{segment}'"),
                });
            }
        }
        Ok(Self(s))
    }

    /// Return the key as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ObjectKey {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<&str> for ObjectKey {
    type Error = Error;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::try_new(s)
    }
}

impl TryFrom<String> for ObjectKey {
    type Error = Error;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::try_new(s)
    }
}

impl<'de> Deserialize<'de> for ObjectKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::try_new(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_key() {
        assert!(ObjectKey::try_new("docs/rfc/7231.md").is_ok());
    }

    #[test]
    fn valid_simple_key() {
        assert!(ObjectKey::try_new("hello.md").is_ok());
    }

    #[test]
    fn empty_rejected() {
        assert!(matches!(ObjectKey::try_new(""), Err(Error::InvalidInput { .. })));
    }

    #[test]
    fn leading_slash_rejected() {
        assert!(matches!(ObjectKey::try_new("/foo"), Err(Error::InvalidInput { .. })));
    }

    #[test]
    fn dotdot_segment_rejected() {
        assert!(matches!(
            ObjectKey::try_new("docs/../etc/passwd"),
            Err(Error::InvalidInput { .. })
        ));
    }

    #[test]
    fn dot_segment_rejected() {
        assert!(matches!(
            ObjectKey::try_new("docs/./foo.md"),
            Err(Error::InvalidInput { .. })
        ));
    }

    #[test]
    fn nul_byte_rejected() {
        assert!(matches!(
            ObjectKey::try_new("foo\0bar"),
            Err(Error::InvalidInput { .. })
        ));
    }

    #[test]
    fn serde_round_trip() {
        let key = ObjectKey::try_new("docs/rfc/7231.md").unwrap();
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, "\"docs/rfc/7231.md\"");
        let back: ObjectKey = serde_json::from_str(&json).unwrap();
        assert_eq!(back, key);
    }

    #[test]
    fn deserialize_rejects_invalid() {
        let result: Result<ObjectKey, _> = serde_json::from_str("\"/leading-slash\"");
        assert!(result.is_err());
    }

    #[test]
    fn try_from_str() {
        assert!(ObjectKey::try_from("notes/note.md").is_ok());
    }

    #[test]
    fn try_from_string() {
        assert!(ObjectKey::try_from("notes/note.md".to_string()).is_ok());
    }

    #[test]
    fn as_str_and_display() {
        let key = ObjectKey::try_new("docs/README.md").unwrap();
        assert_eq!(key.as_str(), "docs/README.md");
        assert_eq!(key.to_string(), "docs/README.md");
    }

    #[test]
    fn send_sync_bounds() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ObjectKey>();
    }
}

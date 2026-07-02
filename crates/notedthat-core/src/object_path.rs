//! `ObjectPath` — normalized object storage key with D40 validation rules.

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use crate::error::Error;

/// A normalized object path within a knowledge-base bucket.
///
/// Rules (D40, §6.12 path normalization):
/// - One leading `/` is stripped if present.
/// - Empty paths and paths that are only `/` are rejected.
/// - Empty segments (from `//` or trailing `/`) are rejected.
/// - `.` and `..` segments are rejected (no resolution — just rejection).
/// - Backslash (`\`) and NUL (`\0`) characters are rejected.
/// - Case and Unicode are preserved verbatim.
/// - Spaces are valid (S3 permits them).
///
/// The stored form has no leading slash and uses `/` as the separator.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ObjectPath(String);

impl ObjectPath {
    /// Validate and construct an [`ObjectPath`] from a string slice.
    pub fn try_from_str(input: &str) -> Result<Self, Error> {
        let s = input.strip_prefix('/').unwrap_or(input);

        if s.is_empty() {
            return Err(Error::InvalidInput { message: "path must not be empty".into() });
        }
        if s.contains('\\') {
            return Err(Error::InvalidInput {
                message: "path must not contain backslash".into(),
            });
        }
        if s.contains('\0') {
            return Err(Error::InvalidInput {
                message: "path must not contain NUL byte".into(),
            });
        }
        for segment in s.split('/') {
            if segment.is_empty() {
                return Err(Error::InvalidInput {
                    message: "path must not contain empty segments (double slashes or trailing slash)".into(),
                });
            }
            if segment == "." || segment == ".." {
                return Err(Error::InvalidInput {
                    message: "path must not contain '.' or '..' segments".into(),
                });
            }
        }
        Ok(Self(s.to_string()))
    }

    /// Returns the normalized path as a `&str` (no leading slash).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ObjectPath {
    type Error = Error;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::try_from_str(value)
    }
}

impl TryFrom<String> for ObjectPath {
    type Error = Error;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from_str(&value)
    }
}

impl AsRef<str> for ObjectPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ObjectPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for ObjectPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::try_from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_from_simple_no_leading_slash() {
        let p = ObjectPath::try_from("foo/bar.md").unwrap();
        assert_eq!(p.as_ref(), "foo/bar.md");
    }

    #[test]
    fn test_try_from_strips_one_leading_slash() {
        let p = ObjectPath::try_from("/foo/bar.md").unwrap();
        assert_eq!(p.as_ref(), "foo/bar.md");
    }

    #[test]
    fn test_try_from_case_preserved() {
        let p = ObjectPath::try_from("FooBar/BAZ.md").unwrap();
        assert_eq!(p.as_ref(), "FooBar/BAZ.md");
    }

    #[test]
    fn test_try_from_unicode_preserved() {
        let p = ObjectPath::try_from("русский.md").unwrap();
        assert_eq!(p.as_ref(), "русский.md");
    }

    #[test]
    fn test_try_from_spaces_valid() {
        let p = ObjectPath::try_from("hello world.md").unwrap();
        assert_eq!(p.as_ref(), "hello world.md");
    }

    #[test]
    fn test_try_from_err_double_leading_slash() {
        assert!(ObjectPath::try_from("//foo/bar.md").is_err());
    }

    #[test]
    fn test_try_from_err_empty() {
        assert!(ObjectPath::try_from("").is_err());
    }

    #[test]
    fn test_try_from_err_slash_only_empty_after_strip() {
        assert!(ObjectPath::try_from("/").is_err());
    }

    #[test]
    fn test_try_from_err_trailing_slash_empty_segment() {
        assert!(ObjectPath::try_from("foo/").is_err());
    }

    #[test]
    fn test_try_from_err_double_slash_middle() {
        assert!(ObjectPath::try_from("foo//bar").is_err());
    }

    #[test]
    fn test_try_from_err_dot_segment_single() {
        assert!(ObjectPath::try_from(".").is_err());
    }

    #[test]
    fn test_try_from_err_dot_segment_prefix() {
        assert!(ObjectPath::try_from("./foo").is_err());
    }

    #[test]
    fn test_try_from_err_double_dot_segment() {
        assert!(ObjectPath::try_from("..").is_err());
    }

    #[test]
    fn test_try_from_err_double_dot_prefix() {
        assert!(ObjectPath::try_from("../foo").is_err());
    }

    #[test]
    fn test_try_from_err_double_dot_middle() {
        assert!(ObjectPath::try_from("foo/../bar").is_err());
    }

    #[test]
    fn test_try_from_err_backslash() {
        assert!(ObjectPath::try_from("foo\\bar").is_err());
    }

    #[test]
    fn test_try_from_err_nul_byte() {
        assert!(ObjectPath::try_from("foo\x00bar").is_err());
    }

    #[test]
    fn test_as_ref_gives_normalized_no_slash() {
        let p = ObjectPath::try_from("/some/path.md").unwrap();
        let s: &str = p.as_ref();
        assert!(!s.starts_with('/'));
        assert_eq!(s, "some/path.md");
    }

    #[test]
    fn test_display_gives_normalized_form() {
        let p = ObjectPath::try_from("/foo/bar.md").unwrap();
        assert_eq!(p.to_string(), "foo/bar.md");
    }

    #[test]
    fn test_try_from_owned_string() {
        let s = String::from("foo/bar.md");
        let p = ObjectPath::try_from(s).unwrap();
        assert_eq!(p.as_ref(), "foo/bar.md");
    }
}

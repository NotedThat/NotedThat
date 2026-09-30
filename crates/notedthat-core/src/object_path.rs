//! `ObjectPath` — normalized object storage key with D40 validation rules.

use crate::error::Error;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// Returns whether a decoded, knowledge-base-relative path is in the internal namespace.
///
/// The path must have no leading slash. The exact `.notedthat` segment and its
/// descendants are internal; similarly named or nested segments are not.
/// This predicate does not normalize, decode, or validate paths.
pub fn is_internal_path(path: &str) -> bool {
    path == ".notedthat" || path.starts_with(".notedthat/")
}

/// Keys that name a knowledge-base-level API route rather than an object.
///
/// Each is a static route beside the object catch-all
/// `/knowledgebases/{kb_slug}/{*object_path}`, and the router prefers the
/// static one, so an object stored under exactly one of these keys could never
/// be read, written or deleted over HTTP or MCP (#279). Every surface refuses
/// to create or address an *object* under one of them, through
/// [`ObjectPath::try_object_key`] or [`ObjectPath::is_reserved`], and the
/// indexer takes one out of the search index rather than indexing it.
///
/// Only the exact key is reserved, never a folder: `search/results.md` is an
/// ordinary object, so the folder `search/` is an ordinary folder, and
/// [`ObjectPath::try_from_str`] (which parses folder and listing prefixes too)
/// accepts all four. Nested keys (`notes/index`) and look-alikes (`index.md`)
/// are ordinary objects.
///
/// The api-http router tests check this list against its route table.
pub const RESERVED_KEYS: &[&str] = &["index", "index/reconcile", "events", "search"];

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
/// A [`RESERVED_KEYS`] entry is a valid `ObjectPath`, because it can name a
/// folder; [`ObjectPath::try_object_key`] also refuses it as an object key.
///
/// The stored form has no leading slash and uses `/` as the separator.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[cfg_attr(
    feature = "openapi",
    schema(description = "An object's path within the knowledge base, without a leading `/`.")
)]
pub struct ObjectPath(String);

impl ObjectPath {
    /// Validate and construct an [`ObjectPath`] from a string slice.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] when the path, after its optional leading `/`, is
    /// empty or contains a backslash, a NUL byte, an empty segment, or a `.` or `..`
    /// segment.
    pub fn try_from_str(input: &str) -> Result<Self, Error> {
        let s = input.strip_prefix('/').unwrap_or(input);

        if s.is_empty() {
            return Err(Error::InvalidInput {
                message: "path must not be empty".into(),
            });
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
                    message:
                        "path must not contain empty segments (double slashes or trailing slash)"
                            .into(),
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

    /// Validate and construct the key of an object to create or address.
    ///
    /// [`ObjectPath::try_from_str`], plus refusing the [`RESERVED_KEYS`],
    /// compared exactly and case-sensitively after the leading `/` is
    /// stripped. Folder and listing prefixes go through `try_from_str`
    /// instead, so a folder named `search` stays usable.
    pub fn try_object_key(input: &str) -> Result<Self, Error> {
        let path = Self::try_from_str(input)?;
        if path.is_reserved() {
            return Err(Error::InvalidInput {
                message: format!(
                    "path '{path}' is reserved: it names an API route of the knowledge base"
                ),
            });
        }
        Ok(path)
    }

    /// Whether this is one of the [`RESERVED_KEYS`], which no object may have.
    #[must_use]
    pub fn is_reserved(&self) -> bool {
        RESERVED_KEYS.contains(&self.0.as_str())
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
    fn internal_path_when_root_namespace_or_descendant() {
        for path in [
            ".notedthat",
            ".notedthat/",
            ".notedthat/config.json",
            ".notedthat/nested/file",
        ] {
            assert!(is_internal_path(path), "{path}");
        }
    }

    #[test]
    fn public_path_when_outside_root_namespace() {
        for path in [
            "",
            "/",
            ".notedthat-other",
            ".notedthat.md",
            "notes/.notedthat/file",
            ".NotedThat/config",
            "notes/readme.md",
        ] {
            assert!(!is_internal_path(path), "{path}");
        }
    }

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

    #[test]
    fn test_try_object_key_err_reserved_key() {
        for path in ["index", "/index", "index/reconcile", "events", "search"] {
            assert!(ObjectPath::try_object_key(path).is_err(), "{path}");
            // Still a valid path: it can name a folder (`search/a.md`).
            let parsed = ObjectPath::try_from(path).unwrap();
            assert!(parsed.is_reserved(), "{path}");
        }
    }

    #[test]
    fn test_try_object_key_err_invalid_path() {
        for path in ["", "a//b", "../a", "a\\b"] {
            assert!(ObjectPath::try_object_key(path).is_err(), "{path}");
        }
    }

    #[test]
    fn test_try_object_key_reserved_look_alikes_valid() {
        for path in [
            "index.md",
            "Index",
            "SEARCH",
            "notes/index",
            "notes/events",
            "index/other",
            "search/results.md",
            "events.md",
        ] {
            let parsed = ObjectPath::try_object_key(path).unwrap();
            assert!(!parsed.is_reserved(), "{path}");
        }
    }
}

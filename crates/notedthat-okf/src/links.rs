//! Resolving OKF cross-links, safely, inside a knowledge base.
//!
//! OKF concepts link to each other with ordinary Markdown links: bundle-relative
//! (`/tables/customers.md`), relative (`./other.md`, `../a/b.md`), or an absolute
//! URL. This module is the security boundary for every feature that turns one of
//! those strings into something the server might fetch.
//!
//! Two rules are load-bearing:
//!
//! 1. **An absolute URL is never resolved and never fetched.** The server holds
//!    S3 credentials and sits inside the network; dereferencing a user-authored
//!    URL on an agent's behalf would be an SSRF primitive.
//! 2. **`..` is resolved, then bounds-checked — never clamped.** Clamping a
//!    traversal to the root is how traversal bugs are born, so a link that would
//!    escape the bundle is reported as unresolvable instead.

use notedthat_core::ObjectPath;

/// The outcome of resolving one Markdown link target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// An absolute URL or non-path URI. Never resolved, never fetched, never
    /// reported as a broken link.
    External(String),
    /// A link that resolves to an object inside this knowledge base.
    Internal(ObjectPath),
    /// A link to a directory — a virtual prefix, since only object bytes are
    /// stored (D40). Carries the prefix with its trailing `/`.
    Directory(String),
    /// A link that cannot be resolved inside the knowledge base.
    Unresolvable {
        /// The link target as written.
        raw: String,
        /// Why it could not be resolved.
        reason: &'static str,
    },
}

/// The directory component of an object key, with its trailing `/`.
///
/// `"tables/customers.md"` yields `"tables/"`; a root-level key yields `""`.
#[must_use]
pub fn dir_of(key: &str) -> &str {
    match key.rfind('/') {
        Some(i) => &key[..=i],
        None => "",
    }
}

/// Resolve a Markdown link target found in a concept stored at `base_dir`.
///
/// `base_dir` is the concept's directory as returned by [`dir_of`].
#[must_use]
pub fn resolve_link(base_dir: &str, raw: &str) -> LinkTarget {
    let trimmed = raw.trim();

    // Strip a fragment and/or query before anything else.
    let path_part = {
        let no_fragment = trimmed.split_once('#').map_or(trimmed, |(p, _)| p);
        no_fragment.split_once('?').map_or(no_fragment, |(p, _)| p)
    };

    // A pure fragment link points at the document itself.
    if path_part.is_empty() {
        return LinkTarget::External(trimmed.to_string());
    }

    if has_uri_scheme(path_part) {
        return LinkTarget::External(trimmed.to_string());
    }

    let Some(decoded) = percent_decode_once(path_part) else {
        return LinkTarget::Unresolvable {
            raw: trimmed.to_string(),
            reason: "link target is not valid percent-encoded UTF-8",
        };
    };
    if decoded.contains('\0') || decoded.contains('\\') {
        return LinkTarget::Unresolvable {
            raw: trimmed.to_string(),
            reason: "link target contains a NUL or backslash",
        };
    }

    let is_directory = decoded.ends_with('/');
    let candidate = if let Some(rest) = decoded.strip_prefix('/') {
        rest.to_string()
    } else {
        format!("{base_dir}{decoded}")
    };

    let Some(normalised) = normalise(&candidate) else {
        return LinkTarget::Unresolvable {
            raw: trimmed.to_string(),
            reason: "link target escapes the knowledge base root",
        };
    };

    if normalised.is_empty() {
        return LinkTarget::Directory(String::new());
    }
    if is_directory {
        return LinkTarget::Directory(format!("{normalised}/"));
    }

    ObjectPath::try_from_str(&normalised).map_or(
        LinkTarget::Unresolvable {
            raw: trimmed.to_string(),
            reason: "link target is not a valid object path",
        },
        LinkTarget::Internal,
    )
}

/// Whether the target carries a URI scheme (`https:`, `mailto:`, `data:`, …).
fn has_uri_scheme(s: &str) -> bool {
    let Some(colon) = s.find(':') else {
        return false;
    };
    // A scheme cannot contain a path separator, which keeps `a/b:c.md` a path.
    if s[..colon].contains('/') {
        return false;
    }
    let mut chars = s[..colon].chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// Percent-decode exactly once. `None` when the result is not valid UTF-8.
fn percent_decode_once(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_string());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(u8::try_from(hi * 16 + lo).ok()?);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// Resolve `.` and `..` segments. `None` when a `..` would escape the root.
fn normalise(path: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                // Bounds-check rather than clamp.
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    Some(segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn internal(base: &str, raw: &str) -> String {
        match resolve_link(base, raw) {
            LinkTarget::Internal(p) => p.as_str().to_string(),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[test]
    fn dir_of_strips_the_file_name() {
        assert_eq!(dir_of("tables/customers.md"), "tables/");
        assert_eq!(dir_of("a/b/c.md"), "a/b/");
        assert_eq!(dir_of("root.md"), "");
    }

    #[test]
    fn bundle_relative_resolves_from_the_root() {
        assert_eq!(
            internal("deep/nested/", "/tables/customers.md"),
            "tables/customers.md"
        );
    }

    #[test]
    fn relative_resolves_against_the_concept_directory() {
        assert_eq!(internal("tables/", "./orders.md"), "tables/orders.md");
        assert_eq!(internal("tables/", "orders.md"), "tables/orders.md");
    }

    #[test]
    fn dotdot_walks_up_one_directory() {
        assert_eq!(internal("a/b/", "../c.md"), "a/c.md");
    }

    #[test]
    fn dotdot_escaping_the_root_is_unresolvable() {
        assert!(matches!(
            resolve_link("a/", "../../etc/passwd"),
            LinkTarget::Unresolvable { .. }
        ));
    }

    #[test]
    fn dotdot_never_clamps_to_the_root() {
        // Clamping would silently turn an escape into a valid in-bundle read.
        match resolve_link("", "../secret.md") {
            LinkTarget::Unresolvable { .. } => {}
            other => panic!("traversal was not rejected: {other:?}"),
        }
    }

    #[test]
    fn absolute_https_is_external() {
        assert_eq!(
            resolve_link("a/", "https://example.com/x.md"),
            LinkTarget::External("https://example.com/x.md".into())
        );
    }

    #[test]
    fn mailto_and_data_are_external() {
        assert!(matches!(
            resolve_link("", "mailto:a@example.com"),
            LinkTarget::External(_)
        ));
        assert!(matches!(
            resolve_link("", "data:text/plain,hi"),
            LinkTarget::External(_)
        ));
    }

    #[test]
    fn a_colon_after_a_slash_is_not_a_scheme() {
        assert_eq!(internal("", "notes/a:b.md"), "notes/a:b.md");
    }

    #[test]
    fn a_pure_fragment_is_external() {
        assert!(matches!(
            resolve_link("a/", "#section"),
            LinkTarget::External(_)
        ));
    }

    #[test]
    fn fragments_and_queries_are_stripped() {
        assert_eq!(
            internal("tables/", "./orders.md#schema"),
            "tables/orders.md"
        );
        assert_eq!(internal("tables/", "./orders.md?v=2"), "tables/orders.md");
    }

    #[test]
    fn trailing_slash_is_a_directory() {
        assert_eq!(
            resolve_link("", "tables/"),
            LinkTarget::Directory("tables/".into())
        );
    }

    #[test]
    fn percent_encoding_is_decoded_once() {
        assert_eq!(internal("", "my%20notes/a.md"), "my notes/a.md");
    }

    #[test]
    fn double_encoded_percent_is_not_decoded_twice() {
        // %252F must decode to the literal "%2F", not to a path separator.
        assert_eq!(internal("", "a%252Fb.md"), "a%2Fb.md");
    }

    #[test]
    fn a_backslash_after_decoding_is_rejected() {
        assert!(matches!(
            resolve_link("", "a%5Cb.md"),
            LinkTarget::Unresolvable { .. }
        ));
    }

    #[test]
    fn a_nul_after_decoding_is_rejected() {
        assert!(matches!(
            resolve_link("", "a%00b.md"),
            LinkTarget::Unresolvable { .. }
        ));
    }

    #[test]
    fn dot_segments_are_collapsed() {
        assert_eq!(internal("a/", "./././b.md"), "a/b.md");
    }

    #[test]
    fn root_directory_link_resolves_to_the_empty_prefix() {
        assert_eq!(
            resolve_link("a/", "/"),
            LinkTarget::Directory(String::new())
        );
    }
}

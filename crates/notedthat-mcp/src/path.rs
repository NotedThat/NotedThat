//! RFC 3986 percent-encoding for object paths and KB slugs.
//!
//! SPEC §6.13 example: `docs/rfc/7231.md` in KB `my-notes`
//! → `GET /v1/knowledgebases/my-notes/docs%2Frfc%2F7231.md`
//!
//! We encode everything except RFC 3986 §2.3 unreserved characters:
//! `A-Z a-z 0-9 - . _ ~`

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

/// Characters to percent-encode in object paths.
///
/// Starts from CONTROLS (0x00-0x1F, 0x7F) and adds all characters that
/// are NOT RFC 3986 §2.3 unreserved (`A-Z a-z 0-9 - . _ ~`).
///
/// This includes reserved characters like `/`, `?`, `#`, `@`, `:`, etc.,
/// as well as space and unicode (handled by `utf8_percent_encode`'s byte-level encoding).
pub const OBJECT_PATH_ENCODE: &AsciiSet = &CONTROLS
    // Space
    .add(b' ')
    // Delimiters (RFC 3986 §2.2 gen-delims)
    .add(b':')
    .add(b'/')
    .add(b'?')
    .add(b'#')
    .add(b'[')
    .add(b']')
    .add(b'@')
    // Sub-delimiters (RFC 3986 §2.2 sub-delims)
    .add(b'!')
    .add(b'$')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b';')
    .add(b'=')
    // Other non-unreserved ASCII
    .add(b'"')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'\\')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Percent-encode an object path for use in HTTP URLs.
///
/// Encodes everything except RFC 3986 §2.3 unreserved characters
/// (`A-Z a-z 0-9 - . _ ~`). This means `/` within the path IS encoded
/// to `%2F`, preserving the full path as a single URL segment.
///
/// # Example
/// ```
/// use notedthat_mcp::path::encode_object_path;
/// assert_eq!(encode_object_path("docs/rfc/7231.md"), "docs%2Frfc%2F7231.md");
/// ```
pub fn encode_object_path(path: &str) -> String {
    utf8_percent_encode(path, OBJECT_PATH_ENCODE).to_string()
}

/// Percent-encode a knowledge base slug for use in HTTP URLs.
///
/// Slugs are `[a-z0-9-]{1,40}` per §6.13, so encoding is defensive only.
///
/// # Example
/// ```
/// use notedthat_mcp::path::encode_kb_slug;
/// assert_eq!(encode_kb_slug("my-notes"), "my-notes");
/// ```
pub fn encode_kb_slug(slug: &str) -> String {
    utf8_percent_encode(slug, OBJECT_PATH_ENCODE).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- encode_object_path ---

    #[test]
    fn multisegment_path_slashes_encoded() {
        assert_eq!(encode_object_path("docs/rfc/7231.md"), "docs%2Frfc%2F7231.md");
    }

    #[test]
    fn path_with_spaces_and_parens() {
        assert_eq!(
            encode_object_path("notes/2024 Q1 (draft).md"),
            "notes%2F2024%20Q1%20%28draft%29.md"
        );
    }

    #[test]
    fn unicode_japanese() {
        // 日本語 = \xE6\x97\xA5\xE6\x9C\xAC\xE8\xAA\x9E
        assert_eq!(
            encode_object_path("日本語.md"),
            "%E6%97%A5%E6%9C%AC%E8%AA%9E.md"
        );
    }

    #[test]
    fn unicode_accented() {
        // café → caf\xC3\xA9
        assert_eq!(encode_object_path("café.md"), "caf%C3%A9.md");
    }

    #[test]
    fn question_mark_and_hash_encoded() {
        assert_eq!(encode_object_path("a?b#c"), "a%3Fb%23c");
    }

    // --- Regression: naive replace would be insufficient ---

    #[test]
    fn regression_naive_replace_space() {
        // Naive "replace / with %2F" would not encode spaces
        let result = encode_object_path("a b");
        assert_ne!(result, "a b", "space must be encoded");
        assert!(result.contains("%20"), "space must encode to %20: {result}");
    }

    #[test]
    fn regression_naive_replace_question_mark() {
        // Naive replace would not encode ?
        let result = encode_object_path("a?b");
        assert_ne!(result, "a?b", "? must be encoded");
        assert!(result.contains("%3F"), "? must encode to %3F: {result}");
    }

    #[test]
    fn regression_naive_replace_parens() {
        let result = encode_object_path("a(b)c");
        assert!(
            result.contains("%28") && result.contains("%29"),
            "parens must be encoded: {result}"
        );
    }

    // --- encode_kb_slug ---

    #[test]
    fn normal_slug_unchanged() {
        assert_eq!(encode_kb_slug("my-notes"), "my-notes");
        assert_eq!(encode_kb_slug("scratch"), "scratch");
        assert_eq!(encode_kb_slug("kb123"), "kb123");
    }

    // --- Unreserved characters pass through unchanged ---

    #[test]
    fn unreserved_chars_unchanged() {
        // RFC 3986 §2.3 unreserved: A-Z a-z 0-9 - . _ ~
        let input = "abcXYZ123-._~";
        assert_eq!(encode_object_path(input), input);
    }
}

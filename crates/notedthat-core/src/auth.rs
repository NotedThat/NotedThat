//! Bearer token verification and extraction per RFC 6750.

use subtle::ConstantTimeEq;

/// Verify a Bearer token in constant time.
///
/// # Token length leakage
///
/// This function short-circuits on length mismatch (returns `false` immediately
/// without invoking the constant-time comparison). This leaks the length of the
/// expected token, which is an **acceptable limitation** for a static Bearer token
/// per D21 (the token length is small and enumerable).
pub fn verify_bearer_token(provided: &str, expected: &str) -> bool {
    if provided.len() != expected.len() {
        return false;
    }
    if expected.is_empty() {
        return false;
    }
    provided.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Extract the Bearer token value from an `Authorization` header value.
///
/// The scheme match is **case-insensitive** per RFC 6750 §2.1. The token
/// is separated from the scheme by exactly one space; tabs and multiple
/// consecutive spaces are rejected.
///
/// Returns `None` if the header value is malformed or missing a token.
pub fn extract_bearer_from_header(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    if rest.is_empty() || rest.starts_with(' ') {
        return None;
    }
    Some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_bearer_token_matching() {
        assert!(verify_bearer_token("secret-token", "secret-token"));
    }

    #[test]
    fn test_verify_bearer_token_different_same_length() {
        assert!(!verify_bearer_token("aaaabbbb", "aaaacccc"));
    }

    #[test]
    fn test_verify_bearer_token_empty_provided() {
        assert!(!verify_bearer_token("", "some-token"));
    }

    #[test]
    fn test_verify_bearer_token_empty_expected() {
        assert!(!verify_bearer_token("some-token", ""));
    }

    #[test]
    fn test_verify_bearer_token_different_lengths() {
        assert!(!verify_bearer_token("short", "much-longer-token"));
    }

    #[test]
    fn test_verify_bearer_token_case_sensitive() {
        assert!(!verify_bearer_token("AbC", "abc"));
    }

    #[test]
    fn test_verify_bearer_token_whitespace_sensitive() {
        assert!(!verify_bearer_token("tok", "tok "));
    }

    #[test]
    fn test_extract_bearer_lowercase_scheme() {
        assert_eq!(extract_bearer_from_header("Bearer abc123"), Some("abc123"));
    }

    #[test]
    fn test_extract_bearer_lowercase_bearer() {
        assert_eq!(extract_bearer_from_header("bearer abc123"), Some("abc123"));
    }

    #[test]
    fn test_extract_bearer_uppercase_scheme() {
        assert_eq!(extract_bearer_from_header("BEARER abc123"), Some("abc123"));
    }

    #[test]
    fn test_extract_bearer_wrong_scheme_basic() {
        assert_eq!(extract_bearer_from_header("Basic dXNlcjpwYXNz"), None);
    }

    #[test]
    fn test_extract_bearer_no_scheme() {
        assert_eq!(extract_bearer_from_header("abc123"), None);
    }

    #[test]
    fn test_extract_bearer_empty_header() {
        assert_eq!(extract_bearer_from_header(""), None);
    }

    #[test]
    fn test_extract_bearer_empty_token_after_space() {
        assert_eq!(extract_bearer_from_header("Bearer "), None);
    }

    #[test]
    fn test_extract_bearer_double_space_rejected() {
        assert_eq!(extract_bearer_from_header("Bearer  abc"), None);
    }

    #[test]
    fn test_extract_bearer_tab_not_space_rejected() {
        assert_eq!(extract_bearer_from_header("Bearer\tabc"), None);
    }

    #[test]
    fn test_extract_bearer_mixed_case_scheme() {
        assert_eq!(extract_bearer_from_header("BeArEr mytoken"), Some("mytoken"));
    }

    #[test]
    fn test_extract_bearer_token_with_dots() {
        assert_eq!(
            extract_bearer_from_header("Bearer eyJ0.eyJz.SflK"),
            Some("eyJ0.eyJz.SflK")
        );
    }

    #[test]
    fn test_verify_bearer_both_empty() {
        assert!(!verify_bearer_token("", ""));
    }
}

//! Bearer token verification/extraction per RFC 6750 and Basic auth helpers per RFC 7617.

use base64::Engine as _;
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

/// Verify HTTP Basic credentials in constant time.
///
/// Empty usernames/passwords are rejected. Length mismatches are rejected before
/// comparison, matching the accepted Bearer-token length leakage precedent.
pub fn verify_basic_credentials(
    provided_username: &str,
    provided_password: &str,
    expected_username: &str,
    expected_password: &str,
) -> bool {
    if provided_username.is_empty()
        || provided_password.is_empty()
        || expected_username.is_empty()
        || expected_password.is_empty()
    {
        return false;
    }

    if provided_username.len() != expected_username.len()
        || provided_password.len() != expected_password.len()
    {
        return false;
    }

    let username_match = provided_username
        .as_bytes()
        .ct_eq(expected_username.as_bytes());
    let password_match = provided_password
        .as_bytes()
        .ct_eq(expected_password.as_bytes());
    let both_match = username_match & password_match;
    bool::from(both_match)
}

/// Extract a non-empty username and password from an HTTP Basic `Authorization` header.
///
/// The scheme match is case-insensitive. Credentials are decoded with the
/// standard Base64 alphabet and split on the first colon so passwords may
/// contain colons.
pub fn extract_basic_from_header(value: &str) -> Option<(String, String)> {
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    if rest.starts_with(' ') {
        return None;
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(rest)
        .ok()?;
    let decoded_str = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded_str.split_once(':')?;
    if username.is_empty() || password.is_empty() {
        return None;
    }
    Some((username.to_string(), password.to_string()))
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
        assert_eq!(
            extract_bearer_from_header("BeArEr mytoken"),
            Some("mytoken")
        );
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

    #[test]
    fn verify_matching_credentials() {
        assert!(verify_basic_credentials("user", "pass", "user", "pass"));
    }

    #[test]
    fn verify_wrong_username() {
        assert!(!verify_basic_credentials("user", "pass", "xxxx", "pass"));
    }

    #[test]
    fn verify_wrong_password() {
        assert!(!verify_basic_credentials("user", "pass", "user", "xxxx"));
    }

    #[test]
    fn verify_empty_provided_username() {
        assert!(!verify_basic_credentials("", "pass", "user", "pass"));
    }

    #[test]
    fn verify_empty_provided_password() {
        assert!(!verify_basic_credentials("user", "", "user", "pass"));
    }

    #[test]
    fn verify_empty_expected_username() {
        assert!(!verify_basic_credentials("user", "pass", "", "pass"));
    }

    #[test]
    fn verify_empty_expected_password() {
        assert!(!verify_basic_credentials("user", "pass", "user", ""));
    }

    #[test]
    fn verify_different_lengths_username_longer() {
        assert!(!verify_basic_credentials("users", "pass", "user", "pass"));
    }

    #[test]
    fn verify_different_lengths_username_shorter() {
        assert!(!verify_basic_credentials("usr", "pass", "user", "pass"));
    }

    #[test]
    fn verify_different_lengths_password_longer() {
        assert!(!verify_basic_credentials("user", "passw", "user", "pass"));
    }

    #[test]
    fn verify_different_lengths_password_shorter() {
        assert!(!verify_basic_credentials("user", "pas", "user", "pass"));
    }

    #[test]
    fn extract_basic_valid() {
        assert_eq!(
            extract_basic_from_header("Basic dXNlcjpwYXNz"),
            Some(("user".to_string(), "pass".to_string()))
        );
    }

    #[test]
    fn extract_basic_mixed_case_scheme() {
        assert_eq!(
            extract_basic_from_header("bAsIc dXNlcjpwYXNz"),
            Some(("user".to_string(), "pass".to_string()))
        );
    }

    #[test]
    fn extract_basic_wrong_scheme() {
        assert_eq!(extract_basic_from_header("Bearer dXNlcjpwYXNz"), None);
    }

    #[test]
    fn extract_basic_empty_header() {
        assert_eq!(extract_basic_from_header(""), None);
    }

    #[test]
    fn extract_basic_double_space_rejected() {
        assert_eq!(extract_basic_from_header("Basic  dXNlcjpwYXNz"), None);
    }

    #[test]
    fn extract_basic_invalid_base64() {
        assert_eq!(extract_basic_from_header("Basic not-valid-base64!!!"), None);
    }

    #[test]
    fn extract_basic_non_utf8_after_decode() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode([0xff, 0xfe]);
        assert_eq!(extract_basic_from_header(&format!("Basic {encoded}")), None);
    }

    #[test]
    fn extract_basic_no_colon() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("userpass");
        assert_eq!(extract_basic_from_header(&format!("Basic {encoded}")), None);
    }

    #[test]
    fn extract_basic_colon_at_start() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(":pass");
        assert_eq!(extract_basic_from_header(&format!("Basic {encoded}")), None);
    }

    #[test]
    fn extract_basic_colon_at_end() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:");
        assert_eq!(extract_basic_from_header(&format!("Basic {encoded}")), None);
    }

    #[test]
    fn extract_basic_password_with_colon() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("user:pass:word");
        assert_eq!(
            extract_basic_from_header(&format!("Basic {encoded}")),
            Some(("user".to_string(), "pass:word".to_string()))
        );
    }
}

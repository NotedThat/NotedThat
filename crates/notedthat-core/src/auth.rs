//! Bearer token verification and extraction per RFC 6750.

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

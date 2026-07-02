//! `ObjectPath` — normalized object storage key with D40 validation rules.

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

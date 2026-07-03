/// Maximum number of Unicode characters (not bytes) in a preview.
pub const PREVIEW_MAX_CHARS: usize = 500;

/// Truncates `text` to at most `max_chars` Unicode characters, returning a new
/// `String` that ends at a valid UTF-8 char boundary.
///
/// Guarantees: `truncate_preview(t, N).chars().count() <= N`
///
/// If `max_chars` is 0, returns an empty string.
/// If `text` has fewer than `max_chars` characters, returns the full text.
pub fn truncate_preview(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(truncate_preview("", 500), "");
    }

    #[test]
    fn input_shorter_than_max_returns_full() {
        let s = "hello world";
        assert_eq!(truncate_preview(s, 500), s);
    }

    #[test]
    fn input_exactly_max_returns_full() {
        let s = "a".repeat(500);
        let result = truncate_preview(&s, 500);
        assert_eq!(result.chars().count(), 500);
        assert_eq!(result, s);
    }

    #[test]
    fn input_longer_than_max_truncates() {
        let s = "a".repeat(600);
        let result = truncate_preview(&s, 500);
        assert_eq!(result.chars().count(), 500);
    }

    #[test]
    fn emoji_at_boundary_not_split() {
        // 499 'a' chars + 2 emoji; max_chars=500 should yield 499 'a' + 1 emoji
        let s = "a".repeat(499) + "🚀🚀";
        let result = truncate_preview(&s, 500);
        assert_eq!(result.chars().count(), 500);
        // Result is valid UTF-8 (would panic if not, since String guarantees it)
        assert!(result.ends_with('🚀'));
    }

    #[test]
    fn multibyte_cjk_exact_count() {
        // Japanese: each char is 3 bytes; 300 * 3 = 900 bytes total
        let s = "日本語".repeat(200); // 600 chars
        let result = truncate_preview(&s, 500);
        assert_eq!(result.chars().count(), 500);
        // Valid UTF-8 (String invariant)
    }

    #[test]
    fn max_chars_zero_returns_empty() {
        assert_eq!(truncate_preview("hello", 0), "");
    }

    #[test]
    fn preview_max_chars_constant_is_500() {
        assert_eq!(PREVIEW_MAX_CHARS, 500);
    }
}

/// Maximum number of Unicode characters (not bytes) in a preview.
pub const PREVIEW_MAX_CHARS: usize = 500;

/// How far back from the cut a truncated preview looks for a word boundary.
const WORD_BACKOFF_CHARS: usize = 50;

/// Truncates `text` to at most `max_chars` Unicode characters, returning a new
/// `String` that ends at a valid UTF-8 char boundary.
///
/// Guarantees: `truncate_preview(t, N).chars().count() <= N`
///
/// Trailing whitespace is dropped before measuring, so a chunk that ends in a
/// paragraph break is not cut or marked `…` because of that break alone.
///
/// If `max_chars` is 0, returns an empty string.
/// If `text` without its trailing whitespace has at most `max_chars`
/// characters, returns it in full with that whitespace trimmed.
/// Otherwise the preview ends at the cut when whitespace follows it, else at
/// the last whitespace within the final `WORD_BACKOFF_CHARS` characters (or at
/// the hard cut when there is none), followed by `…`.
pub fn truncate_preview(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let text = text.trim_end();
    let Some((cut, _)) = text.char_indices().nth(max_chars - 1) else {
        return text.to_owned();
    };
    if text[cut..].chars().nth(1).is_none() {
        return text.to_owned();
    }
    let prefix = &text[..cut];
    // Whitespace at the cut means `prefix` already ends at a word boundary.
    let boundary = if text[cut..].starts_with(char::is_whitespace) {
        Some(prefix.len())
    } else {
        prefix
            .char_indices()
            .rev()
            .take(WORD_BACKOFF_CHARS)
            .find(|(_, character)| character.is_whitespace())
            .map(|(byte, _)| byte)
    };
    let prefix = boundary
        .map(|byte| prefix[..byte].trim_end())
        .filter(|trimmed| !trimmed.is_empty())
        .unwrap_or(prefix);
    format!("{prefix}…")
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
        // 499 'a' chars + 2 emoji; max_chars=500 cuts before the emoji and marks the cut
        let s = "a".repeat(499) + "🚀🚀";
        let result = truncate_preview(&s, 500);
        assert_eq!(result.chars().count(), 500);
        // Result is valid UTF-8 (would panic if not, since String guarantees it)
        assert_eq!(result, "a".repeat(499) + "…");
    }

    #[test]
    fn truncates_at_word_boundary_with_ellipsis() {
        let s = "all flesh died that moved on the earth, including birds";
        let result = truncate_preview(s, 45);
        assert_eq!(result, "all flesh died that moved on the earth,…");
        assert!(result.chars().count() <= 45);
    }

    #[test]
    fn keeps_last_word_when_cut_lands_on_whitespace() {
        assert_eq!(
            truncate_preview("the quick brown fox jumps", 20),
            "the quick brown fox…"
        );
        assert_eq!(
            truncate_preview("the quick brown fox\r\njumps", 20),
            "the quick brown fox…"
        );
    }

    #[test]
    fn hard_cuts_when_no_whitespace_in_window() {
        let s = format!("word {}", "x".repeat(600));
        let result = truncate_preview(&s, 500);
        assert_eq!(result.chars().count(), 500);
        assert!(result.ends_with("x…"));
    }

    #[test]
    fn max_chars_one_returns_ellipsis() {
        assert_eq!(truncate_preview("ab", 1), "…");
        assert_eq!(truncate_preview("a", 1), "a");
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
    fn trailing_whitespace_does_not_count_towards_the_limit() {
        assert_eq!(truncate_preview("abc \n", 3), "abc");
        assert_eq!(truncate_preview("abc\r\n\r\n", 3), "abc");
        assert_eq!(truncate_preview("hello world\n\n", 500), "hello world");
        assert_eq!(truncate_preview("a ", 1), "a");
    }

    #[test]
    fn content_that_fits_keeps_its_last_word_without_ellipsis() {
        let exact = "word ".repeat(99) + "last.";
        assert_eq!(exact.chars().count(), 500);
        assert_eq!(truncate_preview(&(exact.clone() + "\n\n"), 500), exact);
        assert_eq!(truncate_preview(&(exact.clone() + "\r\n\r\n"), 500), exact);

        let short = "word ".repeat(99) + "las";
        assert_eq!(truncate_preview(&(short.clone() + "\n\n\n"), 500), short);
    }

    #[test]
    fn preview_max_chars_constant_is_500() {
        assert_eq!(PREVIEW_MAX_CHARS, 500);
    }
}

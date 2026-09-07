//! Extracting the inline `# Computation` block from a concept body.
//!
//! # Non-execution invariant
//!
//! This module reads a code fence out of Markdown. It does not know what runtime
//! the code targets, cannot resolve a `computation:` path, and cannot execute
//! anything. See SPECIFICATIONS.md D48.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};

/// A computation body found inline in a concept's Markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineComputation {
    /// The fence info string's first word, when the fence carried one.
    pub language: Option<String>,
    /// The computation source, verbatim.
    pub code: String,
    /// Whether a `# Computation` heading was found but carried no code fence.
    ///
    /// The caller surfaces this as a warning: the contract is servable, but the
    /// producer probably meant to fence the code.
    pub unfenced: bool,
}

/// Extract the code fence under the first `# Computation` heading of `body`.
///
/// Heading matching is case-insensitive and trims surrounding whitespace, since
/// OKF calls this a *conventional* heading rather than a keyword. Scanning stops
/// at the next H1, so a later fence in another section is not picked up.
///
/// Returns `None` when the body has no `# Computation` heading at all.
#[must_use]
pub fn extract_inline_computation(body: &str) -> Option<InlineComputation> {
    let mut in_target_heading = false;
    let mut heading_text = String::new();
    let mut inside_section = false;
    let mut fence_language: Option<String> = None;
    let mut code = String::new();
    let mut collecting_code = false;

    for event in Parser::new(body) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                if inside_section && level == HeadingLevel::H1 {
                    // Next top-level section: the computation section is over.
                    return Some(InlineComputation {
                        language: fence_language,
                        code,
                        unfenced: true,
                    });
                }
                in_target_heading = level == HeadingLevel::H1;
                heading_text.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                if in_target_heading && heading_text.trim().eq_ignore_ascii_case("computation") {
                    inside_section = true;
                }
                in_target_heading = false;
            }
            Event::Start(Tag::CodeBlock(kind)) if inside_section => {
                if let CodeBlockKind::Fenced(info) = kind {
                    fence_language = info
                        .split_whitespace()
                        .next()
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned);
                }
                collecting_code = true;
            }
            Event::End(TagEnd::CodeBlock) if collecting_code => {
                return Some(InlineComputation {
                    language: fence_language,
                    code,
                    unfenced: false,
                });
            }
            Event::Text(text) | Event::Code(text) => {
                if in_target_heading {
                    heading_text.push_str(&text);
                } else if collecting_code || inside_section {
                    // Inside a fence, or prose in an unfenced Computation section.
                    code.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak if inside_section && !collecting_code => {
                code.push('\n');
            }
            _ => {}
        }
    }

    if inside_section {
        return Some(InlineComputation {
            language: fence_language,
            code,
            unfenced: true,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_fenced_block_under_the_heading() {
        let body = "# Computation\n\n```sql\nSELECT 1;\n```\n";
        let c = extract_inline_computation(body).unwrap();
        assert_eq!(c.language.as_deref(), Some("sql"));
        assert_eq!(c.code, "SELECT 1;\n");
        assert!(!c.unfenced);
    }

    #[test]
    fn heading_match_is_case_insensitive() {
        let body = "# COMPUTATION\n\n```py\nx = 1\n```\n";
        assert_eq!(extract_inline_computation(body).unwrap().code, "x = 1\n");
    }

    #[test]
    fn a_fence_without_an_info_string_has_no_language() {
        let body = "# Computation\n\n```\nSELECT 1;\n```\n";
        assert_eq!(extract_inline_computation(body).unwrap().language, None);
    }

    #[test]
    fn only_the_first_word_of_the_info_string_is_the_language() {
        let body = "# Computation\n\n```sql linenos\nSELECT 1;\n```\n";
        assert_eq!(
            extract_inline_computation(body)
                .unwrap()
                .language
                .as_deref(),
            Some("sql")
        );
    }

    #[test]
    fn scanning_stops_at_the_next_h1() {
        let body = "# Computation\n\ntext only\n\n# Notes\n\n```sql\nSELECT 2;\n```\n";
        let c = extract_inline_computation(body).unwrap();
        assert!(c.unfenced);
        assert!(!c.code.contains("SELECT 2"));
    }

    #[test]
    fn a_section_with_no_fence_is_reported_unfenced() {
        let body = "# Computation\n\njust prose\n";
        let c = extract_inline_computation(body).unwrap();
        assert!(c.unfenced);
        assert!(c.code.contains("just prose"));
    }

    #[test]
    fn absent_heading_yields_none() {
        assert!(extract_inline_computation("# Schema\n\n```sql\nSELECT 1;\n```\n").is_none());
    }

    #[test]
    fn a_lower_level_heading_named_computation_is_not_the_section() {
        // OKF's conventional heading is `# Computation` at H1.
        assert!(extract_inline_computation("## Computation\n\n```sql\nx\n```\n").is_none());
    }

    #[test]
    fn an_earlier_fence_outside_the_section_is_ignored() {
        let body =
            "# Examples\n\n```sql\nSELECT 99;\n```\n\n# Computation\n\n```sql\nSELECT 1;\n```\n";
        assert_eq!(
            extract_inline_computation(body).unwrap().code,
            "SELECT 1;\n"
        );
    }

    #[test]
    fn empty_body_yields_none() {
        assert!(extract_inline_computation("").is_none());
    }
}

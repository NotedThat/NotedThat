//! Splitting a leading YAML frontmatter block off a Markdown document.
//!
//! The whole point of this module is that it reports **absolute byte offsets into
//! the original document**. Everything downstream — chunking, byte-range reads,
//! search hit dereferencing — depends on offsets that index the stored object, so
//! this never returns a substring's own coordinate space. See SPECIFICATIONS.md D6.

/// Byte spans of a leading YAML frontmatter block, absolute in the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrontmatterSplit<'a> {
    /// The YAML source between the delimiters, excluding both delimiter lines.
    pub yaml: &'a str,
    /// Absolute byte offset where `yaml` begins.
    pub yaml_start: usize,
    /// Absolute byte offset where `yaml` ends.
    pub yaml_end: usize,
    /// Absolute byte offset of the first body byte — just past the closing
    /// delimiter line and its terminator. Equals the input length for a
    /// document that is nothing but frontmatter.
    pub body_start: usize,
}

/// Split a leading `---` YAML frontmatter block off a Markdown document.
///
/// Returns `None` when the document does not open with a frontmatter block, or
/// when the block is never closed — an unterminated block is not frontmatter, so
/// a document that merely opens with a thematic break is left alone.
///
/// Never panics and never allocates.
#[must_use]
pub fn split_frontmatter(raw: &str) -> Option<FrontmatterSplit<'_>> {
    // A BOM is part of the pre-body region but not part of the delimiter.
    let bom = if raw.starts_with('\u{FEFF}') {
        '\u{FEFF}'.len_utf8()
    } else {
        0
    };

    let (first_line, after_first) = line_at(raw, bom)?;
    if !is_open_delimiter(first_line) {
        return None;
    }

    let yaml_start = after_first;
    let mut cursor = after_first;
    while cursor < raw.len() {
        let (line, next) = line_at(raw, cursor)?;
        if is_close_delimiter(line) {
            return Some(FrontmatterSplit {
                yaml: &raw[yaml_start..cursor],
                yaml_start,
                yaml_end: cursor,
                body_start: next,
            });
        }
        cursor = next;
    }

    // Ran off the end without a closing delimiter.
    None
}

/// The line beginning at `start`, and the offset of the line after it.
///
/// The returned line excludes its terminator. `None` only when `start` is past
/// the end of the input.
fn line_at(raw: &str, start: usize) -> Option<(&str, usize)> {
    if start > raw.len() {
        return None;
    }
    let rest = &raw[start..];
    match rest.find('\n') {
        Some(nl) => {
            let mut end = start + nl;
            if end > start && raw.as_bytes()[end - 1] == b'\r' {
                end -= 1;
            }
            Some((&raw[start..end], start + nl + 1))
        }
        None => Some((rest, raw.len())),
    }
}

/// Whether `line` opens a frontmatter block: exactly `---`, trailing blanks allowed.
fn is_open_delimiter(line: &str) -> bool {
    line.trim_end_matches([' ', '\t']) == "---"
}

/// Whether `line` closes a frontmatter block: exactly `---` or `...`.
fn is_close_delimiter(line: &str) -> bool {
    let t = line.trim_end_matches([' ', '\t']);
    t == "---" || t == "..."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_simple_block() {
        let raw = "---\ntype: Metric\n---\n# Body\n";
        let s = split_frontmatter(raw).unwrap();
        assert_eq!(s.yaml, "type: Metric\n");
        assert_eq!(&raw[s.body_start..], "# Body\n");
    }

    #[test]
    fn offsets_are_absolute_in_the_input() {
        let raw = "---\ntype: Metric\n---\n# Body\n";
        let s = split_frontmatter(raw).unwrap();
        assert_eq!(&raw[s.yaml_start..s.yaml_end], s.yaml);
    }

    #[test]
    fn accepts_a_utf8_bom() {
        let raw = "\u{FEFF}---\ntype: Metric\n---\nbody";
        let s = split_frontmatter(raw).unwrap();
        assert_eq!(s.yaml, "type: Metric\n");
        assert_eq!(&raw[s.body_start..], "body");
    }

    #[test]
    fn accepts_crlf_line_endings() {
        let raw = "---\r\ntype: Metric\r\n---\r\nbody";
        let s = split_frontmatter(raw).unwrap();
        assert_eq!(s.yaml, "type: Metric\r\n");
        assert_eq!(&raw[s.body_start..], "body");
    }

    #[test]
    fn accepts_the_dot_terminator() {
        let raw = "---\ntype: Metric\n...\nbody";
        let s = split_frontmatter(raw).unwrap();
        assert_eq!(s.yaml, "type: Metric\n");
        assert_eq!(&raw[s.body_start..], "body");
    }

    #[test]
    fn accepts_trailing_whitespace_on_delimiters() {
        let raw = "---  \ntype: Metric\n---\t\nbody";
        assert!(split_frontmatter(raw).is_some());
    }

    #[test]
    fn rejects_four_dashes() {
        assert!(split_frontmatter("----\ntype: Metric\n----\nbody").is_none());
    }

    #[test]
    fn rejects_an_indented_delimiter() {
        assert!(split_frontmatter(" ---\ntype: Metric\n---\nbody").is_none());
    }

    #[test]
    fn rejects_toml_frontmatter() {
        // Out of scope: a `+++` block stays fully raw under D33.
        assert!(split_frontmatter("+++\ntype = 'Metric'\n+++\nbody").is_none());
    }

    #[test]
    fn rejects_an_unterminated_block() {
        // An opening thematic break is not a frontmatter block.
        assert!(split_frontmatter("---\ntype: Metric\nbody with no close\n").is_none());
    }

    #[test]
    fn rejects_a_lone_delimiter_line() {
        assert!(split_frontmatter("---").is_none());
        assert!(split_frontmatter("---\n").is_none());
    }

    #[test]
    fn empty_block_yields_empty_yaml() {
        let raw = "---\n---\nbody";
        let s = split_frontmatter(raw).unwrap();
        assert_eq!(s.yaml, "");
        assert_eq!(&raw[s.body_start..], "body");
    }

    #[test]
    fn frontmatter_only_document_has_body_start_at_end() {
        let raw = "---\ntype: Metric\n---\n";
        let s = split_frontmatter(raw).unwrap();
        assert_eq!(s.body_start, raw.len());
        assert_eq!(&raw[s.body_start..], "");
    }

    #[test]
    fn frontmatter_only_document_without_trailing_newline() {
        let raw = "---\ntype: Metric\n---";
        let s = split_frontmatter(raw).unwrap();
        assert_eq!(s.body_start, raw.len());
    }

    #[test]
    fn a_later_thematic_break_is_irrelevant() {
        let raw = "---\ntype: Metric\n---\nintro\n\n---\n\nmore\n";
        let s = split_frontmatter(raw).unwrap();
        assert_eq!(s.yaml, "type: Metric\n");
        assert!(raw[s.body_start..].starts_with("intro"));
    }

    #[test]
    fn the_first_close_delimiter_wins() {
        let raw = "---\ntype: Metric\n---\n```\n---\n```\n";
        let s = split_frontmatter(raw).unwrap();
        assert_eq!(s.yaml, "type: Metric\n");
    }

    #[test]
    fn multibyte_content_keeps_char_boundaries() {
        let raw = "---\ntitle: caf\u{e9} \u{1f680}\n---\nbody \u{1f600}";
        let s = split_frontmatter(raw).unwrap();
        assert!(raw.is_char_boundary(s.yaml_start));
        assert!(raw.is_char_boundary(s.yaml_end));
        assert!(raw.is_char_boundary(s.body_start));
        assert_eq!(&raw[s.body_start..], "body \u{1f600}");
    }

    #[test]
    fn empty_input_is_not_frontmatter() {
        assert!(split_frontmatter("").is_none());
    }

    #[test]
    fn plain_markdown_is_not_frontmatter() {
        assert!(split_frontmatter("# Heading\n\ntext\n").is_none());
    }
}

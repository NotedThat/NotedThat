//! The reserved OKF files: `index.md` and `log.md`.
//!
//! Both parsers are **total** — they never fail. OKF §11 requires consumers to
//! tolerate deviations from the conventional shapes, so a line that does not fit
//! is recorded as a [`Deviation`] and the rest of the file is still read.
//!
//! # Why these are line-based rather than event-based
//!
//! `index.md` and `log.md` are not general Markdown: OKF fixes their bullet and
//! heading shapes exactly. More importantly, the editors below must **splice
//! individual lines and leave every other byte untouched**, because an author's
//! prose between sections has to survive a machine edit. Reconstructing the file
//! from parser events would quietly destroy it, so the parse and the edit share
//! one line-oriented view of the file.

use crate::frontmatter::split_frontmatter;

/// The reserved file name for a directory listing.
pub const INDEX_FILE: &str = "index.md";

/// The reserved file name for a directory change log.
pub const LOG_FILE: &str = "log.md";

/// Whether an object key names a reserved OKF file.
#[must_use]
pub fn is_reserved(key: &str) -> bool {
    let name = key.rsplit('/').next().unwrap_or(key);
    name == INDEX_FILE || name == LOG_FILE
}

/// A tolerated departure from a conventional shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deviation {
    /// 1-based line number.
    pub line: u32,
    /// What was unexpected.
    pub message: String,
}

/// A parsed `index.md`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexDoc {
    /// The bundle's declared OKF version. Legal only in the bundle-root file.
    pub okf_version: Option<String>,
    /// Whether the file carried a frontmatter block at all.
    pub has_frontmatter: bool,
    /// Whether that frontmatter carried keys other than `okf_version`.
    pub has_foreign_frontmatter_keys: bool,
    /// Sections, in document order.
    pub sections: Vec<IndexSection>,
    /// Tolerated departures from the conventional shape.
    pub deviations: Vec<Deviation>,
}

/// One `# Heading` section of an `index.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSection {
    /// The heading text.
    pub heading: String,
    /// 1-based line number of the heading.
    pub line: u32,
    /// The entries listed under it.
    pub entries: Vec<IndexEntry>,
}

/// One `* [Title](url) - description` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// Link text.
    pub title: String,
    /// Link target, exactly as written.
    pub url: String,
    /// The trailing description, when present.
    pub description: Option<String>,
    /// Whether the target names a directory (a trailing `/`).
    pub is_directory: bool,
    /// 1-based line number.
    pub line: u32,
}

/// A parsed `log.md`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogDoc {
    /// The `# ` title, when present.
    pub title: Option<String>,
    /// Dated groups, in document order (OKF writes them newest first).
    pub days: Vec<LogDay>,
    /// Tolerated departures from the conventional shape.
    pub deviations: Vec<Deviation>,
}

/// One `## YYYY-MM-DD` group of log entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogDay {
    /// The date heading text, verbatim.
    pub date: String,
    /// 1-based line number of the heading.
    pub line: u32,
    /// Bullet lines under it, without the leading marker.
    pub entries: Vec<String>,
}

/// Parse an `index.md`. Never fails.
#[must_use]
pub fn parse_index(raw: &str) -> IndexDoc {
    let mut doc = IndexDoc::default();

    let (body, first_body_line) = match split_frontmatter(raw) {
        Some(split) => {
            doc.has_frontmatter = true;
            read_index_frontmatter(split.yaml, &mut doc);
            (
                &raw[split.body_start..],
                u32::try_from(raw[..split.body_start].lines().count()).unwrap_or(u32::MAX) + 1,
            )
        }
        None => (raw, 1),
    };

    for (offset, line) in body.lines().enumerate() {
        let lineno = first_body_line.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
        let trimmed = line.trim();

        if let Some(heading) = trimmed.strip_prefix("# ") {
            doc.sections.push(IndexSection {
                heading: heading.trim().to_string(),
                line: lineno,
                entries: Vec::new(),
            });
            continue;
        }

        let Some(bullet) = strip_bullet(trimmed) else {
            continue;
        };

        match parse_index_entry(bullet, lineno) {
            Some(entry) => match doc.sections.last_mut() {
                Some(section) => section.entries.push(entry),
                None => doc.deviations.push(Deviation {
                    line: lineno,
                    message: "entry appears before any `# Section` heading".into(),
                }),
            },
            None => doc.deviations.push(Deviation {
                line: lineno,
                message: "expected `* [Title](url) - description`".into(),
            }),
        }
    }

    doc
}

fn read_index_frontmatter(yaml: &str, doc: &mut IndexDoc) {
    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            doc.has_foreign_frontmatter_keys = true;
            continue;
        };
        if key.trim() == "okf_version" {
            doc.okf_version = Some(value.trim().trim_matches(['"', '\'']).to_string());
        } else {
            doc.has_foreign_frontmatter_keys = true;
        }
    }
}

/// Strip a list marker (`*`, `-`, `+`) from a trimmed line.
fn strip_bullet(trimmed: &str) -> Option<&str> {
    for marker in ["* ", "- ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some(rest.trim_start());
        }
    }
    None
}

fn parse_index_entry(bullet: &str, line: u32) -> Option<IndexEntry> {
    let rest = bullet.strip_prefix('[')?;
    let (title, rest) = rest.split_once("](")?;
    let (url, rest) = rest.split_once(')')?;

    let description = rest
        .trim_start()
        .strip_prefix('-')
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());

    Some(IndexEntry {
        title: title.trim().to_string(),
        url: url.trim().to_string(),
        description,
        is_directory: url.trim().ends_with('/'),
        line,
    })
}

/// Parse a `log.md`. Never fails.
#[must_use]
pub fn parse_log(raw: &str) -> LogDoc {
    let mut doc = LogDoc::default();

    for (offset, line) in raw.lines().enumerate() {
        let lineno = u32::try_from(offset).unwrap_or(u32::MAX).saturating_add(1);
        let trimmed = line.trim();

        if let Some(date) = trimmed.strip_prefix("## ") {
            let date = date.trim().to_string();
            if !is_iso_date(&date) {
                doc.deviations.push(Deviation {
                    line: lineno,
                    message: "date heading is not `YYYY-MM-DD`".into(),
                });
            }
            doc.days.push(LogDay {
                date,
                line: lineno,
                entries: Vec::new(),
            });
            continue;
        }

        if let Some(title) = trimmed.strip_prefix("# ") {
            if doc.title.is_none() {
                doc.title = Some(title.trim().to_string());
            }
            continue;
        }

        if let Some(bullet) = strip_bullet(trimmed) {
            match doc.days.last_mut() {
                Some(day) => day.entries.push(bullet.to_string()),
                None => doc.deviations.push(Deviation {
                    line: lineno,
                    message: "entry appears before any `## YYYY-MM-DD` heading".into(),
                }),
            }
        }
    }

    doc
}

/// Shape check for `YYYY-MM-DD`, without pulling in a date library.
#[must_use]
pub fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && [0, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|&i| b[i].is_ascii_digit())
}

/// Render one index bullet.
#[must_use]
pub fn render_index_entry(title: &str, url: &str, description: Option<&str>) -> String {
    match description.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => format!("* [{title}]({url}) - {d}"),
        None => format!("* [{title}]({url})"),
    }
}

/// Insert or update one entry in an `index.md`, returning the new file.
///
/// An entry is identified by its **URL**, not by its rendered text, so a retitled
/// concept updates its line instead of gaining a second one. The edit is a line
/// splice: every byte this function does not own is preserved exactly, including
/// prose an author wrote between sections.
///
/// `section` names the section to file a new entry under — conventionally the
/// concept's OKF `type`. It is created at the end of the file when absent.
///
/// Idempotent: running it twice over the same input produces identical output.
#[must_use]
pub fn upsert_entry(raw: &str, section: &str, entry: &IndexEntry) -> String {
    let rendered = render_index_entry(&entry.title, &entry.url, entry.description.as_deref());
    let mut lines: Vec<String> = raw.lines().map(ToOwned::to_owned).collect();

    // Replace in place when the URL is already listed anywhere in the file.
    let doc = parse_index(raw);
    for existing in doc.sections.iter().flat_map(|s| &s.entries) {
        if existing.url == entry.url {
            let idx = (existing.line as usize).saturating_sub(1);
            if let Some(slot) = lines.get_mut(idx) {
                slot.clone_from(&rendered);
                return join_preserving_trailing_newline(&lines, raw);
            }
        }
    }

    // Otherwise append to the matching section, creating it if needed.
    if let Some(target) = doc
        .sections
        .iter()
        .find(|s| s.heading.eq_ignore_ascii_case(section))
    {
        let insert_at = target
            .entries
            .last()
            .map_or(target.line as usize, |e| e.line as usize);
        lines.insert(insert_at.min(lines.len()), rendered);
        return join_preserving_trailing_newline(&lines, raw);
    }

    if !lines.is_empty() && !lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.push(String::new());
    }
    lines.push(format!("# {section}"));
    lines.push(String::new());
    lines.push(rendered);
    join_preserving_trailing_newline(&lines, raw)
}

/// Remove the entry whose URL is `url`, returning the new file.
///
/// A section heading is **never** removed, even when it becomes empty: there is
/// no way to know whether the author wrote it. Idempotent.
#[must_use]
pub fn remove_entry(raw: &str, url: &str) -> String {
    let doc = parse_index(raw);
    let Some(target) = doc
        .sections
        .iter()
        .flat_map(|s| &s.entries)
        .find(|e| e.url == url)
    else {
        return raw.to_string();
    };
    let mut lines: Vec<String> = raw.lines().map(ToOwned::to_owned).collect();
    let idx = (target.line as usize).saturating_sub(1);
    if idx < lines.len() {
        lines.remove(idx);
    }
    join_preserving_trailing_newline(&lines, raw)
}

/// Append `bullet` under today's `## <date>` heading in a `log.md`.
///
/// Creates the title and the date heading when absent, keeping dates newest
/// first. Idempotent: an identical bullet already present under that date is not
/// duplicated.
#[must_use]
pub fn append_log_entry(raw: &str, date: &str, bullet: &str) -> String {
    let doc = parse_log(raw);
    let rendered = format!("* {}", bullet.trim());

    if let Some(day) = doc.days.iter().find(|d| d.date == date) {
        if day.entries.iter().any(|e| format!("* {e}") == rendered) {
            return raw.to_string();
        }
        let mut lines: Vec<String> = raw.lines().map(ToOwned::to_owned).collect();
        // Insert directly after the date heading so newest-first holds within a day too.
        let insert_at = (day.line as usize).min(lines.len());
        lines.insert(insert_at, rendered);
        return join_preserving_trailing_newline(&lines, raw);
    }

    let mut lines: Vec<String> = raw.lines().map(ToOwned::to_owned).collect();
    if doc.title.is_none() {
        lines.insert(0, "# Directory Update Log".to_string());
        lines.insert(1, String::new());
    }
    // Newest first: the new date heading goes above every existing one.
    let insert_at = doc
        .days
        .first()
        .map_or(lines.len(), |d| (d.line as usize).saturating_sub(1));
    let insert_at = insert_at.min(lines.len());
    lines.insert(insert_at, format!("## {date}"));
    lines.insert(insert_at + 1, rendered);
    lines.insert(insert_at + 2, String::new());
    join_preserving_trailing_newline(&lines, raw)
}

fn join_preserving_trailing_newline(lines: &[String], original: &str) -> String {
    let mut out = lines.join("\n");
    if original.is_empty() || original.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = "# Tables\n\n\
        * [Customers](./customers.md) - Customer master table\n\
        * [Orders](./orders.md)\n\
        * [Archive](archive/) - Older tables\n\n\
        # Metrics\n\n\
        * [Revenue](./revenue.md) - Daily revenue\n";

    #[test]
    fn is_reserved_matches_only_the_file_name() {
        assert!(is_reserved("index.md"));
        assert!(is_reserved("a/b/log.md"));
        assert!(!is_reserved("indexed.md"));
        assert!(!is_reserved("a/index.md.bak"));
    }

    #[test]
    fn parses_sections_and_entries() {
        let doc = parse_index(INDEX);
        assert_eq!(doc.sections.len(), 2);
        assert_eq!(doc.sections[0].heading, "Tables");
        assert_eq!(doc.sections[0].entries.len(), 3);
        assert_eq!(doc.sections[1].entries[0].title, "Revenue");
    }

    #[test]
    fn parses_description_and_directory_flags() {
        let doc = parse_index(INDEX);
        let tables = &doc.sections[0].entries;
        assert_eq!(
            tables[0].description.as_deref(),
            Some("Customer master table")
        );
        assert_eq!(tables[1].description, None);
        assert!(tables[2].is_directory);
        assert!(!tables[0].is_directory);
    }

    #[test]
    fn records_line_numbers() {
        let doc = parse_index(INDEX);
        assert_eq!(doc.sections[0].line, 1);
        assert_eq!(doc.sections[0].entries[0].line, 3);
    }

    #[test]
    fn a_garbage_bullet_is_a_deviation_not_a_failure() {
        let doc = parse_index("# Tables\n\n* not a link at all\n");
        assert_eq!(doc.sections.len(), 1);
        assert_eq!(doc.deviations.len(), 1);
        assert_eq!(doc.deviations[0].line, 3);
    }

    #[test]
    fn a_bullet_before_any_heading_is_a_deviation() {
        let doc = parse_index("* [A](./a.md)\n");
        assert_eq!(doc.deviations.len(), 1);
        assert!(doc.deviations[0].message.contains("before any"));
    }

    #[test]
    fn root_okf_version_is_read_from_frontmatter() {
        let doc = parse_index("---\nokf_version: \"0.2\"\n---\n\n# Tables\n");
        assert_eq!(doc.okf_version.as_deref(), Some("0.2"));
        assert!(doc.has_frontmatter);
        assert!(!doc.has_foreign_frontmatter_keys);
    }

    #[test]
    fn foreign_frontmatter_keys_are_flagged() {
        let doc = parse_index("---\nokf_version: \"0.2\"\ntype: Index\n---\n# T\n");
        assert!(doc.has_foreign_frontmatter_keys);
    }

    #[test]
    fn line_numbers_account_for_frontmatter() {
        let doc = parse_index("---\nokf_version: \"0.2\"\n---\n# Tables\n\n* [A](./a.md)\n");
        assert_eq!(doc.sections[0].line, 4);
        assert_eq!(doc.sections[0].entries[0].line, 6);
    }

    #[test]
    fn dash_and_plus_markers_are_accepted() {
        let doc = parse_index("# T\n\n- [A](./a.md)\n+ [B](./b.md)\n");
        assert_eq!(doc.sections[0].entries.len(), 2);
    }

    #[test]
    fn upsert_replaces_by_url_not_by_text() {
        let entry = IndexEntry {
            title: "Customer Master".into(),
            url: "./customers.md".into(),
            description: Some("Renamed".into()),
            is_directory: false,
            line: 0,
        };
        let out = upsert_entry(INDEX, "Tables", &entry);
        assert!(out.contains("* [Customer Master](./customers.md) - Renamed"));
        assert!(!out.contains("Customer master table"));
        // Exactly one line still references the URL.
        assert_eq!(out.matches("./customers.md").count(), 1);
    }

    #[test]
    fn upsert_is_idempotent() {
        let entry = IndexEntry {
            title: "New".into(),
            url: "./new.md".into(),
            description: Some("A new thing".into()),
            is_directory: false,
            line: 0,
        };
        let once = upsert_entry(INDEX, "Tables", &entry);
        let twice = upsert_entry(&once, "Tables", &entry);
        assert_eq!(once, twice);
    }

    #[test]
    fn upsert_preserves_unknown_prose() {
        let raw = "# Tables\n\nSome prose the author wrote.\n\n* [A](./a.md)\n";
        let entry = IndexEntry {
            title: "B".into(),
            url: "./b.md".into(),
            description: None,
            is_directory: false,
            line: 0,
        };
        let out = upsert_entry(raw, "Tables", &entry);
        assert!(out.contains("Some prose the author wrote."));
    }

    #[test]
    fn upsert_creates_a_missing_section() {
        let entry = IndexEntry {
            title: "Runbook".into(),
            url: "./rb.md".into(),
            description: None,
            is_directory: false,
            line: 0,
        };
        let out = upsert_entry(INDEX, "Playbooks", &entry);
        assert!(out.contains("# Playbooks"));
        assert!(out.contains("* [Runbook](./rb.md)"));
    }

    #[test]
    fn upsert_into_an_empty_file_creates_everything() {
        let entry = IndexEntry {
            title: "A".into(),
            url: "./a.md".into(),
            description: None,
            is_directory: false,
            line: 0,
        };
        let out = upsert_entry("", "Tables", &entry);
        assert!(out.contains("# Tables"));
        assert!(out.contains("* [A](./a.md)"));
    }

    #[test]
    fn remove_drops_the_entry_but_keeps_the_heading() {
        let out = remove_entry(INDEX, "./revenue.md");
        assert!(!out.contains("./revenue.md"));
        assert!(out.contains("# Metrics"));
    }

    #[test]
    fn remove_is_idempotent_and_a_no_op_when_absent() {
        let once = remove_entry(INDEX, "./revenue.md");
        assert_eq!(remove_entry(&once, "./revenue.md"), once);
        assert_eq!(remove_entry(INDEX, "./nope.md"), INDEX);
    }

    #[test]
    fn parses_a_log() {
        let raw = "# Directory Update Log\n\n## 2026-05-22\n\
                   * **Update**: Added [a](./a.md).\n\n## 2026-05-15\n* **Initialization**: Created.\n";
        let doc = parse_log(raw);
        assert_eq!(doc.title.as_deref(), Some("Directory Update Log"));
        assert_eq!(doc.days.len(), 2);
        assert_eq!(doc.days[0].date, "2026-05-22");
        assert_eq!(doc.days[0].entries.len(), 1);
    }

    #[test]
    fn a_malformed_date_is_a_deviation() {
        let doc = parse_log("## May 22nd\n* thing\n");
        assert_eq!(doc.deviations.len(), 1);
        assert_eq!(doc.days.len(), 1);
    }

    #[test]
    fn iso_date_shape_check() {
        assert!(is_iso_date("2026-05-22"));
        assert!(!is_iso_date("2026-5-22"));
        assert!(!is_iso_date("22-05-2026x"));
        assert!(!is_iso_date(""));
    }

    #[test]
    fn append_log_creates_title_and_heading() {
        let out = append_log_entry("", "2026-05-22", "**Update**: Added [a](./a.md).");
        assert!(out.starts_with("# Directory Update Log"));
        assert!(out.contains("## 2026-05-22"));
        assert!(out.contains("* **Update**: Added [a](./a.md)."));
    }

    #[test]
    fn append_log_is_idempotent_within_a_day() {
        let once = append_log_entry("", "2026-05-22", "**Update**: Added [a](./a.md).");
        let twice = append_log_entry(&once, "2026-05-22", "**Update**: Added [a](./a.md).");
        assert_eq!(once, twice);
    }

    #[test]
    fn append_log_puts_a_new_date_above_older_ones() {
        let existing = "# Directory Update Log\n\n## 2026-05-15\n* old\n";
        let out = append_log_entry(existing, "2026-05-22", "new");
        let newer = out.find("## 2026-05-22").unwrap();
        let older = out.find("## 2026-05-15").unwrap();
        assert!(newer < older, "newest date must come first:\n{out}");
    }

    #[test]
    fn append_log_adds_to_an_existing_day() {
        let existing = "# Directory Update Log\n\n## 2026-05-22\n* first\n";
        let out = append_log_entry(existing, "2026-05-22", "second");
        assert_eq!(out.matches("## 2026-05-22").count(), 1);
        assert!(out.contains("* second"));
        assert!(out.contains("* first"));
    }
}

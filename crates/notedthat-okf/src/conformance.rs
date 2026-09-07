//! OKF v0.2 §11 conformance checking for a single object.
//!
//! # Severity is where a validator can contradict the spec it validates
//!
//! §11 lists broken links, unknown `type` values, unknown extra keys and a
//! missing `index.md` as things a consumer **must tolerate**. Every one of them
//! is therefore a [`Severity::Warning`] here. Only the three conformance clauses
//! produce a [`Severity::Error`], and a bundle is conformant exactly when it has
//! no errors.

use crate::parse::{OkfParseError, parse_document};
use crate::reserved::{INDEX_FILE, LOG_FILE, parse_index, parse_log};
use pulldown_cmark::{Event, Parser, Tag};

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// A tolerated departure. Never affects conformance.
    Warning,
    /// Breaks an OKF §11 conformance clause.
    Error,
}

impl Severity {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// One conformance observation about one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Stable machine-readable rule identifier.
    pub rule: &'static str,
    /// Whether this breaks conformance.
    pub severity: Severity,
    /// 1-based line number, when the finding is anchored to one.
    pub line: Option<u32>,
    /// Human-readable explanation.
    pub message: String,
}

impl Finding {
    fn error(rule: &'static str, message: impl Into<String>) -> Self {
        Self {
            rule,
            severity: Severity::Error,
            line: None,
            message: message.into(),
        }
    }

    fn warning(rule: &'static str, line: Option<u32>, message: impl Into<String>) -> Self {
        Self {
            rule,
            severity: Severity::Warning,
            line,
            message: message.into(),
        }
    }
}

/// Check one object's OKF conformance.
///
/// `key` is the object key relative to the knowledge base root, used to tell a
/// reserved file from a concept and to tell the bundle-root `index.md` from a
/// nested one. Never fails: everything it finds is returned as data.
#[must_use]
pub fn check_object(key: &str, raw: &str) -> Vec<Finding> {
    let name = key.rsplit('/').next().unwrap_or(key);
    let at_bundle_root = !key.contains('/');

    match name {
        INDEX_FILE => check_index(raw, at_bundle_root),
        LOG_FILE => check_log(raw),
        _ => check_concept(raw),
    }
}

fn check_concept(raw: &str) -> Vec<Finding> {
    match parse_document(raw) {
        Ok(_) => Vec::new(),
        Err(OkfParseError::NoFrontmatter) => vec![Finding::error(
            "frontmatter_missing",
            "document has no leading YAML frontmatter block (OKF v0.2 §11 clause 1)",
        )],
        Err(OkfParseError::TooLarge) => vec![Finding::error(
            "frontmatter_unparseable",
            "frontmatter block is too large to parse",
        )],
        Err(OkfParseError::Yaml(detail)) => vec![Finding::error(
            "frontmatter_unparseable",
            format!("frontmatter did not parse as a YAML mapping: {detail}"),
        )],
        Err(OkfParseError::MissingType) => vec![Finding::error(
            "type_missing",
            "frontmatter has no non-empty string `type` (OKF v0.2 §11 clause 2)",
        )],
    }
}

fn check_index(raw: &str, at_bundle_root: bool) -> Vec<Finding> {
    let doc = parse_index(raw);
    let mut findings = Vec::new();

    if doc.has_frontmatter && !at_bundle_root {
        findings.push(Finding::error(
            "index_frontmatter_forbidden",
            "only the bundle-root `index.md` may carry frontmatter (OKF v0.2 §11 clause 3)",
        ));
    } else if doc.has_foreign_frontmatter_keys {
        findings.push(Finding::error(
            "index_frontmatter_forbidden",
            "`okf_version` is the only frontmatter key permitted in an `index.md`",
        ));
    }

    for deviation in &doc.deviations {
        let rule = if deviation.message.contains("before any") {
            "index_bullet_outside_section"
        } else {
            "index_malformed_entry"
        };
        findings.push(Finding::warning(
            rule,
            Some(deviation.line),
            deviation.message.clone(),
        ));
    }

    findings
}

fn check_log(raw: &str) -> Vec<Finding> {
    parse_log(raw)
        .deviations
        .iter()
        .map(|deviation| {
            let rule = if deviation.message.contains("before any") {
                "log_entry_outside_date"
            } else {
                "log_malformed_date"
            };
            Finding::warning(rule, Some(deviation.line), deviation.message.clone())
        })
        .collect()
}

/// A link found in a document body, with the line it appeared on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundLink {
    /// The link target exactly as written.
    pub target: String,
    /// 1-based line number.
    pub line: u32,
}

/// Collect every link destination in a Markdown document.
///
/// Resolution and existence checking are the caller's job — this crate does no
/// I/O. Autolinks and image destinations are included, since both can point at a
/// concept.
#[must_use]
pub fn collect_links(raw: &str) -> Vec<FoundLink> {
    let mut links = Vec::new();
    for (event, range) in Parser::new(raw).into_offset_iter() {
        let Event::Start(Tag::Link { dest_url: dest, .. } | Tag::Image { dest_url: dest, .. }) =
            event
        else {
            continue;
        };
        if dest.is_empty() {
            continue;
        }
        links.push(FoundLink {
            target: dest.to_string(),
            line: line_of(raw, range.start),
        });
    }
    links
}

/// Build a broken-link warning.
///
/// Broken links are warnings, never errors: OKF §11 requires consumers to
/// tolerate them.
#[must_use]
pub fn broken_link(link: &FoundLink) -> Finding {
    Finding::warning(
        "broken_link",
        Some(link.line),
        format!(
            "link target `{}` does not exist in this knowledge base",
            link.target
        ),
    )
}

/// Build a finding for an object that could not be read as UTF-8.
#[must_use]
pub fn non_utf8() -> Finding {
    Finding::error("non_utf8", "object is not valid UTF-8 and cannot be parsed")
}

/// Build a finding for an object skipped because it exceeds the size cap.
#[must_use]
pub fn object_too_large(size: u64, cap: u64) -> Finding {
    Finding::warning(
        "object_too_large",
        None,
        format!("object is {size} bytes, above the {cap}-byte inspection cap; not checked"),
    )
}

/// 1-based line number of a byte offset.
fn line_of(raw: &str, offset: usize) -> u32 {
    let upto = &raw[..offset.min(raw.len())];
    u32::try_from(upto.bytes().filter(|b| *b == b'\n').count()).unwrap_or(u32::MAX) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.rule).collect()
    }

    #[test]
    fn a_conformant_concept_has_no_findings() {
        assert!(
            check_object(
                "tables/customers.md",
                "---\ntype: BigQuery Table\n---\n# X\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn missing_frontmatter_is_an_error() {
        let f = check_object("a.md", "# Just markdown\n");
        assert_eq!(rules(&f), vec!["frontmatter_missing"]);
        assert_eq!(f[0].severity, Severity::Error);
    }

    #[test]
    fn missing_type_is_an_error() {
        let f = check_object("a.md", "---\ntitle: x\n---\nbody\n");
        assert_eq!(rules(&f), vec!["type_missing"]);
    }

    #[test]
    fn unparseable_frontmatter_is_an_error() {
        let f = check_object("a.md", "---\n- not a mapping\n---\nbody\n");
        assert_eq!(rules(&f), vec!["frontmatter_unparseable"]);
    }

    #[test]
    fn an_unknown_type_value_is_not_a_finding() {
        // OKF §11: consumers must tolerate unknown types.
        assert!(check_object("a.md", "---\ntype: Wibble\n---\n").is_empty());
    }

    #[test]
    fn unknown_extra_keys_are_not_a_finding() {
        assert!(check_object("a.md", "---\ntype: Metric\nfuture: 1\n---\n").is_empty());
    }

    #[test]
    fn reserved_files_are_not_checked_as_concepts() {
        // index.md has no `type` and that is correct, not an error.
        assert!(check_object("index.md", "# Tables\n\n* [A](./a.md)\n").is_empty());
        assert!(check_object("log.md", "# Directory Update Log\n").is_empty());
    }

    #[test]
    fn frontmatter_on_a_nested_index_is_an_error() {
        let f = check_object("tables/index.md", "---\nokf_version: \"0.2\"\n---\n# T\n");
        assert_eq!(rules(&f), vec!["index_frontmatter_forbidden"]);
    }

    #[test]
    fn okf_version_at_the_bundle_root_is_allowed() {
        assert!(check_object("index.md", "---\nokf_version: \"0.2\"\n---\n# T\n").is_empty());
    }

    #[test]
    fn a_foreign_key_in_root_index_frontmatter_is_an_error() {
        let f = check_object(
            "index.md",
            "---\nokf_version: \"0.2\"\ntype: Index\n---\n# T\n",
        );
        assert_eq!(rules(&f), vec!["index_frontmatter_forbidden"]);
    }

    #[test]
    fn a_malformed_index_entry_is_a_warning() {
        let f = check_object("index.md", "# T\n\n* not a link\n");
        assert_eq!(rules(&f), vec!["index_malformed_entry"]);
        assert_eq!(f[0].severity, Severity::Warning);
        assert_eq!(f[0].line, Some(3));
    }

    #[test]
    fn a_bullet_before_a_heading_is_a_warning() {
        let f = check_object("index.md", "* [A](./a.md)\n");
        assert_eq!(rules(&f), vec!["index_bullet_outside_section"]);
    }

    #[test]
    fn a_malformed_log_date_is_a_warning() {
        let f = check_object("log.md", "## May 22nd\n* thing\n");
        assert_eq!(rules(&f), vec!["log_malformed_date"]);
        assert_eq!(f[0].severity, Severity::Warning);
    }

    #[test]
    fn broken_links_are_warnings_never_errors() {
        let link = FoundLink {
            target: "/missing.md".into(),
            line: 4,
        };
        assert_eq!(broken_link(&link).severity, Severity::Warning);
    }

    #[test]
    fn collects_link_targets_with_line_numbers() {
        let raw = "---\ntype: Metric\n---\nintro\n\nsee [orders](./orders.md) and [x](/a/b.md)\n";
        let links = collect_links(raw);
        let targets: Vec<&str> = links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, vec!["./orders.md", "/a/b.md"]);
        assert_eq!(links[0].line, 6);
    }

    #[test]
    fn collects_image_destinations_too() {
        let links = collect_links("![alt](./diagram.png)\n");
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn a_document_with_no_links_yields_none() {
        assert!(collect_links("# Heading\n\ntext\n").is_empty());
    }

    #[test]
    fn severity_orders_warning_below_error() {
        assert!(Severity::Warning < Severity::Error);
    }
}

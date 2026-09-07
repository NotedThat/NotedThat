//! Parsing OKF frontmatter into an [`OkfConcept`].
//!
//! # Why this does not derive `Deserialize`
//!
//! A derived struct hard-fails on a type mismatch anywhere in the document —
//! `tags: "a, b"` instead of a list, `usage_count: "12"` instead of an integer.
//! OKF §11 forbids rejecting a concept because an *optional* family is
//! ill-formed. So the YAML is first read into a self-describing
//! [`serde_json::Value`], and every field is then pulled out through helpers that
//! can only succeed or yield nothing. `type` is the single strict field.
//!
//! # Why the parser is budgeted
//!
//! Frontmatter is untrusted third-party content parsed on the indexer's hot path,
//! the indexing worker is serial, and nothing above it imposes a timeout — an
//! alias bomb in one document would stall indexing for a whole knowledge base.
//! This is the concrete reason `serde-saphyr` was chosen, so the budget is used.

use crate::concept::{
    OkfAttester, OkfComputation, OkfConcept, OkfExecutor, OkfGenerated, OkfParameter, OkfSource,
    OkfVerification, OkfWindow,
};
use crate::frontmatter::{FrontmatterSplit, split_frontmatter};
use crate::instant::instant;
use notedthat_core::okf::{Actor, OkfStatus};
use serde_json::{Map, Value};

/// Maximum frontmatter block size accepted for parsing.
pub const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;

/// Why a document is not an OKF concept.
///
/// Never a hard failure for the caller: an `Err` means "fall back to the D33 raw
/// path", not "reject this document".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OkfParseError {
    /// No leading frontmatter block, or the block was never closed.
    #[error("no leading frontmatter block, or the block was unterminated")]
    NoFrontmatter,
    /// The block is larger than [`MAX_FRONTMATTER_BYTES`].
    #[error("frontmatter exceeds {MAX_FRONTMATTER_BYTES} bytes")]
    TooLarge,
    /// The YAML did not parse, or did not parse to a mapping.
    #[error("frontmatter is not a YAML mapping: {0}")]
    Yaml(String),
    /// OKF §11 clause 2: every concept needs a non-empty `type`.
    #[error("frontmatter has no non-empty string `type` (OKF v0.2 §11)")]
    MissingType,
}

/// Parse a document's leading frontmatter into an [`OkfConcept`].
///
/// Returns the concept and the frontmatter split, so the caller has the absolute
/// `body_start` needed to chunk the body without disturbing byte offsets.
///
/// Performs no I/O and never panics.
///
/// # Errors
///
/// Returns [`OkfParseError`] when the document is not an OKF concept. The caller
/// must then index it exactly as it would have before D48.
pub fn parse_document(raw: &str) -> Result<(OkfConcept, FrontmatterSplit<'_>), OkfParseError> {
    let split = split_frontmatter(raw).ok_or(OkfParseError::NoFrontmatter)?;
    let concept = parse_frontmatter_yaml(split.yaml)?;
    Ok((concept, split))
}

/// Parse just the YAML text of a frontmatter block.
///
/// # Errors
///
/// Returns [`OkfParseError`] when the block is oversized, unparseable, not a
/// mapping, or carries no usable `type`.
pub fn parse_frontmatter_yaml(yaml: &str) -> Result<OkfConcept, OkfParseError> {
    if yaml.len() > MAX_FRONTMATTER_BYTES {
        return Err(OkfParseError::TooLarge);
    }

    let value: Value = serde_saphyr::from_str_with_options(yaml, parser_options())
        .map_err(|e| OkfParseError::Yaml(e.to_string()))?;
    let map = value
        .as_object()
        .ok_or_else(|| OkfParseError::Yaml("top-level value is not a mapping".to_string()))?;

    // `type` is the only strict field. OKF §11 clause 2, read literally.
    let concept_type = get_str(map, "type")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or(OkfParseError::MissingType)?;

    Ok(OkfConcept {
        concept_type,
        title: get_str(map, "title"),
        description: get_str(map, "description"),
        resource: get_str(map, "resource"),
        tags: get_str_list(map, "tags"),
        sources: get_map_list(map, "sources")
            .into_iter()
            .filter_map(source_from)
            .collect(),
        usage_window: get_map(map, "usage_window").map(window_from),
        generated: get_map(map, "generated").map(generated_from),
        verified: get_map_list(map, "verified")
            .into_iter()
            .map(verification_from)
            .collect(),
        status: get_str(map, "status").and_then(|s| OkfStatus::parse(&s)),
        stale_after: get_str(map, "stale_after").map(|s| instant(&s)),
        computation: computation_from(map),
    })
}

/// Budgeted, non-erroring parser options for untrusted frontmatter.
///
/// `serde_saphyr::Options` and `Budget` are `#[non_exhaustive]`, so these are
/// built by mutating the defaults rather than with a struct literal.
fn parser_options() -> serde_saphyr::Options {
    // Frontmatter is small by construction; these sit far below the crate
    // defaults, which are sized for whole configuration files.
    let mut budget = serde_saphyr::Budget::default();
    budget.max_events = 20_000;
    budget.max_nodes = 10_000;
    budget.max_depth = 32;
    budget.max_documents = 1;
    budget.max_aliases = 100;
    budget.max_anchors = 100;
    budget.max_total_scalar_bytes = MAX_FRONTMATTER_BYTES;

    let mut options = serde_saphyr::Options::default();
    options.budget = Some(budget);
    // A duplicate key must not cost the whole document.
    options.duplicate_keys = serde_saphyr::DuplicateKeyPolicy::LastWins;
    options
}

fn source_from(m: &Map<String, Value>) -> Option<OkfSource> {
    // `resource` is required within a `sources` entry; an entry without one is
    // dropped, but the concept itself stays conformant.
    Some(OkfSource {
        resource: get_str(m, "resource")?,
        id: get_str(m, "id"),
        title: get_str(m, "title"),
        author: get_str(m, "author").map(Actor::new),
        usage_count: get_u64(m, "usage_count"),
        last_modified: get_str(m, "last_modified").map(|s| instant(&s)),
    })
}

fn window_from(m: &Map<String, Value>) -> OkfWindow {
    OkfWindow {
        from: get_str(m, "from").map(|s| instant(&s)),
        to: get_str(m, "to").map(|s| instant(&s)),
    }
}

fn generated_from(m: &Map<String, Value>) -> OkfGenerated {
    OkfGenerated {
        by: get_str(m, "by").map(Actor::new),
        at: get_str(m, "at").map(|s| instant(&s)),
    }
}

fn verification_from(m: &Map<String, Value>) -> OkfVerification {
    OkfVerification {
        by: get_str(m, "by").map(Actor::new),
        at: get_str(m, "at").map(|s| instant(&s)),
    }
}

fn computation_from(map: &Map<String, Value>) -> Option<OkfComputation> {
    let runtime = get_str(map, "runtime");
    let parameters: Vec<OkfParameter> = get_map_list(map, "parameters")
        .into_iter()
        .filter_map(|m| {
            Some(OkfParameter {
                name: get_str(m, "name")?,
                param_type: get_str(m, "type"),
                required: get_bool(m, "required"),
            })
        })
        .collect();
    let computation = get_str(map, "computation");
    let executor = get_map(map, "executor").map(|m| OkfExecutor {
        resource: get_str(m, "resource"),
        receipt: get_str_list(m, "receipt"),
    });
    let attester = get_map(map, "attester").map(|m| OkfAttester {
        resource: get_str(m, "resource"),
    });

    if runtime.is_none()
        && parameters.is_empty()
        && computation.is_none()
        && executor.is_none()
        && attester.is_none()
    {
        return None;
    }
    Some(OkfComputation {
        runtime,
        parameters,
        computation,
        executor,
        attester,
    })
}

// --- Total extraction helpers. Each can only succeed or yield nothing. ---

fn get_str(m: &Map<String, Value>, k: &str) -> Option<String> {
    m.get(k)?.as_str().map(ToOwned::to_owned)
}

/// String members of an array. A bare string is read as a one-element list;
/// non-string members are dropped rather than failing the document.
fn get_str_list(m: &Map<String, Value>, k: &str) -> Vec<String> {
    match m.get(k) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(ToOwned::to_owned))
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn get_map<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a Map<String, Value>> {
    m.get(k)?.as_object()
}

/// Mapping members of an array.
///
/// A **bare mapping is read as a one-element list**, which is OKF v0.2's explicit
/// rule for `verified` (§5.3) and costs nothing to apply uniformly.
fn get_map_list<'a>(m: &'a Map<String, Value>, k: &str) -> Vec<&'a Map<String, Value>> {
    match m.get(k) {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_object).collect(),
        Some(Value::Object(o)) => vec![o],
        _ => Vec::new(),
    }
}

fn get_u64(m: &Map<String, Value>, k: &str) -> Option<u64> {
    m.get(k)?.as_u64()
}

fn get_bool(m: &Map<String, Value>, k: &str) -> Option<bool> {
    m.get(k)?.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;
    use notedthat_core::okf::OkfTrust;

    fn parse(yaml: &str) -> Result<OkfConcept, OkfParseError> {
        parse_frontmatter_yaml(yaml)
    }

    #[test]
    fn minimal_concept_needs_only_a_type() {
        let c = parse("type: Metric\n").unwrap();
        assert_eq!(c.concept_type, "Metric");
    }

    #[test]
    fn missing_type_is_not_a_concept() {
        assert_eq!(parse("title: x\n"), Err(OkfParseError::MissingType));
    }

    #[test]
    fn empty_type_is_not_a_concept() {
        assert_eq!(parse("type: \"\"\n"), Err(OkfParseError::MissingType));
    }

    #[test]
    fn whitespace_only_type_is_not_a_concept() {
        assert_eq!(parse("type: \"   \"\n"), Err(OkfParseError::MissingType));
    }

    #[test]
    fn non_string_type_is_not_a_concept() {
        // OKF §11 clause 2 read literally: `type` must be a non-empty string.
        assert_eq!(parse("type: 42\n"), Err(OkfParseError::MissingType));
    }

    #[test]
    fn type_is_trimmed() {
        assert_eq!(
            parse("type: \"  Metric  \"\n").unwrap().concept_type,
            "Metric"
        );
    }

    #[test]
    fn unknown_type_values_are_accepted() {
        // No central registry — consumers must tolerate anything.
        assert_eq!(parse("type: Wibble\n").unwrap().concept_type, "Wibble");
    }

    #[test]
    fn unknown_keys_are_ignored_not_rejected() {
        let c = parse("type: Metric\nfuture_family: {a: 1}\n").unwrap();
        assert_eq!(c.concept_type, "Metric");
    }

    #[test]
    fn tags_as_a_list_are_kept() {
        let c = parse("type: Metric\ntags: [finance, core]\n").unwrap();
        assert_eq!(c.tags, vec!["finance", "core"]);
    }

    #[test]
    fn tags_as_a_bare_string_become_one_element() {
        let c = parse("type: Metric\ntags: finance\n").unwrap();
        assert_eq!(c.tags, vec!["finance"]);
    }

    #[test]
    fn non_string_tag_members_are_dropped_not_fatal() {
        let c = parse("type: Metric\ntags: [finance, 42, core]\n").unwrap();
        assert_eq!(c.tags, vec!["finance", "core"]);
    }

    #[test]
    fn verified_as_a_bare_mapping_becomes_one_element() {
        // OKF v0.2 §5.3 states this explicitly.
        let c = parse("type: Metric\nverified: {by: 'human:alice', at: '2026-01-01'}\n").unwrap();
        assert_eq!(c.verified.len(), 1);
        assert_eq!(c.trust(), OkfTrust::HumanReviewed);
    }

    #[test]
    fn verified_as_a_list_is_kept() {
        let c = parse("type: Metric\nverified:\n  - by: agent/1\n  - by: 'human:bob'\n").unwrap();
        assert_eq!(c.verified.len(), 2);
        assert_eq!(c.trust(), OkfTrust::HumanReviewed);
    }

    #[test]
    fn verified_as_a_scalar_yields_no_entries() {
        let c = parse("type: Metric\nverified: yes-please\n").unwrap();
        assert!(c.verified.is_empty());
        assert_eq!(c.trust(), OkfTrust::Unverified);
    }

    #[test]
    fn a_source_without_resource_is_dropped_but_the_concept_survives() {
        let c =
            parse("type: Metric\nsources:\n  - title: no resource\n  - resource: /a.md\n").unwrap();
        assert_eq!(c.sources.len(), 1);
        assert_eq!(c.sources[0].resource, "/a.md");
    }

    #[test]
    fn a_string_usage_count_yields_none_and_stays_conformant() {
        let c = parse("type: Metric\nsources:\n  - resource: /a.md\n    usage_count: \"12\"\n")
            .unwrap();
        assert_eq!(c.sources[0].usage_count, None);
    }

    #[test]
    fn wrong_case_status_falls_back_to_stable() {
        let c = parse("type: Metric\nstatus: Draft\n").unwrap();
        assert_eq!(c.status, None);
        assert_eq!(c.effective_status(), notedthat_core::okf::OkfStatus::Stable);
    }

    #[test]
    fn known_status_is_kept() {
        let c = parse("type: Metric\nstatus: deprecated\n").unwrap();
        assert_eq!(c.status, Some(OkfStatus::Deprecated));
    }

    #[test]
    fn stale_after_resolves_a_bare_date() {
        let c = parse("type: Metric\nstale_after: \"2026-12-31\"\n").unwrap();
        let sa = c.stale_after.unwrap();
        assert_eq!(sa.raw, "2026-12-31");
        assert!(sa.epoch_secs.is_some());
    }

    #[test]
    fn unparseable_stale_after_keeps_its_lexical_form() {
        let c = parse("type: Metric\nstale_after: someday\n").unwrap();
        let sa = c.stale_after.unwrap();
        assert_eq!(sa.raw, "someday");
        assert_eq!(sa.epoch_secs, None);
    }

    #[test]
    fn generated_family_is_extracted() {
        let c = parse("type: Metric\ngenerated:\n  by: agent/1.0\n  at: '2026-01-01'\n").unwrap();
        let g = c.generated.unwrap();
        assert_eq!(g.by.unwrap().as_str(), "agent/1.0");
        assert!(g.at.is_some());
    }

    #[test]
    fn attested_computation_family_is_extracted() {
        let yaml = "type: Attested Computation\n\
                    runtime: bigquery\n\
                    parameters:\n  - {name: start_date, type: date, required: true}\n\
                    computation: /computations/rev.sql\n\
                    executor: {resource: /executors/bq.md, receipt: [job_id, bytes_billed]}\n\
                    attester: {resource: /attesters/fin.md}\n";
        let c = parse(yaml).unwrap();
        let comp = c.computation.unwrap();
        assert_eq!(comp.runtime.as_deref(), Some("bigquery"));
        assert_eq!(comp.parameters.len(), 1);
        assert_eq!(comp.parameters[0].name, "start_date");
        assert_eq!(comp.parameters[0].required, Some(true));
        assert_eq!(comp.computation.as_deref(), Some("/computations/rev.sql"));
        assert_eq!(
            comp.executor.unwrap().receipt,
            vec!["job_id", "bytes_billed"]
        );
        assert_eq!(
            comp.attester.unwrap().resource.as_deref(),
            Some("/attesters/fin.md")
        );
    }

    #[test]
    fn computation_family_extracted_even_under_a_different_type() {
        // A producer may spell the type differently; discovery should still work.
        let c = parse("type: Metric\nruntime: dbt\n").unwrap();
        assert_eq!(c.runtime(), Some("dbt"));
    }

    #[test]
    fn no_computation_family_yields_none() {
        assert!(parse("type: Metric\n").unwrap().computation.is_none());
    }

    #[test]
    fn oversize_frontmatter_is_rejected_before_parsing() {
        let yaml = format!(
            "type: Metric\npadding: \"{}\"\n",
            "x".repeat(MAX_FRONTMATTER_BYTES)
        );
        assert_eq!(parse(&yaml), Err(OkfParseError::TooLarge));
    }

    #[test]
    fn a_non_mapping_document_is_a_yaml_error() {
        assert!(matches!(
            parse("- just\n- a list\n"),
            Err(OkfParseError::Yaml(_))
        ));
    }

    #[test]
    fn empty_frontmatter_is_a_yaml_error_not_a_panic() {
        assert!(matches!(parse(""), Err(OkfParseError::Yaml(_))));
    }

    #[test]
    fn duplicate_keys_do_not_lose_the_type() {
        let c = parse("type: Metric\ntitle: a\ntitle: b\n").unwrap();
        assert_eq!(c.concept_type, "Metric");
        assert_eq!(c.title.as_deref(), Some("b"));
    }

    #[test]
    fn an_alias_bomb_is_bounded_and_does_not_hang() {
        // Billion-laughs shape. The budget must stop it; the only requirement is
        // that this returns rather than exhausting memory.
        let mut yaml = String::from("type: Metric\na: &a [x, x, x, x, x, x, x, x, x]\n");
        for (i, prev) in ('b'..='h').zip('a'..='g') {
            use std::fmt::Write as _;
            let _ = writeln!(
                yaml,
                "{i}: &{i} [*{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}]"
            );
        }
        let _ = parse(&yaml);
    }

    #[test]
    fn deeply_nested_input_is_bounded() {
        let yaml = format!("type: Metric\nx: {}{}\n", "[".repeat(200), "]".repeat(200));
        let _ = parse(&yaml);
    }

    #[test]
    fn parse_document_reports_the_body_offset() {
        let raw = "---\ntype: Metric\n---\n# Body\n";
        let (c, split) = parse_document(raw).unwrap();
        assert_eq!(c.concept_type, "Metric");
        assert_eq!(&raw[split.body_start..], "# Body\n");
    }

    #[test]
    fn parse_document_without_frontmatter() {
        assert_eq!(
            parse_document("# Just markdown\n").unwrap_err(),
            OkfParseError::NoFrontmatter
        );
    }
}

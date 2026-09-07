//! Document metadata as a separately indexed unit.
//!
//! A document's *body* and its *metadata* answer different questions, so they are
//! indexed as different points. A metadata point carries the frontmatter's real
//! byte range — so it still dereferences to stored bytes under D6 — while its
//! dense vector and BM25 document are built from a rendered prose form of the
//! fields, which is what makes metadata independently **searchable** rather than
//! merely filterable.
//!
//! [`MetadataExtractor`] is the general mechanism; [`OkfExtractor`] is the first
//! implementation. See SPECIFICATIONS.md D48.

use notedthat_okf::OkfConcept;
use qdrant_client::qdrant::Value;

/// Payload value of `chunk_kind` for an ordinary body chunk.
pub const CHUNK_KIND_BODY: &str = "body";

/// Payload value of `chunk_kind` for a document's metadata point.
pub const CHUNK_KIND_METADATA: &str = "metadata";

/// Metadata extracted from one document.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentMetadata {
    /// Absolute byte offset where the metadata block begins in the stored object.
    pub byte_start: usize,
    /// Absolute byte offset where it ends.
    pub byte_end: usize,
    /// Absolute byte offset of the first body byte, for the chunker.
    pub body_start: usize,
    /// The stored bytes of the metadata block, verbatim.
    ///
    /// This becomes the point's `text` payload, which keeps `preview` honest and
    /// keeps `raw[byte_start..byte_end] == text` true for metadata points too.
    pub raw_text: String,
    /// A rendered prose form of the fields, used for the embedding and for BM25.
    ///
    /// This is the one place where what is indexed differs from what is stored,
    /// and it is deliberate: embedding raw YAML punctuation retrieves badly.
    pub indexed_text: String,
    /// Payload entries to attach to every point of the document.
    pub payload: Vec<(String, Value)>,
}

/// Derives indexable metadata from a document's raw bytes.
///
/// Returning `None` means "this document has no metadata I understand", and the
/// caller must then index it exactly as it would have before D48.
pub trait MetadataExtractor: Send + Sync {
    /// Extract metadata, or `None` to fall back to raw indexing.
    fn extract(&self, raw: &str) -> Option<DocumentMetadata>;
}

/// Extracts OKF v0.2 frontmatter.
///
/// Returns `Some` only for a document that is OKF-conformant — the frontmatter
/// parses **and** carries a non-empty `type`. Every other document, including one
/// with non-OKF YAML frontmatter, keeps D33's fully-raw behaviour.
#[derive(Debug, Clone, Copy, Default)]
pub struct OkfExtractor;

impl MetadataExtractor for OkfExtractor {
    fn extract(&self, raw: &str) -> Option<DocumentMetadata> {
        let (concept, split) = notedthat_okf::parse_document(raw).ok()?;
        Some(DocumentMetadata {
            byte_start: 0,
            byte_end: split.body_start,
            body_start: split.body_start,
            raw_text: raw[..split.body_start].to_string(),
            indexed_text: render_indexed_text(&concept),
            payload: okf_payload(&concept),
        })
    }
}

/// Render an OKF concept as the prose the embedder and BM25 actually see.
#[must_use]
pub fn render_indexed_text(concept: &OkfConcept) -> String {
    let mut out = String::new();
    if let Some(title) = &concept.title {
        out.push_str(title);
        out.push('\n');
    }
    if let Some(description) = &concept.description {
        out.push_str(description);
        out.push('\n');
    }
    out.push_str("Type: ");
    out.push_str(&concept.concept_type);
    out.push('\n');
    if !concept.tags.is_empty() {
        out.push_str("Tags: ");
        out.push_str(&concept.tags.join(", "));
        out.push('\n');
    }
    if let Some(resource) = &concept.resource {
        out.push_str("Resource: ");
        out.push_str(resource);
        out.push('\n');
    }
    if let Some(runtime) = concept.runtime() {
        out.push_str("Runtime: ");
        out.push_str(runtime);
        out.push('\n');
    }
    out.push_str("Status: ");
    out.push_str(concept.effective_status().as_str());
    out.push('\n');
    out
}

/// The `okf_*` payload entries for a concept, plus `tags`.
///
/// `okf_status` and `okf_trust` are always written — an absent `status` key is
/// stored as `"stable"` — which turns their filters into plain equalities instead
/// of an OR-with-empty. `okf_stale_after` is written **only** when the instant
/// parsed; an absent key means "no expiry", which is what Qdrant's `IsEmpty`
/// matches. A sentinel value would be strictly worse: it would turn
/// `exclude_stale` into a magic-number comparison and lie about the document.
#[must_use]
pub fn okf_payload(concept: &OkfConcept) -> Vec<(String, Value)> {
    use notedthat_okf::concept::{MAX_DESCRIPTION_CHARS, MAX_TITLE_CHARS};

    // These are copied onto every point of the document, so they are capped.
    let annotation = concept.to_annotation(0);

    let mut payload: Vec<(String, Value)> = vec![
        (
            "okf_type".to_string(),
            Value::from(concept.concept_type.clone()),
        ),
        (
            "okf_status".to_string(),
            Value::from(concept.effective_status().as_str().to_string()),
        ),
        (
            "okf_trust".to_string(),
            Value::from(concept.trust().as_str().to_string()),
        ),
    ];

    debug_assert!(
        annotation
            .title
            .as_ref()
            .is_none_or(|t| t.chars().count() <= MAX_TITLE_CHARS)
    );
    debug_assert!(
        annotation
            .description
            .as_ref()
            .is_none_or(|d| d.chars().count() <= MAX_DESCRIPTION_CHARS)
    );

    if let Some(title) = annotation.title {
        payload.push(("okf_title".to_string(), Value::from(title)));
    }
    if let Some(description) = annotation.description {
        payload.push(("okf_description".to_string(), Value::from(description)));
    }
    if let Some(resource) = annotation.resource {
        payload.push(("okf_resource".to_string(), Value::from(resource)));
    }
    if let Some(runtime) = annotation.runtime {
        payload.push(("okf_runtime".to_string(), Value::from(runtime)));
    }
    if let Some(stale_after) = &concept.stale_after {
        // The lexical form is echoed so a client can re-evaluate against its own
        // clock; the epoch is what the server-side filter compares.
        payload.push((
            "okf_stale_after_raw".to_string(),
            Value::from(stale_after.raw.clone()),
        ));
        if let Some(epoch) = stale_after.epoch_secs {
            payload.push(("okf_stale_after".to_string(), Value::from(epoch)));
        }
    }

    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(raw: &str) -> Option<DocumentMetadata> {
        OkfExtractor.extract(raw)
    }

    fn payload_keys(md: &DocumentMetadata) -> Vec<&str> {
        md.payload.iter().map(|(k, _)| k.as_str()).collect()
    }

    #[test]
    fn a_conformant_document_yields_metadata() {
        let md = extract("---\ntype: Metric\n---\n# Body\n").unwrap();
        assert_eq!(md.byte_start, 0);
        assert_eq!(md.byte_end, md.body_start);
    }

    #[test]
    fn raw_text_is_the_stored_bytes_verbatim() {
        // This is what keeps `raw[byte_start..byte_end] == text` true for the
        // metadata point.
        let raw = "---\ntype: Metric\n---\n# Body\n";
        let md = extract(raw).unwrap();
        assert_eq!(md.raw_text, raw[md.byte_start..md.byte_end]);
    }

    #[test]
    fn indexed_text_is_prose_not_yaml() {
        let md = extract("---\ntype: Metric\ntitle: Daily Revenue\n---\n").unwrap();
        assert!(md.indexed_text.contains("Daily Revenue"));
        assert!(!md.indexed_text.contains("title:"));
    }

    #[test]
    fn indexed_text_includes_every_retrievable_field() {
        let raw = "---\ntype: Metric\ntitle: Daily Revenue\n\
                   description: Revenue per day\ntags: [finance, core]\n\
                   resource: bq://p/d/t\nruntime: bigquery\n---\n";
        let text = extract(raw).unwrap().indexed_text;
        for expected in [
            "Daily Revenue",
            "Revenue per day",
            "Type: Metric",
            "Tags: finance, core",
            "Resource: bq://p/d/t",
            "Runtime: bigquery",
            "Status: stable",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }

    #[test]
    fn a_document_without_frontmatter_yields_nothing() {
        assert!(extract("# Just markdown\n").is_none());
    }

    #[test]
    fn non_okf_frontmatter_yields_nothing() {
        // D33 stays true for Obsidian-style notes: no `type`, no OKF handling.
        assert!(extract("---\ntags: [a, b]\n---\nbody\n").is_none());
    }

    #[test]
    fn status_and_trust_are_always_written() {
        let md = extract("---\ntype: Metric\n---\n").unwrap();
        let keys = payload_keys(&md);
        assert!(keys.contains(&"okf_status"));
        assert!(keys.contains(&"okf_trust"));
    }

    #[test]
    fn absent_status_is_stored_as_stable() {
        let md = extract("---\ntype: Metric\n---\n").unwrap();
        let (_, value) = md
            .payload
            .iter()
            .find(|(k, _)| k == "okf_status")
            .expect("okf_status present");
        assert_eq!(value.as_str().map(String::as_str), Some("stable"));
    }

    #[test]
    fn stale_after_is_absent_when_unparseable_never_a_sentinel() {
        let md = extract("---\ntype: Metric\nstale_after: someday\n---\n").unwrap();
        let keys = payload_keys(&md);
        assert!(!keys.contains(&"okf_stale_after"));
        // The lexical form is still echoed so a client can re-evaluate it.
        assert!(keys.contains(&"okf_stale_after_raw"));
    }

    #[test]
    fn stale_after_is_stored_as_an_absolute_epoch() {
        let md =
            extract("---\ntype: Metric\nstale_after: \"2026-01-01T00:00:00Z\"\n---\n").unwrap();
        let (_, value) = md
            .payload
            .iter()
            .find(|(k, _)| k == "okf_stale_after")
            .expect("okf_stale_after present");
        assert_eq!(value.as_integer(), Some(1_767_225_600));
    }

    #[test]
    fn optional_fields_are_omitted_rather_than_written_empty() {
        let md = extract("---\ntype: Metric\n---\n").unwrap();
        let keys = payload_keys(&md);
        for absent in [
            "okf_title",
            "okf_description",
            "okf_resource",
            "okf_runtime",
        ] {
            assert!(!keys.contains(&absent), "{absent} should be omitted");
        }
    }

    #[test]
    fn title_and_description_are_capped_for_the_payload() {
        use notedthat_okf::concept::{MAX_DESCRIPTION_CHARS, MAX_TITLE_CHARS};
        let raw = format!(
            "---\ntype: Metric\ntitle: \"{}\"\ndescription: \"{}\"\n---\n",
            "t".repeat(MAX_TITLE_CHARS + 50),
            "d".repeat(MAX_DESCRIPTION_CHARS + 50)
        );
        let md = extract(&raw).unwrap();
        let field = |key: &str| -> String {
            md.payload
                .iter()
                .find(|(k, _)| k == key)
                .and_then(|(_, v)| v.as_str().cloned())
                .unwrap()
        };
        assert_eq!(field("okf_title").chars().count(), MAX_TITLE_CHARS);
        assert_eq!(
            field("okf_description").chars().count(),
            MAX_DESCRIPTION_CHARS
        );
    }

    #[test]
    fn a_frontmatter_only_document_still_yields_metadata() {
        // The case that would otherwise be completely unsearchable.
        let md = extract("---\ntype: Metric\ntitle: Only Metadata\n---\n").unwrap();
        assert_eq!(
            md.body_start,
            "---\ntype: Metric\ntitle: Only Metadata\n---\n".len()
        );
        assert!(md.indexed_text.contains("Only Metadata"));
    }
}

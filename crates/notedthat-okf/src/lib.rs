//! Open Knowledge Format (OKF) v0.2 support for `NotedThat`.
//!
//! OKF describes a knowledge bundle as a directory of Markdown files with YAML
//! frontmatter, where every non-reserved `.md` file is a *concept* carrying a
//! required `type`. This crate is the pure, I/O-free half of `NotedThat`'s support
//! for it. See SPECIFICATIONS.md D48, which amends D33.
//!
//! # Non-execution invariant
//!
//! `type: Attested Computation` concepts are **catalogued, never executed**. The
//! `computation`, `executor.resource` and `attester.resource` fields are opaque
//! strings here. This crate has no storage client, no HTTP client and no
//! filesystem access in scope, so the invariant is enforced by construction
//! rather than by convention. Resolving any of them to a fetch would turn a
//! read-only catalogue into a fetcher of attacker-controlled URIs.
#![deny(missing_docs)]

pub mod computation;
pub mod concept;
pub mod conformance;
pub mod frontmatter;
pub mod instant;
pub mod links;
pub mod parse;
pub mod reserved;

pub use computation::{InlineComputation, extract_inline_computation};
pub use concept::{
    OkfAttester, OkfComputation, OkfConcept, OkfExecutor, OkfGenerated, OkfParameter, OkfSource,
    OkfVerification, OkfWindow,
};
pub use conformance::{
    Finding, FoundLink, Severity, broken_link, check_object, collect_links, non_utf8,
    object_too_large,
};
pub use frontmatter::{FrontmatterSplit, split_frontmatter};
pub use instant::{instant, parse_iso8601};
pub use links::{LinkTarget, dir_of, resolve_link};
pub use parse::{MAX_FRONTMATTER_BYTES, OkfParseError, parse_document, parse_frontmatter_yaml};
pub use reserved::{
    Deviation, INDEX_FILE, IndexDoc, IndexEntry, IndexSection, LOG_FILE, LogDay, LogDoc,
    append_log_entry, is_iso_date, is_reserved, parse_index, parse_log, remove_entry,
    render_index_entry, upsert_entry,
};

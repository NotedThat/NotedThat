//! Open Knowledge Format (OKF) v0.2 routes.
//!
//! These live under `/v1/okf/{kb_slug}/…` rather than under
//! `/v1/knowledgebases/{kb_slug}/…` on purpose. The object routes are a catch-all
//! (`{*object_path}`), so every static sibling segment permanently shadows an
//! object key — `search` already does. Diverging the namespace before `{kb_slug}`
//! costs one new top-level path shape and shadows nothing.
//!
//! The target object is always a **query parameter**, never a path segment, which
//! also sidesteps registering a second catch-all.
//!
//! # Non-execution invariant
//!
//! [`contract_route`] serves an Attested Computation contract. It resolves a
//! `computation:` path **only inside the knowledge base**, and never dereferences
//! an absolute URL: the server holds S3 credentials and sits inside the network,
//! so fetching a user-authored URL on an agent's behalf would be an SSRF
//! primitive. See SPECIFICATIONS.md D48.

pub mod browse_route;
pub mod contract_route;
pub mod reindex_route;
pub mod validate_route;
pub mod walk;

/// Maximum request body size for the OKF routes that take one (16 KiB).
pub const OKF_BODY_MAX_BYTES: usize = 16 * 1024;

/// Default number of objects inspected per validate request.
pub const DEFAULT_VALIDATE_LIMIT: u32 = 200;

/// Hard cap on objects inspected per validate request.
pub const MAX_VALIDATE_LIMIT: u32 = 1_000;

/// Largest object this module will fetch and parse (1 MiB).
///
/// `list_objects` already returns each object's size, so an oversized object is
/// reported without ever being fetched.
pub const MAX_DOC_BYTES: u64 = 1024 * 1024;

/// Maximum number of findings returned before the report is truncated.
pub const MAX_FINDINGS: usize = 500;

/// Maximum link existence checks issued per validate request.
pub const MAX_LINK_CHECKS: usize = 200;

/// Concurrent object fetches during a bundle walk.
pub const FETCH_CONCURRENCY: usize = 8;

/// Wall-clock budget for one validate request.
///
/// On expiry the report is returned with what it has plus a cursor, rather than
/// holding a connection past the client's own timeout.
pub const VALIDATE_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

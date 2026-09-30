//! Validation of the knowledge base and object path a tool call names.
//!
//! Every tool that builds an API URL from caller input parses it here first.
//! `url::PathSegmentsMut::push` drops `.` and `..` segments rather than
//! encoding them, so an unvalidated `path: ".."` would address the knowledge
//! base listing and an unvalidated `kb: ".."` would shift the path into the
//! slug's place (#279). The API's own rules refuse both, and a reserved key
//! such as `index`, before any request is sent.

use crate::error::McpToolError;
use notedthat_core::{KbSlug, ObjectPath};
use rmcp::ErrorData as McpError;

/// Parse a caller-supplied knowledge base slug.
pub(super) fn parse_kb(kb: &str) -> Result<KbSlug, McpError> {
    KbSlug::try_new(kb)
        .map_err(|error| McpToolError::InvalidRequest(format!("invalid kb: {error}")).into())
}

/// Parse a caller-supplied object path; `what` names the argument in the error.
pub(super) fn parse_object_path(path: &str, what: &str) -> Result<ObjectPath, McpError> {
    ObjectPath::try_object_key(path)
        .map_err(|error| McpToolError::InvalidRequest(format!("invalid {what}: {error}")).into())
}

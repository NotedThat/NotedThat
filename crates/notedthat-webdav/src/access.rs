//! Mapping `WebDAV` methods onto access verbs, and reaching the policy for a
//! knowledge base.
//!
//! The `WebDAV` surface and the HTTP API share one evaluator (D50) but not one
//! method vocabulary, so the translation lives here rather than being spelled
//! out at each call site.

use crate::state::WebDavState;
use notedthat_core::{AccessPolicy, KbSlug, Verb};
use std::sync::Arc;

/// The startup policy snapshot for a knowledge base.
///
/// A declared knowledge base always has one; the fallback is "grants nothing"
/// because the only safe reading of a missing policy is that it permits nothing.
pub(crate) fn policy_for(state: &WebDavState, kb: &KbSlug) -> Arc<AccessPolicy> {
    state
        .access_policies
        .get(kb.as_str())
        .cloned()
        .unwrap_or_else(|| {
            tracing::error!(
                kb = kb.as_str(),
                "declared knowledge base has no access policy; refusing everything"
            );
            Arc::new(AccessPolicy::empty())
        })
}

/// The verbs a `WebDAV` method needs on its target.
///
/// `PROPFIND` maps to [`Verb::List`] at every depth, including `Depth: 0` on a
/// single file — where the HTTP API's `HEAD` maps to [`Verb::Read`]. The
/// asymmetry is deliberate: `PROPFIND` returns properties and never bytes, and a
/// `list` grant on a collection already exposes the size, etag and mtime of its
/// children. Requiring `read` for `Depth: 0` would produce a listing whose own
/// entries refuse to describe themselves.
///
/// `OPTIONS` is the only method needing no verb of its own — it reports what the
/// others allow. Everything unrecognised is treated as a mutation and needs
/// [`Verb::Write`]: `PROPPATCH`, `LOCK` and `UNLOCK` are refused further in with
/// `405`, and an anonymous caller should be told to authenticate before being
/// told which methods this server declines to implement.
pub(crate) fn verbs_for_method(method: &str) -> &'static [Verb] {
    match method {
        "OPTIONS" => &[],
        "PROPFIND" => &[Verb::List],
        "GET" | "HEAD" => &[Verb::Read],
        "DELETE" => &[Verb::Delete],
        // A move is a write at the destination and a delete at the source; the
        // handler checks each against its own key.
        "MOVE" => &[Verb::Write, Verb::Delete],
        // `PUT`, `MKCOL`, `COPY`, and everything unrecognised. Lumping the
        // unknown in with the writes is the fail-closed reading, and it is what
        // keeps an unauthenticated `PROPPATCH` answering `401` rather than
        // revealing which methods this server declines to implement.
        _ => &[Verb::Write],
    }
}

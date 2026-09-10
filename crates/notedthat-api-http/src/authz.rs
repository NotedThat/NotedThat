//! Per-key authorization for the HTTP API.
//!
//! [`crate::middleware::auth_middleware`] establishes *who* is asking; this
//! decides *whether they may*. The two are separate because access rules are
//! path-scoped (D50), and a route pattern cannot answer `read` on
//! `{*object_path}` without the key.
//!
//! Handlers reach for [`KbAccess`] immediately after parsing the knowledge base
//! and key, and before touching storage — so a denial costs no backend call and
//! reads the same whether or not the object exists.

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::Request;
use notedthat_core::{AccessPolicy, KbSlug, KeyFilter, Principal, Verb};
use std::sync::Arc;

/// A resolved knowledge base, the asking principal, and the policy between them.
#[derive(Clone)]
pub(crate) struct KbAccess {
    kb: KbSlug,
    principal: Principal,
    policy: Arc<AccessPolicy>,
}

impl KbAccess {
    /// Resolve a declared knowledge base and its startup policy snapshot.
    ///
    /// # Errors
    ///
    /// [`CoreError::NotFound`] when the slug is not declared. That is not an
    /// authorization answer and must stay a `404`: an undeclared knowledge base
    /// does not exist for anyone, whatever they hold.
    pub(crate) fn resolve(state: &AppState, slug: &str, req: &Request) -> Result<Self, ApiError> {
        let kb = crate::router::lookup_kb(state, slug)?;

        // A declared knowledge base with no policy entry cannot happen —
        // `provision_kbs` fills both maps from the same loop and `run` asserts
        // they agree — but if it ever did, the safe reading is "grants nothing".
        let policy = state.access_policies.get(slug).cloned().unwrap_or_else(|| {
            tracing::error!(
                kb = slug,
                "declared knowledge base has no access policy; refusing everything"
            );
            Arc::new(AccessPolicy::empty())
        });

        Ok(Self {
            kb,
            principal: crate::middleware::principal(req),
            policy,
        })
    }

    /// The resolved knowledge base.
    pub(crate) fn kb(&self) -> &KbSlug {
        &self.kb
    }

    /// Authorize one verb on one key.
    ///
    /// # Errors
    ///
    /// [`ApiError::Unauthorized`] for an anonymous caller, because credentials
    /// might change the answer; [`ApiError::Forbidden`] for a credentialed one,
    /// because theirs will not.
    pub(crate) fn require(&self, verb: Verb, key: &str) -> Result<(), ApiError> {
        if self.policy.allows(self.principal, verb, key) {
            return Ok(());
        }
        Err(self.denial())
    }

    /// Authorize a verb that has no single key, such as search.
    ///
    /// Only for operations where there genuinely is no key. Never use it as a
    /// gate on an object route: it cannot see the implicit `.notedthat` grant
    /// that keeps a locked-out operator's repair path open.
    ///
    /// # Errors
    ///
    /// As [`Self::require`].
    pub(crate) fn require_any(&self, verb: Verb) -> Result<(), ApiError> {
        if self.policy.grants_any(self.principal, verb) {
            return Ok(());
        }
        Err(self.denial())
    }

    /// Whether any declared rule grants this principal the verb, on any path.
    ///
    /// The browse surface asks this directly rather than through
    /// [`Self::require_any`] because it answers a denial with `404`, not with a
    /// status this helper could produce.
    pub(crate) fn policy_grants_any(&self, verb: Verb) -> bool {
        self.policy.grants_any(self.principal, verb)
    }

    /// Whether this principal may apply `verb` to `key`.
    ///
    /// The predicate behind [`Self::require`], for callers that want the answer
    /// rather than an error — a row that renders unlinked instead of failing.
    pub(crate) fn allows(&self, verb: Verb, key: &str) -> bool {
        self.policy.allows(self.principal, verb, key)
    }

    /// A predicate over keys for this principal and verb, compiled once.
    pub(crate) fn filter(&self, verb: Verb) -> KeyFilter<'_> {
        self.policy.key_filter(self.principal, verb)
    }

    fn denial(&self) -> ApiError {
        match self.principal {
            Principal::Anyone => ApiError::Unauthorized,
            Principal::SignedIn => ApiError::Forbidden,
        }
    }
}

/// Where a filtered listing should ask storage to start.
pub(crate) enum ScanScope {
    /// Scan from this prefix; `None` means the whole knowledge base.
    From(Option<String>),
    /// The caller's prefix and the grant's scope cannot overlap, so no key can
    /// match and storage need not be asked at all.
    Disjoint,
}

/// Narrow the backend scan to the overlap of the caller's prefix and the grant's.
///
/// Without this, a grant scoped to `public/**` would scan a whole knowledge base
/// to return one page of it — the difference between a feature and a trap on a
/// large deployment. Both listing surfaces call this, because both answer the
/// same question: `/api/v1`'s object listing and `/browse`'s directory read.
///
/// `hint` comes from [`KeyFilter::literal_prefix_hint`], which is documented as
/// a whole leading *segment* and therefore carries no trailing `/`. A grant on
/// `public/**` yields `"public"`, and the keys it admits are exactly `public`
/// itself plus everything under `public/` — so `hint` and `format!("{hint}/")`
/// are the only two shapes a comparison here may treat as inside the grant.
/// Comparing against the bare segment alone would read `public-internal/` as an
/// extension of `public` and scan a whole knowledge base to return nothing.
pub(crate) fn effective_prefix(requested: Option<&str>, hint: Option<&str>) -> ScanScope {
    match (requested, hint) {
        (None, None) => ScanScope::From(None),
        (Some(prefix), None) => ScanScope::From(Some(prefix.to_string())),
        (None, Some(hint)) => ScanScope::From(Some(hint.to_string())),
        // The caller asked from inside the granted segment, so their prefix is
        // the tighter of the two bounds.
        (Some(prefix), Some(hint)) if prefix.starts_with(&format!("{hint}/")) => {
            ScanScope::From(Some(prefix.to_string()))
        }
        // The caller asked from at or above the granted segment — including
        // asking for the segment itself, which the grant may match as a key.
        // The grant is the tighter bound, and every key it admits starts with
        // it, so narrowing to it cannot drop a row.
        (Some(prefix), Some(hint)) if hint.starts_with(prefix) => {
            ScanScope::From(Some(hint.to_string()))
        }
        // Neither contains the other on a segment boundary, so no key can
        // satisfy both.
        (Some(_), Some(_)) => ScanScope::Disjoint,
    }
}

/// Whether `principal` should see `slug` in a knowledge-base listing.
///
/// A declared knowledge base with no policy entry reads as "grants nothing",
/// the same fallback [`KbAccess::resolve`] uses — the two must agree, or a
/// knowledge base could be invisible in the listing yet reachable by name.
pub(crate) fn visible_in_listing(state: &AppState, slug: &str, principal: Principal) -> bool {
    match state.access_policies.get(slug) {
        Some(policy) => policy.visible_in_listing(principal),
        None => AccessPolicy::empty().visible_in_listing(principal),
    }
}

#[cfg(test)]
mod tests {
    use super::{ScanScope, effective_prefix};

    /// The chosen scope, flattened to something an assertion can read.
    fn scan(requested: Option<&str>, hint: Option<&str>) -> String {
        match effective_prefix(requested, hint) {
            ScanScope::From(None) => "<whole knowledge base>".to_string(),
            ScanScope::From(Some(prefix)) => prefix,
            ScanScope::Disjoint => "<disjoint>".to_string(),
        }
    }

    #[test]
    fn an_unscoped_grant_leaves_the_caller_prefix_alone() {
        assert_eq!(scan(None, None), "<whole knowledge base>");
        assert_eq!(scan(Some("docs/"), None), "docs/");
    }

    #[test]
    fn a_scoped_grant_narrows_a_whole_knowledge_base_scan() {
        assert_eq!(scan(None, Some("public")), "public");
    }

    #[test]
    fn the_caller_prefix_wins_when_it_is_inside_the_granted_segment() {
        assert_eq!(scan(Some("public/deep/"), Some("public")), "public/deep/");
    }

    #[test]
    fn the_grant_wins_when_the_caller_asks_at_or_above_it() {
        // `public/**` matches the key `public` itself, so narrowing to the bare
        // segment rather than `public/` is what keeps that row reachable.
        assert_eq!(scan(Some("public"), Some("public")), "public");
        assert_eq!(scan(Some("pub"), Some("public")), "public");
        assert_eq!(scan(Some(""), Some("public")), "public");
    }

    #[test]
    fn a_sibling_sharing_only_a_textual_prefix_is_disjoint() {
        // Without a segment-boundary comparison this reads as an extension of
        // `public` and burns the whole scan budget returning nothing.
        assert_eq!(scan(Some("public-internal/"), Some("public")), "<disjoint>");
        assert_eq!(scan(Some("publicity.md"), Some("public")), "<disjoint>");
        assert_eq!(scan(Some("archive/"), Some("public")), "<disjoint>");
    }
}

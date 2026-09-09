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

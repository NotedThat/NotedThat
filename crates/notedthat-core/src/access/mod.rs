//! Manifest-declared access rules: who may do what, and where.
//!
//! See SPECIFICATIONS.md §6.7 for the manifest shape and D50 for the model.
//!
//! # The model
//!
//! A knowledge base's manifest carries an ordered array of rules. Each names a
//! [`Principal`], the [`Verb`]s it grants, and the [`KeyPattern`]s it is scoped
//! to. Rules are **allow-only** and the answer for a `(principal, verb, key)`
//! triple is the union of every matching rule, so **rule order never changes a
//! decision** — the array is ordered only because JSON arrays are.
//!
//! ```json
//! "access": [
//!   { "who": "anyone",    "may": ["list", "read"], "under": ["public/**"] },
//!   { "who": "anyone",    "may": ["search"] },
//!   { "who": "signed-in", "may": ["list", "read", "write", "delete", "search"] }
//! ]
//! ```
//!
//! # One decision function
//!
//! Every authorization decision on every surface — HTTP API, `WebDAV`, the browse
//! pages, and MCP by way of the API — ends at [`KeyFilter::allows`]. That is
//! deliberate: the model this replaces had the same check hand-written in five
//! places, and they had already begun to diverge.

mod pattern;

use crate::error::Error;
use crate::object_path::is_internal_path;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub use pattern::KeyPattern;

/// Who a rule grants to.
///
/// There are exactly two principals in v1 and no way to mint more; per-identity
/// grants remain deferred (D27).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Principal {
    /// A caller supplying no credential at all.
    Anyone,
    /// A caller holding the configured Bearer token or `WebDAV` Basic credential.
    SignedIn,
}

/// What a rule grants.
///
/// There is no `discover` verb. A knowledge base appears in a listing when the
/// principal holds any grant in it at all, which is a property of the other
/// verbs rather than a capability of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verb {
    /// Enumerate object metadata: HTTP object listing, `WebDAV` `PROPFIND`, browse pages.
    List,
    /// Read object bytes: HTTP `GET`/`HEAD`, `WebDAV` `GET`/`HEAD`.
    Read,
    /// Create or modify an object: `PUT`, `PATCH`, string replace, `WebDAV` `PUT`.
    Write,
    /// Remove an object: `DELETE`, `WebDAV` `DELETE`.
    Delete,
    /// Run a search and receive hits, including their paths and previews.
    Search,
}

impl Verb {
    /// Every verb, for grants that mean "everything".
    pub const ALL: [Self; 5] = [
        Self::List,
        Self::Read,
        Self::Write,
        Self::Delete,
        Self::Search,
    ];

    /// Returns whether this verb modifies stored content.
    ///
    /// Mutating verbs are never honoured for [`Principal::Anyone`]; see
    /// [`AccessPolicy::key_filter`].
    pub const fn is_mutating(self) -> bool {
        matches!(self, Self::Write | Self::Delete)
    }
}

/// One grant: a principal, the verbs it may use, and the keys it may use them on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessRule {
    /// Who this rule grants to.
    pub who: Principal,
    /// The verbs granted. Never empty; an empty grant fails validation.
    pub may: BTreeSet<Verb>,
    /// The key patterns this grant is scoped to.
    ///
    /// Omitted in the manifest means the whole knowledge base, stored here as
    /// the literal `**` pattern so the evaluator has no special case to forget.
    #[serde(
        default = "whole_kb_patterns",
        skip_serializing_if = "is_whole_kb_patterns"
    )]
    pub under: Vec<KeyPattern>,
}

fn whole_kb_patterns() -> Vec<KeyPattern> {
    vec![KeyPattern::whole_kb()]
}

fn is_whole_kb_patterns(patterns: &[KeyPattern]) -> bool {
    matches!(patterns, [only] if only.is_whole_kb())
}

impl AccessRule {
    /// Build a rule granting `verbs` to `who` across the whole knowledge base.
    pub fn new(who: Principal, verbs: impl IntoIterator<Item = Verb>) -> Self {
        Self {
            who,
            may: verbs.into_iter().collect(),
            under: whole_kb_patterns(),
        }
    }

    /// Scope this rule to the given patterns.
    #[must_use]
    pub fn under(mut self, patterns: impl IntoIterator<Item = KeyPattern>) -> Self {
        self.under = patterns.into_iter().collect();
        self
    }
}

/// The complete access rules for one knowledge base.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccessPolicy(Vec<AccessRule>);

impl FromIterator<AccessRule> for AccessPolicy {
    fn from_iter<T: IntoIterator<Item = AccessRule>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl AccessPolicy {
    /// A policy granting nothing to anyone. The fail-closed fallback.
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    /// The default when a manifest declares no `access` field.
    ///
    /// Every manifest written before this model existed is in that state, so
    /// this default is what makes upgrading a no-op for credentialed access:
    /// the token keeps exactly the reach it had, and only anonymous grants —
    /// which lived in the removed `public_read` field — disappear.
    pub fn signed_in_full() -> Self {
        Self(vec![AccessRule::new(Principal::SignedIn, Verb::ALL)])
    }

    /// Returns whether no rule grants anything at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The declared rules, in manifest order.
    pub fn rules(&self) -> &[AccessRule] {
        &self.0
    }

    /// Validate the policy at startup.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] naming the first problem found.
    pub fn validate(&self) -> Result<(), Error> {
        for rule in &self.0 {
            if rule.may.is_empty() {
                return Err(config(
                    "an access rule grants no verbs; remove it or name a verb",
                ));
            }
            if rule.under.is_empty() {
                return Err(config(
                    "an access rule has an empty `under`; omit the field to grant the whole \
                     knowledge base, or name a pattern",
                ));
            }
            if rule.who == Principal::Anyone {
                if let Some(verb) = rule.may.iter().copied().find(|verb| verb.is_mutating()) {
                    return Err(config(format!(
                        "an access rule grants `{}` to `anyone`; anonymous writes are never \
                         honoured, so this rule cannot mean what it says",
                        serde_verb(verb),
                    )));
                }
                if let Some(pattern) = rule
                    .under
                    .iter()
                    .find(|pattern| pattern.literal_prefix() == Some(".notedthat"))
                {
                    return Err(config(format!(
                        "an access rule grants `anyone` access to `{pattern}`; the `.notedthat` \
                         namespace is never reachable anonymously"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Whether `principal` may apply `verb` to `key`.
    ///
    /// This is the decision function. Everything else in this module is either a
    /// way of reaching it or a cheaper approximation used to avoid work before
    /// a key is known.
    pub fn allows(&self, principal: Principal, verb: Verb, key: &str) -> bool {
        self.key_filter(principal, verb).allows(key)
    }

    /// Whether any *declared* rule grants `principal` this verb on any path.
    ///
    /// Path-independent, so it can refuse a search before an embedding call or
    /// gate a knowledge-base listing. It is never a substitute for
    /// [`Self::allows`] on a concrete key.
    ///
    /// Deliberately blind to the implicit `.notedthat` grant that
    /// [`KeyFilter::allows`] gives [`Principal::SignedIn`]: that grant exists so
    /// a locked-out operator can rewrite a manifest, and a *keyed* request does
    /// reach it. Reporting it here would make this function answer `true` for
    /// every verb of every credentialed caller, which would make it useless for
    /// the two things it is for. So only use it where there is genuinely no key
    /// — never as a route-level gate on an object route, or the recovery path
    /// closes.
    pub fn grants_any(&self, principal: Principal, verb: Verb) -> bool {
        if principal == Principal::Anyone && verb.is_mutating() {
            return false;
        }
        self.0
            .iter()
            .any(|rule| rule.who == principal && rule.may.contains(&verb))
    }

    /// Whether this knowledge base appears in a listing for `principal`.
    ///
    /// Replaces the old `discover` capability: visibility is derived from
    /// holding some grant rather than declared separately.
    ///
    /// Always true for [`Principal::SignedIn`]. That is not an oversight — the
    /// knowledge-base list is how an operator finds the knowledge base whose
    /// manifest they need to repair, and the internal namespace is always
    /// reachable with a credential (see [`KeyFilter::allows`]), so a
    /// credentialed caller always has something to do in every declared base.
    pub fn visible_in_listing(&self, principal: Principal) -> bool {
        principal == Principal::SignedIn
            || self.0.iter().any(|rule| {
                rule.who == principal && rule.may.iter().copied().any(|verb| !verb.is_mutating())
            })
    }

    /// A reusable per-key predicate, compiled once per request.
    ///
    /// Listing a knowledge base asks the same `(principal, verb)` question of
    /// thousands of keys, so the rule scan happens once here rather than inside
    /// the loop.
    pub fn key_filter(&self, principal: Principal, verb: Verb) -> KeyFilter<'_> {
        // Anonymous mutation is never honoured, whatever the manifest says.
        // `validate` also refuses such a rule at startup, but a policy can reach
        // this function from a test, from a manifest written by a future server,
        // or from a hot reload that does not exist yet — so the guarantee lives
        // where the decision is made, not only where the file is read.
        if principal == Principal::Anyone && verb.is_mutating() {
            return KeyFilter {
                principal,
                patterns: Vec::new(),
            };
        }

        let patterns = self
            .0
            .iter()
            .filter(|rule| rule.who == principal && rule.may.contains(&verb))
            .flat_map(|rule| rule.under.iter())
            .collect();
        KeyFilter {
            principal,
            patterns,
        }
    }
}

/// A compiled `(principal, verb)` predicate over object keys.
#[derive(Debug, Clone)]
pub struct KeyFilter<'a> {
    principal: Principal,
    patterns: Vec<&'a KeyPattern>,
}

impl KeyFilter<'_> {
    /// Whether this filter's principal may apply its verb to `key`.
    pub fn allows(&self, key: &str) -> bool {
        // The `.notedthat` namespace is not addressable by an access rule in
        // either direction.
        //
        // Anonymous callers can never reach it, so a `**` grant cannot leak the
        // manifest or anything else the server keeps there. The signed-in
        // principal always can, which is the recovery path for the lockout that
        // becomes possible once rules bind the credential holder too: a manifest
        // that revokes everything else is still repairable by PUTting a
        // corrected one through the API, rather than requiring bucket access an
        // operator on managed storage may not have. (The repair takes effect on
        // the next restart — policies are a startup snapshot.)
        if is_internal_path(key) {
            return self.principal == Principal::SignedIn;
        }
        self.patterns.iter().any(|pattern| pattern.matches(key))
    }

    /// Whether every key is allowed, so a caller can skip per-key filtering.
    ///
    /// Never true for [`Principal::Anyone`], which must always have the internal
    /// namespace filtered out of its listings.
    pub fn is_allow_all(&self) -> bool {
        self.principal == Principal::SignedIn
            && self.patterns.iter().any(|pattern| pattern.is_whole_kb())
    }

    /// Whether no key can ever be allowed, so a caller can return an empty page
    /// without touching storage.
    pub fn is_deny_all(&self) -> bool {
        self.patterns.is_empty() && self.principal == Principal::Anyone
    }

    /// The key prefix every grant in this filter shares, if there is one.
    ///
    /// Lets a listing push a grant's narrowing down into the storage backend
    /// instead of scanning a whole knowledge base only to discard most of it.
    /// `None` means no useful bound — scan from the caller's own prefix.
    pub fn literal_prefix_hint(&self) -> Option<&str> {
        let mut shared: Option<&str> = None;
        for pattern in &self.patterns {
            let prefix = pattern.literal_prefix()?;
            match shared {
                None => shared = Some(prefix),
                Some(existing) if existing == prefix => {}
                Some(_) => return None,
            }
        }
        shared
    }
}

fn config(message: impl Into<String>) -> Error {
    Error::Config {
        message: message.into(),
    }
}

/// The manifest spelling of a verb, for error messages.
fn serde_verb(verb: Verb) -> &'static str {
    match verb {
        Verb::List => "list",
        Verb::Read => "read",
        Verb::Write => "write",
        Verb::Delete => "delete",
        Verb::Search => "search",
    }
}

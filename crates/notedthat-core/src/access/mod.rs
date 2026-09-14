//! Manifest-declared access rules: who may do what, and where.
//!
//! See SPECIFICATIONS.md §6.7 for the manifest shape and D51 for the model.
//!
//! # The model
//!
//! A knowledge base's manifest carries an ordered array of rules. Each names a
//! subject ([`Who`]), the [`Verb`]s it grants, and the [`KeyPattern`]s it is
//! scoped to. A subject is `anyone`, `signed-in`, or — for callers an identity
//! provider vouched for — `group:<name>` or `user:<name>`; a request's
//! [`Principal`] is matched against it by [`Who::matches`]. Rules are
//! **allow-only** and the answer for a `(principal, verb, key)` triple is the
//! union of every matching rule, so **rule order never changes a decision** —
//! the array is ordered only because JSON arrays are.
//!
//! ```json
//! "access": [
//!   { "who": "anyone",        "may": ["list", "read"], "under": ["public/**"] },
//!   { "who": "anyone",        "may": ["search"] },
//!   { "who": "signed-in",     "may": ["list", "read", "search"] },
//!   { "who": "group:editors", "may": ["write", "delete"] }
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
use std::collections::{BTreeMap, BTreeSet};

pub use pattern::KeyPattern;

/// The caller a request was authenticated as.
///
/// Established once at the HTTP boundary and carried through every surface. It
/// is never stored in a manifest — a rule names a [`Who`], and [`Who::matches`]
/// is the only place the two meet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// A caller supplying no credential at all.
    Anyone,
    /// A caller whose credential verified.
    SignedIn(Identity),
}

/// What a verified credential says about its holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// The deployment's own credential: `NOTEDTHAT_API_TOKEN` or the `WebDAV`
    /// Basic pair. It has no subject and no groups, so no `user:` or `group:`
    /// rule can ever match it by accident, and it alone reaches `.notedthat`.
    ServiceToken,
    /// A person or agent an identity provider vouched for.
    User(UserIdentity),
}

/// The identity-provider view of a caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserIdentity {
    /// What `user:` rules are matched against — the provider's username claim.
    pub subject: String,
    /// What `group:` rules are matched against.
    pub groups: BTreeSet<String>,
}

impl Principal {
    /// The principal behind the deployment's own credential.
    pub fn service_token() -> Self {
        Self::SignedIn(Identity::ServiceToken)
    }

    /// A principal an identity provider vouched for.
    pub fn user(subject: impl Into<String>, groups: impl IntoIterator<Item = String>) -> Self {
        Self::SignedIn(Identity::User(UserIdentity {
            subject: subject.into(),
            groups: groups.into_iter().collect(),
        }))
    }

    /// Whether no credential was supplied.
    pub fn is_anonymous(&self) -> bool {
        matches!(self, Self::Anyone)
    }

    /// Whether a credential verified, whoever it belongs to.
    pub fn is_signed_in(&self) -> bool {
        matches!(self, Self::SignedIn(_))
    }

    /// Whether this is the deployment's own credential.
    pub fn is_service_token(&self) -> bool {
        matches!(self, Self::SignedIn(Identity::ServiceToken))
    }

    /// How far this principal reaches outside the rules.
    fn reach(&self) -> Reach {
        match self {
            Self::Anyone => Reach::Anonymous,
            Self::SignedIn(Identity::User(_)) => Reach::User,
            Self::SignedIn(Identity::ServiceToken) => Reach::ServiceToken,
        }
    }
}

/// The part of a principal a compiled filter needs to keep.
///
/// A [`KeyFilter`] borrows its patterns from the policy and is built per
/// request; carrying the whole principal would drag a clone of every group
/// name along with it, and the two decisions the filter makes outside the rules
/// — the internal namespace and the allow-all shortcut — need only this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    Anonymous,
    User,
    ServiceToken,
}

/// Who a rule applies to.
///
/// Spelled as one string in the manifest: `anyone`, `signed-in`,
/// `group:<name>` or `user:<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Who {
    /// A caller supplying no credential at all.
    Anyone,
    /// Any caller whose credential verified: the service token and every user.
    SignedIn,
    /// A user whose identity provider places them in this group.
    Group(String),
    /// A user with exactly this subject.
    User(String),
}

impl Who {
    /// Whether a rule naming this subject applies to `principal`.
    pub fn matches(&self, principal: &Principal) -> bool {
        match (self, principal) {
            (Self::Anyone, Principal::Anyone) | (Self::SignedIn, Principal::SignedIn(_)) => true,
            (Self::Group(group), Principal::SignedIn(Identity::User(user))) => {
                user.groups.contains(group)
            }
            (Self::User(subject), Principal::SignedIn(Identity::User(user))) => {
                user.subject == *subject
            }
            _ => false,
        }
    }

    /// Whether this subject can only ever match an identity-provider user.
    pub fn needs_identity(&self) -> bool {
        matches!(self, Self::Group(_) | Self::User(_))
    }
}

impl std::str::FromStr for Who {
    type Err = Error;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        match source {
            "anyone" => return Ok(Self::Anyone),
            "signed-in" => return Ok(Self::SignedIn),
            _ => {}
        }
        let named = |prefix: &str, build: fn(String) -> Self| {
            source.strip_prefix(prefix).map(|name| {
                if name.is_empty() {
                    Err(config(format!(
                        "`{source}` names nobody; write `{prefix}<name>`"
                    )))
                } else {
                    Ok(build(name.to_string()))
                }
            })
        };
        named("group:", Self::Group)
            .or_else(|| named("user:", Self::User))
            .unwrap_or_else(|| {
                Err(config(format!(
                    "unknown principal `{source}`; expected `anyone`, `signed-in`, \
                     `group:<name>` or `user:<name>`"
                )))
            })
    }
}

impl std::fmt::Display for Who {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Anyone => f.write_str("anyone"),
            Self::SignedIn => f.write_str("signed-in"),
            Self::Group(name) => write!(f, "group:{name}"),
            Self::User(name) => write!(f, "user:{name}"),
        }
    }
}

impl TryFrom<String> for Who {
    type Error = Error;

    fn try_from(source: String) -> Result<Self, Self::Error> {
        source.parse()
    }
}

impl From<Who> for String {
    fn from(who: Who) -> Self {
        who.to_string()
    }
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

/// One grant: a subject, the verbs it may use, and the keys it may use them on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessRule {
    /// Who this rule applies to.
    pub who: Who,
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
    pub fn new(who: Who, verbs: impl IntoIterator<Item = Verb>) -> Self {
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
        Self(vec![AccessRule::new(Who::SignedIn, Verb::ALL)])
    }

    /// Returns whether no rule grants anything at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The declared rules, in manifest order.
    pub fn rules(&self) -> &[AccessRule] {
        &self.0
    }

    /// Whether any rule names a `group:` or `user:` subject.
    ///
    /// Such rules can only ever match a caller an identity provider vouched
    /// for, so a deployment without one is asked to notice at startup.
    pub fn names_an_identity(&self) -> bool {
        self.0.iter().any(|rule| rule.who.needs_identity())
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
            if rule.who == Who::Anyone
                && let Some(verb) = rule.may.iter().copied().find(|verb| verb.is_mutating())
            {
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
                    "an access rule scopes `{}` to `{pattern}`; the `.notedthat` namespace is \
                     not addressable by a rule — the service token always reaches it and \
                     nobody else ever does",
                    rule.who,
                )));
            }
        }
        Ok(())
    }

    /// Whether `principal` may apply `verb` to `key`.
    ///
    /// This is the decision function. Everything else in this module is either a
    /// way of reaching it or a cheaper approximation used to avoid work before
    /// a key is known.
    pub fn allows(&self, principal: &Principal, verb: Verb, key: &str) -> bool {
        self.key_filter(principal, verb).allows(key)
    }

    /// Whether any *declared* rule grants `principal` this verb on any path.
    ///
    /// Path-independent, so it can refuse a search before an embedding call or
    /// gate a knowledge-base listing. It is never a substitute for
    /// [`Self::allows`] on a concrete key.
    ///
    /// Deliberately blind to the implicit `.notedthat` grant that
    /// [`KeyFilter::allows`] gives the service token: that grant exists so a
    /// locked-out operator can rewrite a manifest, and a *keyed* request does
    /// reach it. Reporting it here would make this function answer `true` for
    /// every verb of the service token, which would make it useless for the
    /// two things it is for. So only use it where there is genuinely no key —
    /// never as a route-level gate on an object route, or the recovery path
    /// closes.
    pub fn grants_any(&self, principal: &Principal, verb: Verb) -> bool {
        if principal.is_anonymous() && verb.is_mutating() {
            return false;
        }
        self.0
            .iter()
            .any(|rule| rule.who.matches(principal) && rule.may.contains(&verb))
    }

    /// Whether this knowledge base appears in a listing for `principal`.
    ///
    /// Replaces the old `discover` capability: visibility is derived from
    /// holding some grant rather than declared separately.
    ///
    /// Always true for the service token. That is not an oversight — the
    /// knowledge-base list is how an operator finds the knowledge base whose
    /// manifest they need to repair, and the internal namespace is always
    /// reachable with that credential (see [`KeyFilter::allows`]), so it always
    /// has something to do in every declared base. Everyone else is visible
    /// exactly when some verb is granted to them.
    pub fn visible_in_listing(&self, principal: &Principal) -> bool {
        principal.is_service_token()
            || Verb::ALL
                .iter()
                .any(|verb| self.grants_any(principal, *verb))
    }

    /// A reusable per-key predicate, compiled once per request.
    ///
    /// Listing a knowledge base asks the same `(principal, verb)` question of
    /// thousands of keys, so the rule scan happens once here rather than inside
    /// the loop.
    pub fn key_filter(&self, principal: &Principal, verb: Verb) -> KeyFilter<'_> {
        let reach = principal.reach();
        // Anonymous mutation is never honoured, whatever the manifest says.
        // `validate` also refuses such a rule at startup, but a policy can reach
        // this function from a test, from a manifest written by a future server,
        // or from a hot reload that does not exist yet — so the guarantee lives
        // where the decision is made, not only where the file is read.
        if reach == Reach::Anonymous && verb.is_mutating() {
            return KeyFilter {
                reach,
                patterns: Vec::new(),
            };
        }

        let patterns = self
            .0
            .iter()
            .filter(|rule| rule.who.matches(principal) && rule.may.contains(&verb))
            .flat_map(|rule| rule.under.iter())
            .collect();
        KeyFilter { reach, patterns }
    }
}

/// A compiled `(principal, verb)` predicate over object keys.
#[derive(Debug, Clone)]
pub struct KeyFilter<'a> {
    reach: Reach,
    patterns: Vec<&'a KeyPattern>,
}

impl KeyFilter<'_> {
    /// Whether this filter's principal may apply its verb to `key`.
    pub fn allows(&self, key: &str) -> bool {
        // The `.notedthat` namespace is not addressable by an access rule in
        // either direction.
        //
        // Anonymous callers and identity-provider users can never reach it, so
        // a `**` grant cannot leak the manifest — which carries the policy,
        // group names included — or anything else the server keeps there. The
        // service token always can, which is the recovery path for the lockout
        // that becomes possible once rules bind the credential holder too: a
        // manifest that revokes everything else is still repairable by PUTting
        // a corrected one through the API, rather than requiring bucket access
        // an operator on managed storage may not have. (The repair takes effect
        // on the next restart — policies are a startup snapshot.)
        if is_internal_path(key) {
            return self.reach == Reach::ServiceToken;
        }
        self.patterns.iter().any(|pattern| pattern.matches(key))
    }

    /// Whether every key is allowed, so a caller can skip per-key filtering.
    ///
    /// Only ever true for the service token: everyone else must always have
    /// the internal namespace filtered out of their listings.
    pub fn is_allow_all(&self) -> bool {
        self.reach == Reach::ServiceToken
            && self.patterns.iter().any(|pattern| pattern.is_whole_kb())
    }

    /// Whether no key can ever be allowed, so a caller can return an empty page
    /// without touching storage.
    pub fn is_deny_all(&self) -> bool {
        self.patterns.is_empty() && self.reach != Reach::ServiceToken
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
/// The policy set a deployment gets when no manifest declares access rules.
///
/// [`crate::kb::KbManifest`] defaults `access` to [`AccessPolicy::signed_in_full`],
/// so this is what startup provisioning produces for a set of knowledge bases
/// whose manifests predate the model — and the sensible starting point anywhere
/// a policy map has to be built without one.
pub fn signed_in_policies(
    declared: &BTreeMap<String, crate::slug::KbSlug>,
) -> BTreeMap<String, std::sync::Arc<AccessPolicy>> {
    declared
        .keys()
        .map(|slug| {
            (
                slug.clone(),
                std::sync::Arc::new(AccessPolicy::signed_in_full()),
            )
        })
        .collect()
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

#![allow(missing_docs)]

use notedthat_core::{AccessPolicy, AccessRule, KeyPattern, Principal, Verb};

const INTERNAL: &str = ".notedthat/manifest.json";

fn patterns(sources: &[&str]) -> Vec<KeyPattern> {
    sources
        .iter()
        .map(|source| KeyPattern::parse(source).expect("valid pattern"))
        .collect()
}

fn policy(rules: impl IntoIterator<Item = AccessRule>) -> AccessPolicy {
    rules.into_iter().collect()
}

/// `anyone` may list and read under `public/`; nothing else is granted.
fn public_prefix_policy() -> AccessPolicy {
    policy([
        AccessRule::new(Principal::Anyone, [Verb::List, Verb::Read])
            .under(patterns(&["public/**"])),
        AccessRule::new(Principal::SignedIn, Verb::ALL),
    ])
}

#[test]
fn a_prefix_scoped_grant_stops_at_its_prefix() {
    // Given
    let policy = public_prefix_policy();

    // When / Then
    assert!(policy.allows(Principal::Anyone, Verb::Read, "public/index.md"));
    assert!(policy.allows(Principal::Anyone, Verb::Read, "public/deep/note.md"));
    assert!(!policy.allows(Principal::Anyone, Verb::Read, "private/secret.md"));
    assert!(!policy.allows(Principal::Anyone, Verb::Read, "publicity/near-miss.md"));
}

#[test]
fn a_verb_not_named_by_any_rule_is_denied_even_within_the_granted_prefix() {
    // Given
    let policy = public_prefix_policy();

    // When / Then
    assert!(policy.allows(Principal::Anyone, Verb::List, "public/index.md"));
    assert!(!policy.allows(Principal::Anyone, Verb::Search, "public/index.md"));
    assert!(!policy.allows(Principal::Anyone, Verb::Write, "public/index.md"));
    assert!(!policy.allows(Principal::Anyone, Verb::Delete, "public/index.md"));
}

#[test]
fn an_empty_policy_grants_nobody_anything_outside_the_internal_namespace() {
    // Given
    let policy = AccessPolicy::empty();

    // When / Then
    for principal in [Principal::Anyone, Principal::SignedIn] {
        for verb in Verb::ALL {
            assert!(
                !policy.allows(principal, verb, "notes.md"),
                "{principal:?} should not hold {verb:?} under an empty policy"
            );
        }
    }
}

#[test]
fn the_default_policy_gives_the_credential_holder_everything_and_anonymous_nothing() {
    // Given — this is what an upgraded manifest with no `access` field means.
    let policy = AccessPolicy::signed_in_full();

    // When / Then
    for verb in Verb::ALL {
        assert!(policy.allows(Principal::SignedIn, verb, "notes.md"));
        assert!(!policy.allows(Principal::Anyone, verb, "notes.md"));
    }
}

#[test]
fn anonymous_callers_never_reach_the_internal_namespace_even_under_a_whole_kb_grant() {
    // Given — the broadest anonymous grant that validation permits.
    let policy = policy([AccessRule::new(
        Principal::Anyone,
        [Verb::List, Verb::Read, Verb::Search],
    )]);

    // When / Then
    assert!(policy.allows(Principal::Anyone, Verb::Read, "notes.md"));
    assert!(!policy.allows(Principal::Anyone, Verb::Read, INTERNAL));
    assert!(!policy.allows(Principal::Anyone, Verb::List, ".notedthat"));
}

#[test]
fn the_credential_holder_reaches_the_internal_namespace_with_no_rule_granting_it() {
    // Given — the lockout every operator is one manifest typo away from.
    let policy = AccessPolicy::empty();

    // When / Then — the repair path stays open.
    for verb in Verb::ALL {
        assert!(
            policy.allows(Principal::SignedIn, verb, INTERNAL),
            "{verb:?} on the manifest must survive a policy that grants nothing"
        );
    }
}

#[test]
fn rule_order_does_not_change_any_decision() {
    // Given — the same rules, written in both orders.
    let rules = [
        AccessRule::new(Principal::Anyone, [Verb::Read]).under(patterns(&["public/**"])),
        AccessRule::new(Principal::Anyone, [Verb::List]).under(patterns(&["docs/**"])),
        AccessRule::new(Principal::SignedIn, [Verb::Read, Verb::Write]),
    ];
    let forward = policy(rules.clone());
    let reversed = policy(rules.into_iter().rev());

    // When / Then
    for principal in [Principal::Anyone, Principal::SignedIn] {
        for verb in Verb::ALL {
            for key in ["public/a.md", "docs/a.md", "other/a.md", INTERNAL] {
                assert_eq!(
                    forward.allows(principal, verb, key),
                    reversed.allows(principal, verb, key),
                    "order changed the answer for {principal:?}/{verb:?} on {key}"
                );
            }
        }
    }
}

#[test]
fn search_is_grantable_without_read_and_read_without_search() {
    // Given — the independence the capability model had, kept.
    let search_only = policy([AccessRule::new(Principal::Anyone, [Verb::Search])]);
    let read_only = policy([AccessRule::new(Principal::Anyone, [Verb::Read])]);

    // When / Then
    assert!(search_only.allows(Principal::Anyone, Verb::Search, "a.md"));
    assert!(!search_only.allows(Principal::Anyone, Verb::Read, "a.md"));
    assert!(read_only.allows(Principal::Anyone, Verb::Read, "a.md"));
    assert!(!read_only.allows(Principal::Anyone, Verb::Search, "a.md"));
}

#[test]
fn an_anonymous_mutating_grant_is_inert_even_if_it_reaches_the_evaluator() {
    // Given — validation refuses this, so construct it directly.
    let policy = policy([AccessRule::new(
        Principal::Anyone,
        [Verb::Read, Verb::Write, Verb::Delete],
    )]);

    // When / Then
    assert!(policy.allows(Principal::Anyone, Verb::Read, "a.md"));
    assert!(!policy.allows(Principal::Anyone, Verb::Write, "a.md"));
    assert!(!policy.allows(Principal::Anyone, Verb::Delete, "a.md"));
    assert!(!policy.grants_any(Principal::Anyone, Verb::Write));
}

#[test]
fn an_anonymous_mutating_grant_fails_validation() {
    // Given
    let policy = policy([AccessRule::new(
        Principal::Anyone,
        [Verb::Read, Verb::Write],
    )]);

    // When
    let error = policy.validate().expect_err("anonymous write is refused");

    // Then
    assert!(
        error.to_string().contains("write") && error.to_string().contains("anyone"),
        "the error should name the verb and the principal: {error}"
    );
}

#[test]
fn an_anonymous_grant_naming_the_internal_namespace_fails_validation() {
    // Given
    let policy = policy([
        AccessRule::new(Principal::Anyone, [Verb::Read]).under(patterns(&[".notedthat/**"]))
    ]);

    // When
    let error = policy
        .validate()
        .expect_err("anonymous .notedthat is refused");

    // Then
    assert!(
        error.to_string().contains(".notedthat"),
        "the error should name the namespace: {error}"
    );
}

#[test]
fn a_rule_granting_nothing_fails_validation() {
    // Given
    let policy = policy([AccessRule::new(Principal::Anyone, [])]);

    // When / Then
    assert!(
        policy.validate().is_err(),
        "an empty `may` is an editing mistake"
    );
}

#[test]
fn an_explicitly_empty_under_fails_validation() {
    // Given — ambiguous between "nothing" and "everything", so refuse it.
    let policy = policy([AccessRule::new(Principal::Anyone, [Verb::Read]).under([])]);

    // When / Then
    assert!(policy.validate().is_err());
}

#[test]
fn a_valid_policy_passes_validation() {
    // Given / When / Then
    public_prefix_policy().validate().expect("valid policy");
    AccessPolicy::signed_in_full()
        .validate()
        .expect("default policy");
    AccessPolicy::empty()
        .validate()
        .expect("empty policy is inert, not invalid");
}

#[test]
fn a_knowledge_base_is_visible_when_the_principal_holds_any_read_shaped_grant() {
    // Given
    let public = policy([AccessRule::new(Principal::Anyone, [Verb::Search])]);
    let private = AccessPolicy::signed_in_full();

    // When / Then
    assert!(public.visible_in_listing(Principal::Anyone));
    assert!(!private.visible_in_listing(Principal::Anyone));
    assert!(
        AccessPolicy::empty().visible_in_listing(Principal::SignedIn),
        "a credentialed caller must be able to find the base whose manifest they need to fix"
    );
}

#[test]
fn a_filter_reports_allow_all_only_when_no_per_key_work_is_needed() {
    // Given
    let signed_in_everything = AccessPolicy::signed_in_full();
    let anonymous_everything = policy([AccessRule::new(Principal::Anyone, [Verb::Read])]);
    let scoped = public_prefix_policy();

    // When / Then
    assert!(
        signed_in_everything
            .key_filter(Principal::SignedIn, Verb::Read)
            .is_allow_all()
    );
    assert!(
        !anonymous_everything
            .key_filter(Principal::Anyone, Verb::Read)
            .is_allow_all(),
        "anonymous listings must still drop the internal namespace"
    );
    assert!(
        !scoped
            .key_filter(Principal::Anyone, Verb::Read)
            .is_allow_all()
    );
}

#[test]
fn a_filter_reports_deny_all_so_a_listing_can_skip_storage_entirely() {
    // Given
    let policy = public_prefix_policy();

    // When / Then
    assert!(
        policy
            .key_filter(Principal::Anyone, Verb::Delete)
            .is_deny_all()
    );
    assert!(
        !policy
            .key_filter(Principal::Anyone, Verb::Read)
            .is_deny_all()
    );
    assert!(
        !policy
            .key_filter(Principal::SignedIn, Verb::Read)
            .is_deny_all(),
        "the credential holder always has the internal namespace, so never deny-all"
    );
}

#[test]
fn a_filter_reports_the_prefix_its_grants_share() {
    // Given
    let one_prefix =
        policy([AccessRule::new(Principal::Anyone, [Verb::List]).under(patterns(&["public/**"]))]);
    let two_prefixes =
        policy([AccessRule::new(Principal::Anyone, [Verb::List])
            .under(patterns(&["public/**", "docs/**"]))]);

    // When / Then
    assert_eq!(
        one_prefix
            .key_filter(Principal::Anyone, Verb::List)
            .literal_prefix_hint(),
        Some("public"),
        "a single-prefix grant should narrow the backend scan"
    );
    assert_eq!(
        two_prefixes
            .key_filter(Principal::Anyone, Verb::List)
            .literal_prefix_hint(),
        None,
        "two disjoint prefixes give no single usable bound"
    );
    assert_eq!(
        AccessPolicy::signed_in_full()
            .key_filter(Principal::SignedIn, Verb::List)
            .literal_prefix_hint(),
        None
    );
}

#[test]
fn a_policy_round_trips_through_json_in_the_documented_manifest_shape() {
    // Given
    let json = r#"[
      { "who": "anyone",    "may": ["list", "read"], "under": ["public/**"] },
      { "who": "anyone",    "may": ["search"] },
      { "who": "signed-in", "may": ["list", "read", "write", "delete", "search"] }
    ]"#;

    // When
    let policy: AccessPolicy = serde_json::from_str(json).expect("documented shape parses");
    let reencoded = serde_json::to_string(&policy).expect("serialize");
    let round_tripped: AccessPolicy = serde_json::from_str(&reencoded).expect("re-parse");

    // Then
    assert_eq!(policy.rules().len(), 3);
    assert!(policy.allows(Principal::Anyone, Verb::Read, "public/a.md"));
    assert!(!policy.allows(Principal::Anyone, Verb::Read, "private/a.md"));
    assert!(policy.allows(Principal::Anyone, Verb::Search, "private/a.md"));
    assert_eq!(round_tripped, policy);
    assert_eq!(
        reencoded.matches("\"under\"").count(),
        1,
        "only the scoped rule should carry `under`; whole-knowledge-base grants stay \
         terse on the way out: {reencoded}"
    );
}

#[test]
fn an_omitted_under_grants_the_whole_knowledge_base() {
    // Given
    let policy: AccessPolicy =
        serde_json::from_str(r#"[{ "who": "anyone", "may": ["read"] }]"#).expect("parse");

    // When / Then
    assert!(policy.allows(Principal::Anyone, Verb::Read, "anywhere/at/all.md"));
    assert!(!policy.allows(Principal::Anyone, Verb::Read, INTERNAL));
}

#[test]
fn an_unknown_verb_or_principal_is_refused_rather_than_ignored() {
    // Given / When / Then — a typo must not silently become a weaker policy.
    assert!(
        serde_json::from_str::<AccessPolicy>(r#"[{ "who": "anyone", "may": ["browse"] }]"#)
            .is_err(),
        "`browse` is not a verb in this model"
    );
    assert!(
        serde_json::from_str::<AccessPolicy>(r#"[{ "who": "everyone", "may": ["read"] }]"#)
            .is_err()
    );
    assert!(
        serde_json::from_str::<AccessPolicy>(r#"[{ "who": "signed_in", "may": ["read"] }]"#)
            .is_err(),
        "the principal is spelled `signed-in`"
    );
}

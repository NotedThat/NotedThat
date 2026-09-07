use notedthat_core::search::SearchFilter;
use qdrant_client::qdrant::{Condition, Filter, Range};

/// The result of translating a `SearchFilter`.
///
/// - `qdrant`: conditions natively expressible in Qdrant (passed as the outer filter to `QueryPoints`)
/// - `post`: conditions that must be applied client-side after Qdrant returns hits
#[derive(Debug, Default)]
pub struct TranslatedFilter {
    /// Qdrant-native filter. `None` when no Qdrant-side conditions apply.
    pub qdrant: Option<Filter>,
    /// Client-side post-filter applied to each hit.
    pub post: PostFilter,
}

/// Conditions applied client-side after Qdrant returns hits.
///
/// Required because qdrant-client 1.15 has no keyword-index prefix matcher.
#[derive(Debug, Clone, Default)]
pub struct PostFilter {
    /// Only keep hits whose `object_key` starts with this prefix.
    pub object_key_prefix: Option<String>,
}

impl PostFilter {
    /// Returns `true` iff no client-side filtering is needed.
    pub fn is_empty(&self) -> bool {
        self.object_key_prefix.is_none()
    }

    /// Returns `true` iff this hit passes all post-filter conditions.
    pub fn matches(&self, object_key: &str) -> bool {
        match &self.object_key_prefix {
            Some(prefix) => object_key.starts_with(prefix.as_str()),
            None => true,
        }
    }
}

/// Translate a [`SearchFilter`] into Qdrant-native conditions and a client-side [`PostFilter`].
///
/// The `object_key_prefix` field goes into [`PostFilter`] because qdrant-client 1.15
/// does not expose a keyword-index prefix matcher (`MatchText` requires a text index and
/// tokenises differently).
///
/// All other fields are translated to Qdrant [`Condition`]s and AND-composed via `Filter::must`.
///
/// `now_unix` is the request's evaluation instant. It is passed in rather than
/// read from the clock so that translation stays a pure, unit-testable function;
/// only `exclude_stale` uses it.
pub fn translate_filter(filter: &SearchFilter, now_unix: i64) -> TranslatedFilter {
    let mut conditions: Vec<Condition> = Vec::new();

    // mime: exact keyword match against the mime payload index added in T1.
    if let Some(mime) = &filter.mime {
        conditions.push(Condition::matches("mime", mime.clone()));
    }

    // heading_path_prefix: enforce "heading_path starts with these segments" via
    // per-index equality on heading_path[0], heading_path[1], etc.
    for (i, segment) in filter.heading_path_prefix.iter().enumerate() {
        conditions.push(Condition::matches(
            format!("heading_path[{i}]"),
            segment.clone(),
        ));
    }

    // updated_after: mtime >= value
    if let Some(after) = filter.updated_after {
        conditions.push(Condition::range(
            "mtime",
            Range {
                gte: Some(unix_seconds_as_range_bound(after)),
                gt: None,
                lte: None,
                lt: None,
            },
        ));
    }

    // updated_before: mtime <= value
    if let Some(before) = filter.updated_before {
        conditions.push(Condition::range(
            "mtime",
            Range {
                lte: Some(unix_seconds_as_range_bound(before)),
                gt: None,
                gte: None,
                lt: None,
            },
        ));
    }

    // tags: MatchAny (via `Condition::matches` with Vec<String> in qdrant-client 1.15)
    // — but ONLY if non-empty.
    // An empty Vec would emit MatchAny([]) which means "match nothing" — the wrong semantics.
    if !filter.tags.is_empty() {
        conditions.push(Condition::matches("tags", filter.tags.clone()));
    }

    // --- OKF v0.2 (D48). Every one of these is opt-in; the default is to return
    // everything and let the annotation on each hit speak for itself. ---

    if let Some(concept_type) = &filter.okf_type {
        conditions.push(Condition::matches("okf_type", concept_type.clone()));
    }

    // `okf_status` is written for every OKF document — an absent `status` key is
    // indexed as "stable" — so this is a plain equality, not an OR-with-empty.
    if let Some(status) = filter.okf_status {
        conditions.push(Condition::matches(
            "okf_status",
            status.as_str().to_string(),
        ));
    }

    // Keyword payloads have no ordinal comparison, so enumerate the tiers at or
    // above the minimum and MatchAny over them. The set has at most three members.
    if let Some(min_trust) = filter.okf_min_trust {
        let allowed: Vec<String> = min_trust
            .at_least()
            .map(|t| t.as_str().to_string())
            .collect();
        conditions.push(Condition::matches("okf_trust", allowed));
    }

    if let Some(runtime) = &filter.okf_runtime {
        conditions.push(Condition::matches("okf_runtime", runtime.clone()));
    }

    if let Some(chunk_kind) = &filter.chunk_kind {
        conditions.push(Condition::matches("chunk_kind", chunk_kind.clone()));
    }

    // `okf_type` is written for every OKF document and only for OKF documents,
    // so its presence is the marker for "this document carries OKF frontmatter".
    if filter.okf_only {
        conditions.push(Condition::from(Filter::must_not([Condition::is_empty(
            "okf_type",
        )])));
    }

    // "no expiry set OR the expiry is still in the future", as a nested OR folded
    // into the outer AND. This has to be server-side: a client-side version would
    // interact badly with the capped over-fetch, silently under-filling results on
    // a stale-heavy corpus, and `is_empty` semantics ("missing or null or empty")
    // are not reproducible client-side because `point_to_hit` discards the
    // difference between "no key" and "an OKF document with no expiry".
    if filter.exclude_stale {
        conditions.push(Condition::from(Filter::should([
            Condition::is_empty("okf_stale_after"),
            Condition::range(
                "okf_stale_after",
                Range {
                    gt: Some(unix_seconds_as_range_bound(now_unix)),
                    gte: None,
                    lte: None,
                    lt: None,
                },
            ),
        ])));
    }

    // Build the Qdrant-side filter if any conditions were added.
    let qdrant = if conditions.is_empty() {
        None
    } else {
        Some(Filter::must(conditions))
    };

    // object_key_prefix: client-side only (no native Qdrant prefix condition).
    let post = PostFilter {
        object_key_prefix: filter.object_key_prefix.clone(),
    };

    TranslatedFilter { qdrant, post }
}

#[allow(clippy::cast_precision_loss)]
fn unix_seconds_as_range_bound(value: i64) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use notedthat_core::okf::{OkfStatus, OkfTrust};
    use notedthat_core::search::SearchFilter;

    #[test]
    fn empty_filter_returns_no_conditions() {
        let t = translate_filter(&SearchFilter::default(), 0);
        assert!(t.qdrant.is_none());
        assert!(t.post.is_empty());
    }

    #[test]
    fn mime_filter_produces_qdrant_condition() {
        let f = SearchFilter {
            mime: Some("text/markdown".into()),
            ..Default::default()
        };
        let t = translate_filter(&f, 0);
        assert!(t.qdrant.is_some());
        assert!(t.post.is_empty());
    }

    #[test]
    fn single_heading_path_prefix_produces_one_condition() {
        let f = SearchFilter {
            heading_path_prefix: vec!["A".into()],
            ..Default::default()
        };
        let t = translate_filter(&f, 0);
        let filter = t.qdrant.as_ref().unwrap();
        assert_eq!(filter.must.len(), 1);
        assert!(t.post.is_empty());
    }

    #[test]
    fn two_heading_path_prefix_segments_produce_two_conditions() {
        let f = SearchFilter {
            heading_path_prefix: vec!["A".into(), "B".into()],
            ..Default::default()
        };
        let t = translate_filter(&f, 0);
        let filter = t.qdrant.as_ref().unwrap();
        assert_eq!(filter.must.len(), 2);
    }

    #[test]
    fn updated_after_produces_range_condition() {
        let f = SearchFilter {
            updated_after: Some(1_000_000),
            ..Default::default()
        };
        let t = translate_filter(&f, 0);
        assert!(t.qdrant.is_some());
        let filter = t.qdrant.unwrap();
        assert_eq!(filter.must.len(), 1);
    }

    #[test]
    fn updated_before_produces_range_condition() {
        let f = SearchFilter {
            updated_before: Some(2_000_000),
            ..Default::default()
        };
        let t = translate_filter(&f, 0);
        assert!(t.qdrant.is_some());
    }

    #[test]
    fn non_empty_tags_produce_match_any_condition() {
        let f = SearchFilter {
            tags: vec!["rust".into()],
            ..Default::default()
        };
        let t = translate_filter(&f, 0);
        assert!(t.qdrant.is_some());
        assert!(t.post.is_empty());
    }

    #[test]
    fn empty_tags_produce_no_condition() {
        let f = SearchFilter {
            tags: vec![],
            ..Default::default()
        };
        let t = translate_filter(&f, 0);
        assert!(t.qdrant.is_none());
        assert!(t.post.is_empty());
    }

    #[test]
    fn object_key_prefix_goes_to_post_filter_only() {
        let f = SearchFilter {
            object_key_prefix: Some("docs/".into()),
            ..Default::default()
        };
        let t = translate_filter(&f, 0);
        assert!(t.qdrant.is_none());
        assert_eq!(t.post.object_key_prefix.as_deref(), Some("docs/"));
    }

    #[test]
    fn combined_all_fields_produces_5_qdrant_conditions_plus_post() {
        let f = SearchFilter {
            object_key_prefix: Some("docs/".into()),
            mime: Some("text/markdown".into()),
            heading_path_prefix: vec!["A".into()],
            updated_after: Some(1_000),
            updated_before: Some(2_000),
            tags: vec!["rust".into()],
            ..Default::default()
        };
        let t = translate_filter(&f, 0);
        let filter = t.qdrant.unwrap();
        assert_eq!(filter.must.len(), 5);
        assert_eq!(t.post.object_key_prefix.as_deref(), Some("docs/"));
    }

    #[test]
    fn combined_all_okf_fields_produces_seven_more_conditions() {
        let f = SearchFilter {
            mime: Some("text/markdown".into()),
            okf_type: Some("Metric".into()),
            okf_status: Some(OkfStatus::Stable),
            okf_min_trust: Some(OkfTrust::MachineConfirmed),
            okf_runtime: Some("bigquery".into()),
            chunk_kind: Some("metadata".into()),
            okf_only: true,
            exclude_stale: true,
            ..Default::default()
        };
        let filter = translate_filter(&f, 0).qdrant.unwrap();
        assert_eq!(filter.must.len(), 8);
    }

    #[test]
    fn okf_type_produces_one_condition() {
        let f = SearchFilter {
            okf_type: Some("Metric".into()),
            ..Default::default()
        };
        assert_eq!(translate_filter(&f, 0).qdrant.unwrap().must.len(), 1);
    }

    #[test]
    fn okf_min_trust_unverified_matches_all_three_tiers() {
        let f = SearchFilter {
            okf_min_trust: Some(OkfTrust::Unverified),
            ..Default::default()
        };
        let filter = translate_filter(&f, 0).qdrant.unwrap();
        assert_eq!(match_any_len(&filter.must[0]), 3);
    }

    #[test]
    fn okf_min_trust_human_reviewed_matches_one_tier() {
        let f = SearchFilter {
            okf_min_trust: Some(OkfTrust::HumanReviewed),
            ..Default::default()
        };
        let filter = translate_filter(&f, 0).qdrant.unwrap();
        assert_eq!(match_any_len(&filter.must[0]), 1);
    }

    #[test]
    fn exclude_stale_produces_a_nested_should_with_two_clauses() {
        use qdrant_client::qdrant::condition::ConditionOneOf;
        let f = SearchFilter {
            exclude_stale: true,
            ..Default::default()
        };
        let filter = translate_filter(&f, 1_234).qdrant.unwrap();
        assert_eq!(filter.must.len(), 1);
        match &filter.must[0].condition_one_of {
            Some(ConditionOneOf::Filter(nested)) => {
                // "no expiry set" OR "expiry still in the future".
                assert_eq!(nested.should.len(), 2);
                assert!(nested.must.is_empty());
            }
            other => panic!("expected a nested filter, got {other:?}"),
        }
    }

    #[test]
    fn exclude_stale_false_produces_no_condition() {
        let f = SearchFilter {
            exclude_stale: false,
            ..Default::default()
        };
        assert!(translate_filter(&f, 0).qdrant.is_none());
    }

    #[test]
    fn okf_only_produces_a_nested_must_not() {
        use qdrant_client::qdrant::condition::ConditionOneOf;
        let f = SearchFilter {
            okf_only: true,
            ..Default::default()
        };
        let filter = translate_filter(&f, 0).qdrant.unwrap();
        match &filter.must[0].condition_one_of {
            Some(ConditionOneOf::Filter(nested)) => assert_eq!(nested.must_not.len(), 1),
            other => panic!("expected a nested filter, got {other:?}"),
        }
    }

    /// Number of values in a `MatchAny` keyword condition.
    fn match_any_len(condition: &Condition) -> usize {
        use qdrant_client::qdrant::condition::ConditionOneOf;
        use qdrant_client::qdrant::r#match::MatchValue;
        match &condition.condition_one_of {
            Some(ConditionOneOf::Field(field)) => {
                match field.r#match.as_ref().and_then(|m| m.match_value.as_ref()) {
                    Some(MatchValue::Keywords(keywords)) => keywords.strings.len(),
                    other => panic!("expected keywords match, got {other:?}"),
                }
            }
            other => panic!("expected a field condition, got {other:?}"),
        }
    }

    #[test]
    fn post_filter_matches_prefix_correctly() {
        let post = PostFilter {
            object_key_prefix: Some("docs/".into()),
        };
        assert!(post.matches("docs/foo.md"));
        assert!(post.matches("docs/bar/baz.md"));
        assert!(!post.matches("notes/x.md"));
    }

    #[test]
    fn post_filter_none_matches_everything() {
        let post = PostFilter {
            object_key_prefix: None,
        };
        assert!(post.matches("docs/foo.md"));
        assert!(post.matches("notes/bar.md"));
    }

    #[test]
    fn post_filter_is_empty() {
        assert!(PostFilter::default().is_empty());
        assert!(
            !PostFilter {
                object_key_prefix: Some("docs/".into())
            }
            .is_empty()
        );
    }
}

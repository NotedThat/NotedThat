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
pub fn translate_filter(filter: &SearchFilter) -> TranslatedFilter {
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
    use notedthat_core::search::SearchFilter;

    #[test]
    fn empty_filter_returns_no_conditions() {
        let t = translate_filter(&SearchFilter::default());
        assert!(t.qdrant.is_none());
        assert!(t.post.is_empty());
    }

    #[test]
    fn mime_filter_produces_qdrant_condition() {
        let f = SearchFilter {
            mime: Some("text/markdown".into()),
            ..Default::default()
        };
        let t = translate_filter(&f);
        assert!(t.qdrant.is_some());
        assert!(t.post.is_empty());
    }

    #[test]
    fn single_heading_path_prefix_produces_one_condition() {
        let f = SearchFilter {
            heading_path_prefix: vec!["A".into()],
            ..Default::default()
        };
        let t = translate_filter(&f);
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
        let t = translate_filter(&f);
        let filter = t.qdrant.as_ref().unwrap();
        assert_eq!(filter.must.len(), 2);
    }

    #[test]
    fn updated_after_produces_range_condition() {
        let f = SearchFilter {
            updated_after: Some(1_000_000),
            ..Default::default()
        };
        let t = translate_filter(&f);
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
        let t = translate_filter(&f);
        assert!(t.qdrant.is_some());
    }

    #[test]
    fn non_empty_tags_produce_match_any_condition() {
        let f = SearchFilter {
            tags: vec!["rust".into()],
            ..Default::default()
        };
        let t = translate_filter(&f);
        assert!(t.qdrant.is_some());
        assert!(t.post.is_empty());
    }

    #[test]
    fn empty_tags_produce_no_condition() {
        let f = SearchFilter {
            tags: vec![],
            ..Default::default()
        };
        let t = translate_filter(&f);
        assert!(t.qdrant.is_none());
        assert!(t.post.is_empty());
    }

    #[test]
    fn object_key_prefix_goes_to_post_filter_only() {
        let f = SearchFilter {
            object_key_prefix: Some("docs/".into()),
            ..Default::default()
        };
        let t = translate_filter(&f);
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
        };
        let t = translate_filter(&f);
        let filter = t.qdrant.unwrap();
        assert_eq!(filter.must.len(), 5);
        assert_eq!(t.post.object_key_prefix.as_deref(), Some("docs/"));
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

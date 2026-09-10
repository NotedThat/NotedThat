#![allow(missing_docs)]

use notedthat_core::KeyPattern;

fn pattern(source: &str) -> KeyPattern {
    KeyPattern::parse(source).unwrap_or_else(|error| panic!("`{source}` should parse: {error}"))
}

/// Assert a pattern's verdict on a batch of keys, so every grant in this file
/// sits next to the denial that bounds it.
fn assert_matches(source: &str, allowed: &[&str], denied: &[&str]) {
    let pattern = pattern(source);
    for key in allowed {
        assert!(pattern.matches(key), "`{source}` should match `{key}`");
    }
    for key in denied {
        assert!(!pattern.matches(key), "`{source}` should not match `{key}`");
    }
}

/// The single most important test in this file. `globset` would answer `true`
/// here unless the caller remembered `literal_separator(true)`, which is the
/// whole reason this matcher is hand-rolled: a rule scoped to one directory must
/// not silently grant everything beneath it.
#[test]
fn a_single_star_never_crosses_a_path_separator() {
    // Given / When / Then
    assert_matches(
        "public/*",
        &["public/index.md", "public/a b.md", "public/.hidden"],
        &[
            "public/deep/secret.md",
            "public/a/b/c.md",
            "public",
            "private/index.md",
        ],
    );
}

#[test]
fn a_double_star_spans_whole_segments_including_the_prefix_itself() {
    assert_matches(
        "public/**",
        &[
            "public",
            "public/index.md",
            "public/a/b/c.md",
            "public/a/b/c/d/e.md",
        ],
        &["publicity/index.md", "private/index.md", "publicx"],
    );
}

#[test]
fn a_bare_double_star_matches_every_key() {
    assert_matches(
        "**",
        &["a", "a/b", "a/b/c/d/e", ".notedthat/manifest.json"],
        &[],
    );
}

#[test]
fn a_double_star_matches_in_the_middle_of_a_pattern() {
    assert_matches(
        "docs/**/index.md",
        &["docs/index.md", "docs/a/index.md", "docs/a/b/c/index.md"],
        &["docs/index.txt", "docs/a/index.md/more", "other/index.md"],
    );
}

#[test]
fn a_trailing_wildcard_suffix_matches_within_any_segment_depth() {
    assert_matches(
        "**/*.md",
        &["a.md", "x/y/a.md", "deeply/nested/path/note.md"],
        &["a.txt", "a.md/b", "notes/a.markdown"],
    );
}

#[test]
fn brace_alternation_expands_to_each_branch_and_nothing_else() {
    assert_matches(
        "docs/{rfc,draft}/*.md",
        &["docs/rfc/1.md", "docs/draft/1.md"],
        &[
            "docs/other/1.md",
            "docs/rfc/sub/1.md",
            "docs/rfcdraft/1.md",
            "docs/1.md",
        ],
    );
}

#[test]
fn brace_alternation_composes_across_segments() {
    assert_matches(
        "{a,b}/{c,d}.md",
        &["a/c.md", "a/d.md", "b/c.md", "b/d.md"],
        &["c/a.md", "a/e.md", "a/b/c.md"],
    );
}

#[test]
fn a_question_mark_matches_exactly_one_character_and_never_a_separator() {
    assert_matches(
        "log-?.txt",
        &["log-1.txt", "log-a.txt"],
        &["log-.txt", "log-12.txt", "log-/.txt"],
    );
}

#[test]
fn a_pattern_with_no_wildcards_matches_only_that_exact_key() {
    assert_matches(
        "public/index.md",
        &["public/index.md"],
        &["public/index.md.bak", "Public/index.md", "public/index"],
    );
}

#[test]
fn matching_is_case_sensitive_and_preserves_unicode() {
    assert_matches(
        "notes/Ünïcode-*.md",
        &["notes/Ünïcode-über.md"],
        &["notes/ünïcode-über.md", "notes/Unicode-uber.md"],
    );
}

/// The matcher is iterative with a single backtrack point, so these terminate
/// promptly rather than exploring an exponential search space. If someone
/// reintroduces recursion or naive backtracking, this test hangs.
#[test]
fn pathological_patterns_terminate_promptly() {
    // Given
    let deep_key = std::iter::repeat_n("seg", 256)
        .collect::<Vec<_>>()
        .join("/");
    let long_run = "a".repeat(10_000);

    // When
    let started = std::time::Instant::now();
    let nested_stars = pattern("**/**/**/**/**/x").matches(&deep_key);
    let star_run = pattern("*a*a*a*a*a*a*b").matches(&long_run);
    let elapsed = started.elapsed();

    // Then
    assert!(!nested_stars, "the key does not end in `x`");
    assert!(!star_run, "the key contains no `b`");
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "matching took {elapsed:?}; the matcher should be linear"
    );
}

#[test]
fn a_pattern_at_the_alternative_cap_parses_and_one_over_is_refused() {
    // Given — eight two-way groups is 256 alternatives, the cap exactly.
    let at_cap = "{a,b}".repeat(8);
    let over_cap = "{a,b}".repeat(9);

    // When / Then
    assert!(
        KeyPattern::parse(&at_cap).is_ok(),
        "256 alternatives is allowed"
    );
    let error = KeyPattern::parse(&over_cap).expect_err("512 alternatives is refused");
    assert!(
        error.to_string().contains("brace alternatives"),
        "the error should name the cap: {error}"
    );
}

#[test]
fn malformed_patterns_are_refused_at_parse_time() {
    // Given — each of these is a manifest an operator could plausibly write.
    let cases = [
        ("", "empty"),
        ("a**b", "'**' must be a whole segment"),
        ("**a/b", "'**' must be a whole segment"),
        ("{a", "unclosed"),
        ("a}", "matching"),
        ("{a,{b,c}}", "nested"),
        ("{a,}", "empty branch"),
        ("{**,a}/b", "'**' must be a whole segment"),
        ("a//b", "empty segments"),
        ("../x", "'.' or '..'"),
        ("a/./b", "'.' or '..'"),
        ("/leading", "must not start with '/'"),
        ("trailing/", "empty segments"),
        ("back\\slash", "backslash"),
    ];

    // When / Then
    for (source, expected) in cases {
        let error = KeyPattern::parse(source)
            .map(|_| ())
            .expect_err(&format!("`{source}` should be refused"));
        assert!(
            error.to_string().contains(expected),
            "`{source}` should mention `{expected}`, said: {error}"
        );
    }
}

#[test]
fn oversized_patterns_are_refused_before_expansion() {
    // Given
    let too_long = "a".repeat(1025);
    let too_many_segments = vec!["a"; 65].join("/");

    // When / Then
    assert!(
        KeyPattern::parse(&too_long).is_err(),
        "1025 bytes is refused"
    );
    assert!(
        KeyPattern::parse(&too_many_segments).is_err(),
        "65 segments is refused"
    );
}

#[test]
fn a_pattern_round_trips_through_json_preserving_its_source_text() {
    // Given
    let source = "docs/{rfc,draft}/**/*.md";
    let parsed = pattern(source);

    // When
    let encoded = serde_json::to_string(&parsed).expect("serialize");
    let decoded: KeyPattern = serde_json::from_str(&encoded).expect("deserialize");

    // Then
    assert_eq!(encoded, format!("\"{source}\""));
    assert_eq!(decoded, parsed);
    assert_eq!(decoded.as_str(), source);
}

#[test]
fn an_invalid_pattern_cannot_be_deserialized() {
    // Given / When
    let result: Result<KeyPattern, _> = serde_json::from_str("\"a**b\"");

    // Then
    assert!(
        result.is_err(),
        "deserialization must validate, or a manifest could smuggle in a bad pattern"
    );
}

#[test]
fn the_whole_kb_pattern_is_double_star_and_knows_itself() {
    // Given / When
    let whole = KeyPattern::whole_kb();

    // Then
    assert_eq!(whole.as_str(), "**");
    assert!(whole.is_whole_kb());
    assert!(!pattern("public/**").is_whole_kb());
}

#[test]
fn a_literal_leading_segment_is_reported_as_a_prefix_hint() {
    // Given / When / Then — the hint is what lets a listing push a grant's
    // narrowing into the storage backend instead of scanning and discarding.
    assert_eq!(pattern("public/**").literal_prefix(), Some("public"));
    assert_eq!(pattern("public/a/*.md").literal_prefix(), Some("public"));
    assert_eq!(pattern("**").literal_prefix(), None);
    assert_eq!(pattern("*/a.md").literal_prefix(), None);
    assert_eq!(
        pattern("{a,b}/x").literal_prefix(),
        None,
        "diverging branches share no usable prefix"
    );
    assert_eq!(
        pattern("docs/{rfc,draft}/x").literal_prefix(),
        Some("docs"),
        "branches below a shared literal segment keep the hint"
    );
}

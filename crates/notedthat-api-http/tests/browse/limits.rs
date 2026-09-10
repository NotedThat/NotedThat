use notedthat_core::{Principal, Verb};

use super::fixture::{app_with_keys, grant, grant_under, page, policy};

/// Matches `listing::BROWSE_MAX_KEYS`.
const CAP: usize = 10_000;

fn everything() -> notedthat_core::AccessPolicy {
    policy([grant(Principal::Anyone, [Verb::List, Verb::Read])])
}

#[tokio::test]
async fn a_listing_below_the_cap_shows_no_truncation_notice() {
    // Given / When
    let keys: Vec<String> = (0..50).map(|index| format!("k{index:05}.md")).collect();
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let app = app_with_keys(everything(), &refs).await;
    let html = page(&app, "/browse/notes/", None).await;

    // Then
    assert!(!html.contains("truncated"), "{html}");
    assert!(html.contains("50 objects"), "{html}");
}

#[tokio::test]
async fn a_listing_at_the_cap_renders_what_it_read_and_says_where_it_stopped() {
    // Given — one key past the cap.
    let keys: Vec<String> = (0..=CAP).map(|index| format!("k{index:06}.md")).collect();
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let app = app_with_keys(everything(), &refs).await;

    // When
    let html = page(&app, "/browse/notes/", None).await;

    // Then — WebDAV answers 507 here because its consumer is a sync client that
    // would read a short listing as a complete one. A person gets a partial page
    // and a sentence saying so, because keys arrive in lexicographic order and
    // what is shown is a correct prefix of the truth.
    assert!(html.contains("Listing truncated"), "{html}");
    assert!(html.contains(&format!("{CAP}")), "{html}");
    assert!(
        html.contains("k000000.md"),
        "the page still shows what it read"
    );
}

#[tokio::test]
async fn a_narrow_grant_is_reached_even_when_a_denied_prefix_would_fill_the_cap() {
    // Given — a knowledge base whose denied bulk sorts before the granted area,
    // and more of it than the cap can read.
    let mut keys: Vec<String> = (0..=CAP)
        .map(|index| format!("archive/{index:06}.md"))
        .collect();
    keys.push("public/index.md".to_string());
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let app = app_with_keys(
        policy([grant_under(
            Principal::Anyone,
            [Verb::List, Verb::Read],
            &["public/**"],
        )]),
        &refs,
    )
    .await;

    // When — the knowledge-base root, where the caller names no prefix of their
    // own, so only the grant can narrow the scan.
    let html = page(&app, "/browse/notes/", None).await;

    // Then — the scan starts at the grant, not at `archive/`, so the published
    // area is on the page and the read was complete. Scanning from the root
    // would spend the whole cap inside `archive/` and render an empty page with
    // a notice that reads as "there is more" rather than "you saw none of it".
    assert!(html.contains("1 folder, 0 objects"), "{html}");
    assert!(html.contains("public/"), "{html}");
    assert!(!html.contains("Listing truncated"), "{html}");
}

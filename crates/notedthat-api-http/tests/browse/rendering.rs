use notedthat_core::{Principal, Verb};

use super::fixture::{app, grant_under, page, policy};

fn public_tree() -> notedthat_core::AccessPolicy {
    policy([grant_under(
        Principal::Anyone,
        [Verb::List, Verb::Read],
        &["public/**"],
    )])
}

#[tokio::test]
async fn html_metacharacters_in_an_object_name_are_escaped() {
    // Given — a seeded key named `a & b <c>.md`.
    let app = app(public_tree()).await;

    // When
    let html = page(&app, "/browse/notes/public/", None).await;

    // Then
    assert!(html.contains("a &amp; b &lt;c&gt;.md"), "{html}");
    assert!(!html.contains("<c>"), "{html}");
}

#[tokio::test]
async fn a_bidirectional_override_in_a_name_is_replaced_before_display() {
    // Given — `report\u{202E}fdp.exe` displays as `reportexe.pdf` if passed
    // through, which is a lie about what the reader is clicking.
    let app = app(public_tree()).await;

    // When
    let html = page(&app, "/browse/notes/public/", None).await;

    // Then — replaced in the label, preserved in the link.
    assert!(
        !html.contains('\u{202E}'),
        "the override must not reach the page"
    );
    assert!(html.contains('\u{FFFD}'), "{html}");
    assert!(
        html.contains("%E2%80%AE"),
        "the href still names the real key: {html}"
    );
}

#[tokio::test]
async fn the_page_carries_no_script_and_no_external_reference() {
    // Given / When
    let app = app(public_tree()).await;
    let html = page(&app, "/browse/notes/public/", None).await;

    // Then — "no client-side application" is an acceptance criterion, so it is
    // worth asserting rather than assuming.
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("http://"), "{html}");
    assert!(!html.contains("https://"), "{html}");
    assert!(!html.contains("<img"), "{html}");
}

#[tokio::test]
async fn folders_sort_before_objects_and_each_group_sorts_lexicographically() {
    // Given / When
    let app = app(public_tree()).await;
    let html = page(&app, "/browse/notes/public/", None).await;

    // Then
    let folder = html.find("drafts/").expect("the folder row");
    let object = html.find("index.md").expect("an object row");
    assert!(folder < object, "folders come first");

    let ampersand = html.find("a &amp; b").expect("`a & b` row");
    let spaced = html.find("a b.md").expect("`a b` row");
    assert!(ampersand < spaced, "objects sort by key");
}

#[tokio::test]
async fn a_folder_row_shows_a_placeholder_rather_than_an_invented_size_or_date() {
    // Given / When — WebDAV has to invent a timestamp because the protocol
    // demands one; a page does not, and printing 1970 beside a folder would be
    // a lie.
    let app = app(public_tree()).await;
    let html = page(&app, "/browse/notes/public/", None).await;

    // Then
    assert!(html.contains('—'), "{html}");
    assert!(!html.contains("1970-01-01"), "{html}");
}

#[tokio::test]
async fn the_summary_counts_folders_and_objects_separately() {
    // Given / When
    let app = app(public_tree()).await;
    let html = page(&app, "/browse/notes/public/drafts/", None).await;

    // Then
    assert!(html.contains("1 folder, 1 object"), "{html}");
}

#[tokio::test]
async fn the_breadcrumb_links_every_ancestor() {
    // Given / When
    let app = app(public_tree()).await;
    let html = page(&app, "/browse/notes/public/drafts/", None).await;

    // Then
    for expected in [
        "/browse/",
        "/browse/notes/",
        "/browse/notes/public/",
        "/browse/notes/public/drafts/",
    ] {
        assert!(
            html.contains(&format!("href=\"{expected}\"")),
            "missing breadcrumb {expected}: {html}"
        );
    }
}

use axum::http::StatusCode;
use notedthat_core::{Principal, Verb};

use super::fixture::{app, get, grant, grant_under, hrefs, location, page, parent_link, policy};

/// The everyday grant: anonymous callers may walk and read `public/`.
fn public_tree() -> notedthat_core::AccessPolicy {
    policy([grant_under(
        Principal::Anyone,
        [Verb::List, Verb::Read],
        &["public/**"],
    )])
}

#[tokio::test]
async fn the_index_lists_a_knowledge_base_that_grants_something() {
    // Given / When
    let app = app(public_tree()).await;
    let html = page(&app, "/browse/", None).await;

    // Then
    assert!(html.contains("notes/"), "{html}");
    assert!(
        !html.contains("private/"),
        "a knowledge base with no anonymous grant must not be named: {html}"
    );
    assert!(
        hrefs(&html).contains(&"/browse/notes/".to_string()),
        "{html}"
    );
}

#[tokio::test]
async fn the_index_says_so_plainly_when_nothing_is_published() {
    // Given — the credential holder's grant only.
    let app = app(policy([grant(Principal::SignedIn, Verb::ALL)])).await;

    // When
    let html = page(&app, "/browse/", None).await;

    // Then — an empty page rather than a 404, which would leak nothing either
    // way but reads as a mistake.
    assert!(html.contains("Nothing is published here."), "{html}");
    assert!(!html.contains("/browse/notes/"), "{html}");
}

#[tokio::test]
async fn a_knowledge_base_root_lists_its_top_level_and_nothing_deeper() {
    // Given / When
    let app = app(public_tree()).await;
    let html = page(&app, "/browse/notes/", None).await;

    // Then — one folder, from a grant scoped to it.
    assert!(
        hrefs(&html).contains(&"/browse/notes/public/".to_string()),
        "{html}"
    );
    assert!(
        !html.contains("internal"),
        "a prefix outside the grant must not appear: {html}"
    );
    assert!(
        !html.contains("index.md"),
        "a key two levels down must not appear at the root: {html}"
    );
}

#[tokio::test]
async fn a_nested_directory_lists_only_its_own_level() {
    // Given / When
    let app = app(public_tree()).await;
    let html = page(&app, "/browse/notes/public/", None).await;
    let links = hrefs(&html);

    // Then
    assert!(
        links.contains(&"/browse/notes/public/drafts/".to_string()),
        "{html}"
    );
    assert!(
        links.contains(&"/api/v1/knowledgebases/notes/public/index.md".to_string()),
        "{html}"
    );
    assert!(
        !html.contains("buried.md"),
        "a key two levels down must not appear: {html}"
    );
}

#[tokio::test]
async fn every_directory_offers_a_parent_link_that_walks_back_to_the_index() {
    // Given
    let app = app(public_tree()).await;

    // When — walk up from the deepest page by following `../` only.
    let mut uri = "/browse/notes/public/drafts/deep/".to_string();
    let mut walked = vec![uri.clone()];
    for _ in 0..4 {
        let html = page(&app, &uri, None).await;
        uri = parent_link(&html);
        walked.push(uri.clone());
    }

    // Then
    assert_eq!(
        walked,
        vec![
            "/browse/notes/public/drafts/deep/",
            "/browse/notes/public/drafts/",
            "/browse/notes/public/",
            "/browse/notes/",
            "/browse/",
        ],
        "the parent chain must reach the index"
    );
}

#[tokio::test]
async fn a_directory_url_without_its_trailing_slash_redirects_to_the_canonical_form() {
    // Given
    let app = app(public_tree()).await;

    // When
    let prefix = get(&app, "/browse", None).await;
    let kb = get(&app, "/browse/notes", None).await;
    let folder = get(&app, "/browse/notes/public", None).await;

    // Then — the mount point is a permanent fact about the route; a folder is a
    // fact about the data, which can change under us.
    assert_eq!(prefix.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(location(&prefix), "/browse/");
    assert_eq!(kb.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(location(&kb), "/browse/notes/");
    assert_eq!(folder.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(location(&folder), "/browse/notes/public/");
}

#[tokio::test]
async fn an_object_url_redirects_to_its_existing_api_representation() {
    // Given — the acceptance criterion: no second download path.
    let app = app(public_tree()).await;

    // When
    let response = get(&app, "/browse/notes/public/index.md", None).await;

    // Then
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        location(&response),
        "/api/v1/knowledgebases/notes/public/index.md"
    );
}

#[tokio::test]
async fn a_folder_and_an_object_of_the_same_name_are_both_reachable() {
    // Given — WebDAV must hide one; HTML has no such constraint, and a surface
    // whose job is showing what is stored should not silently drop a stored
    // object.
    let app = app(public_tree()).await;

    // When
    let html = page(&app, "/browse/notes/public/", None).await;
    let folder = get(&app, "/browse/notes/public/collide/", None).await;
    let object = get(&app, "/browse/notes/public/collide", None).await;

    // Then
    let links = hrefs(&html);
    assert!(
        links.contains(&"/browse/notes/public/collide/".to_string()),
        "{html}"
    );
    assert!(
        links.contains(&"/api/v1/knowledgebases/notes/public/collide".to_string()),
        "{html}"
    );
    assert_eq!(folder.status(), StatusCode::OK);
    assert_eq!(object.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_granted_but_empty_knowledge_base_renders_rather_than_disappearing() {
    // Given — a declared, granted knowledge base with nothing in it.
    let app = super::fixture::app_with_keys(public_tree(), &[]).await;

    // When
    let html = page(&app, "/browse/notes/", None).await;

    // Then — a knowledge base should not 404 at its own front door.
    assert!(html.contains("0 folders, 0 objects"), "{html}");
}

#[tokio::test]
async fn a_folder_with_nothing_visible_in_it_is_not_found() {
    // Given / When — folders are synthesised from keys, so a folder with no
    // visible keys genuinely does not exist.
    let app = app(public_tree()).await;
    let outside = get(&app, "/browse/notes/internal/", None).await;
    let absent = get(&app, "/browse/notes/nope/", None).await;

    // Then
    assert_eq!(outside.status(), StatusCode::NOT_FOUND);
    assert_eq!(absent.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_undeclared_knowledge_base_is_not_found() {
    // Given / When
    let app = app(public_tree()).await;
    let response = get(&app, "/browse/nope/", None).await;

    // Then
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

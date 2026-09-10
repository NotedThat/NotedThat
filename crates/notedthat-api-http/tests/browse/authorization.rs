use axum::http::StatusCode;
use notedthat_core::{AccessPolicy, Principal, Verb};

use super::fixture::{TOKEN, app, get, grant, grant_under, hrefs, page, policy};

fn list_only() -> AccessPolicy {
    policy([grant_under(Principal::Anyone, [Verb::List], &["public/**"])])
}

#[tokio::test]
async fn a_glob_scoped_grant_shows_its_subtree_and_hides_the_rest() {
    // Given / When
    let app = app(policy([grant_under(
        Principal::Anyone,
        [Verb::List, Verb::Read],
        &["public/drafts/**"],
    )]))
    .await;
    let root = page(&app, "/browse/notes/", None).await;

    // Then — only the branch leading to the grant is synthesised.
    assert!(
        hrefs(&root).contains(&"/browse/notes/public/".to_string()),
        "{root}"
    );
    let public = page(&app, "/browse/notes/public/", None).await;
    assert!(
        hrefs(&public).contains(&"/browse/notes/public/drafts/".to_string()),
        "{public}"
    );
    assert!(
        !public.contains("index.md"),
        "a sibling outside the grant must not appear: {public}"
    );
}

#[tokio::test]
async fn a_row_is_hyperlinked_only_where_read_is_granted_for_that_key() {
    // Given — list everything under `public/`, read only the drafts. With glob
    // scoping a single directory can genuinely be part-readable, so this has to
    // be decided per row.
    let app = app(policy([
        grant_under(Principal::Anyone, [Verb::List], &["public/**"]),
        grant_under(Principal::Anyone, [Verb::Read], &["public/drafts/**"]),
    ]))
    .await;

    // When
    let public = page(&app, "/browse/notes/public/", None).await;
    let drafts = page(&app, "/browse/notes/public/drafts/", None).await;

    // Then
    assert!(
        !hrefs(&public).contains(&"/api/v1/knowledgebases/notes/public/index.md".to_string()),
        "an unreadable object must render as plain text: {public}"
    );
    assert!(
        public.contains("index.md"),
        "but it is still listed: {public}"
    );
    assert!(
        hrefs(&drafts).contains(&"/api/v1/knowledgebases/notes/public/drafts/note.md".to_string()),
        "{drafts}"
    );
}

#[tokio::test]
async fn a_listing_without_any_read_grant_says_so_and_links_nothing() {
    // Given / When
    let app = app(list_only()).await;
    let html = page(&app, "/browse/notes/public/", None).await;

    // Then
    assert!(html.contains("listed but not readable"), "{html}");
    assert!(
        !hrefs(&html).iter().any(|href| href.starts_with("/api/v1/")),
        "no object link should be emitted: {html}"
    );
}

#[tokio::test]
async fn an_object_url_is_not_found_when_read_is_denied_for_that_key() {
    // Given — listable but not readable, so the object exists and stays hidden.
    let app = app(list_only()).await;

    // When
    let response = get(&app, "/browse/notes/public/index.md", None).await;

    // Then — indistinguishable from an object that is not there.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_private_knowledge_base_is_not_found_rather_than_forbidden() {
    // Given / When — telling an anonymous caller `403` would turn the status
    // into an oracle for enumerating what exists.
    let app = app(policy([grant(Principal::SignedIn, Verb::ALL)])).await;
    let index = page(&app, "/browse/", None).await;
    let directory = get(&app, "/browse/notes/", None).await;

    // Then
    assert!(!index.contains("/browse/notes/"), "{index}");
    assert_eq!(directory.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_bearer_token_browses_as_the_credential_holder() {
    // Given
    let app = app(policy([grant(Principal::SignedIn, Verb::ALL)])).await;

    // When
    let anonymous = get(&app, "/browse/notes/", None).await;
    let html = page(&app, "/browse/notes/", Some(TOKEN)).await;

    // Then
    assert_eq!(anonymous.status(), StatusCode::NOT_FOUND);
    assert!(
        hrefs(&html).contains(&"/browse/notes/internal/".to_string()),
        "{html}"
    );
}

#[tokio::test]
async fn a_restricted_credential_is_told_it_is_forbidden_rather_than_shown_a_404() {
    // Given — a credentialed caller has already proved they exist, so `403`
    // reveals nothing they could not enumerate and is far easier to debug.
    let app = app(policy([grant_under(
        Principal::SignedIn,
        [Verb::List, Verb::Read],
        &["public/**"],
    )]))
    .await;

    // When
    let granted = get(&app, "/browse/notes/public/", Some(TOKEN)).await;
    let denied = get(&app, "/browse/notes/internal/", Some(TOKEN)).await;

    // Then
    assert_eq!(granted.status(), StatusCode::OK);
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_invalid_credential_is_refused_rather_than_downgraded_to_anonymous() {
    // Given — the rule that stops a typo'd token from silently becoming a
    // public view, applied on this surface too.
    let app = app(policy([grant_under(
        Principal::Anyone,
        [Verb::List],
        &["public/**"],
    )]))
    .await;

    // When
    let wrong = get(&app, "/browse/notes/", Some("not-the-token")).await;
    let absent = get(&app, "/browse/notes/", None).await;

    // Then
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(absent.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_internal_namespace_never_appears_for_any_principal() {
    // Given — the broadest grant each principal can hold.
    let anonymous_app = app(policy([grant(
        Principal::Anyone,
        [Verb::List, Verb::Read, Verb::Search],
    )]))
    .await;
    let signed_in_app = app(policy([grant(Principal::SignedIn, Verb::ALL)])).await;

    // When
    let anonymous = page(&anonymous_app, "/browse/notes/", None).await;
    let signed_in = page(&signed_in_app, "/browse/notes/", Some(TOKEN)).await;
    let direct = get(&signed_in_app, "/browse/notes/.notedthat/", Some(TOKEN)).await;

    // Then — browse goes further than the access model requires: `.notedthat`
    // is server bookkeeping, and these pages are for reading documents.
    assert!(!anonymous.contains(".notedthat"), "{anonymous}");
    assert!(!signed_in.contains(".notedthat"), "{signed_in}");
    assert_eq!(direct.status(), StatusCode::NOT_FOUND);
}

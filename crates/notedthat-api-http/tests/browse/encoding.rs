use axum::http::StatusCode;
use notedthat_core::{Principal, Verb};

use super::fixture::{app, app_with_keys, get, grant_under, hrefs, location, page, policy};

fn public_tree() -> notedthat_core::AccessPolicy {
    policy([grant_under(
        Principal::Anyone,
        [Verb::List, Verb::Read],
        &["public/**"],
    )])
}

#[tokio::test]
async fn a_name_with_a_space_is_encoded_in_the_link_and_readable_in_the_label() {
    // Given / When
    let app = app(public_tree()).await;
    let html = page(&app, "/browse/notes/public/", None).await;

    // Then
    assert!(
        hrefs(&html).contains(&"/api/v1/knowledgebases/notes/public/a%20b.md".to_string()),
        "{html}"
    );
    assert!(html.contains("a b.md"), "the label stays readable: {html}");
}

#[tokio::test]
async fn a_generated_link_round_trips_back_to_the_page_that_produced_it() {
    // Given — the property that actually matters: follow what is rendered and
    // arrive where it points.
    let app = app_with_keys(
        public_tree(),
        &[
            "public/odd name/a #b ?c.md",
            "public/odd name/naïve.md",
            "public/plain.md",
        ],
    )
    .await;

    // When
    let root = page(&app, "/browse/notes/public/", None).await;
    let folder_href = hrefs(&root)
        .into_iter()
        .find(|href| href.contains("odd") && href.ends_with('/'))
        .expect("a folder link");
    let folder = page(&app, &folder_href, None).await;
    let object_href = hrefs(&folder)
        .into_iter()
        .find(|href| href.starts_with("/api/v1/") && href.contains("%23"))
        .expect("a link to the awkward key");
    let object = get(&app, &object_href, None).await;

    // Then
    assert!(folder.contains("naïve.md"), "{folder}");
    assert_eq!(object.status(), StatusCode::OK, "{object_href}");
}

#[tokio::test]
async fn reserved_characters_never_survive_unencoded_into_a_link() {
    // Given / When
    let app = app_with_keys(public_tree(), &["public/a #b ?c.md"]).await;
    let html = page(&app, "/browse/notes/public/", None).await;

    // Then — a `#` left raw would truncate the URL at the fragment, and a `?`
    // would turn the rest of the key into a query string.
    for href in hrefs(&html) {
        assert!(!href.contains('#'), "{href}");
        assert!(!href.contains('?'), "{href}");
        assert!(!href.contains(' '), "{href}");
    }
}

#[tokio::test]
async fn a_percent_encoded_knowledge_base_slug_reaches_the_same_page() {
    // Given / When
    let app = app(public_tree()).await;
    let plain = get(&app, "/browse/notes/", None).await;
    let encoded = get(&app, "/browse/%6Eotes/", None).await;

    // Then
    assert_eq!(plain.status(), StatusCode::OK);
    assert_eq!(encoded.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_url_containing_a_dot_dot_segment_is_not_found() {
    // Given / When
    let app = app(public_tree()).await;

    // Then — `ObjectPath` rejects dot segments rather than resolving them, and
    // browse answers 404 rather than 400: on a page, a traversal attempt and a
    // typo deserve the same reply.
    for uri in [
        "/browse/notes/public/../internal/",
        "/browse/notes/../notes/public/",
        "/browse/notes/public/%2e%2e/internal/",
    ] {
        let response = get(&app, uri, None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn a_url_with_invalid_percent_escapes_renders_a_page_rather_than_a_bare_error() {
    // Given / When
    let app = app(public_tree()).await;
    let response = get(&app, "/browse/notes/public/%ff%ff/", None).await;

    // Then
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .expect("content-type"),
        "text/html; charset=utf-8",
        "even a rejection should be a page on this surface"
    );
}

#[tokio::test]
async fn an_object_key_that_could_escape_its_knowledge_base_is_never_listed() {
    // Given — a key with a `..` segment. Our write path rejects these, but S3
    // accepts them and a bucket can be filled out of band; rendered as a link,
    // `/api/v1/knowledgebases/notes/a/../../docs/x` resolves into a *different*
    // knowledge base in the browser.
    //
    // The seeding here goes through `ObjectPath`, so this asserts the reachable
    // half: nothing in the tree produces such an href.
    let app = app(public_tree()).await;

    // When
    let html = page(&app, "/browse/notes/public/", None).await;

    // Then
    for href in hrefs(&html) {
        assert!(!href.contains("/../"), "{href}");
        assert!(!href.contains("/./"), "{href}");
    }
}

#[tokio::test]
async fn a_redirect_target_is_encoded_the_same_way_a_link_would_be() {
    // Given / When
    let app = app_with_keys(public_tree(), &["public/odd name/plain.md"]).await;
    let folder = get(&app, "/browse/notes/public/odd%20name", None).await;

    // Then
    assert_eq!(folder.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(location(&folder), "/browse/notes/public/odd%20name/");
}

//! Integration tests for the `WebDAV` surface against an in-memory `Storage`.
//!
//! These used to run against a `SeaweedFS` testcontainer started fresh for each
//! test — twenty-one container boots for twenty-one tests. The subject here is
//! the `WebDAV` surface (routing, method semantics, conditional requests, COPY
//! and MOVE, listing), not the S3 wire protocol, so the backend is
//! `InMemoryStorage`, which implements the same `Storage` contract with the same
//! `ETag`, conditional and range semantics. Every assertion below is unchanged.
//!
//! Because nothing here needs Docker any more, these are ordinary tests rather
//! than `#[ignore]` ones, and run in the normal `cargo test` pass.
//!
//! Run with: cargo test -p notedthat-webdav --test `webdav_integration`

#![allow(missing_docs)]

use axum::http::StatusCode;
use base64::Engine as _;
use notedthat_core::{KbSlug, Storage, testing::InMemoryStorage};
use notedthat_webdav::{router::build_router, state::WebDavState};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

async fn start_webdav_server() -> (tokio::task::JoinHandle<()>, String, String, String) {
    let storage = Arc::new(InMemoryStorage::default());

    let kb_notes = KbSlug::try_new("notes").unwrap();
    storage
        .ensure_bucket(&kb_notes)
        .await
        .expect("ensure_bucket for notes");

    let (tx, _rx) = mpsc::channel(100);

    let mut declared_kbs = BTreeMap::new();
    declared_kbs.insert("notes".to_string(), KbSlug::try_new("notes").unwrap());
    declared_kbs.insert("scratch".to_string(), KbSlug::try_new("scratch").unwrap());

    let username = "test-user".to_string();
    let password = "test-pass".to_string();

    let state = WebDavState {
        username: Arc::new(username.clone()),
        password: Arc::new(password.clone()),
        storage: storage as Arc<dyn notedthat_core::Storage>,
        staging_config: notedthat_core::StagingConfig::default(),
        access_policies: Arc::new(notedthat_core::signed_in_policies(&declared_kbs)),
        declared_kbs: Arc::new(declared_kbs),
        indexer_tx: tx,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let url = format!("http://{addr}");

    let app = build_router(state);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    // Brief pause so axum's accept loop is ready.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    (handle, url, username, password)
}

fn basic_auth(user: &str, pass: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {encoded}")
}

fn webdav_method(name: &'static [u8]) -> reqwest::Method {
    reqwest::Method::from_bytes(name).expect("valid method name")
}

// ---------------------------------------------------------------------------
// 1. OPTIONS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_options_returns_dav_1() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .request(reqwest::Method::OPTIONS, format!("{url}/webdav"))
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("OPTIONS request");

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let dav = resp
        .headers()
        .get("dav")
        .expect("DAV header must be present");
    let dav_value = dav.to_str().unwrap();
    assert_eq!(
        dav_value, "1",
        "DAV header must be exactly '1', not '1,2,3'"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// 2. Request without auth returns 401
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_request_without_auth_returns_401() {
    let (handle, url, _username, _password) = start_webdav_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/webdav/notes/anything.md"))
        .send()
        .await
        .expect("GET without auth");

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(
        resp.headers().contains_key("www-authenticate"),
        "WWW-Authenticate header must be present on 401"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// 3. PROPFIND root lists declared KBs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_propfind_root_lists_kbs() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .request(webdav_method(b"PROPFIND"), format!("{url}/webdav"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Depth", "1")
        .send()
        .await
        .expect("PROPFIND /");

    assert_eq!(resp.status().as_u16(), 207, "expected 207 Multi-Status");
    let body = resp.text().await.expect("body text");
    assert!(
        body.contains("notes"),
        "root PROPFIND response should mention 'notes' KB; body: {body}"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// 4. PROPFIND on KB lists objects
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_propfind_kb_lists_objects() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();

    // PUT a file so there is something to list.
    let put_resp = client
        .put(format!("{url}/webdav/notes/listed-file.md"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Content-Type", "text/markdown")
        .body("# Listed")
        .send()
        .await
        .expect("PUT listed-file.md");
    assert_eq!(put_resp.status(), StatusCode::CREATED);

    // PROPFIND the KB.
    let resp = client
        .request(webdav_method(b"PROPFIND"), format!("{url}/webdav/notes/"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Depth", "1")
        .send()
        .await
        .expect("PROPFIND /notes/");

    assert_eq!(resp.status().as_u16(), 207, "expected 207 Multi-Status");
    let body = resp.text().await.expect("body text");
    assert!(
        body.contains("listed-file.md"),
        "PROPFIND /notes/ should list 'listed-file.md'; body: {body}"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// 5. PROPFIND Depth: infinity → 501
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_propfind_depth_infinity_returns_501() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .request(webdav_method(b"PROPFIND"), format!("{url}/webdav"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Depth", "infinity")
        .send()
        .await
        .expect("PROPFIND Depth: infinity");

    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);

    handle.abort();
}

// ---------------------------------------------------------------------------
// 6. PUT creates object and returns ETag
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_put_creates_object_and_returns_etag() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{url}/webdav/notes/new-object.md"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Content-Type", "text/markdown")
        .body("# Hello WebDAV")
        .send()
        .await
        .expect("PUT new-object.md");

    assert_eq!(resp.status(), StatusCode::CREATED);
    assert!(
        resp.headers().contains_key("etag"),
        "ETag header must be present on 201 response"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// 7. PUT .md with Content-Type: application/octet-stream → MIME sniff
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_put_md_with_octet_stream_stored_as_text_markdown() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();

    // PUT with octet-stream Content-Type for a .md file.
    let put_resp = client
        .put(format!("{url}/webdav/notes/sniff-test.md"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Content-Type", "application/octet-stream")
        .body("# MIME Sniff Test")
        .send()
        .await
        .expect("PUT sniff-test.md");
    assert_eq!(put_resp.status(), StatusCode::CREATED);

    // HEAD to verify the stored Content-Type was sniffed as text/markdown.
    let head_resp = client
        .head(format!("{url}/webdav/notes/sniff-test.md"))
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("HEAD sniff-test.md");

    assert_eq!(head_resp.status(), StatusCode::OK);
    let content_type = head_resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.contains("text/markdown"),
        "Content-Type after MIME sniff should be text/markdown; got: {content_type}"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// 8. PUT 17 MiB body succeeds (no DefaultBodyLimit on WebDAV router)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_put_17mib_body_succeeds() {
    let (handle, url, username, password) = start_webdav_server().await;

    // 17 MiB > the 16 MiB DefaultBodyLimit used by the HTTP API router.
    // The WebDAV router has NO DefaultBodyLimit, so this must succeed.
    let body_17mib = vec![b'x'; 17 * 1024 * 1024];

    let client = reqwest::Client::new();
    let resp = client
        .put(format!("{url}/webdav/notes/large-upload.bin"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Content-Type", "application/octet-stream")
        .body(body_17mib)
        .send()
        .await
        .expect("PUT 17 MiB body");

    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "17 MiB PUT must return 201 (WebDAV router has no DefaultBodyLimit)"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// 9. PUT with wrong If-Match returns 412
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_put_with_if_match_wrong_etag_returns_412() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();

    // Create the object first so it exists.
    let first = client
        .put(format!("{url}/webdav/notes/conditional.md"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Content-Type", "text/markdown")
        .body("initial content")
        .send()
        .await
        .expect("initial PUT");
    assert_eq!(first.status(), StatusCode::CREATED);

    // Attempt overwrite with an ETag that doesn't match.
    let resp = client
        .put(format!("{url}/webdav/notes/conditional.md"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Content-Type", "text/markdown")
        .header("If-Match", "\"definitely-wrong-etag\"")
        .body("overwrite attempt")
        .send()
        .await
        .expect("PUT with wrong If-Match");

    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);

    handle.abort();
}

// ---------------------------------------------------------------------------
// 10. GET full body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_full_body() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();
    let content = "# Full Body Test\nHello, World!\n";

    client
        .put(format!("{url}/webdav/notes/get-test.md"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Content-Type", "text/markdown")
        .body(content)
        .send()
        .await
        .expect("PUT get-test.md");

    let resp = client
        .get(format!("{url}/webdav/notes/get-test.md"))
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("GET get-test.md");

    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.expect("body text");
    assert_eq!(body, content, "GET body must match what was PUT");

    handle.abort();
}

// ---------------------------------------------------------------------------
// 11. GET with Range header → 206 Partial Content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_range() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();
    let content = "Hello, Range Requests!";

    client
        .put(format!("{url}/webdav/notes/range-test.md"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Content-Type", "text/markdown")
        .body(content)
        .send()
        .await
        .expect("PUT range-test.md");

    let resp = client
        .get(format!("{url}/webdav/notes/range-test.md"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Range", "bytes=0-4")
        .send()
        .await
        .expect("GET range-test.md with Range header");

    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);

    handle.abort();
}

// ---------------------------------------------------------------------------
// 12. HEAD returns metadata, no body
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_head_returns_metadata_no_body() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();

    client
        .put(format!("{url}/webdav/notes/head-test.md"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Content-Type", "text/markdown")
        .body("# Head Test")
        .send()
        .await
        .expect("PUT head-test.md");

    let resp = client
        .head(format!("{url}/webdav/notes/head-test.md"))
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("HEAD head-test.md");

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().contains_key("content-length"),
        "HEAD must return Content-Length header"
    );
    // reqwest does not read HEAD body; body length is 0 by protocol.
    assert_eq!(
        resp.content_length().unwrap_or(0),
        0,
        "HEAD response must carry no body bytes"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// 13. DELETE is idempotent (both returns 204)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_idempotent_returns_204() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();

    // First DELETE — object does not exist; still idempotent.
    let resp1 = client
        .delete(format!("{url}/webdav/notes/delete-me.md"))
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("first DELETE");

    // Second DELETE — same path, already gone.
    let resp2 = client
        .delete(format!("{url}/webdav/notes/delete-me.md"))
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("second DELETE");

    assert_eq!(
        resp1.status(),
        StatusCode::NO_CONTENT,
        "first DELETE must be 204"
    );
    assert_eq!(
        resp2.status(),
        StatusCode::NO_CONTENT,
        "second DELETE must be 204"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// 14. MKCOL returns 201, but PROPFIND does not show empty folder
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_mkcol_returns_201_no_persistence() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();

    // MKCOL should succeed (create_dir always returns Ok in v1).
    let mkcol_resp = client
        .request(
            webdav_method(b"MKCOL"),
            format!("{url}/webdav/notes/newfolder/"),
        )
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("MKCOL /notes/newfolder/");

    assert_eq!(mkcol_resp.status(), StatusCode::CREATED);

    // PROPFIND /notes/ must NOT list "newfolder" — S3 has no empty-directory primitive.
    let propfind_resp = client
        .request(webdav_method(b"PROPFIND"), format!("{url}/webdav/notes/"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Depth", "1")
        .send()
        .await
        .expect("PROPFIND /notes/ after MKCOL");

    assert_eq!(propfind_resp.status().as_u16(), 207);
    let body = propfind_resp.text().await.expect("body text");
    assert!(
        !body.contains("newfolder"),
        "PROPFIND must not show 'newfolder' — S3 collapses empty collections; body: {body}"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// 15. MOVE single object succeeds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_single_object_move_succeeds() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();

    client
        .put(format!("{url}/webdav/notes/move-source.md"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Content-Type", "text/markdown")
        .body("# Move Me")
        .send()
        .await
        .expect("PUT move-source.md");

    let move_resp = client
        .request(
            webdav_method(b"MOVE"),
            format!("{url}/webdav/notes/move-source.md"),
        )
        .header("Authorization", basic_auth(&username, &password))
        .header("Destination", format!("{url}/webdav/notes/move-dest.md"))
        .send()
        .await
        .expect("MOVE move-source.md → move-dest.md");

    assert_eq!(move_resp.status(), StatusCode::CREATED);

    // Source must be gone.
    let get_src = client
        .get(format!("{url}/webdav/notes/move-source.md"))
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("GET move-source.md after MOVE");
    assert_eq!(
        get_src.status(),
        StatusCode::NOT_FOUND,
        "source must be absent after MOVE"
    );

    // Destination must be present with the correct body.
    let get_dst = client
        .get(format!("{url}/webdav/notes/move-dest.md"))
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("GET move-dest.md after MOVE");
    assert_eq!(get_dst.status(), StatusCode::OK);
    assert_eq!(get_dst.text().await.unwrap(), "# Move Me");

    handle.abort();
}

// ---------------------------------------------------------------------------
// 16. COPY single object succeeds (source and dest both present)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_single_object_copy_succeeds() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();

    client
        .put(format!("{url}/webdav/notes/copy-source.md"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Content-Type", "text/markdown")
        .body("# Copy Me")
        .send()
        .await
        .expect("PUT copy-source.md");

    let copy_resp = client
        .request(
            webdav_method(b"COPY"),
            format!("{url}/webdav/notes/copy-source.md"),
        )
        .header("Authorization", basic_auth(&username, &password))
        .header("Destination", format!("{url}/webdav/notes/copy-dest.md"))
        .send()
        .await
        .expect("COPY copy-source.md → copy-dest.md");

    assert_eq!(copy_resp.status(), StatusCode::CREATED);

    // Source must still be present.
    let get_src = client
        .get(format!("{url}/webdav/notes/copy-source.md"))
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("GET copy-source.md after COPY");
    assert_eq!(
        get_src.status(),
        StatusCode::OK,
        "source must still be present after COPY"
    );

    // Destination must be present with the correct body.
    let get_dst = client
        .get(format!("{url}/webdav/notes/copy-dest.md"))
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("GET copy-dest.md after COPY");
    assert_eq!(get_dst.status(), StatusCode::OK);
    assert_eq!(get_dst.text().await.unwrap(), "# Copy Me");

    handle.abort();
}

// ---------------------------------------------------------------------------
// 17. MOVE of KB root (collection) returns 403 + <nt:no-collection-move/>
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_collection_move_returns_403_no_collection_move() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();

    let resp = client
        .request(webdav_method(b"MOVE"), format!("{url}/webdav/notes/"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Destination", format!("{url}/webdav/notes/dest.md"))
        .send()
        .await
        .expect("MOVE /notes/ (collection)");

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = resp.text().await.expect("body text");
    assert!(
        body.contains("no-collection-move"),
        "XML error must contain <nt:no-collection-move/>; body: {body}"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// 18. MOVE across KBs returns 403 + <nt:cannot-modify-source/>
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cross_kb_move_returns_403_cannot_modify_source() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();

    // PUT in "notes".
    client
        .put(format!("{url}/webdav/notes/cross-kb-source.md"))
        .header("Authorization", basic_auth(&username, &password))
        .header("Content-Type", "text/markdown")
        .body("# Cross KB")
        .send()
        .await
        .expect("PUT cross-kb-source.md");

    // MOVE to "scratch" (different KB) — must be rejected.
    let resp = client
        .request(
            webdav_method(b"MOVE"),
            format!("{url}/webdav/notes/cross-kb-source.md"),
        )
        .header("Authorization", basic_auth(&username, &password))
        .header(
            "Destination",
            format!("{url}/webdav/scratch/cross-kb-dest.md"),
        )
        .send()
        .await
        .expect("MOVE notes → scratch");

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = resp.text().await.expect("body text");
    assert!(
        body.contains("cannot-modify-source"),
        "XML error must contain <nt:cannot-modify-source/>; body: {body}"
    );

    handle.abort();
}

// ---------------------------------------------------------------------------
// 19. LOCK returns 405
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_lock_returns_405() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .request(
            webdav_method(b"LOCK"),
            format!("{url}/webdav/notes/any-file.md"),
        )
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("LOCK request");

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

    handle.abort();
}

// ---------------------------------------------------------------------------
// 20. UNLOCK returns 405
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_unlock_returns_405() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .request(
            webdav_method(b"UNLOCK"),
            format!("{url}/webdav/notes/any-file.md"),
        )
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("UNLOCK request");

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

    handle.abort();
}

// ---------------------------------------------------------------------------
// 21. PROPPATCH returns 405
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_proppatch_returns_405() {
    let (handle, url, username, password) = start_webdav_server().await;

    let client = reqwest::Client::new();
    let resp = client
        .request(
            webdav_method(b"PROPPATCH"),
            format!("{url}/webdav/notes/any-file.md"),
        )
        .header("Authorization", basic_auth(&username, &password))
        .send()
        .await
        .expect("PROPPATCH request");

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

    handle.abort();
}

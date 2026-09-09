//! End-to-end coverage of the `/browse` surface (#100).
//!
//! The subject is a real server: startup, provisioning, the merged routers and
//! the access rules a manifest actually carries. Only storage, the vector store
//! and the embedder are substituted, so nothing here depends on Docker.
//!
//! What it proves that the router tests cannot: that a manifest written to
//! storage before boot becomes the policy the pages enforce, and that following
//! a link a page rendered lands on the object's bytes over real HTTP.
//!
//! Run with: `cargo test -p notedthat-server --test browse_e2e`
#![allow(missing_docs)]

use notedthat_api_http::testing::InMemoryStorage;
use notedthat_core::{
    AccessRule, ConditionalHeaders, KbManifest, KbSlug, ObjectPath, Principal, Storage, TenantSlug,
    Verb,
};
use notedthat_indexer::testing::{InMemoryVectorStore, StubEmbedder};
use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
use notedthat_server::run::Backends;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

const EMBEDDING_DIM: u32 = 3;
const TOKEN: &str = "browse-e2e-token";
const PUBLIC_KB: &str = "notes";
const PRIVATE_KB: &str = "vault";

/// Objects seeded straight into storage, before the server boots.
const SEEDED: &[&str] = &[
    "public/index.md",
    "public/a note.md",
    "public/drafts/deep/buried.md",
    "internal/secret.md",
    ".notedthat/keep-out.md",
];

fn kb(slug: &str) -> KbSlug {
    KbSlug::try_new(slug).expect("valid slug")
}

/// Seed both knowledge bases, and give `notes` a manifest publishing `public/`.
async fn seed(storage: &InMemoryStorage) {
    for slug in [PUBLIC_KB, PRIVATE_KB] {
        storage.ensure_bucket(&kb(slug)).await.expect("bucket");
    }

    for key in SEEDED {
        storage
            .put_object(
                &kb(PUBLIC_KB),
                &ObjectPath::try_from(*key).expect("valid path"),
                bytes::Bytes::from(format!("body of {key}")),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .expect("seed object");
    }
    storage
        .put_object(
            &kb(PRIVATE_KB),
            &ObjectPath::try_from("locked.md").expect("valid path"),
            bytes::Bytes::from_static(b"private"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("seed private object");

    // The manifest an operator would write to publish part of a knowledge base.
    let mut manifest = KbManifest::new_v1(
        &TenantSlug::default(),
        &kb(PUBLIC_KB),
        PUBLIC_KB,
        1_700_000_000,
    );
    manifest.access = [
        AccessRule::new(Principal::SignedIn, Verb::ALL),
        AccessRule::new(Principal::Anyone, [Verb::List, Verb::Read])
            .under([notedthat_core::KeyPattern::parse("public/**").expect("valid pattern")]),
    ]
    .into_iter()
    .collect();
    storage
        .write_manifest(&kb(PUBLIC_KB), &manifest)
        .await
        .expect("public manifest");

    // `vault` gets the default: credentialed access only.
    let private = KbManifest::new_v1(
        &TenantSlug::default(),
        &kb(PRIVATE_KB),
        PRIVATE_KB,
        1_700_000_000,
    );
    storage
        .write_manifest(&kb(PRIVATE_KB), &private)
        .await
        .expect("private manifest");
}

fn test_config(listen_addr: std::net::SocketAddr) -> Config {
    Config {
        api_token: TOKEN.to_string(),
        kbs: BTreeMap::from([
            (PUBLIC_KB.to_string(), kb(PUBLIC_KB)),
            (PRIVATE_KB.to_string(), kb(PRIVATE_KB)),
        ]),
        tenant_slug: TenantSlug::default(),
        listen_addr,
        storage: notedthat_server::config::unroutable_storage_placeholder(),
        log_format: LogFormat::Pretty,
        qdrant: ServerQdrantConfig {
            url: "http://127.0.0.1:6334".to_string(),
            api_key: None,
            timeout_ms: 30_000,
            connect_timeout_ms: 10_000,
        },
        embedder: EmbedderConfig {
            endpoint_url: "http://127.0.0.1:9999".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dimensions: EMBEDDING_DIM,
            batch_size: 32,
            timeout_ms: 30_000,
            max_retries: 3,
            max_input_tokens: 8192,
        },
        webdav_username: "browse-e2e-user".to_string(),
        webdav_password: "browse-e2e-pass".to_string(),
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        max_patchable_size: 10 * 1024 * 1024,
        staging: notedthat_core::StagingConfig::default(),
    }
}

async fn wait_for_health(addr: std::net::SocketAddr) {
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/healthz");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(response) = client.get(&url).send().await
            && response.status().is_success()
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server did not answer /healthz within 30s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A running server plus a client that does not follow redirects.
///
/// Redirects are part of the contract here — the canonical trailing slash, and
/// the hand-off to the object's API representation — so they have to be
/// observable rather than followed silently.
struct Server {
    base: String,
    client: reqwest::Client,
    handle: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start() -> Self {
        let storage = Arc::new(InMemoryStorage::default());
        seed(&storage).await;

        let bound_addr = notedthat_api_http::testing::reserve_addr();
        let config = test_config(bound_addr);
        let backends = Backends {
            storage,
            store: Arc::new(InMemoryVectorStore::new()),
            embedder: Arc::new(StubEmbedder::new(EMBEDDING_DIM as usize)),
        };
        let handle = tokio::spawn(async move {
            notedthat_server::run::run_with(config, backends)
                .await
                .expect("server run failed");
        });
        wait_for_health(bound_addr).await;

        Self {
            base: format!("http://{bound_addr}"),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client"),
            handle,
        }
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .expect("request")
    }

    async fn get_authenticated(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("authorization", format!("Bearer {TOKEN}"))
            .send()
            .await
            .expect("request")
    }

    async fn page(&self, path: &str) -> String {
        let response = self.get(path).await;
        assert_eq!(response.status().as_u16(), 200, "GET {path}");
        response.text().await.expect("body")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn hrefs(html: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("href=\"") {
        rest = &rest[start + 6..];
        let Some(end) = rest.find('"') else { break };
        found.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    found
}

/// The whole point of the feature, walked the way a person would.
#[tokio::test]
async fn a_visitor_walks_from_the_index_to_an_object_and_gets_its_bytes() {
    // Given
    let server = Server::start().await;

    // When — index, into the knowledge base, into the published folder, then
    // follow the object link and read what it serves.
    let index = server.page("/browse/").await;
    let kb_page = server.page("/browse/notes/").await;
    let public = server.page("/browse/notes/public/").await;
    let object_href = hrefs(&public)
        .into_iter()
        .find(|href| href.ends_with("index.md"))
        .expect("a link to the object");
    let redirect = server.get(&object_href).await;
    let bytes = server
        .get(&object_href)
        .await
        .text()
        .await
        .expect("object body");

    // Then
    assert!(index.contains("notes/"), "{index}");
    assert!(
        hrefs(&kb_page).contains(&"/browse/notes/public/".to_string()),
        "{kb_page}"
    );
    assert!(public.contains("index.md"), "{public}");
    assert_eq!(
        object_href, "/api/v1/knowledgebases/notes/public/index.md",
        "the link must be the existing representation, not a second pipeline"
    );
    assert_eq!(redirect.status().as_u16(), 200);
    assert_eq!(bytes, "body of public/index.md");
}

#[tokio::test]
async fn nested_paths_and_parent_links_survive_a_real_round_trip() {
    // Given
    let server = Server::start().await;

    // When
    let deep = server.page("/browse/notes/public/drafts/deep/").await;
    let parent_row = deep.find("<tr class=\"up\">").expect("a parent row");
    let parent = hrefs(&deep[parent_row..])
        .first()
        .cloned()
        .expect("a parent link");
    let up = server.page(&parent).await;

    // Then
    assert!(deep.contains("buried.md"), "{deep}");
    assert_eq!(parent, "/browse/notes/public/drafts/");
    assert!(
        hrefs(&up).contains(&"/browse/notes/public/drafts/deep/".to_string()),
        "the parent should link back down: {up}"
    );
}

#[tokio::test]
async fn a_percent_encoded_link_resolves_to_the_object_it_names() {
    // Given — a key with a space, which has to survive encoding both ways.
    let server = Server::start().await;

    // When
    let public = server.page("/browse/notes/public/").await;
    let href = hrefs(&public)
        .into_iter()
        .find(|href| href.contains("%20"))
        .expect("an encoded link");
    let body = server.get(&href).await.text().await.expect("body");

    // Then
    assert_eq!(href, "/api/v1/knowledgebases/notes/public/a%20note.md");
    assert_eq!(body, "body of public/a note.md");
}

#[tokio::test]
async fn a_directory_url_without_its_slash_redirects_rather_than_failing() {
    // Given
    let server = Server::start().await;

    // When
    let prefix = server.get("/browse").await;
    let folder = server.get("/browse/notes/public").await;

    // Then
    assert_eq!(prefix.status().as_u16(), 308);
    assert_eq!(
        prefix.headers().get("location").expect("location"),
        "/browse/"
    );
    assert_eq!(folder.status().as_u16(), 307);
    assert_eq!(
        folder.headers().get("location").expect("location"),
        "/browse/notes/public/"
    );
}

#[tokio::test]
async fn private_content_is_absent_anonymously_and_present_with_a_credential() {
    // Given
    let server = Server::start().await;

    // When
    let index = server.page("/browse/").await;
    let private_kb = server.get("/browse/vault/").await;
    let outside_the_grant = server.get("/browse/notes/internal/").await;
    let authenticated_index = server
        .get_authenticated("/browse/")
        .await
        .text()
        .await
        .expect("body");
    let authenticated_private = server.get_authenticated("/browse/vault/").await;

    // Then — the manifest published `public/**` and nothing else, and that is
    // exactly what an anonymous visitor can reach.
    assert!(!index.contains("vault"), "{index}");
    assert_eq!(private_kb.status().as_u16(), 404);
    assert_eq!(outside_the_grant.status().as_u16(), 404);
    assert!(
        authenticated_index.contains("vault"),
        "{authenticated_index}"
    );
    assert_eq!(authenticated_private.status().as_u16(), 200);
}

#[tokio::test]
async fn the_internal_namespace_appears_on_no_page_the_surface_serves() {
    // Given
    let server = Server::start().await;

    // When
    let pages = [
        server.page("/browse/").await,
        server.page("/browse/notes/").await,
        server.page("/browse/notes/public/").await,
        server
            .get_authenticated("/browse/notes/")
            .await
            .text()
            .await
            .expect("body"),
    ];
    let direct = server.get_authenticated("/browse/notes/.notedthat/").await;

    // Then
    for page in &pages {
        assert!(!page.contains(".notedthat"), "{page}");
        assert!(!page.contains("keep-out"), "{page}");
    }
    assert_eq!(direct.status().as_u16(), 404);
}

#[tokio::test]
async fn the_machine_surfaces_agree_with_what_the_page_showed() {
    // Given — one evaluator, three surfaces. If the browse page and the JSON
    // API disagreed about what is public, one of them would be a bug.
    let server = Server::start().await;

    // When
    let page = server.page("/browse/notes/public/").await;
    let api = server
        .get("/api/v1/knowledgebases/notes?prefix=public/")
        .await;
    let api_body = api.text().await.expect("body");

    // Then
    assert!(page.contains("index.md"));
    assert!(api_body.contains("public/index.md"), "{api_body}");
    assert!(
        !api_body.contains("internal/secret.md"),
        "the API must withhold what the page withheld: {api_body}"
    );
}

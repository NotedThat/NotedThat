//! End-to-end coverage of OIDC identity tokens against a real Authelia (D53).
//!
//! `oidc_e2e` proves the server against a wiremock issuer and tokens the test
//! mints itself. This suite proves the other half: that a token a real
//! provider mints — discovered over TLS through `NOTEDTHAT_OIDC_CA_CERT`,
//! with Authelia's claims policy and audience settings from
//! `docker/authelia/configuration.yml` — is the token the verifier expects.
//! The provider configuration under test is the one the Compose overlay and
//! the manual QA script use, so a regression there fails here.
//!
//! Authelia derives the issuer from the request host and matches it against a
//! configured cookie domain, so the container is bound to the fixed host port
//! `9091` its configuration names; the `127.0.0.1` cookie domain exists for
//! this suite. Stop the Compose overlay before running it.
//!
//! Run with: `cargo test -p notedthat-server --test oidc_authelia_e2e -- --ignored`
#![allow(missing_docs)]

use notedthat_api_http::testing::InMemoryStorage;
use notedthat_core::{
    AccessRule, ConditionalHeaders, KbManifest, KbSlug, KeyPattern, ObjectPath, Storage,
    TenantSlug, Verb, Who,
};
use notedthat_indexer::testing::{InMemoryVectorStore, StubEmbedder};
use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
use notedthat_server::oidc::OidcSettings;
use notedthat_server::run::Backends;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

const EMBEDDING_DIM: u32 = 3;
const TOKEN: &str = "authelia-e2e-service-token";
const KB: &str = "notes";
/// Fixed by `session.cookies` in the Authelia configuration.
const ISSUER: &str = "https://127.0.0.1:9091";

fn authelia_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker/authelia")
}

fn kb() -> KbSlug {
    KbSlug::try_new(KB).expect("valid slug")
}

async fn seed(storage: &InMemoryStorage) {
    storage.ensure_bucket(&kb()).await.expect("bucket");
    for key in ["handbook.md", "hr/salaries.md"] {
        storage
            .put_object(
                &kb(),
                &ObjectPath::try_from(key).expect("valid path"),
                bytes::Bytes::from(format!("body of {key}")),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .expect("seed object");
    }
    let mut manifest = KbManifest::new_v1(&TenantSlug::default(), &kb(), KB, 1_700_000_000);
    manifest.access = [
        AccessRule::new(Who::SignedIn, [Verb::List, Verb::Read]),
        AccessRule::new(Who::Group("editors".into()), [Verb::Write]),
        AccessRule::deny(Who::Group("interns".into()), [Verb::Read])
            .under([KeyPattern::parse("hr/**").expect("valid pattern")]),
    ]
    .into_iter()
    .collect();
    storage
        .write_manifest(&kb(), &manifest)
        .await
        .expect("manifest");
}

fn test_config(listen_addr: std::net::SocketAddr) -> Config {
    Config {
        api_token: TOKEN.to_string(),
        kbs: BTreeMap::from([(KB.to_string(), kb())]),
        tenant_slug: TenantSlug::default(),
        listen_addr,
        storage: notedthat_server::config::unroutable_storage_placeholder(),
        events: notedthat_server::config::EventsConfig::None,
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
        webdav_username: "authelia-e2e-user".to_string(),
        webdav_password: "authelia-e2e-pass".to_string(),
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        mcp_anonymous: notedthat_server::config::McpAnonymous::Auto,
        max_patchable_size: 10 * 1024 * 1024,
        mcp_max_read_bytes: 16 * 1024 * 1024,
        mcp_max_sessions: notedthat_mcp::DEFAULT_MAX_SESSIONS,
        ready_probe_interval_ms: 5_000,
        staging: notedthat_core::StagingConfig::default(),
        oidc: Some(OidcSettings {
            issuer: ISSUER.to_string(),
            audiences: vec!["notedthat".to_string()],
            username_claim: OidcSettings::DEFAULT_USERNAME_CLAIM.to_string(),
            groups_claim: OidcSettings::DEFAULT_GROUPS_CLAIM.to_string(),
            http_timeout: Duration::from_secs(10),
            resource: None,
            ca_cert: Some(authelia_dir().join("ca.crt")),
        }),
    }
}

/// Start Authelia from the checked-in development configuration.
async fn authelia() -> ContainerAsync<GenericImage> {
    let dir = authelia_dir();
    let file = |name: &str| std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    GenericImage::new("authelia/authelia", "4.39")
        .with_wait_for(WaitFor::message_on_stdout("Listening for TLS connections"))
        .with_mapped_port(9091, 9091_u16.tcp())
        .with_copy_to("/config/configuration.yml", file("configuration.yml"))
        .with_copy_to("/config/users_database.yml", file("users_database.yml"))
        .with_copy_to("/config/tls.crt", file("tls.crt"))
        .with_copy_to("/config/tls.key", file("tls.key"))
        .start()
        .await
        .expect("Authelia container starts")
}

/// A client that trusts Authelia's self-signed certificate.
fn authelia_client() -> reqwest::Client {
    let ca = std::fs::read(authelia_dir().join("ca.crt")).expect("ca.crt");
    reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&ca).expect("PEM"))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client")
}

/// Run the authorization-code flow through Authelia's API and return the
/// access token, exactly as `docs/manual-qa/oidc-mcp.sh` does.
async fn access_token(user: &str, password: &str) -> String {
    let client = authelia_client();
    let login = client
        .post(format!("{ISSUER}/api/firstfactor"))
        .json(&serde_json::json!({
            "username": user,
            "password": password,
            "keepMeLoggedIn": false,
        }))
        .send()
        .await
        .expect("first factor");
    assert_eq!(login.status(), 200, "first factor for {user}");
    // The session cookie, carried by hand: reqwest's cookie store is a feature
    // this crate does not enable, and one header is all the flow needs.
    let session = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .find(|pair| pair.starts_with("authelia_session="))
        .expect("session cookie")
        .to_string();

    let redirect_uri = "http://127.0.0.1:1/callback";
    let authorization = client
        .get(format!("{ISSUER}/api/oidc/authorization"))
        .header("cookie", &session)
        .query(&[
            ("client_id", "notedthat"),
            ("response_type", "code"),
            ("scope", "openid profile groups"),
            ("redirect_uri", redirect_uri),
            ("state", "authelia-e2e-state"),
        ])
        .send()
        .await
        .expect("authorization");
    let location = authorization
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| panic!("authorization answered {}", authorization.status()))
        .to_string();
    let code = url::Url::parse(&location)
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(key, _)| key == "code")
                .map(|(_, value)| value.to_string())
        })
        .unwrap_or_else(|| panic!("no code in {location}"));

    let token: serde_json::Value = client
        .post(format!("{ISSUER}/api/oidc/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri),
            ("client_id", "notedthat"),
            ("client_secret", "notedthat-client-secret"),
        ])
        .send()
        .await
        .expect("token")
        .json()
        .await
        .expect("token json");
    token["access_token"]
        .as_str()
        .unwrap_or_else(|| panic!("no access token: {token}"))
        .to_string()
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

#[tokio::test]
#[ignore = "requires Docker (Authelia testcontainer on host port 9091)"]
async fn a_token_authelia_mints_is_bound_by_the_manifest_on_every_surface() {
    // Given — Authelia up, then the server discovering it over TLS.
    let _authelia = authelia().await;
    let storage = Arc::new(InMemoryStorage::default());
    seed(&storage).await;
    let bound_addr = notedthat_api_http::testing::reserve_addr();
    let config = test_config(bound_addr);
    let backends = Backends {
        storage,
        store: Arc::new(InMemoryVectorStore::new()),
        embedder: Arc::new(StubEmbedder::new(EMBEDDING_DIM as usize)),
        events: None,
    };
    let server = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("server run failed");
    });
    wait_for_health(bound_addr).await;
    let base = format!("http://{bound_addr}");
    let http = reqwest::Client::new();

    // When — tokens for an editor and an intern, from Authelia itself.
    let alice = access_token("alice", "alice-password").await;
    let ivan = access_token("ivan", "ivan-password").await;

    // Then — the API.
    let get = |path: &str, token: &str| {
        http.get(format!("{base}/api/v1/knowledgebases/notes/{path}"))
            .bearer_auth(token)
            .send()
    };
    assert_eq!(get("handbook.md", &alice).await.expect("get").status(), 200);
    assert_eq!(get("handbook.md", &ivan).await.expect("get").status(), 200);
    assert_eq!(
        get("hr%2Fsalaries.md", &alice).await.expect("get").status(),
        200
    );
    assert_eq!(
        get("hr%2Fsalaries.md", &ivan).await.expect("get").status(),
        403,
        "the intern's group is barred from hr/"
    );
    let put = |token: &str| {
        http.put(format!("{base}/api/v1/knowledgebases/notes/handbook.md"))
            .bearer_auth(token)
            .header("content-type", "text/markdown")
            .body("edited")
            .send()
    };
    assert_eq!(
        put(&alice).await.expect("put").status(),
        201,
        "editors write"
    );
    assert_eq!(
        put(&ivan).await.expect("put").status(),
        403,
        "interns do not"
    );
    assert_eq!(
        get(".notedthat%2Fmanifest.json", &alice)
            .await
            .expect("get")
            .status(),
        403,
        "no identity reaches the manifest"
    );

    // WebDAV and browse, with the same bearer.
    assert_eq!(
        http.get(format!("{base}/webdav/notes/hr/salaries.md"))
            .bearer_auth(&ivan)
            .send()
            .await
            .expect("dav")
            .status(),
        403
    );
    assert_eq!(
        http.get(format!("{base}/browse/notes/"))
            .bearer_auth(&ivan)
            .send()
            .await
            .expect("browse")
            .status(),
        200
    );

    // MCP acts as the caller.
    let mcp = notedthat_mcp::testing::McpSession::connect(&base, &ivan)
        .call_tool(
            1,
            "read",
            &serde_json::json!({ "kb": "notes", "path": "hr/salaries.md" }),
        )
        .await;
    assert!(mcp.to_string().contains("forbidden"), "{mcp}");

    server.abort();
}

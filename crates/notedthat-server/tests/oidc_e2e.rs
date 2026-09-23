//! End-to-end coverage of OIDC bearer identities (D53).
//!
//! The subject is a real server booted against a wiremock issuer: discovery
//! at startup, a token minted with the checked-in test key, and the group and
//! user rules a manifest actually carries, enforced on the HTTP API, `WebDAV`,
//! the browse pages and MCP. Only storage, the vector
//! store and the embedder are substituted, so nothing here depends on Docker.
//!
//! Run with: `cargo test -p notedthat-server --test oidc_e2e`
#![allow(missing_docs)]

use notedthat_api_http::testing::InMemoryStorage;
use notedthat_core::{
    AccessRule, ConditionalHeaders, KbManifest, KbSlug, KeyPattern, ObjectPath, Storage,
    TenantSlug, Verb, Who,
};
use notedthat_indexer::testing::{InMemoryVectorStore, StubEmbedder};
use notedthat_server::config::{
    Config, EmbedderConfig, LogFormat, McpAnonymous, ServerQdrantConfig,
};
use notedthat_server::oidc::test_support::{JWKS_JSON, claims, mint, settings};
use notedthat_server::run::Backends;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const EMBEDDING_DIM: u32 = 3;
const TOKEN: &str = "oidc-e2e-service-token";
const KB: &str = "notes";

fn kb() -> KbSlug {
    KbSlug::try_new(KB).expect("valid slug")
}

fn pattern(source: &str) -> KeyPattern {
    KeyPattern::parse(source).expect("valid pattern")
}

/// Everyone signed in may list and read; editors may write; interns lose
/// `hr/`; alice alone may delete under `personal/alice/`.
async fn seed(storage: &InMemoryStorage) {
    storage.ensure_bucket(&kb()).await.expect("bucket");
    for key in [
        "handbook.md",
        "hr/salaries.md",
        "personal/alice/todo.md",
        ".notedthat/keep-out.md",
    ] {
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
        AccessRule::deny(Who::Group("interns".into()), [Verb::List, Verb::Read])
            .under([pattern("hr/**")]),
        AccessRule::new(Who::User("alice".into()), [Verb::Delete])
            .under([pattern("personal/alice/**")]),
    ]
    .into_iter()
    .collect();
    storage
        .write_manifest(&kb(), &manifest)
        .await
        .expect("manifest");
}

fn test_config(
    listen_addr: std::net::SocketAddr,
    issuer: &str,
    resource: Option<String>,
) -> Config {
    let mut oidc = settings(issuer);
    oidc.resource = resource;
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
        webdav_username: "oidc-e2e-user".to_string(),
        webdav_password: "oidc-e2e-pass".to_string(),
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        mcp_anonymous: McpAnonymous::Auto,
        max_patchable_size: 10 * 1024 * 1024,
        mcp_max_read_bytes: 16 * 1024 * 1024,
        mcp_max_sessions: notedthat_mcp::DEFAULT_MAX_SESSIONS,
        ready_probe_interval_ms: 5_000,
        staging: notedthat_core::StagingConfig::default(),
        oidc: Some(oidc),
    }
}

/// A wiremock issuer publishing the fixture key.
async fn issuer() -> MockServer {
    let server = MockServer::start().await;
    let issuer = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": issuer,
            "jwks_uri": format!("{issuer}/jwks"),
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_string(JWKS_JSON))
        .mount(&server)
        .await;
    server
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

struct Server {
    base: String,
    issuer: MockServer,
    client: reqwest::Client,
    handle: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start() -> Self {
        Self::start_with(None).await
    }

    async fn start_with(resource: Option<String>) -> Self {
        Self::start_as(resource, false, McpAnonymous::Auto).await
    }

    /// A server whose writes are announced, so a session can subscribe (D66).
    async fn start_with_events() -> Self {
        Self::start_inner(None, false, McpAnonymous::Auto, true).await
    }

    /// A server whose one knowledge base additionally grants `anyone`
    /// `list` and `read` when `public`, under the given MCP anonymous mode.
    async fn start_as(resource: Option<String>, public: bool, mcp_anonymous: McpAnonymous) -> Self {
        Self::start_inner(resource, public, mcp_anonymous, false).await
    }

    async fn start_inner(
        resource: Option<String>,
        public: bool,
        mcp_anonymous: McpAnonymous,
        events: bool,
    ) -> Self {
        let issuer = issuer().await;
        let storage = Arc::new(InMemoryStorage::default());
        seed(&storage).await;
        if public {
            let mut manifest = storage.read_manifest(&kb()).await.expect("manifest");
            let mut rules: Vec<AccessRule> = manifest.access.rules().to_vec();
            rules.push(AccessRule::new(Who::Anyone, [Verb::List, Verb::Read]));
            manifest.access = rules.into_iter().collect();
            storage
                .write_manifest(&kb(), &manifest)
                .await
                .expect("public manifest");
        }

        let bound_addr = notedthat_api_http::testing::reserve_addr();
        let mut config = test_config(bound_addr, &issuer.uri(), resource);
        config.mcp_anonymous = mcp_anonymous;
        let backends = Backends {
            storage,
            store: Arc::new(InMemoryVectorStore::new()),
            embedder: Arc::new(StubEmbedder::new(EMBEDDING_DIM as usize)),
            events: events
                .then(|| Arc::new(notedthat_events::MemoryPublisher::new(1024)) as Arc<_>),
        };
        let handle = tokio::spawn(async move {
            notedthat_server::run::run_with(config, backends)
                .await
                .expect("server run failed");
        });
        wait_for_health(bound_addr).await;

        Self {
            base: format!("http://{bound_addr}"),
            issuer,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client"),
            handle,
        }
    }

    /// A token for `subject` in `groups`, signed by the fixture key.
    fn token(&self, subject: &str, groups: &[&str]) -> String {
        mint(&claims(&self.issuer.uri(), subject, groups))
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
    ) -> reqwest::Response {
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send().await.expect("request")
    }

    async fn get(&self, path: &str, token: Option<&str>) -> reqwest::Response {
        self.send(reqwest::Method::GET, path, token).await
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[tokio::test]
async fn a_group_scoped_manifest_binds_an_oidc_caller_on_the_api() {
    // Given
    let server = Server::start().await;
    let alice = server.token("alice", &["editors"]);
    let intern = server.token("ivan", &["interns"]);
    let bob = server.token("bob", &[]);
    let handbook = "/api/v1/knowledgebases/notes/handbook.md";
    let salaries = "/api/v1/knowledgebases/notes/hr%2Fsalaries.md";

    // When / Then — signed-in grants apply to every verified identity.
    assert_eq!(server.get(handbook, Some(&bob)).await.status(), 200);
    assert_eq!(server.get(salaries, Some(&bob)).await.status(), 200);

    // A prefix-scoped denial removes what the broad grant gave.
    assert_eq!(server.get(handbook, Some(&intern)).await.status(), 200);
    assert_eq!(server.get(salaries, Some(&intern)).await.status(), 403);

    // A group grant reaches only its members; a user grant only its subject.
    let put = |token: &str| {
        server
            .client
            .put(format!("{}{handbook}", server.base))
            .bearer_auth(token)
            .header("content-type", "text/markdown")
            .body("edited")
            .send()
    };
    assert_eq!(put(&alice).await.expect("put").status(), 201);
    assert_eq!(put(&bob).await.expect("put").status(), 403);
    let todo = "/api/v1/knowledgebases/notes/personal%2Falice%2Ftodo.md";
    assert_eq!(
        server
            .send(reqwest::Method::DELETE, todo, Some(&alice))
            .await
            .status(),
        204
    );
    assert_eq!(
        server
            .send(reqwest::Method::DELETE, handbook, Some(&alice))
            .await
            .status(),
        403,
        "alice's delete grant is scoped to her own prefix"
    );
}

#[tokio::test]
async fn a_token_the_issuer_did_not_sign_is_refused_not_downgraded() {
    // Given — well-formed claims, wrong issuer, so the signature and `iss`
    // both fail; and a plain string that is not a token at all.
    let server = Server::start().await;
    let foreign = mint(&claims(
        "https://elsewhere.example.com",
        "alice",
        &["editors"],
    ));

    // When / Then — the base publishes nothing anonymously, so a downgrade
    // would show as a concealed 404 rather than the 401 a refusal earns.
    for token in [foreign.as_str(), "sk-not-a-jwt"] {
        let response = server
            .get("/api/v1/knowledgebases/notes/handbook.md", Some(token))
            .await;
        assert_eq!(response.status(), 401, "{token}");
    }
    assert_eq!(
        server
            .get("/api/v1/knowledgebases/notes/handbook.md", None)
            .await
            .status(),
        404,
        "and anonymous callers still get the concealed answer"
    );
}

#[tokio::test]
async fn an_oidc_caller_cannot_reach_the_internal_namespace_but_the_service_token_can() {
    // Given
    let server = Server::start().await;
    let alice = server.token("alice", &["editors"]);
    let manifest = "/api/v1/knowledgebases/notes/.notedthat%2Fmanifest.json";

    // When / Then
    assert_eq!(server.get(manifest, Some(&alice)).await.status(), 403);
    assert_eq!(server.get(manifest, Some(TOKEN)).await.status(), 200);

    let listing = server
        .get("/api/v1/knowledgebases/notes", Some(&alice))
        .await;
    assert_eq!(listing.status(), 200);
    let body: serde_json::Value = listing.json().await.expect("json");
    let keys: Vec<&str> = body["objects"]
        .as_array()
        .expect("objects")
        .iter()
        .map(|object| object["key"].as_str().expect("key"))
        .collect();
    assert!(keys.contains(&"handbook.md"), "{keys:?}");
    assert!(
        !keys.iter().any(|key| key.starts_with(".notedthat")),
        "{keys:?}"
    );
}

#[tokio::test]
async fn the_same_token_is_bound_by_the_same_rules_on_webdav_and_browse() {
    // Given
    let server = Server::start().await;
    let intern = server.token("ivan", &["interns"]);

    // When / Then — WebDAV: the broad read grant, minus the denial.
    assert_eq!(
        server
            .get("/webdav/notes/handbook.md", Some(&intern))
            .await
            .status(),
        200
    );
    assert_eq!(
        server
            .get("/webdav/notes/hr/salaries.md", Some(&intern))
            .await
            .status(),
        403
    );

    // Browse: a directory synthesised only from denied keys does not exist for
    // the intern, but does for a user the denial does not name.
    assert_eq!(
        server
            .get("/browse/notes/hr/", Some(&intern))
            .await
            .status(),
        403
    );
    let bob = server.token("bob", &[]);
    assert_eq!(
        server.get("/browse/notes/hr/", Some(&bob)).await.status(),
        200
    );
}

#[tokio::test]
async fn the_protected_resource_document_and_challenge_are_served_when_configured() {
    // Given — a deployment that names its public URL.
    let server = Server::start_with(Some("https://notes.example.com".to_string())).await;

    // When
    let document = server
        .get("/.well-known/oauth-protected-resource", None)
        .await;
    let refused = server
        .get("/api/v1/knowledgebases/notes/handbook.md", Some("nope"))
        .await;
    let mcp = server
        .client
        .post(format!("{}/mcp", server.base))
        .header("content-type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .send()
        .await
        .expect("mcp");

    // Then
    assert_eq!(document.status(), 200);
    let json: serde_json::Value = document.json().await.expect("json");
    assert_eq!(json["resource"], "https://notes.example.com");
    assert_eq!(json["authorization_servers"][0], server.issuer.uri());

    let expected = "Bearer resource_metadata=\"https://notes.example.com/.well-known/oauth-protected-resource\"";
    for response in [&refused, &mcp] {
        assert_eq!(response.status(), 401);
        assert_eq!(
            response
                .headers()
                .get("www-authenticate")
                .expect("challenge")
                .to_str()
                .expect("ascii"),
            expected
        );
    }
}

#[tokio::test]
async fn never_keeps_the_challenge_on_a_public_deployment() {
    // Given — a deployment with a public knowledge base and an identity
    // provider, whose operator wants OAuth clients challenged on connect.
    let server = Server::start_as(
        Some("https://notes.example.com".to_string()),
        true,
        McpAnonymous::Never,
    )
    .await;

    // When — an MCP client connects with nothing.
    let mcp = server
        .client
        .post(format!("{}/mcp", server.base))
        .header("content-type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .send()
        .await
        .expect("mcp");

    // Then — 401 with the challenge that names the authorization server,
    // although the HTTP API serves the same caller anonymously.
    assert_eq!(mcp.status(), 401);
    assert_eq!(
        mcp.headers()
            .get("www-authenticate")
            .expect("challenge")
            .to_str()
            .expect("ascii"),
        "Bearer resource_metadata=\"https://notes.example.com/.well-known/oauth-protected-resource\""
    );
    assert_eq!(
        server
            .get("/api/v1/knowledgebases/notes/handbook.md", None)
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn auto_admits_anonymous_mcp_on_a_public_deployment_but_never_a_bad_token() {
    // Given — the same public deployment under the default mode.
    let server = Server::start_as(
        Some("https://notes.example.com".to_string()),
        true,
        McpAnonymous::Auto,
    )
    .await;
    // When / Then — no credential is the anonymous caller, and gets the tools …
    let anonymous = notedthat_mcp::testing::McpSession::anonymous(&server.base)
        .request(1, "tools/list", &serde_json::json!({}))
        .await;
    assert!(anonymous["result"]["tools"].is_array(), "{anonymous}");

    // … while a token the issuer did not sign is still refused with the
    // challenge, never downgraded to that same anonymous caller.
    let refused = notedthat_mcp::testing::McpSession::connect(&server.base, "not.a.jwt")
        .send(1, "tools/list", &serde_json::json!({}))
        .await;
    assert_eq!(refused.status(), 401);
    assert!(refused.headers().get("www-authenticate").is_some());
}

#[tokio::test]
async fn without_a_public_url_nothing_is_published() {
    let server = Server::start().await;
    assert_eq!(
        server
            .get("/.well-known/oauth-protected-resource", None)
            .await
            .status(),
        404
    );
    let refused = server
        .get("/api/v1/knowledgebases/notes/handbook.md", Some("nope"))
        .await;
    assert_eq!(refused.status(), 401);
    assert!(refused.headers().get("www-authenticate").is_none());
}

#[tokio::test]
async fn an_unreachable_issuer_refuses_startup() {
    // Given — an issuer that answers nothing useful.
    let dead = MockServer::start().await;
    let storage = Arc::new(InMemoryStorage::default());
    seed(&storage).await;
    let config = test_config(
        notedthat_api_http::testing::reserve_addr(),
        &dead.uri(),
        None,
    );
    let backends = Backends {
        storage,
        store: Arc::new(InMemoryVectorStore::new()),
        embedder: Arc::new(StubEmbedder::new(EMBEDDING_DIM as usize)),
        events: None,
    };

    // When
    let error = notedthat_server::run::run_with(config, backends)
        .await
        .expect_err("refused");

    // Then — the setting is named, D39-style.
    let message = format!("{error:#}");
    assert!(message.contains("NOTEDTHAT_OIDC_ISSUER"), "{message}");
    assert!(message.contains("openid-configuration"), "{message}");
}

/// One JSON-RPC call over the streamable HTTP transport, as `token`.
async fn mcp_call(
    server: &Server,
    token: &str,
    tool: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    notedthat_mcp::testing::McpSession::connect(&server.base, token)
        .call_tool(1, tool, &arguments)
        .await
}

#[tokio::test]
async fn an_mcp_tool_call_with_an_oidc_token_sees_only_that_identitys_grants() {
    // Given
    let server = Server::start().await;
    let intern = server.token("ivan", &["interns"]);
    let alice = server.token("alice", &["editors"]);

    // When — the intern reads what the denial spares, then what it covers.
    let handbook = mcp_call(
        &server,
        &intern,
        "read",
        serde_json::json!({ "kb": "notes", "path": "handbook.md" }),
    )
    .await;
    let salaries = mcp_call(
        &server,
        &intern,
        "read",
        serde_json::json!({ "kb": "notes", "path": "hr/salaries.md" }),
    )
    .await;
    // And alice writes, which the intern may not.
    let alice_writes = mcp_call(
        &server,
        &alice,
        "write",
        serde_json::json!({ "kb": "notes", "path": "handbook.md", "content": "via mcp" }),
    )
    .await;
    let intern_writes = mcp_call(
        &server,
        &intern,
        "write",
        serde_json::json!({ "kb": "notes", "path": "handbook.md", "content": "via mcp" }),
    )
    .await;

    // Then — MCP acts as the caller, so the manifest binds the tool call
    // exactly as it binds a direct request. A refusal is a tool error, not a
    // transport error.
    assert_eq!(handbook["result"]["isError"], false, "{handbook}");
    assert!(
        salaries["result"]["isError"] == true || salaries.get("error").is_some(),
        "the intern's read of hr/ must be refused: {salaries}"
    );
    assert!(
        salaries.to_string().contains("forbidden"),
        "the refusal is the API's 403, mapped: {salaries}"
    );
    assert_eq!(alice_writes["result"]["isError"], false, "{alice_writes}");
    assert!(
        intern_writes.to_string().contains("forbidden"),
        "{intern_writes}"
    );
}

/// A second bearer for the same identity — what an OIDC client holds after a
/// refresh. `claims` stamps `iat`/`exp` in whole seconds, so minting twice for
/// the same subject and groups inside one second returns the *same* string and
/// would test nothing; different groups make the bearer genuinely different and
/// prove the binding keys on the subject alone.
fn refreshed(server: &Server, subject: &str) -> String {
    server.token(subject, &["editors", "on-call"])
}

#[tokio::test]
async fn an_mcp_session_answers_only_to_the_principal_that_opened_it() {
    // Given — alice's session, and ivan, who holds a valid credential of his
    // own on the same deployment.
    let server = Server::start().await;
    let alice = server.token("alice", &["editors"]);
    let intern = server.token("ivan", &["interns"]);
    let hers = notedthat_mcp::testing::McpSession::connect(&server.base, &alice);
    hers.session_init().await;
    let id = hers
        .session_id()
        .expect("a stateful transport issues a session id");

    // When — ivan presents her session id on each of the three methods.
    let his = notedthat_mcp::testing::McpSession::connect(&server.base, &intern);
    his.adopt_session(&id);
    let on_a_call = his
        .send(
            1,
            "tools/call",
            &serde_json::json!({ "name": "list_knowledgebases" }),
        )
        .await;
    let on_the_leg = his.open_notifications().await;
    let on_a_delete = his.close().await;

    // Then — the two that rmcp refuses for an unknown id are refused, and the
    // one it accepts for any id is accepted while doing nothing. Neither tells
    // him whether the session exists.
    assert_eq!(
        on_a_call.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a tool call on another principal's session"
    );
    assert_eq!(
        on_the_leg.status(),
        reqwest::StatusCode::NOT_FOUND,
        "the notification leg is the one that leaks, so it is the one that matters"
    );
    assert_eq!(
        on_a_delete.status(),
        reqwest::StatusCode::ACCEPTED,
        "a DELETE answers as it does for an id nobody holds"
    );

    // And — alice still has her session and her leg. Before the binding, his
    // DELETE would have ended it.
    let still_hers = hers
        .send(
            2,
            "tools/call",
            &serde_json::json!({ "name": "list_knowledgebases" }),
        )
        .await;
    assert_eq!(still_hers.status(), reqwest::StatusCode::OK);
    assert_eq!(
        hers.open_notifications().await.status(),
        reqwest::StatusCode::OK
    );
}

#[tokio::test]
async fn a_refreshed_token_keeps_its_mcp_session_and_its_notification_leg() {
    // Given — alice's session, opened with the bearer she held then.
    let server = Server::start().await;
    let alice = server.token("alice", &["editors"]);
    let session = notedthat_mcp::testing::McpSession::connect(&server.base, &alice);
    session.session_init().await;
    let id = session.session_id().expect("session id");

    // When — she refreshes: a different bearer for the same subject, carrying
    // different groups. A client keeps one MCP session for the life of its
    // connection and rotates its token underneath it, which is why the binding
    // cannot be on the bearer.
    let after =
        notedthat_mcp::testing::McpSession::connect(&server.base, &refreshed(&server, "alice"));
    after.adopt_session(&id);

    // Then — it is still her session, on every method.
    let call = after
        .send(
            1,
            "tools/call",
            &serde_json::json!({ "name": "list_knowledgebases" }),
        )
        .await;
    assert_eq!(
        call.status(),
        reqwest::StatusCode::OK,
        "the refreshed token kept the session"
    );
    assert_eq!(
        after.open_notifications().await.status(),
        reqwest::StatusCode::OK,
        "and kept its notification leg"
    );
    assert_eq!(after.close().await.status(), reqwest::StatusCode::ACCEPTED);
}

#[tokio::test]
async fn another_principal_cannot_read_a_sessions_resource_notifications() {
    // Given — alice subscribed to an object, with her notification leg open.
    // This is the leak the binding closes, in the shape it actually takes: not
    // an object's bytes, but which URIs she watches and when they change.
    let server = Server::start_with_events().await;
    let alice = server.token("alice", &["editors"]);
    let intern = server.token("ivan", &["interns"]);
    let hers = notedthat_mcp::testing::McpSession::connect(&server.base, &alice);
    let mut her_stream = hers.notifications().await;
    let id = hers.session_id().expect("session id");
    let subscribed = hers
        .request(
            1,
            "resources/subscribe",
            &serde_json::json!({ "uri": format!("notedthat://{}/handbook.md", kb()) }),
        )
        .await;
    assert!(subscribed.get("error").is_none(), "{subscribed}");

    // When — ivan holds her session id and reaches for the leg it feeds.
    let his = notedthat_mcp::testing::McpSession::connect(&server.base, &intern);
    his.adopt_session(&id);
    let refused = his.open_notifications().await;

    // Then — he never gets a stream to read, so there is nothing to leak.
    assert_eq!(refused.status(), reqwest::StatusCode::NOT_FOUND);

    // And — the object changes, and the notification goes to its subscriber,
    // which is the half a refusal must not have broken.
    let written = server
        .client
        .put(format!(
            "{}/api/v1/knowledgebases/{}/handbook.md",
            server.base,
            kb()
        ))
        .header("Authorization", format!("Bearer {alice}"))
        .header("Content-Type", "text/markdown")
        .body("updated under a bound session")
        .send()
        .await
        .expect("PUT");
    assert!(written.status().is_success(), "{}", written.status());

    let notified = her_stream.next(Duration::from_secs(5)).await;
    assert_eq!(notified["method"], "notifications/resources/updated");
    assert_eq!(
        notified["params"]["uri"],
        format!("notedthat://{}/handbook.md", kb())
    );
}

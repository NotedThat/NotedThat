//! Two server replicas sharing one NATS `JetStream` log: ids are strictly
//! increasing across both, a client reconnecting to the other replica with
//! `Last-Event-ID` receives exactly what it missed, once, in order, and a
//! position the stream has retained out is `410 gone` (D55).
//!
//! `#[ignore]`d: needs Docker for the broker. Run with
//! `cargo test -p notedthat-server --test events_nats_e2e -- --ignored`.
#![allow(missing_docs)]

#[path = "support/patch_env.rs"]
mod patch_env;
#[path = "support/sse.rs"]
mod sse;

use std::sync::Arc;
use std::time::Duration;

use notedthat_api_http::testing::{InMemoryStorage, reserve_addr};
use notedthat_core::{EventPublisher, KbSlug, TenantSlug};
use notedthat_events::{NatsConfig, NatsPublisher};
use notedthat_indexer::testing::{InMemoryVectorStore, StubEmbedder};
use notedthat_server::config::{
    Config, EmbedderConfig, EventsConfig, LogFormat, ServerQdrantConfig,
};
use notedthat_server::run::Backends;
use patch_env::API_TOKEN;
use reqwest::StatusCode;
use sse::{Subscription, change_events};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

const EMBEDDING_DIM: u32 = 4;
const WAIT: Duration = Duration::from_secs(10);

struct Broker {
    _container: ContainerAsync<GenericImage>,
    url: String,
}

async fn start_broker() -> Broker {
    let container = GenericImage::new("nats", "2.12-alpine")
        .with_exposed_port(4222_u16.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Server is ready"))
        .with_cmd(["-js"])
        .start()
        .await
        .expect("start NATS container");
    let port = container
        .get_host_port_ipv4(4222_u16)
        .await
        .expect("NATS mapped port");
    Broker {
        _container: container,
        url: format!("nats://127.0.0.1:{port}"),
    }
}

struct Replica {
    base_url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Replica {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn config(kb: &str, addr: std::net::SocketAddr) -> Config {
    let mut kbs = std::collections::BTreeMap::new();
    kbs.insert(kb.to_string(), KbSlug::try_new(kb).expect("kb slug"));
    Config {
        api_token: API_TOKEN.to_string(),
        kbs,
        tenant_slug: TenantSlug::default(),
        listen_addr: addr,
        storage: notedthat_server::config::unroutable_storage_placeholder(),
        // `run_with` takes the log from `Backends`; this is never consulted.
        events: EventsConfig::None,
        log_format: LogFormat::Pretty,
        qdrant: ServerQdrantConfig {
            url: "http://127.0.0.1:1".to_string(),
            api_key: None,
            timeout_ms: 30_000,
            connect_timeout_ms: 10_000,
        },
        embedder: EmbedderConfig {
            endpoint_url: "http://127.0.0.1:1".to_string(),
            model: "test-model".to_string(),
            api_key: "test-key".to_string(),
            dimensions: EMBEDDING_DIM,
            batch_size: 1,
            timeout_ms: 30_000,
            max_retries: 3,
            max_input_tokens: 8192,
        },
        webdav_username: "e2e-webdav-user".to_string(),
        webdav_password: "e2e-webdav-pass".to_string(),
        mcp_http_allowed_origins: vec!["null".to_string()],
        mcp_http_allowed_hosts: vec!["127.0.0.1".to_string(), "localhost".to_string()],
        mcp_anonymous: notedthat_server::config::McpAnonymous::Auto,
        max_patchable_size: 10 * 1024 * 1024,
        mcp_max_read_bytes: 16 * 1024 * 1024,
        mcp_max_sessions: notedthat_mcp::DEFAULT_MAX_SESSIONS,
        ready_probe_interval_ms: 5_000,
        staging: notedthat_core::StagingConfig::default(),
        oidc: None,
    }
}

/// One replica over the shared storage and the shared broker.
async fn start_replica(kb: &str, storage: Arc<InMemoryStorage>, nats: &NatsConfig) -> Replica {
    let publisher = NatsPublisher::connect(nats)
        .await
        .expect("connect the replica to NATS");
    let events: Arc<dyn EventPublisher> = Arc::new(publisher);
    let addr = reserve_addr();
    let config = config(kb, addr);
    let backends = Backends {
        storage,
        store: Arc::new(InMemoryVectorStore::new()),
        embedder: Arc::new(StubEmbedder::new(EMBEDDING_DIM as usize)),
        events: Some(events),
    };
    let handle = tokio::spawn(async move {
        notedthat_server::run::run_with(config, backends)
            .await
            .expect("replica run failed");
    });
    let base_url = format!("http://{addr}");
    wait_for_ok(&format!("{base_url}/healthz")).await;
    Replica { base_url, handle }
}

async fn wait_for_ok(url: &str) {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(response) = client.get(url).send().await
            && response.status().is_success()
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{url} did not answer within 30s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn unique(prefix: &str) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_nanos();
    format!("{prefix}{nonce}")
}

async fn put(client: &reqwest::Client, replica: &Replica, kb: &str, key: &str) -> StatusCode {
    client
        .put(format!(
            "{}/api/v1/knowledgebases/{kb}/{key}",
            replica.base_url
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Content-Type", "text/markdown")
        .body(format!("# {key}"))
        .send()
        .await
        .expect("PUT")
        .status()
}

async fn delete(client: &reqwest::Client, replica: &Replica, kb: &str, key: &str) -> StatusCode {
    client
        .delete(format!(
            "{}/api/v1/knowledgebases/{kb}/{key}",
            replica.base_url
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .send()
        .await
        .expect("DELETE")
        .status()
}

async fn subscribe(
    client: &reqwest::Client,
    replica: &Replica,
    kb: &str,
    last_event_id: Option<&str>,
) -> reqwest::Response {
    let mut request = client
        .get(format!(
            "{}/api/v1/knowledgebases/{kb}/events",
            replica.base_url
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Accept", "text/event-stream");
    if let Some(id) = last_event_id {
        request = request.header("Last-Event-ID", id);
    }
    request.send().await.expect("subscribe")
}

#[tokio::test]
#[ignore = "requires a NATS JetStream testcontainer"]
async fn events_replay_across_replicas_and_a_retained_out_position_is_gone() {
    let broker = start_broker().await;
    let kb = unique("evt");
    let nats = NatsConfig {
        url: broker.url.clone(),
        stream: unique("nt-e2e-"),
        max_age: Duration::from_secs(3600),
    };
    // One store, two servers: what a multi-replica deployment on the s3
    // backend looks like (D49). Started one after the other so provisioning
    // does not race on the shared storage.
    let storage = Arc::new(InMemoryStorage::default());
    let a = start_replica(&kb, storage.clone(), &nats).await;
    let b = start_replica(&kb, storage, &nats).await;
    let client = reqwest::Client::new();

    for replica in [&a, &b] {
        let response = client
            .get(format!("{}/readyz", replica.base_url))
            .send()
            .await
            .expect("readyz");
        assert_eq!(response.status(), StatusCode::OK);
    }

    // Subscribe on B; write on A.
    let mut on_b = Subscription::open(subscribe(&client, &b, &kb, None).await);
    for key in ["a.md", "b.md", "c.md"] {
        assert_eq!(put(&client, &a, &kb, key).await, StatusCode::CREATED);
    }
    // The indexer on A reports each write it finishes on the same stream;
    // this test is about the change events, so it reads past those.
    let seen = on_b.events_where(3, WAIT, change_events).await;
    assert_eq!(
        seen.iter().map(sse::Frame::key).collect::<Vec<_>>(),
        vec!["a.md", "b.md", "c.md"]
    );
    assert!(
        seen.windows(2).all(|w| w[0].id_number() < w[1].id_number()),
        "ids strictly increase: {seen:?}"
    );
    assert!(seen.iter().all(|f| f.source() == "http"));
    let last_seen = seen[2].id.clone().unwrap();
    drop(on_b);

    // Miss two events, then come back — to A this time.
    assert_eq!(put(&client, &a, &kb, "d.md").await, StatusCode::CREATED);
    assert_eq!(
        delete(&client, &a, &kb, "a.md").await,
        StatusCode::NO_CONTENT
    );

    let mut on_a = Subscription::open(subscribe(&client, &a, &kb, Some(&last_seen)).await);
    let missed = on_a.events_where(2, WAIT, change_events).await;
    assert_eq!(
        missed
            .iter()
            .map(|f| (f.event.as_deref().unwrap(), f.key()))
            .collect::<Vec<_>>(),
        vec![("object.written", "d.md"), ("object.deleted", "a.md")]
    );
    assert!(missed[0].id_number() > last_seen.parse::<u64>().unwrap());
    on_a.expect_silence_where(Duration::from_millis(500), change_events)
        .await;

    // And B gives the identical answer for the same position.
    let mut on_b = Subscription::open(subscribe(&client, &b, &kb, Some(&last_seen)).await);
    let again = on_b.events_where(2, WAIT, change_events).await;
    assert_eq!(
        again.iter().map(sse::Frame::id_number).collect::<Vec<_>>(),
        missed.iter().map(sse::Frame::id_number).collect::<Vec<_>>()
    );

    // Retain the early events out from under a client and it is told so.
    let js = async_nats::jetstream::new(
        async_nats::connect(&broker.url)
            .await
            .expect("test client connects"),
    );
    let stream = js.get_stream(&nats.stream).await.expect("stream");
    let d_seq = missed[0].id_number();
    stream
        .purge()
        .sequence(d_seq)
        .await
        .expect("purge below d.md");
    let response = subscribe(&client, &a, &kb, Some(&seen[0].id.clone().unwrap())).await;
    assert_eq!(response.status(), StatusCode::GONE);
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "gone");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains(&d_seq.to_string()),
        "names the oldest retained id: {body}"
    );
    // A position at the new edge is fine, and the live path still works.
    let response = subscribe(&client, &b, &kb, Some(&(d_seq - 1).to_string())).await;
    let mut edge = Subscription::open(response);
    let replay = edge.events(2, WAIT).await;
    assert_eq!(replay[0].id_number(), d_seq);
}

#[tokio::test]
#[ignore = "requires a NATS JetStream testcontainer"]
async fn losing_the_broker_fails_readiness_and_writes_answer_503_with_retry_after() {
    let broker = start_broker().await;
    let kb = unique("evt");
    let nats = NatsConfig {
        url: broker.url.clone(),
        stream: unique("nt-e2e-"),
        max_age: Duration::from_secs(3600),
    };
    let replica = start_replica(&kb, Arc::new(InMemoryStorage::default()), &nats).await;
    let client = reqwest::Client::new();
    assert_eq!(
        put(&client, &replica, &kb, "before.md").await,
        StatusCode::CREATED
    );

    drop(broker);

    // The client notices the disconnect on its own; give it a moment.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let response = client
            .get(format!("{}/readyz", replica.base_url))
            .send()
            .await
            .expect("readyz");
        if response.status() == StatusCode::SERVICE_UNAVAILABLE {
            let body: serde_json::Value = response.json().await.expect("json");
            assert_eq!(body["status"], "unavailable");
            assert_eq!(body["checks"]["events"]["backend"], "nats");
            assert_eq!(body["checks"]["events"]["status"], "unavailable");
            assert_eq!(body["checks"]["events"]["reason"], "disconnected");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "readyz stayed 200 after the broker went away"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // The object is stored, the event is not, and the client is told to retry.
    let response = client
        .put(format!(
            "{}/api/v1/knowledgebases/{kb}/after.md",
            replica.base_url
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .header("Content-Type", "text/markdown")
        .body("# after")
        .send()
        .await
        .expect("PUT");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "5");
    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["error"], "backend_unavailable");
    assert!(
        body["message"].as_str().unwrap().contains("object stored"),
        "{body}"
    );
    let read = client
        .get(format!(
            "{}/api/v1/knowledgebases/{kb}/after.md",
            replica.base_url
        ))
        .header("Authorization", format!("Bearer {API_TOKEN}"))
        .send()
        .await
        .expect("GET");
    assert_eq!(read.status(), StatusCode::OK, "the bytes were stored");
}

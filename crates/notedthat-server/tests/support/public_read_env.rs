use notedthat_core::{KbSlug, Storage, TenantSlug};
use notedthat_server::config::{Config, EmbedderConfig, LogFormat, ServerQdrantConfig};
use notedthat_storage_s3::{S3Config, S3Storage};
use std::{collections::BTreeMap, net::SocketAddr, time::Duration};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

pub const API_TOKEN: &str = "phase3-api-token";
pub const DAV_USER: &str = "phase3-user";
pub const DAV_PASS: &str = "phase3-pass";
pub const PUBLIC_KB: &str = "public";
pub const PRIVATE_KB: &str = "private";
pub const PUBLIC_BODY: &str = "# Public\n\nphase-three-public-needle";
pub const PRIVATE_BODY: &str = "# Private\n\nphase-three-private-needle";
pub const INTERNAL_BODY: &str = "phase-three-internal-needle";
pub const PROPFIND: &str =
    r#"<?xml version="1.0"?><D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>"#;

const SEAWEEDFS_IAM: &[u8] = br#"{"identities":[{"name":"test","credentials":[{"accessKey":"any","secretKey":"any"}],"actions":["Admin","Read","Write","List","Tagging"]}]}"#;

pub struct Backends {
    seaweed: Option<ContainerAsync<GenericImage>>,
    qdrant: Option<ContainerAsync<GenericImage>>,
    pub storage: S3Storage,
    s3_config: S3Config,
    qdrant_url: String,
    embedder: MockServer,
}

impl Backends {
    pub async fn start() -> Self {
        let seaweed = GenericImage::new("chrislusf/seaweedfs", "4.18")
            .with_exposed_port(8333_u16.tcp())
            .with_wait_for(WaitFor::message_on_stderr("Start Seaweed S3 API Server"))
            .with_copy_to("/tmp/s3.json", SEAWEEDFS_IAM.to_vec())
            .with_cmd(["server", "-s3", "-filer", "-s3.config=/tmp/s3.json"])
            .start()
            .await
            .expect("start persistent SeaweedFS container");
        let s3_port = seaweed
            .get_host_port_ipv4(8333_u16)
            .await
            .expect("SeaweedFS mapped port");
        let s3_config = S3Config {
            endpoint_url: Some(format!("http://127.0.0.1:{s3_port}")),
            region: "us-east-1".to_string(),
            access_key_id: "any".to_string(),
            secret_access_key: "any".to_string(),
            force_path_style: true,
        };
        let storage = S3Storage::new(s3_config.build_client(), TenantSlug::default());

        let qdrant = GenericImage::new("qdrant/qdrant", "v1.15.4")
            .with_exposed_port(6334_u16.tcp())
            .with_wait_for(WaitFor::message_on_stdout("Qdrant gRPC listening on 6334"))
            .start()
            .await
            .expect("start persistent Qdrant container");
        let qdrant_port = qdrant
            .get_host_port_ipv4(6334_u16)
            .await
            .expect("Qdrant mapped port");
        let embedder = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"index": 0, "embedding": [1.0, 0.0, 0.0, 0.0], "object": "embedding"}]
            })))
            .mount(&embedder)
            .await;

        Self {
            seaweed: Some(seaweed),
            qdrant: Some(qdrant),
            storage,
            s3_config,
            qdrant_url: format!("http://127.0.0.1:{qdrant_port}"),
            embedder,
        }
    }

    pub fn config(&self) -> Config {
        let kbs = [PUBLIC_KB, PRIVATE_KB]
            .into_iter()
            .map(|slug| {
                (
                    slug.to_string(),
                    KbSlug::try_new(slug).expect("fixture KB slug"),
                )
            })
            .collect::<BTreeMap<_, _>>();
        Config {
            api_token: API_TOKEN.to_string(),
            kbs,
            tenant_slug: TenantSlug::default(),
            listen_addr: free_addr(),
            storage: notedthat_server::config::StorageConfig::S3(self.s3_config.clone()),
            log_format: LogFormat::Pretty,
            qdrant: ServerQdrantConfig {
                url: self.qdrant_url.clone(),
                api_key: None,
                timeout_ms: 30_000,
                connect_timeout_ms: 10_000,
            },
            embedder: EmbedderConfig {
                endpoint_url: self.embedder.uri(),
                model: "phase3-model".to_string(),
                api_key: "phase3-embedder-key".to_string(),
                dimensions: 4,
                batch_size: 32,
                timeout_ms: 30_000,
                max_retries: 1,
                max_input_tokens: 8192,
            },
            webdav_username: DAV_USER.to_string(),
            webdav_password: DAV_PASS.to_string(),
            mcp_http_allowed_origins: vec!["null".to_string()],
            mcp_http_allowed_hosts: vec!["127.0.0.1".to_string()],
            max_patchable_size: 10 * 1024 * 1024,
            staging: notedthat_core::StagingConfig::default(),
        }
    }

    pub async fn remove(mut self) {
        let seaweed_id = self.seaweed.as_ref().expect("SeaweedFS present").id();
        let qdrant_id = self.qdrant.as_ref().expect("Qdrant present").id();
        println!("TEARDOWN removing containers seaweed={seaweed_id} qdrant={qdrant_id}");
        tokio::time::timeout(
            Duration::from_secs(20),
            self.qdrant.take().expect("Qdrant present").rm(),
        )
        .await
        .expect("Qdrant removal timeout")
        .expect("remove Qdrant");
        tokio::time::timeout(
            Duration::from_secs(20),
            self.seaweed.take().expect("SeaweedFS present").rm(),
        )
        .await
        .expect("SeaweedFS removal timeout")
        .expect("remove SeaweedFS");
        println!("TEARDOWN containers removed");
    }
}

pub fn kb(slug: &str) -> KbSlug {
    KbSlug::try_new(slug).expect("fixture KB slug")
}

pub async fn stored_manifest(storage: &S3Storage, slug: &str) -> notedthat_core::KbManifest {
    storage
        .read_manifest(&kb(slug))
        .await
        .expect("stored manifest")
}

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("ephemeral port address")
}

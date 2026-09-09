#![allow(missing_docs)]

//! `S3Storage` and `FsStorage`, run through one scenario set and asserted to agree.
//!
//! This is the half that needs Docker, so it is `#[ignore]` and runs in CI's integration
//! job. Its containerless sibling, `storage_conformance_local.rs`, compares `FsStorage`
//! against `InMemoryStorage` on every push — together they make `FsStorage` a pivot, so
//! agreement here plus agreement there means the substitute every E2E suite runs on
//! matches the backend it stands in for.
//!
//! **One container for the whole suite.** The existing S3 integration suite starts a
//! fresh `SeaweedFS` per test function across seventeen tests, and CI's integration job
//! has a fifteen-minute budget whose breach *cancels* the job and silently skips
//! release-plz. One scenario set through one container adds roughly one container
//! lifetime rather than ten.
//!
//! Run with:
//! ``sh
//! cargo test -p notedthat-storage-fs --locked -- --include-ignored
//! ``

#[path = "support/conformance.rs"]
mod conformance;

use std::sync::Arc;

use conformance::{
    Backend, assert_agree, assert_pinned, assert_premises, observe_all, observe_pinned_divergences,
};
use notedthat_core::{KbSlug, Storage, TenantSlug};
use notedthat_storage_fs::{FsConfig, FsStorage};
use notedthat_storage_s3::{S3Config, S3Storage};
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

/// Start `SeaweedFS` and wait for the log line it prints once its listener is bound.
///
/// Never a fixed sleep: on a loaded machine the container is not ready when the sleep
/// expires and the first request fails against a client timeout, reported as something
/// else entirely.
async fn start_seaweedfs() -> (impl std::any::Any, String) {
    let s3_iam = serde_json::json!({
        "identities": [{
            "name": "test",
            "credentials": [{"accessKey": "any", "secretKey": "any"}],
            "actions": ["Admin", "Read", "Write", "List", "Tagging"]
        }]
    });
    let config_bytes = serde_json::to_vec(&s3_iam).expect("serialize IAM config");
    let container = GenericImage::new("chrislusf/seaweedfs", "4.18")
        .with_exposed_port(8333_u16.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Start Seaweed S3 API Server"))
        .with_cmd(["server", "-s3", "-filer", "-s3.config=/tmp/s3.json"])
        .with_copy_to("/tmp/s3.json", config_bytes)
        .start()
        .await
        .expect("failed to start SeaweedFS testcontainer");
    let port = container
        .get_host_port_ipv4(8333_u16)
        .await
        .expect("failed to get SeaweedFS port");
    (container, format!("http://127.0.0.1:{port}"))
}

#[tokio::test]
#[ignore = "requires a SeaweedFS testcontainer"]
async fn s3_storage_and_fs_storage_agree() {
    let (_container, endpoint) = start_seaweedfs().await;
    // `S3Storage` is not `Clone`, so share it the way the server does.
    let s3: Arc<dyn Storage> = Arc::new(S3Storage::new(
        S3Config {
            endpoint_url: Some(endpoint),
            region: "us-east-1".to_string(),
            access_key_id: "any".to_string(),
            secret_access_key: "any".to_string(),
            force_path_style: true,
        }
        .build_client(),
        TenantSlug::default(),
    ));

    let dir = tempfile::tempdir().expect("tempdir");
    let config = FsConfig::new(dir.path().to_path_buf());
    let root = notedthat_storage_fs::open_root(&config)
        .await
        .expect("storage root");
    let fs = FsStorage::new(&config, root.root().to_path_buf(), TenantSlug::default());

    // One suffix for both, so an observation echoing the slug still compares.
    let suffix = format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() % 1_000_000)
            .unwrap_or_default()
    );

    let from_s3 = observe_all(s3.as_ref(), &suffix, |kb| {
        let s3 = Arc::clone(&s3);
        async move {
            s3.ensure_bucket(&kb).await.expect("bucket");
        }
    })
    .await;
    let from_fs = observe_all(&fs, &suffix, |kb| {
        let fs = fs.clone();
        async move {
            fs.ensure_bucket(&kb).await.expect("bucket");
        }
    })
    .await;

    assert_premises(Backend::S3, &from_s3);
    assert_premises(Backend::Fs, &from_fs);
    assert_agree(Backend::S3, &from_s3, Backend::Fs, &from_fs);

    // Behaviours the backends are allowed to differ on are checked against what is
    // recorded, rather than against each other.
    let kb = KbSlug::try_new(format!("conf-div-{suffix}")).expect("slug");
    s3.ensure_bucket(&kb).await.expect("bucket");
    fs.ensure_bucket(&kb).await.expect("bucket");
    assert_pinned(
        Backend::S3,
        &observe_pinned_divergences(s3.as_ref(), &kb).await,
    );
    assert_pinned(Backend::Fs, &observe_pinned_divergences(&fs, &kb).await);
}

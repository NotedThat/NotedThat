#![allow(missing_docs)]

//! The storage integration suite, run against a real S3 server.
//!
//! Every test body lives in `support/integration_scenarios.rs` and is shared with
//! `storage_integration_local.rs` and `storage_integration_memory.rs`, which run the same
//! bodies against a real directory tree and against the in-memory substitute. This file
//! contributes only the fixture: a `SeaweedFS` container, an `S3Storage` pointed at it,
//! and one knowledge base — one bucket — per scenario.
//!
//! This is the half that needs Docker, so it is `#[ignore]` and runs in CI's integration
//! job. Run with:
//! ```sh
//! cargo test -p notedthat-storage-fs --locked --test storage_integration_s3 -- --include-ignored
//! ```
//!
//! **One container while scenarios overlap in time**, which under this suite's default
//! parallelism means one container for the run. This used to be one container per test
//! function, and CI's integration job has a fifteen-minute budget whose breach *cancels*
//! the job and silently skips release-plz. The container is shared through a `Weak`
//! rather than a `static` holding it: `testcontainers` removes a container on `Drop` and
//! has no reaper process, so a container parked in a `static` is never dropped and
//! outlives the test run. Held as a `Weak`, it is started by whichever test needs it
//! first, shared by every test overlapping that one, and removed when the last of them
//! finishes.
//!
//! That lifetime is the union of the tests holding an `Arc`, so it has gaps wherever the
//! running scenarios all finish before the next one asks for the container — and
//! `--test-threads=1` is nothing but gaps: every scenario starts and removes one of its
//! own, twenty boots plus twenty removals, which is worse than the arrangement this
//! replaced. **Do not run this suite serialized**, which is unfortunately the first flag
//! anyone reaches for when debugging a container test.
//!
//! A strong reference held for the binary's lifetime would close those gaps, but libtest
//! gives a test binary no teardown hook to drop it from, so it would reinstate exactly
//! the leak the `Weak` exists to avoid: a container left running after the run. The gaps
//! are the cheaper of the two, so they are written down here instead of removed.

#[path = "support/integration_scenarios.rs"]
mod scenarios;

use std::sync::{Arc, Weak};

use notedthat_core::{KbSlug, TenantSlug};
use notedthat_storage_s3::{S3Config, S3Storage};
use scenarios::{kb_for, storage_integration_scenarios};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

struct Seaweed {
    _container: ContainerAsync<GenericImage>,
    endpoint: String,
}

/// Start `SeaweedFS` and wait for the log line it prints once its listener is bound.
///
/// Never a fixed sleep: on a loaded machine the container is not ready when the sleep
/// expires and the first request fails against a client timeout, reported as something
/// else entirely.
async fn start_seaweedfs() -> Seaweed {
    // SeaweedFS 4.18 requires an IAM config file to accept signed S3 requests.
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
        // `-volume.max` is not tuning: SeaweedFS gives every bucket a volume
        // collection of its own, and one scenario per bucket exhausts the default
        // allowance long before the suite finishes — the master then logs "Not enough
        // data nodes found!" and PUTs fail with an opaque "service error".
        .with_cmd([
            "server",
            "-s3",
            "-filer",
            "-s3.config=/tmp/s3.json",
            "-volume.max=200",
        ])
        .with_copy_to("/tmp/s3.json", config_bytes)
        .start()
        .await
        .expect("failed to start SeaweedFS testcontainer");
    let port = container
        .get_host_port_ipv4(8333_u16)
        .await
        .expect("failed to get SeaweedFS port");
    Seaweed {
        _container: container,
        endpoint: format!("http://127.0.0.1:{port}"),
    }
}

/// The container every currently-running scenario shares.
///
/// The lock is held across the start so that a cold suite boots one container rather than
/// one per test that raced to find the slot empty.
///
/// `Weak`, so the container's lifetime is the union of the scenarios holding an `Arc` and
/// nothing lingers past the last one. See the module comment for what that costs when the
/// scenarios do not overlap.
static SHARED: tokio::sync::Mutex<Weak<Seaweed>> = tokio::sync::Mutex::const_new(Weak::new());

async fn shared_seaweed() -> Arc<Seaweed> {
    let mut slot = SHARED.lock().await;
    if let Some(running) = slot.upgrade() {
        return running;
    }
    let started = Arc::new(start_seaweedfs().await);
    *slot = Arc::downgrade(&started);
    started
}

struct Fixture {
    _seaweed: Arc<Seaweed>,
    storage: S3Storage,
    kb: KbSlug,
}

async fn fixture(scenario: &str) -> Fixture {
    let seaweed = shared_seaweed().await;
    let storage = S3Storage::new(
        S3Config {
            endpoint_url: Some(seaweed.endpoint.clone()),
            region: "us-east-1".to_string(),
            access_key_id: "any".to_string(),
            secret_access_key: "any".to_string(),
            force_path_style: true,
        }
        .build_client(),
        TenantSlug::default(),
    );
    Fixture {
        _seaweed: seaweed,
        storage,
        kb: kb_for(scenario),
    }
}

macro_rules! s3_scenario {
    ($name:ident) => {
        #[tokio::test]
        #[ignore = "requires a SeaweedFS testcontainer"]
        async fn $name() {
            let fixture = fixture(stringify!($name)).await;
            scenarios::$name(&fixture.storage, &fixture.kb).await;
        }
    };
}

storage_integration_scenarios!(s3_scenario);

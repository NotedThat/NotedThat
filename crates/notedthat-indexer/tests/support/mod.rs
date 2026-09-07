//! Shared harness for the Qdrant-backed integration tests.
//!
//! # Why this exists
//!
//! Every one of these tests used to start its container with
//! `WaitFor::seconds(5)` — a fixed sleep, not a readiness check. That is the
//! classic container-test flake: on an unloaded machine five seconds is plenty,
//! and on a loaded one it is not, so the first RPC lands before Qdrant is
//! listening and dies against the client's timeout. The symptom was misleading
//! too — collection calls succeeded while the first `upsert_points` returned
//! `Cancelled: Timeout expired`, which reads like a Qdrant bug rather than a
//! harness one.
//!
//! Two things fix it, and both are needed:
//!
//! 1. Wait for the line Qdrant prints when its gRPC listener is actually bound,
//!    instead of guessing a duration.
//! 2. Then poll `health_check` until it answers, so a bound-but-not-yet-serving
//!    port cannot slip through either.
//!
//! The client-side half of the same bug was a real defect and is fixed in
//! `notedthat-indexer`: `QdrantConfig` now sets an explicit timeout, because
//! `qdrant-client` defaults to 5 seconds for *every* RPC — including a full
//! embedding batch upserted with `wait(true)`.

#![allow(dead_code)]

use std::time::{Duration, Instant};
use testcontainers::{
    ContainerAsync, GenericImage,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

/// The Qdrant image these tests pin.
pub const QDRANT_IMAGE: &str = "qdrant/qdrant";

/// The Qdrant tag these tests pin.
pub const QDRANT_TAG: &str = "v1.15.4";

/// Qdrant's gRPC port inside the container.
const GRPC_PORT: u16 = 6334;

/// The log line Qdrant emits once its gRPC listener is bound.
const GRPC_READY_LOG: &str = "Qdrant gRPC listening on 6334";

/// How long to wait for gRPC to answer after the container reports ready.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Start a Qdrant container and return it with a URL that is ready to use.
///
/// The returned container must be kept alive for the duration of the test —
/// dropping it stops and removes the container.
///
/// # Panics
///
/// Panics if Docker is unavailable or Qdrant does not answer within
/// [`READY_TIMEOUT`].
pub async fn start_qdrant() -> (ContainerAsync<GenericImage>, String) {
    let container = GenericImage::new(QDRANT_IMAGE, QDRANT_TAG)
        .with_exposed_port(GRPC_PORT.tcp())
        .with_wait_for(WaitFor::message_on_stdout(GRPC_READY_LOG))
        .start()
        .await
        .expect("failed to start Qdrant testcontainer — is Docker running?");

    let port = container
        .get_host_port_ipv4(GRPC_PORT)
        .await
        .expect("failed to get Qdrant gRPC port");
    let url = format!("http://127.0.0.1:{port}");

    await_grpc_ready(&url).await;
    (container, url)
}

/// A raw `qdrant_client::Qdrant` with a timeout suited to test workloads.
///
/// `Qdrant::from_url(..).build()` inherits the crate's **5-second** default for
/// every RPC. That is the client-side half of the flake these tests kept hitting:
/// on a loaded machine a single `upsert_points(..).wait(true)` can exceed it, and
/// the error — `Cancelled: Timeout expired` — points at the server rather than at
/// the deadline. Production code goes through `QdrantConfig`, which now sets this
/// explicitly; a raw client has to opt in.
///
/// # Panics
///
/// Panics if the client cannot be constructed.
#[must_use]
pub fn raw_client(url: &str) -> qdrant_client::Qdrant {
    qdrant_client::Qdrant::from_url(url)
        .timeout(RPC_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .expect("raw qdrant client build failed")
}

/// Per-RPC timeout for test clients.
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Connection-establishment timeout for test clients.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll `health_check` until Qdrant answers, or panic at the deadline.
async fn await_grpc_ready(url: &str) {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut last_error;

    loop {
        match qdrant_client::Qdrant::from_url(url)
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(5))
            .build()
        {
            Ok(client) => match client.health_check().await {
                Ok(_) => return,
                Err(err) => last_error = err.to_string(),
            },
            Err(err) => last_error = err.to_string(),
        }

        assert!(
            Instant::now() < deadline,
            "Qdrant gRPC at {url} did not become ready within {READY_TIMEOUT:?}; \
             last error: {last_error}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

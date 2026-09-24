//! The Prometheus exporter and the listener that serves it (D69).
//!
//! # Why this is a second listener
//!
//! The exposition carries no credential. There is nothing for a scraper to
//! present and no principal the exporter could evaluate, so anything that can
//! reach the socket can read it. `NOTEDTHAT_LISTEN_ADDR` defaults to
//! `0.0.0.0:8080` and is meant to be publicly reachable — it serves `/browse`,
//! `/api/v1`, `/webdav` and `/mcp` — so `/metrics` there would be world-readable
//! the moment an operator set the flag, held off only by a proxy rule somebody
//! has to remember. A second socket bound to loopback makes the boundary a
//! property of the bind instead.
//!
//! This is deliberately not a reversal of D39. That decision moved `WebDAV` and
//! MCP onto the one listener because they are authenticated *product* surfaces
//! every client has to reach, and three ports meant three exposures for no
//! gain. `/metrics` is the opposite on every axis: no client calls it, it
//! authenticates nobody, and it must be unreachable from wherever the product
//! listener is reachable. `REMOVED_LISTENER_ENV_VARS` keeps its teeth.
//!
//! # Why the recorder is installed once per process
//!
//! `metrics` takes a global recorder and `install_recorder` fails if one is
//! already installed — but a server is not global: this crate's E2E suites
//! start several in one test binary. Failing the second start for a reason that
//! has nothing to do with either server would be a test-only bug in production
//! code, so the handle is installed at most once and shared. In production
//! there is one server per process and the distinction never arises.
//!
//! # Nothing accumulates while metrics are off
//!
//! No recorder is installed unless the feature is on, and the facade's macros
//! are a no-op without one. There is no history to collect after the fact: the
//! counters begin at the restart that enabled them. This is stated in
//! `docs/CONFIGURATION.md` because it is the first thing an operator asks after
//! an incident.

use anyhow::{Context, anyhow};
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use notedthat_core::KbSlug;
use notedthat_core::metrics::{HISTOGRAM_BUCKETS, label, name};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::config::{Config, EventsConfig, StorageConfig};

/// How often the exporter's own bookkeeping is swept.
const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

/// How often the indexing queue's depth is sampled.
///
/// Sampled rather than written on every send: the queue has three producers and
/// one consumer on different tasks, so an on-change gauge would mean touching
/// every `try_send`, `send` and `recv` site in two crates and would still drift
/// — a refused write is not a depth change, but it is exactly when a reading is
/// wanted. `Sender::capacity` is exact and free, and a fixed interval bounds the
/// cost whatever the throughput. What sampling misses, a sub-second spike to
/// full, is counted exactly by `notedthat_index_events_refused_total`.
const QUEUE_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// The exposition's content type, spelled as Prometheus spells it.
const EXPOSITION_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The process's recorder handle, installed at most once.
static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
/// Serialises the install, so two servers starting at once cannot race into
/// `install_recorder` and have one of them fail.
static INSTALLING: Mutex<()> = Mutex::new(());

/// The process's Prometheus handle, installing the recorder on first use.
fn shared_handle() -> anyhow::Result<PrometheusHandle> {
    if let Some(handle) = HANDLE.get() {
        return Ok(handle.clone());
    }
    let _guard = INSTALLING
        .lock()
        .map_err(|_| anyhow!("the metrics recorder install lock was poisoned"))?;
    if let Some(handle) = HANDLE.get() {
        return Ok(handle.clone());
    }

    let mut builder = PrometheusBuilder::new();
    // Without an explicit bucket set a histogram is rendered as a summary with
    // per-process quantiles, which cannot be aggregated across replicas. The
    // catalogue's table is the source of truth and is asserted complete there.
    for (metric, buckets) in HISTOGRAM_BUCKETS {
        builder = builder
            .set_buckets_for_metric(Matcher::Full((*metric).to_string()), buckets)
            .with_context(|| format!("failed to set the buckets for {metric}"))?;
    }
    // No idle timeout: a series that expired between scrapes would read as a
    // counter reset, and `rate()` would invent a spike that never happened.
    let handle = builder
        .install_recorder()
        .context("failed to install the Prometheus recorder")?;
    let _ = HANDLE.set(handle.clone());
    describe_all();
    Ok(handle)
}

/// The `# HELP` text for the catalogue.
///
/// Registered once, after the recorder, because a description recorded before
/// there is a recorder to hold it is discarded and the first scrape ships
/// without its `HELP` lines.
fn describe_all() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!(name::HTTP_REQUESTS, "Requests answered, by matched route.");
    describe_histogram!(
        name::HTTP_REQUEST_DURATION,
        metrics::Unit::Seconds,
        "Time to produce a response head. Not time to last byte: a stream's \
         duration is its subscriber gauge, not this."
    );
    describe_gauge!(name::HTTP_IN_FLIGHT, "Requests in flight.");

    describe_counter!(name::SEARCH_REQUESTS, "Searches answered.");
    describe_histogram!(
        name::SEARCH_DURATION,
        metrics::Unit::Seconds,
        "End-to-end search latency, including the query embedding."
    );
    describe_histogram!(
        name::SEARCH_HITS,
        "Hits returned, after every access filter."
    );

    describe_counter!(name::EMBEDDING_REQUESTS, "Embedding endpoint calls.");
    describe_histogram!(
        name::EMBEDDING_DURATION,
        metrics::Unit::Seconds,
        "How long an embedding call took."
    );
    describe_counter!(name::EMBEDDING_ERRORS, "Embedding calls that failed.");
    describe_histogram!(name::EMBEDDING_TEXTS, "Texts sent in one embedding call.");

    describe_gauge!(name::INDEX_QUEUE_DEPTH, "Events waiting to be indexed.");
    describe_gauge!(name::INDEX_QUEUE_CAPACITY, "How many the queue holds.");
    describe_counter!(name::INDEX_EVENTS_ENQUEUED, "Indexing events queued.");
    describe_counter!(
        name::INDEX_EVENTS_REFUSED,
        "Writes refused because the queue was full."
    );
    describe_counter!(
        name::INDEX_EVENTS_COMPLETED,
        "Indexing events the worker finished with."
    );
    describe_counter!(
        name::INDEX_STALE_MARKS,
        "Knowledge bases marked as possibly behind their backend."
    );
    describe_gauge!(
        name::INDEX_WORKER_ALIVE,
        "1 while the indexer worker's loop runs, 0 once it has ended."
    );

    describe_counter!(name::VECTOR_STORE_OPERATIONS, "Vector store calls.");
    describe_histogram!(
        name::VECTOR_STORE_DURATION,
        metrics::Unit::Seconds,
        "How long a vector store call took."
    );
    describe_counter!(name::VECTOR_STORE_ERRORS, "Vector store calls that failed.");

    describe_counter!(name::EVENTS_PUBLISHED, "Object change events published.");
    describe_counter!(
        name::EVENTS_PUBLISH_FAILED,
        "Object change events the log refused."
    );
    describe_gauge!(name::EVENTS_SUBSCRIBERS, "Subscribers on the events route.");
    describe_counter!(
        name::EVENTS_REPLAY_GONE,
        "Subscribers turned away because their position is no longer retained."
    );

    describe_counter!(name::FS_WATCH_LOST, "Filesystem watches lost at runtime.");

    describe_counter!(name::RECONCILE_PASSES, "Reconciliation passes run.");
    describe_histogram!(
        name::RECONCILE_DURATION,
        metrics::Unit::Seconds,
        "How long a reconciliation pass took."
    );
    describe_counter!(
        name::RECONCILE_OBJECTS,
        "What reconciliation passes found, by class."
    );
    describe_gauge!(
        name::RECONCILE_OBJECTS_ON_DISK,
        "Objects the last whole-knowledge-base pass found in the backend."
    );

    describe_counter!(name::STORAGE_OPERATIONS, "Storage backend calls.");
    describe_histogram!(
        name::STORAGE_DURATION,
        metrics::Unit::Seconds,
        "How long a storage backend call took."
    );
    describe_counter!(name::STORAGE_ERRORS, "Storage calls that failed.");

    describe_gauge!(name::BUILD_INFO, "Always 1; the build is in the labels.");
}

/// The revision this binary was built from, when the build was told one.
///
/// Read from the environment at compile time rather than from a `build.rs` that
/// shells out to `git`: there is no build script in this workspace, one would
/// have to run for every crate `cargo package` builds, and a script that reads
/// `git` from a vendored tarball reports whatever happens to be checked out
/// around it. The container build passes its `IMAGE_REVISION` through; a
/// `cargo install` build honestly says `unknown`.
fn build_revision() -> &'static str {
    option_env!("NOTEDTHAT_BUILD_REVISION").unwrap_or("unknown")
}

/// Record `notedthat_build_info`: what this process is, as labels on a 1.
pub(crate) fn record_build_info(config: &Config) {
    let storage = match config.storage {
        StorageConfig::S3(_) => "s3",
        StorageConfig::Fs(_) => "fs",
    };
    let events = match config.events {
        EventsConfig::None => "none",
        EventsConfig::Memory { .. } => "memory",
        EventsConfig::Nats(_) => "nats",
    };
    metrics::gauge!(
        name::BUILD_INFO,
        label::VERSION => env!("CARGO_PKG_VERSION"),
        label::REVISION => build_revision(),
        label::BACKEND => storage,
        "events_backend" => events,
    )
    .set(1.0);
}

/// Sample the indexing queue's depth until `shutdown`.
pub(crate) async fn sample_queue_depth<T: Send + 'static>(
    sender: tokio::sync::mpsc::Sender<T>,
    shutdown: CancellationToken,
) {
    let capacity = sender.max_capacity();
    #[allow(clippy::cast_precision_loss)]
    metrics::gauge!(name::INDEX_QUEUE_CAPACITY).set(capacity as f64);
    let mut ticker = tokio::time::interval(QUEUE_SAMPLE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = ticker.tick() => {
                #[allow(clippy::cast_precision_loss)]
                metrics::gauge!(name::INDEX_QUEUE_DEPTH)
                    .set((capacity - sender.capacity()) as f64);
            }
        }
    }
}

/// One reconciliation pass's timing and outcome (D69).
///
/// A guard rather than a record at the end, because both reconcilers return
/// early from several places — a missing collection, an unreadable index or
/// bucket, a worker that has gone — and each of those is an outcome worth
/// counting. Recording only where a pass ran to completion would make a backend
/// that fails every pass indistinguishable from one that is not reconciling at
/// all, which is the case an operator most needs to see.
pub(crate) struct PassMetric {
    kb: String,
    cause: String,
    started: Instant,
    outcome: &'static str,
}

impl PassMetric {
    /// Begin timing a pass. Until [`Self::completed`] is called it will record
    /// as `incomplete`.
    pub(crate) fn started(kb: &KbSlug, cause: &str) -> Self {
        Self {
            kb: kb.as_str().to_string(),
            cause: cause.to_string(),
            started: Instant::now(),
            outcome: "incomplete",
        }
    }

    /// The pass ran to completion and its report was recorded.
    pub(crate) fn completed(&mut self) {
        self.outcome = "completed";
    }

    /// The consumer stopped listening part-way through, so most of the
    /// comparison never reached the queue.
    pub(crate) fn abandoned(&mut self) {
        self.outcome = "abandoned";
    }
}

impl Drop for PassMetric {
    fn drop(&mut self) {
        metrics::histogram!(name::RECONCILE_DURATION, label::KB => self.kb.clone())
            .record(self.started.elapsed().as_secs_f64());
        metrics::counter!(
            name::RECONCILE_PASSES,
            label::KB => self.kb.clone(),
            label::CAUSE => self.cause.clone(),
            label::OUTCOME => self.outcome,
        )
        .increment(1);
    }
}

/// A running metrics listener.
pub(crate) struct MetricsListener {
    serve: tokio::task::JoinHandle<()>,
    upkeep: tokio::task::JoinHandle<()>,
}

impl MetricsListener {
    /// Wait for the listener and its upkeep task to finish draining.
    pub(crate) async fn join(self) {
        if let Err(error) = self.serve.await {
            tracing::error!(%error, "the metrics listener panicked");
        }
        self.upkeep.abort();
    }
}

/// Install the recorder, before anything has a measurement to record.
///
/// Separate from [`start`] and called earlier because the facade's macros are a
/// no-op until a recorder exists, and startup records: provisioning talks to
/// storage, and the indexer's health record sets `notedthat_index_worker_alive`
/// the moment it is built. Installed after the listener bound, those would be
/// silently discarded and the gauge would never appear at all — a series
/// missing for the life of the process, with nothing to show it had gone.
pub(crate) fn install(config: &Config) -> anyhow::Result<()> {
    if config.metrics_listen_addr.is_none() {
        return Ok(());
    }
    shared_handle()?;
    record_build_info(config);
    Ok(())
}

/// Bind and serve the metrics listener, if this run configured one.
pub(crate) async fn start(
    config: &Config,
    shutdown: CancellationToken,
) -> anyhow::Result<Option<MetricsListener>> {
    let Some(addr) = config.metrics_listen_addr else {
        return Ok(None);
    };

    let handle = shared_handle()?;

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind the metrics listener on {addr}"))?;
    let bound_addr = listener.local_addr()?;
    info!(metrics = %bound_addr, "notedthat-server metrics listening");

    let graceful = shutdown.clone();
    let serve = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router(handle))
            .with_graceful_shutdown(async move { graceful.cancelled().await })
            .await
        {
            tracing::error!(%error, "the metrics listener failed");
        }
    });

    let upkeep_handle = HANDLE
        .get()
        .cloned()
        .ok_or_else(|| anyhow!("the metrics recorder vanished after installation"))?;
    let upkeep = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(UPKEEP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                _ = ticker.tick() => upkeep_handle.run_upkeep(),
            }
        }
    });

    Ok(Some(MetricsListener { serve, upkeep }))
}

/// The metrics listener's router: one route, and `404` for everything else.
///
/// Deliberately not built from `notedthat_api_http`, which would put `AppState`
/// and every product middleware on an unauthenticated operator socket. This
/// listener exists for one route, and a mistyped scrape path must be answered
/// by nothing.
fn router(handle: PrometheusHandle) -> Router {
    Router::new()
        .route("/metrics", get(render))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .with_state(handle)
}

async fn render(State(handle): State<PrometheusHandle>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, EXPOSITION_CONTENT_TYPE)],
        handle.render(),
    )
}

#[cfg(test)]
mod tests {
    use super::{EXPOSITION_CONTENT_TYPE, router, shared_handle};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;

    /// The recorder is global and one-shot; this asserts the sharing that keeps
    /// a second server in the same process from failing to start.
    #[test]
    fn the_handle_is_installed_once_and_shared() {
        let first = shared_handle().expect("the first install succeeds");
        let second = shared_handle().expect("the second call shares rather than reinstalling");
        // Not a comparison of the two renderings: every test in this binary
        // shares the registry and records into it concurrently, so they are
        // equal only by luck. What has to hold is that a measurement recorded
        // once is visible through both handles — which is the whole reason a
        // second server in one process may start at all.
        metrics::counter!("notedthat_test_shared_handle_total").increment(1);
        for (which, handle) in [("first", &first), ("second", &second)] {
            assert!(
                handle
                    .render()
                    .contains("notedthat_test_shared_handle_total"),
                "the {which} handle renders a different registry"
            );
        }
    }

    #[tokio::test]
    async fn the_route_answers_the_prometheus_text_format() {
        let handle = shared_handle().expect("a recorder is available");
        metrics::counter!("notedthat_test_probe_total").increment(1);
        let response = router(handle)
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("a GET /metrics request is well formed"),
            )
            .await
            .expect("the router answers");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .expect("the exposition names its content type"),
            EXPOSITION_CONTENT_TYPE
        );
    }

    /// The operator socket is not a second product surface.
    #[tokio::test]
    async fn every_other_path_is_404() {
        let handle = shared_handle().expect("a recorder is available");
        for uri in ["/", "/healthz", "/api/v1/knowledgebases", "/metrics/"] {
            let response = router(handle.clone())
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .expect("the request is well formed"),
                )
                .await
                .expect("the router answers");
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{uri} must not be served by the metrics listener"
            );
        }
    }
}

//! Server startup and lifecycle management.

use crate::config::{Config, StorageConfig};
use crate::provision::provision_kbs;
use anyhow::Context;
use notedthat_api_http::{
    router::{MAX_BODY_BYTES, build_router},
    state::AppState,
};
use notedthat_indexer::{
    IndexEvent, IndexerWorker, QdrantClient, QdrantConfig, QdrantProvisioner, VectorStore,
    embedder::openai::{OpenAiCompatibleConfig, OpenAiCompatibleEmbedder},
};
use notedthat_storage_fs::{FsStorage, RootLock};
use notedthat_storage_s3::S3Storage;
use notedthat_webdav::{router::build_router as build_dav_router, state::WebDavState};
use std::{sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

mod fs_watch;
mod mcp_http;

#[cfg(test)]
#[path = "run/mcp_http_listener.rs"]
mod mcp_http_listener;

// `Backends` lives in a private module and is re-exported only under
// `test-support`, so a production build cannot name it. Its three fields are
// `Arc<dyn Storage>`, `Arc<dyn VectorStore>` and `Arc<dyn Embedder>`: exposing
// them unconditionally would pin those traits into this crate's semver surface,
// and changing them — which is precisely what the seam exists to allow — would
// become a breaking change to `notedthat-server`.
mod backends {
    use super::VectorStore;
    use std::sync::Arc;

    /// The three external services the server runs on.
    ///
    /// Kept as trait objects and constructed separately from the rest of startup
    /// so the server can be brought up against substitutes. Tests use that to
    /// run the real routers, indexer worker and shutdown sequence in-process,
    /// with no S3, Qdrant or embedding endpoint to reach.
    pub struct Backends {
        /// Object storage backing every read and write.
        pub storage: Arc<dyn notedthat_core::Storage>,
        /// Vector store used for provisioning, indexing and search.
        pub store: Arc<dyn VectorStore>,
        /// Embedding endpoint shared by the indexer worker and the searcher.
        pub embedder: Arc<dyn notedthat_indexer::embedder::Embedder>,
    }
}

#[cfg(feature = "test-support")]
pub use backends::Backends;

/// Build the production backends described by `config`.
///
/// Construction is cheap and connectionless: nothing here reaches the network,
/// so a failure means bad configuration rather than an unreachable service. The
/// filesystem backend's root is proven usable and claimed earlier, by
/// [`open_storage_root`], so that stays true here.
fn backends_from_config(
    config: &Config,
    root: Option<&RootLock>,
) -> anyhow::Result<backends::Backends> {
    let storage: Arc<dyn notedthat_core::Storage> = match &config.storage {
        StorageConfig::S3(s3) => {
            info!(
                backend = "s3",
                endpoint = ?s3.endpoint_url,
                path_style = s3.force_path_style,
                "storage backend selected"
            );
            Arc::new(S3Storage::new(
                s3.build_client(),
                config.tenant_slug.clone(),
            ))
        }
        StorageConfig::Fs(fs) => {
            let root = root.context(
                "the filesystem backend needs a claimed storage root; call open_storage_root first",
            )?;
            info!(
                backend = "fs",
                root = %root.root().display(),
                metadata = %fs.metadata,
                "storage backend selected"
            );
            Arc::new(FsStorage::new(
                fs,
                root.root().to_path_buf(),
                config.tenant_slug.clone(),
            ))
        }
    };

    let qdrant_config = QdrantConfig {
        url: config.qdrant.url.clone(),
        api_key: config.qdrant.api_key.clone(),
        timeout: Duration::from_millis(config.qdrant.timeout_ms),
        connect_timeout: Duration::from_millis(config.qdrant.connect_timeout_ms),
    };
    // Held as the VectorStore seam rather than as the concrete client, so the
    // provisioner, searcher and indexer worker all share one substitutable
    // backend (see notedthat_indexer::vector_store).
    let store: Arc<dyn VectorStore> =
        Arc::new(QdrantClient::new(&qdrant_config).context("failed to build Qdrant client")?);

    let embedder_config = OpenAiCompatibleConfig {
        endpoint_url: config.embedder.endpoint_url.clone(),
        model: config.embedder.model.clone(),
        api_key: config.embedder.api_key.clone(),
        dim: config.embedder.dimensions as usize,
        max_input_tokens: config.embedder.max_input_tokens,
        timeout: Duration::from_millis(config.embedder.timeout_ms),
        max_retries: config.embedder.max_retries,
    };
    let embedder: Arc<dyn notedthat_indexer::embedder::Embedder> = Arc::new(
        OpenAiCompatibleEmbedder::new(embedder_config).context("failed to build embedder")?,
    );

    Ok(backends::Backends {
        storage,
        store,
        embedder,
    })
}

/// Build infrastructure components (indexer, provisioning, app state) over `backends`.
///
/// Assumes `config.staging` has already been validated; [`serve`] does that
/// before anything else so the check cannot be shadowed by a backend failure.
async fn build_infrastructure(
    config: Config,
    backends: backends::Backends,
) -> anyhow::Result<(
    AppState,
    WebDavState,
    CancellationToken,
    tokio::task::JoinHandle<()>,
    Option<fs_watch::FsWatch>,
)> {
    let backends::Backends {
        storage,
        store,
        embedder,
    } = backends;

    let (indexer_tx, indexer_rx) = mpsc::channel::<IndexEvent>(1024);
    let indexer_shutdown = CancellationToken::new();
    let declared_kbs = Arc::new(config.kbs.clone());

    let kb_list: Vec<_> = config.kbs.values().cloned().collect();
    let provisioner = QdrantProvisioner::new(store.clone());
    let access_policies = Arc::new(
        provision_kbs(
            storage.as_ref(),
            &config.tenant_slug,
            &kb_list,
            &provisioner,
            &config.embedder.model,
            config.embedder.dimensions,
            Some(config.embedder.endpoint_url.as_str()),
        )
        .await?,
    );

    let dav_state = WebDavState {
        username: Arc::new(config.webdav_username.clone()),
        password: Arc::new(config.webdav_password.clone()),
        storage: storage.clone(),
        declared_kbs: declared_kbs.clone(),
        access_policies: access_policies.clone(),
        indexer_tx: indexer_tx.clone(),
        staging_config: config.staging.clone(),
    };

    // Hybrid searcher shares the same embedder instance used at index time (§6.4, D18).
    // Using separate instances risks model or endpoint drift between write and query paths.
    let searcher: Arc<dyn notedthat_indexer::Searcher> = Arc::new(
        notedthat_indexer::searcher::HybridSearcher::new(store.clone(), embedder.clone()),
    );

    let state = AppState {
        storage: storage.clone(),
        declared_kbs,
        access_policies,
        bearer_token: Arc::new(config.api_token.clone()),
        max_body_size: MAX_BODY_BYTES,
        max_patchable_size: config.max_patchable_size,
        indexer_tx,
        searcher,
    };

    let worker_handle = tokio::spawn(
        IndexerWorker::new(
            storage.clone(),
            embedder.clone(),
            store.clone(),
            indexer_rx,
            indexer_shutdown.clone(),
            config.embedder.batch_size,
        )
        .with_staging_config(config.staging.clone())
        .run(),
    );

    // After provisioning, so every knowledge base's directory exists to be watched, and
    // after the worker is spawned, so the first reconciliation has somewhere to send.
    let watch = match &config.storage {
        StorageConfig::S3(_) => None,
        StorageConfig::Fs(fs) => fs_watch::start(
            fs,
            config.tenant_slug.clone(),
            kb_list,
            store.clone(),
            state.indexer_tx.clone(),
        )?,
    };

    Ok((state, dav_state, indexer_shutdown, worker_handle, watch))
}

/// Start the HTTP server with the provided configuration.
///
/// # Startup sequence (fail-fast per D39)
///
/// 1. Validate the staging directory, and for the filesystem backend the storage root.
/// 2. Build the storage, vector-store and embedder clients from config.
/// 3. Provision all declared KBs (validate bucket names, ensure buckets, write manifests).
/// 4. Bind the TCP listener.
/// 5. Serve requests until SIGTERM / SIGINT.
///
/// Any failure in steps 1-4 returns `Err` immediately (non-zero exit via `main`).
///
/// # Errors
///
/// Returns an error if the staging directory or storage root is unusable, provisioning
/// fails, the listener cannot bind, or axum serving fails.
pub async fn run(config: Config) -> anyhow::Result<()> {
    // Validate staging BEFORE constructing any backend. `backends_from_config`
    // can fail on a malformed Qdrant URL or embedder config, and if it ran first
    // it would shadow the staging error — which is the more actionable of the
    // two, since it names a directory the operator controls.
    config
        .staging
        .validate()
        .await
        .context("failed to validate NOTEDTHAT_UPLOAD_TMP_DIR")?;

    // Same reasoning one step further: a wrong or already-claimed storage root is more
    // actionable than a backend construction failure, and must not be shadowed by one.
    // Bound to a named local — dropping the guard would release the claim and silently
    // turn the single-process guarantee off.
    let storage_root = open_storage_root(&config).await?;

    let backends = backends_from_config(&config, storage_root.as_ref())?;
    serve(config, backends).await
}

/// Prove the filesystem storage root is usable and claim it for this process.
///
/// Returns `Ok(None)` for the S3 backend, which has no root to claim.
async fn open_storage_root(config: &Config) -> anyhow::Result<Option<RootLock>> {
    match &config.storage {
        StorageConfig::S3(_) => Ok(None),
        StorageConfig::Fs(fs) => Ok(Some(
            notedthat_storage_fs::open_root(fs)
                .await
                .context("failed to claim NOTEDTHAT_FS_ROOT")?,
        )),
    }
}

/// Start the HTTP server with the provided configuration and backends.
///
/// Skips both `backends_from_config` and the storage-root claim: the caller supplies the
/// backends, so it is the caller that knows which root, if any, this server will touch.
///
/// # The caller's obligation
///
/// `config.storage` is still read for one thing — it is what says whether there is a
/// filesystem tree to watch, and which one — so a caller passing `StorageConfig::Fs` gets
/// recursive watches over every declared knowledge base's directory and a second
/// `FsStorage` over that root, which can write to it (a sidecar repair, never object
/// content). **That caller must hold the [`RootLock`] itself**, exactly as [`run`] does on
/// its own path, because the single-process guarantee the lock exists for is that two
/// servers must not both be repairing sidecars in one tree. `tests/fs_backend_e2e.rs` is
/// the worked example: it calls `open_root` and keeps the guard alive for the server's
/// lifetime. A caller that does not want the obligation passes `StorageConfig::S3`, or
/// sets `NOTEDTHAT_FS_WATCH=false`.
///
/// Same startup sequence as [`run`], but over backends the caller supplies.
/// Tests use this to exercise the real routers, indexer worker and shutdown
/// path against in-memory substitutes.
///
/// # Errors
///
/// Returns an error if staging validation fails, provisioning fails, a listener
/// cannot bind, or axum serving fails.
#[cfg(feature = "test-support")]
pub async fn run_with(config: Config, backends: Backends) -> anyhow::Result<()> {
    config
        .staging
        .validate()
        .await
        .context("failed to validate NOTEDTHAT_UPLOAD_TMP_DIR")?;

    serve(config, backends).await
}

/// Shared startup body behind [`run`] and `run_with`.
///
/// Both callers validate `config.staging` before constructing or accepting
/// backends, so this body may assume it is already valid.
async fn serve(config: Config, backends: backends::Backends) -> anyhow::Result<()> {
    let (state, dav_state, indexer_shutdown, worker_handle, fs_watch) =
        build_infrastructure(config.clone(), backends).await?;
    let shutdown_token = CancellationToken::new();
    let serve_result = async {
        let listener = TcpListener::bind(config.listen_addr)
            .await
            .with_context(|| format!("failed to bind HTTP listener on {}", config.listen_addr))?;
        let bound_addr = listener.local_addr()?;
        let internal_api_url = mcp_http::internal_http_api_url(bound_addr);

        info!(http = %bound_addr, "notedthat-server listening");

        let app = build_router(state)
            .merge(build_dav_router(dav_state))
            .merge(mcp_http::build_router(
                &config,
                &internal_api_url,
                shutdown_token.child_token(),
            )?);

        let graceful_shutdown = shutdown_token.clone();
        let signal_shutdown = shutdown_token.clone();
        let shutdown_trigger = tokio::spawn(async move {
            shutdown_signal().await;
            signal_shutdown.cancel();
        });

        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async move { graceful_shutdown.cancelled().await })
            .await
            .context("HTTP listener failed");
        shutdown_trigger.abort();
        result
    }
    .await;
    shutdown_token.cancel();
    // Before the drain, not after: the watcher holds a sender on the indexing queue, so
    // draining while it still runs would chase a live producer and never see the queue
    // close — thirty seconds of every shutdown, spent waiting for work that keeps arriving.
    if let Some(fs_watch) = fs_watch {
        fs_watch.stop().await;
    }
    complete_shutdown(indexer_shutdown, worker_handle).await;
    serve_result
}

async fn complete_shutdown(
    indexer_shutdown: CancellationToken,
    worker_handle: tokio::task::JoinHandle<()>,
) {
    drain_indexer(indexer_shutdown, worker_handle).await;
    info!("shutdown complete");
}

async fn drain_indexer(
    indexer_shutdown: CancellationToken,
    worker_handle: tokio::task::JoinHandle<()>,
) {
    tracing::info!("shutdown signal received; draining indexer queue");
    indexer_shutdown.cancel();
    let join_result = tokio::time::timeout(Duration::from_secs(31), worker_handle).await;
    match join_result {
        Ok(Ok(())) => tracing::info!("indexer worker drained cleanly"),
        Ok(Err(e)) => tracing::error!(error = %e, "indexer worker panicked"),
        Err(_) => tracing::warn!("indexer worker did not drain within 31s; abandoning"),
    }
}

/// Wait for SIGTERM or SIGINT, then return to trigger graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            tracing::warn!(
                error = %e,
                "SIGINT handler installation failed; only SIGTERM will trigger graceful shutdown"
            );
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "SIGTERM handler installation failed; only SIGINT will trigger graceful shutdown"
                );
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("SIGINT received, shutting down"),
        () = terminate => tracing::info!("SIGTERM received, shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn completes_shutdown_by_cancelling_and_draining_indexer_immediately() {
        let indexer_shutdown = CancellationToken::new();
        let worker_shutdown = indexer_shutdown.clone();
        let (cancelled_tx, cancelled_rx) = oneshot::channel();
        let worker_handle = tokio::spawn(async move {
            worker_shutdown.cancelled().await;
            cancelled_tx
                .send(())
                .expect("test observes indexer cancellation once");
        });

        tokio::time::timeout(
            Duration::from_secs(1),
            complete_shutdown(indexer_shutdown, worker_handle),
        )
        .await
        .expect("shutdown completion should not wait after listeners quiesce");

        cancelled_rx
            .await
            .expect("indexer worker should receive cancellation");
    }
}

//! Server startup and lifecycle management.

use crate::config::Config;
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
use notedthat_storage_s3::S3Storage;
use notedthat_webdav::{router::build_router as build_dav_router, state::WebDavState};
use std::{sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

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
/// so a failure means bad configuration rather than an unreachable service.
fn backends_from_config(config: &Config) -> anyhow::Result<backends::Backends> {
    let client = config.s3.build_client();
    let storage = Arc::new(S3Storage::new(client, config.tenant_slug.clone()));

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
    let public_read_policies = Arc::new(
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
        public_read_policies: public_read_policies.clone(),
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
        public_read_policies,
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

    Ok((state, dav_state, indexer_shutdown, worker_handle))
}

/// Start the HTTP server with the provided configuration.
///
/// # Startup sequence (fail-fast per D39)
///
/// 1. Build S3 client from config.
/// 2. Provision all declared KBs (validate bucket names, ensure buckets, write manifests).
/// 3. Bind the TCP listener.
/// 4. Serve requests until SIGTERM / SIGINT.
///
/// Any failure in steps 1-3 returns `Err` immediately (non-zero exit via `main`).
///
/// # Errors
///
/// Returns an error if S3 provisioning fails, the listener cannot bind, or axum serving fails.
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

    let backends = backends_from_config(&config)?;
    serve(config, backends).await
}

/// Start the HTTP server with the provided configuration and backends.
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
    let (state, dav_state, indexer_shutdown, worker_handle) =
        build_infrastructure(config.clone(), backends).await?;

    // Bind every enabled listener before serving any of them (G11: atomic startup failure).
    // HTTP binds first because the MCP listener talks back through the actual HTTP API socket.
    let http_listener = TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("failed to bind HTTP listener on {}", config.listen_addr))?;
    let internal_api_url = mcp_http::internal_http_api_url(http_listener.local_addr()?);
    let dav_listener = TcpListener::bind(config.webdav_listen_addr)
        .await
        .with_context(|| {
            format!(
                "failed to bind WebDAV listener on {}",
                config.webdav_listen_addr
            )
        })?;
    let mcp_listener = mcp_http::bind_listener(&config).await?;

    info!(
        http = %config.listen_addr,
        dav = %config.webdav_listen_addr,
        mcp = ?mcp_listener.as_ref().and_then(|listener| listener.local_addr().ok()),
        "notedthat-server listening"
    );

    let http_app = build_router(state);
    let dav_app = build_dav_router(dav_state);
    let shutdown_token = CancellationToken::new();
    let http_shutdown = shutdown_token.clone();
    let dav_shutdown = shutdown_token.clone();
    let shutdown_on_http_error = shutdown_token.clone();
    let shutdown_on_dav_error = shutdown_token.clone();
    let mcp_shutdown = shutdown_token.child_token();
    let mcp_serve = match mcp_listener {
        Some(listener) => {
            let mcp_app = mcp_http::build_router(&config, &internal_api_url, mcp_shutdown.clone())?;
            Some(
                axum::serve(listener, mcp_app)
                    .with_graceful_shutdown(async move { mcp_shutdown.cancelled().await }),
            )
        }
        None => None,
    };
    let shutdown_on_mcp_error = shutdown_token.clone();

    let http_serve = axum::serve(http_listener, http_app)
        .with_graceful_shutdown(async move { http_shutdown.cancelled().await });
    let dav_serve = axum::serve(dav_listener, dav_app)
        .with_graceful_shutdown(async move { dav_shutdown.cancelled().await });

    let shutdown_trigger = tokio::spawn(async move {
        shutdown_signal().await;
        shutdown_token.cancel();
    });

    let http_handle = tokio::spawn(async move {
        let result = http_serve.await.context("HTTP listener failed");
        if result.is_err() {
            shutdown_on_http_error.cancel();
        }
        result
    });
    let dav_handle = tokio::spawn(async move {
        let result = dav_serve.await.context("WebDAV listener failed");
        if result.is_err() {
            shutdown_on_dav_error.cancel();
        }
        result
    });
    let mcp_handle = mcp_serve.map(|serve| {
        tokio::spawn(async move {
            let result = serve.await.context("MCP HTTP listener failed");
            if result.is_err() {
                shutdown_on_mcp_error.cancel();
            }
            result
        })
    });

    if let Some(mcp_handle) = mcp_handle {
        let serve_result = tokio::try_join!(http_handle, dav_handle, mcp_handle);
        shutdown_trigger.abort();
        let (http_result, dav_result, mcp_result) =
            serve_result.context("server task join failed")?;
        http_result?;
        dav_result?;
        mcp_result?;
    } else {
        let serve_result = tokio::try_join!(http_handle, dav_handle);
        shutdown_trigger.abort();
        let (http_result, dav_result) = serve_result.context("server task join failed")?;
        http_result?;
        dav_result?;
    }

    complete_shutdown(indexer_shutdown, worker_handle).await;
    Ok(())
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

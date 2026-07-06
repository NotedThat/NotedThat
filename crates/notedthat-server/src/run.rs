//! Server startup and lifecycle management.

use crate::config::Config;
use crate::provision::provision_kbs;
use anyhow::Context;
use notedthat_api_http::{
    router::{MAX_BODY_BYTES, build_router},
    state::AppState,
};
use notedthat_indexer::{
    IndexEvent, IndexerWorker, QdrantClient, QdrantConfig, QdrantProvisioner,
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

/// Grace period for in-flight `WebDAV` uploads after shutdown signal.
/// Runs BEFORE the existing 31-second indexer drain.
pub const WEBDAV_INFLIGHT_GRACE: Duration = Duration::from_secs(60);

/// Build infrastructure components (S3, Qdrant, embedder, indexer, app state).
async fn build_infrastructure(
    config: Config,
) -> anyhow::Result<(
    AppState,
    WebDavState,
    CancellationToken,
    tokio::task::JoinHandle<()>,
)> {
    let client = config.s3.build_client();
    let storage = Arc::new(S3Storage::new(client, config.tenant_slug.clone()));

    let qdrant_config = QdrantConfig {
        url: config.qdrant.url.clone(),
        api_key: config.qdrant.api_key.clone(),
    };
    let qdrant_client =
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

    let (indexer_tx, indexer_rx) = mpsc::channel::<IndexEvent>(1024);
    let indexer_shutdown = CancellationToken::new();
    let declared_kbs = Arc::new(config.kbs.clone());

    let dav_state = WebDavState {
        username: Arc::new(config.webdav_username.clone()),
        password: Arc::new(config.webdav_password.clone()),
        storage: storage.clone() as Arc<dyn notedthat_core::Storage>,
        declared_kbs: declared_kbs.clone(),
        indexer_tx: indexer_tx.clone(),
    };

    let kb_list: Vec<_> = config.kbs.values().cloned().collect();
    let provisioner = QdrantProvisioner::new((*qdrant_client).clone());
    provision_kbs(
        storage.as_ref(),
        &config.tenant_slug,
        &kb_list,
        &provisioner,
        &config.embedder.model,
        config.embedder.dimensions,
        Some(config.embedder.endpoint_url.as_str()),
    )
    .await?;

    // Hybrid searcher shares the same embedder instance used at index time (§6.4, D18).
    // Using separate instances risks model or endpoint drift between write and query paths.
    let searcher: Arc<dyn notedthat_indexer::Searcher> = Arc::new(
        notedthat_indexer::searcher::HybridSearcher::new(qdrant_client.clone(), embedder.clone()),
    );

    let state = AppState {
        storage: storage.clone() as Arc<dyn notedthat_core::Storage>,
        declared_kbs,
        bearer_token: Arc::new(config.api_token.clone()),
        max_body_size: MAX_BODY_BYTES,
        max_patchable_size: config.max_patchable_size,
        indexer_tx,
        searcher,
    };

    let worker_handle = tokio::spawn(
        IndexerWorker::new(
            storage.clone() as Arc<dyn notedthat_core::Storage>,
            embedder.clone(),
            qdrant_client.clone(),
            indexer_rx,
            indexer_shutdown.clone(),
            config.embedder.batch_size,
        )
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
    let (state, dav_state, indexer_shutdown, worker_handle) =
        build_infrastructure(config.clone()).await?;

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

    tracing::info!(
        "listeners quiesced; waiting {}s for in-flight WebDAV uploads",
        WEBDAV_INFLIGHT_GRACE.as_secs()
    );
    tokio::time::sleep(WEBDAV_INFLIGHT_GRACE).await;
    drain_indexer(indexer_shutdown, worker_handle).await;

    info!("shutdown complete");
    Ok(())
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

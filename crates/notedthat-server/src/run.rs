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
use std::{sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

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
        declared_kbs: Arc::new(config.kbs.clone()),
        bearer_token: Arc::new(config.api_token.clone()),
        max_body_size: MAX_BODY_BYTES,
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

    let app = build_router(state);
    let listener = TcpListener::bind(config.listen_addr).await?;
    info!(addr = %config.listen_addr, "notedthat-server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal_and_drain(indexer_shutdown, worker_handle))
        .await?;

    info!("shutdown complete");
    Ok(())
}

async fn shutdown_signal_and_drain(
    indexer_shutdown: CancellationToken,
    worker_handle: tokio::task::JoinHandle<()>,
) {
    shutdown_signal().await;
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

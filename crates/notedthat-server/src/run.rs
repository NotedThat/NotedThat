//! Server startup and lifecycle management.

use crate::config::Config;
use crate::provision::provision_kbs;
use notedthat_api_http::{
    router::{MAX_BODY_BYTES, build_router},
    state::AppState,
};
use notedthat_storage_s3::S3Storage;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
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

    let kb_list: Vec<_> = config.kbs.values().cloned().collect();
    provision_kbs(storage.as_ref(), &config.tenant_slug, &kb_list).await?;

    let state = AppState {
        storage: storage.clone() as Arc<dyn notedthat_core::Storage>,
        declared_kbs: Arc::new(config.kbs.clone()),
        bearer_token: Arc::new(config.api_token.clone()),
        max_body_size: MAX_BODY_BYTES,
    };

    let app = build_router(state);
    let listener = TcpListener::bind(config.listen_addr).await?;
    info!(addr = %config.listen_addr, "notedthat-server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("shutdown complete");
    Ok(())
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

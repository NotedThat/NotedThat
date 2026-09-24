//! The product listener's accept loop (D70).
//!
//! `axum::serve` builds hyper's connection with no timer, and hyper enforces a
//! header-read timeout only once it has one — so behind `axum::serve` a client
//! that opens a connection and sends its request head a byte a minute holds a
//! task for as long as it likes. This is the same loop with the timer set.
//!
//! What it keeps from `axum::serve`: HTTP/1.1 with upgrades, one task per
//! connection, and a graceful shutdown that stops accepting, lets every
//! connection finish the request it is on, and returns once all of them have
//! closed. The metrics listener still uses `axum::serve`; it is bound to
//! loopback and answers one cheap route.
//!
//! # The header-read timeout is also the idle timeout
//!
//! hyper starts the clock whenever a connection is waiting for a request head,
//! and a kept-alive connection between two requests is waiting for one. So an
//! idle connection is closed after the same interval. A proxy that keeps
//! upstream connections alive must drop idle ones sooner than this, or it
//! will now and then send a request down a connection the server has just
//! closed.

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use notedthat_core::metrics::name;
use std::io;
use std::pin::pin;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// How long to wait after an accept error that is not about one connection —
/// out of file descriptors, most often — before accepting again. The same
/// pause `axum::serve` takes, so a persistent failure does not spin a core.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_secs(1);

/// Serve `app` on `listener` until `shutdown` is cancelled, then drain.
///
/// # Errors
///
/// None today: every accept error is logged and retried, as `axum::serve`
/// does. The signature keeps the caller's error handling where it was.
pub(super) async fn serve(
    listener: TcpListener,
    app: Router,
    header_read_timeout: Duration,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let connections = TaskTracker::new();
    loop {
        let stream = tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _remote)) => stream,
                Err(error) if is_connection_error(&error) => continue,
                Err(error) => {
                    tracing::error!(%error, "failed to accept a connection");
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                    continue;
                }
            },
        };
        let service = TowerToHyperService::new(app.clone());
        let shutdown = shutdown.clone();
        connections.spawn(async move {
            let mut builder = http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(header_read_timeout);
            let mut connection = pin!(
                builder
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
            );
            let result = tokio::select! {
                result = connection.as_mut() => result,
                () = shutdown.cancelled() => {
                    // Finish the request in flight, then close. A connection
                    // idle between requests closes at once.
                    connection.as_mut().graceful_shutdown();
                    connection.await
                }
            };
            if let Err(error) = result {
                if error.is_timeout() {
                    metrics::counter!(name::HTTP_HEADER_READ_TIMEOUTS).increment(1);
                    tracing::debug!(%error, "closed a connection that sent no request head in time");
                } else {
                    tracing::trace!(%error, "connection ended with an error");
                }
            }
        });
    }
    // No more connections are accepted from here; the socket is released
    // before the drain, so a caller that rebinds the address after this
    // returns is not refused by a listener nobody is reading from.
    drop(listener);
    connections.close();
    connections.wait().await;
    Ok(())
}

/// An accept error about one connection, which the next accept will not see.
fn is_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

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

#[cfg(test)]
mod tests {
    use super::serve;
    use axum::Router;
    use axum::routing::get;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    /// The half of the drain that is easy to lose: a request already being
    /// worked on when the shutdown arrives must still get its whole response.
    ///
    /// Dropping `graceful_shutdown()` from the select would leave this passing
    /// only for connections that happened to be idle — `connections.wait()`
    /// returns for those either way — so this holds one open across the
    /// cancellation on purpose.
    #[tokio::test]
    async fn a_request_in_flight_at_shutdown_still_gets_its_response() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let app = Router::new().route(
            "/slow",
            get({
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                move || {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    async move {
                        started.notify_one();
                        release.notified().await;
                        "finished anyway"
                    }
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let shutdown = CancellationToken::new();
        let server = tokio::spawn(serve(
            listener,
            app,
            Duration::from_secs(30),
            shutdown.clone(),
        ));

        // Given: a request the server has started but not answered.
        let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
        client
            .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("request");
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("the handler should have been reached");

        // When: the server is asked to stop while it is still working.
        shutdown.cancel();
        // Give the accept loop a moment to notice, so the release below cannot
        // be what lets the response through.
        tokio::time::sleep(Duration::from_millis(50)).await;
        release.notify_one();

        // Then: the response arrives in full, and the server returns.
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut answer))
            .await
            .expect("the connection should close once the response is sent")
            .expect("read");
        let answer = String::from_utf8_lossy(&answer);
        assert!(answer.contains("200 OK"), "{answer}");
        assert!(answer.contains("finished anyway"), "{answer}");

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the drain should return once every connection has closed")
            .expect("join")
            .expect("serve");
    }

    /// And the other half: the socket is released before the drain, so a
    /// caller that rebinds the address once `serve` returns is not refused.
    #[tokio::test]
    async fn the_address_is_free_once_serve_returns() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let shutdown = CancellationToken::new();
        let server = tokio::spawn(serve(
            listener,
            Router::new().route("/", get(|| async { "ok" })),
            Duration::from_secs(30),
            shutdown.clone(),
        ));

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("drain")
            .expect("join")
            .expect("serve");

        TcpListener::bind(addr)
            .await
            .expect("the address must be free once serve has returned");
    }
}

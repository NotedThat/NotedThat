//! How long one request may take, and how many may be in flight at once (D70).
//!
//! One middleware, [`bound`], enforcing two limits. It is attached *per route*,
//! never to the merged app, and that is the whole of the streaming exemption:
//! `GET /mcp` and the API events route are registered on routers this layer is
//! not applied to.
//!
//! So what is bounded follows from *which sub-router* a route is registered on,
//! not from a list of paths — but that is a weaker guarantee than "everything
//! new is bounded", and worth stating exactly. The exempt routers are the outer
//! one in [`crate::router`], which also carries `/healthz` and `/readyz`, and
//! the API's streaming router. Adding a route beside the probes leaves it
//! unbounded, which is why `router::bounded_routes` pins the outer router's
//! contents rather than trusting review to notice. A router's fallback is not
//! covered either, `route_layer` not applying to one, so an unmatched path is
//! answered outside the cap: it bounds concurrent *matched* requests.
//!
//! # What the limits measure
//!
//! Both end when the inner service produces its **response head**, which is
//! the span [`crate::metrics`] times, for the same reason: a download that
//! takes minutes to transfer is the network's time, not the server's, and a
//! bound on the body would cut it short. The work worth bounding — a search's
//! embedder call, a `PROPFIND` walk, a `PATCH` rewrite — all happens before the
//! head. How long a body may take to transfer is the proxy's business.
//!
//! # Why the cap refuses instead of queueing
//!
//! A request that waits for a permit is still holding a connection and a task,
//! which is the resource the cap exists to protect. So it takes a permit or is
//! answered at once, with the same `503` and `Retry-After` every other capacity
//! refusal on this server gives (D38).

use crate::error::refusal;
use crate::metrics::route_and_surface;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use futures::StreamExt as _;
use notedthat_core::metrics::{label, name, refused_reason};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, oneshot};
use tower_http::request_id::RequestId;

/// The two limits one route is held to.
///
/// Cheap to clone. Several values may share one [`Semaphore`] — the server
/// gives `/webdav` a longer timeout than everything else, but one listener has
/// one in-flight cap, so both are built over the same permits.
#[derive(Debug, Clone)]
pub struct RequestBounds {
    timeout: Duration,
    client_idle: Duration,
    in_flight: Arc<Semaphore>,
}

impl RequestBounds {
    /// Bounds with this timeout, drawing permits from `in_flight`.
    ///
    /// `client_idle` is the longest the server waits between two frames of the
    /// request body — `NOTEDTHAT_HEADER_READ_TIMEOUT_MS`, the same bound hyper
    /// applies to the request head, for the same reason: it measures how long
    /// the client is taking, not how long the server is.
    #[must_use]
    pub fn new(timeout: Duration, client_idle: Duration, in_flight: Arc<Semaphore>) -> Self {
        Self {
            timeout,
            client_idle,
            in_flight,
        }
    }

    /// The same permits and idle bound, with a different timeout.
    #[must_use]
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        Self {
            timeout,
            client_idle: self.client_idle,
            in_flight: Arc::clone(&self.in_flight),
        }
    }

    /// No effective limit: a day per request and as many permits as a
    /// semaphore holds. For routers built outside a running server — tests,
    /// and callers that bound the listener some other way.
    #[must_use]
    pub fn unbounded() -> Self {
        Self::new(
            Duration::from_hours(24),
            Duration::from_hours(24),
            Arc::new(Semaphore::new(Semaphore::MAX_PERMITS)),
        )
    }
}

/// How a request body stopped producing frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyEnd {
    /// The client finished sending it.
    Complete,
    /// It went quiet for longer than [`RequestBounds::client_idle`].
    Idle,
}

/// Wrap `body` so that the gap between two frames is bounded, and `signal`
/// fires once it ends either way.
///
/// The frames themselves are passed through untouched; what is measured is the
/// pause between them, so a slow but progressing upload is never cut off while
/// a stalled one is. Trailers are dropped, which HTTP/1 request bodies on these
/// surfaces do not carry.
fn watch_body(body: Body, idle: Duration, signal: oneshot::Sender<BodyEnd>) -> Body {
    let stream = futures::stream::unfold(
        (body.into_data_stream(), Some(signal), false),
        move |(mut frames, mut signal, done)| async move {
            if done {
                return None;
            }
            match tokio::time::timeout(idle, frames.next()).await {
                Ok(Some(Ok(chunk))) => Some((Ok(chunk), (frames, signal, false))),
                Ok(Some(Err(e))) => {
                    // The client's own failure; whoever is reading the body
                    // sees it, and there is nothing left to bound.
                    if let Some(signal) = signal.take() {
                        let _ = signal.send(BodyEnd::Complete);
                    }
                    Some((Err(e), (frames, signal, true)))
                }
                Ok(None) => {
                    if let Some(signal) = signal.take() {
                        let _ = signal.send(BodyEnd::Complete);
                    }
                    None
                }
                Err(_) => {
                    if let Some(signal) = signal.take() {
                        let _ = signal.send(BodyEnd::Idle);
                    }
                    // An ERROR, never a clean end. Ending the stream here
                    // would hand the reader a short body indistinguishable
                    // from a complete one, and a handler that reads to EOF
                    // would store a truncated upload and answer `201`.
                    Some((
                        Err(axum::Error::new(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "the request body stopped arriving",
                        ))),
                        (frames, signal, true),
                    ))
                }
            }
        },
    );
    Body::from_stream(stream)
}

/// Take a permit and run the request within its timeout, or refuse it.
///
/// A refusal is still counted by [`crate::metrics::track_requests`] under its
/// status, because that layer wraps this one; here it is also counted by
/// *why*, so an operator can tell the cap from a backend's own `503`.
pub async fn bound(State(bounds): State<RequestBounds>, req: Request, next: Next) -> Response {
    let Ok(_permit) = Arc::clone(&bounds.in_flight).try_acquire_owned() else {
        count_refusal(&req, refused_reason::IN_FLIGHT);
        return refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend_unavailable",
            "the server is at its limit of requests in flight; retry shortly".to_string(),
            request_id(&req),
        );
    };
    // Taken before `next.run` consumes the request: a timeout has nothing
    // else left to read them from.
    let refused = RefusedRequest::of(&req);

    // A request with a body is two spans, not one, and only the second is the
    // server's. Bounding both with `timeout` made a large upload fail for
    // being large: every write handler reads the body to completion before it
    // produces a head, so a 1 GiB `PUT` over a slow uplink spent the whole
    // deadline on transfer it does not control. The transfer is bounded by
    // `client_idle` between frames instead, and `timeout` starts once the
    // client has finished sending.
    let (req, body) = if has_declared_body(&req) {
        let (signal, ended) = oneshot::channel();
        let req = req.map(|body| watch_body(body, bounds.client_idle, signal));
        (req, Some(ended))
    } else {
        (req, None)
    };

    let mut handler = std::pin::pin!(next.run(req));
    if let Some(mut ended) = body {
        tokio::select! {
            // Biased: when the body's verdict and the handler's answer are
            // both ready — which is exactly what a stalled body produces, the
            // handler having just been handed the error — the verdict wins, so
            // the refusal says what happened instead of whatever the handler
            // made of a truncated read.
            biased;
            end = &mut ended => match end {
                Ok(BodyEnd::Idle) => {
                    refused.count(refused_reason::TIMEOUT);
                    return refusal(
                        StatusCode::REQUEST_TIMEOUT,
                        "request_timeout",
                        format!(
                            "the request body stopped arriving for more than {} ms",
                            bounds.client_idle.as_millis()
                        ),
                        refused.request_id,
                    );
                }
                // Complete, or the watcher dropped without a verdict; from
                // here the remaining time is the server's own.
                Ok(BodyEnd::Complete) | Err(_) => {}
            },
            // The handler can answer before the body ends — a rejected
            // `Content-Length`, a precondition, an auth failure downstream.
            response = &mut handler => {
                // …or it can answer *because* the body timed out, having just
                // been handed the error, in the same poll that sends the
                // verdict. `biased` cannot order that: the verdict is still
                // pending when this branch is chosen and arrives during it. So
                // ask again before answering, or a stalled upload is reported
                // as whatever the handler made of a truncated read.
                if matches!(ended.try_recv(), Ok(BodyEnd::Idle)) {
                    refused.count(refused_reason::TIMEOUT);
                    return refusal(
                        StatusCode::REQUEST_TIMEOUT,
                        "request_timeout",
                        format!(
                            "the request body stopped arriving for more than {} ms",
                            bounds.client_idle.as_millis()
                        ),
                        refused.request_id,
                    );
                }
                return response;
            }
        }
    }

    if let Ok(response) = tokio::time::timeout(bounds.timeout, handler).await {
        response
    } else {
        refused.count(refused_reason::TIMEOUT);
        refusal(
            StatusCode::GATEWAY_TIMEOUT,
            "request_timeout",
            format!(
                "the request did not complete within {} ms",
                bounds.timeout.as_millis()
            ),
            refused.request_id,
        )
    }
}

/// Whether this request has a body worth waiting for before the clock starts.
///
/// True only when the client declared one — a non-zero `Content-Length`,
/// or a `Transfer-Encoding` making it chunked. Read from the head rather than
/// from the body's size hint so the answer does not depend on whether anything
/// has polled it yet.
///
/// A request that declared no body is not waited for, because a handler never
/// polls one and the deadline would otherwise stay unarmed for good — which
/// would exempt `GET …/search`, the route the timeout exists for. The residual
/// gap is the mirror image: a handler that ignores a body the client *did*
/// declare and then hangs is bounded by the in-flight cap and by the
/// connection's own idle timeout, not by `timeout`.
fn has_declared_body(req: &Request) -> bool {
    let declared = req
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let chunked = req.headers().contains_key(http::header::TRANSFER_ENCODING);
    chunked || declared.is_some_and(|len| len > 0)
}

/// The request id, when the surface's own request-id layer has already run.
///
/// The API assigns one before this layer; `WebDAV` and MCP assign theirs
/// inside it, or not at all. An id minted here would appear in the body and in
/// no header or log line, so none is better than an uncorrelatable one.
fn request_id(req: &Request) -> Option<String> {
    req.extensions()
        .get::<RequestId>()
        .and_then(|id| id.header_value().to_str().ok())
        .map(str::to_owned)
}

/// What a refusal is recorded under, captured while the request still exists.
struct RefusedRequest {
    route: String,
    surface: &'static str,
    request_id: Option<String>,
}

impl RefusedRequest {
    fn of(req: &Request) -> Self {
        let (route, surface) = route_and_surface(req);
        Self {
            route,
            surface,
            request_id: request_id(req),
        }
    }

    fn count(&self, reason: &'static str) {
        metrics::counter!(
            name::HTTP_REQUESTS_REFUSED,
            label::SURFACE => self.surface,
            label::ROUTE => self.route.clone(),
            label::REASON => reason,
        )
        .increment(1);
    }
}

fn count_refusal(req: &Request, reason: &'static str) {
    RefusedRequest::of(req).count(reason);
}

#[cfg(test)]
mod tests {
    use super::{RequestBounds, bound};
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header::RETRY_AFTER};
    use axum::middleware::from_fn_with_state;
    use axum::response::Response;
    use axum::routing::get;
    use metrics_util::debugging::DebuggingRecorder;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{Notify, Semaphore};
    use tower::ServiceExt;
    use tower_http::request_id::{MakeRequestUuid, SetRequestIdLayer};

    const TIMEOUT: Duration = Duration::from_secs(5);
    /// Long enough not to be what any test below is measuring.
    const IDLE: Duration = Duration::from_secs(300);

    /// A router with one bounded route that answers at once and one that
    /// answers only when `release` is notified, plus an unbounded route
    /// standing in for a stream.
    fn app(bounds: &RequestBounds, started: Arc<Notify>, release: Arc<Notify>) -> Router {
        let bounded = Router::new()
            .route("/fast", get(|| async { "fast" }))
            .route(
                "/slow",
                get(move || {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    async move {
                        started.notify_one();
                        release.notified().await;
                        "slow"
                    }
                }),
            )
            .route_layer(from_fn_with_state(bounds.clone(), bound));
        Router::new()
            .route(
                "/stream",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    "stream"
                }),
            )
            .merge(bounded)
            .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
    }

    fn get_req(path: &str) -> Request<Body> {
        Request::get(path).body(Body::empty()).expect("request")
    }

    async fn json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("a JSON body")
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_past_its_timeout_is_504_in_the_error_envelope() {
        let bounds = RequestBounds::new(TIMEOUT, IDLE, Arc::new(Semaphore::new(4)));
        let app = app(&bounds, Arc::new(Notify::new()), Arc::new(Notify::new()));

        let response = app.oneshot(get_req("/slow")).await.expect("infallible");

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert!(
            response.headers().get(RETRY_AFTER).is_none(),
            "retrying the same slow request will not make it faster"
        );
        let body = json(response).await;
        assert_eq!(body["error"], "request_timeout");
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|m| m.contains("5000 ms")),
            "{body}"
        );
        assert!(
            body["request_id"].as_str().is_some_and(|id| !id.is_empty()),
            "the id the request-id layer assigned belongs in the body: {body}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_route_outside_the_layer_outlives_the_timeout() {
        let bounds = RequestBounds::new(TIMEOUT, IDLE, Arc::new(Semaphore::new(1)));
        let app = app(&bounds, Arc::new(Notify::new()), Arc::new(Notify::new()));

        let response = app.oneshot(get_req("/stream")).await.expect("infallible");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test(start_paused = true)]
    async fn past_the_cap_a_request_is_refused_503_with_retry_after_at_once() {
        let bounds = RequestBounds::new(TIMEOUT, IDLE, Arc::new(Semaphore::new(1)));
        let started = Arc::new(Notify::new());
        let app = app(&bounds, Arc::clone(&started), Arc::new(Notify::new()));

        // Given: the only permit is held by a request still being worked on.
        let holder = tokio::spawn(app.clone().oneshot(get_req("/slow")));
        started.notified().await;

        // When: another bounded request arrives.
        let refused = app
            .clone()
            .oneshot(get_req("/fast"))
            .await
            .expect("infallible");

        // Then: it is answered straight away, in the D38 capacity shape.
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            refused
                .headers()
                .get(RETRY_AFTER)
                .map(axum::http::HeaderValue::as_bytes),
            Some(&b"5"[..])
        );
        assert_eq!(json(refused).await["error"], "backend_unavailable");

        // And the stream is not counted against the cap at all.
        let stream = tokio::spawn(app.clone().oneshot(get_req("/stream")));
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(
            stream.await.expect("join").expect("infallible").status(),
            StatusCode::OK
        );

        // The minute the stream took is past the holder's timeout too.
        let held = holder.await.expect("join").expect("infallible");
        assert_eq!(held.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_request_gives_its_permit_back() {
        let bounds = RequestBounds::new(TIMEOUT, IDLE, Arc::new(Semaphore::new(1)));
        let app = app(&bounds, Arc::new(Notify::new()), Arc::new(Notify::new()));

        let first = app
            .clone()
            .oneshot(get_req("/slow"))
            .await
            .expect("infallible");
        assert_eq!(first.status(), StatusCode::GATEWAY_TIMEOUT);

        let second = app.oneshot(get_req("/fast")).await.expect("infallible");
        assert_eq!(second.status(), StatusCode::OK);
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_disconnects_gives_its_permit_back() {
        let bounds = RequestBounds::new(TIMEOUT, IDLE, Arc::new(Semaphore::new(1)));
        let started = Arc::new(Notify::new());
        let app = app(&bounds, Arc::clone(&started), Arc::new(Notify::new()));

        // hyper drops the service future when the client goes away.
        let abandoned = tokio::spawn(app.clone().oneshot(get_req("/slow")));
        started.notified().await;
        abandoned.abort();
        let _ = abandoned.await;

        let next = app.oneshot(get_req("/fast")).await.expect("infallible");
        assert_eq!(next.status(), StatusCode::OK);
    }

    #[test]
    fn each_refusal_is_counted_by_route_and_reason() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let bounds = RequestBounds::new(TIMEOUT, IDLE, Arc::new(Semaphore::new(1)));
                let started = Arc::new(Notify::new());
                let app = app(&bounds, Arc::clone(&started), Arc::new(Notify::new()));
                let holder = tokio::spawn(app.clone().oneshot(get_req("/slow")));
                started.notified().await;
                let refused = app.clone().oneshot(get_req("/fast")).await;
                assert_eq!(
                    refused.expect("infallible").status(),
                    StatusCode::SERVICE_UNAVAILABLE
                );
                let timed_out = holder.await.expect("join").expect("infallible");
                assert_eq!(timed_out.status(), StatusCode::GATEWAY_TIMEOUT);
            });
        });

        let mut series: Vec<String> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(key, _, _, _)| {
                let key = key.key();
                let labels = key
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{}{{{labels}}}", key.name())
            })
            .collect();
        series.sort();
        assert_eq!(
            series,
            [
                "notedthat_http_requests_refused_total{surface=root,route=/fast,reason=in_flight}",
                "notedthat_http_requests_refused_total{surface=root,route=/slow,reason=timeout}",
            ]
        );
    }
}

//! How long one request may take, and how many may be in flight at once (D70).
//!
//! One middleware, [`bound`], enforcing two limits. It is attached *per route*,
//! never to the merged app, and that is the whole of the streaming exemption:
//! `GET /mcp` and the API events route are registered on routers this layer is
//! not applied to. There is no path list to keep in sync, so a new route is
//! bounded unless someone deliberately routes it beside the streams.
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
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use notedthat_core::metrics::{label, name, refused_reason};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tower_http::request_id::RequestId;

/// The two limits one route is held to.
///
/// Cheap to clone. Several values may share one [`Semaphore`] — the server
/// gives `/webdav` a longer timeout than everything else, but one listener has
/// one in-flight cap, so both are built over the same permits.
#[derive(Debug, Clone)]
pub struct RequestBounds {
    timeout: Duration,
    in_flight: Arc<Semaphore>,
}

impl RequestBounds {
    /// Bounds with this timeout, drawing permits from `in_flight`.
    #[must_use]
    pub fn new(timeout: Duration, in_flight: Arc<Semaphore>) -> Self {
        Self { timeout, in_flight }
    }

    /// The same permits, with a different timeout.
    #[must_use]
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        Self {
            timeout,
            in_flight: Arc::clone(&self.in_flight),
        }
    }

    /// No effective limit: a day per request and as many permits as a
    /// semaphore holds. For routers built outside a running server — tests,
    /// and callers that bound the listener some other way.
    #[must_use]
    pub fn unbounded() -> Self {
        Self::new(
            Duration::from_secs(24 * 60 * 60),
            Arc::new(Semaphore::new(Semaphore::MAX_PERMITS)),
        )
    }
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
    if let Ok(response) = tokio::time::timeout(bounds.timeout, next.run(req)).await {
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
        let bounds = RequestBounds::new(TIMEOUT, Arc::new(Semaphore::new(4)));
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
        let bounds = RequestBounds::new(TIMEOUT, Arc::new(Semaphore::new(1)));
        let app = app(&bounds, Arc::new(Notify::new()), Arc::new(Notify::new()));

        let response = app.oneshot(get_req("/stream")).await.expect("infallible");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test(start_paused = true)]
    async fn past_the_cap_a_request_is_refused_503_with_retry_after_at_once() {
        let bounds = RequestBounds::new(TIMEOUT, Arc::new(Semaphore::new(1)));
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
        let bounds = RequestBounds::new(TIMEOUT, Arc::new(Semaphore::new(1)));
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
        let bounds = RequestBounds::new(TIMEOUT, Arc::new(Semaphore::new(1)));
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
                let bounds = RequestBounds::new(TIMEOUT, Arc::new(Semaphore::new(1)));
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

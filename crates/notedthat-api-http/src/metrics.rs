//! Request metrics for every surface on the unified listener (D68).
//!
//! One layer, applied to the merged router in `notedthat-server` after `WebDAV`
//! and MCP are merged onto it. Applied inside [`crate::router::build_router`]'s
//! own `ServiceBuilder` it would cover the API and the root routes only, since
//! the other two surfaces are merged afterwards.
//!
//! # Why the route label is the pattern
//!
//! `route` is axum's [`MatchedPath`] — the *pattern*, `…/{kb_slug}/{*object_path}`,
//! not the path that matched it. This is the whole of the label-safety story:
//! the object route and every `WebDAV` route carry an object key in their path,
//! and a key in a label would put customer data in an exposition that is
//! retained for months by whoever scrapes it. A request that matched no route
//! has no pattern, and its URI is attacker-chosen, so all of them share the one
//! constant [`notedthat_core::metrics::ROUTE_UNMATCHED`].
//!
//! # Why the method label is an allow-list
//!
//! `http::Method` accepts arbitrary extension tokens, and this layer records
//! before any `405`, so `req.method().as_str()` would let a client mint
//! unbounded series by looping over invented verbs. Only the methods this
//! server actually answers are labelled; anything else is `other`.
//!
//! # What the duration means
//!
//! Time to *response head*, not to last byte: `next.run` returns as soon as a
//! handler produces a `Response`. That is deliberate. The events route returns
//! instantly and then streams for hours, so body-completion timing would put
//! hour-long observations in this histogram and pin the in-flight gauge at the
//! subscriber count. A live stream's cost is `notedthat_events_subscribers`,
//! and a large read's backend cost is `notedthat_storage_*`. Anyone "fixing"
//! this to measure body completion would silently destroy both.

use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderValue, Method};
use axum::middleware::Next;
use axum::response::Response;
use notedthat_core::metrics::{ROUTE_UNMATCHED, label, name, surface};
use std::time::Instant;

use crate::router::helpers::SOURCE_HEADER;

/// Mount points, matched against the route *pattern* rather than the path.
const WEBDAV_PREFIX: &str = "/webdav";
const MCP_ROUTE: &str = "/mcp";
const SSE_PREFIX: &str = "/sse";
const BROWSE_PREFIX: &str = "/browse";
const API_PREFIX: &str = "/api/v1";

/// The methods this server answers, as static labels.
///
/// The `WebDAV` verbs are here because `/webdav` answers them; anything outside
/// this list shares one series, because the set of tokens a client may send is
/// not bounded by anything we control.
fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::PATCH => "PATCH",
        Method::OPTIONS => "OPTIONS",
        Method::TRACE => "TRACE",
        Method::CONNECT => "CONNECT",
        _ => match method.as_str() {
            "PROPFIND" => "PROPFIND",
            "PROPPATCH" => "PROPPATCH",
            "MKCOL" => "MKCOL",
            "COPY" => "COPY",
            "MOVE" => "MOVE",
            "LOCK" => "LOCK",
            "UNLOCK" => "UNLOCK",
            _ => "other",
        },
    }
}

/// Which surface served a request, from its matched pattern.
///
/// The `x-notedthat-source` header is consulted only under `/api/v1`, which is
/// the only place the MCP server's loopback client sets it: an MCP tool call
/// reaches the API over loopback, and without this it would be counted twice —
/// once as `mcp` for the transport hop and again as `api` for the work. The
/// header is informational and any client may send it, but it is matched
/// byte-exactly against two constants, so the worst a client achieves is
/// mis-filing its own request between `api` and `mcp`.
fn surface_of(route: &str, source: Option<&HeaderValue>) -> &'static str {
    if route == ROUTE_UNMATCHED {
        return surface::ROOT;
    }
    if route.starts_with(WEBDAV_PREFIX) {
        return surface::WEBDAV;
    }
    if route == MCP_ROUTE || route.starts_with(SSE_PREFIX) {
        return surface::MCP;
    }
    if route.starts_with(BROWSE_PREFIX) {
        return surface::BROWSE;
    }
    if route.starts_with(API_PREFIX) {
        return if source.map(HeaderValue::as_bytes) == Some(b"mcp") {
            surface::MCP
        } else {
            surface::API
        };
    }
    surface::ROOT
}

/// One request's place in `notedthat_http_requests_in_flight`.
///
/// A guard rather than a matched pair of statements, because when a client
/// disconnects hyper drops this middleware's future and nothing after
/// `next.run` runs. Without the guard the gauge would ratchet upwards forever
/// under exactly the conditions worth measuring.
struct InFlight(&'static str);

impl InFlight {
    fn enter(surface: &'static str) -> Self {
        metrics::gauge!(name::HTTP_IN_FLIGHT, label::SURFACE => surface).increment(1.0);
        Self(surface)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        metrics::gauge!(name::HTTP_IN_FLIGHT, label::SURFACE => self.0).decrement(1.0);
    }
}

/// Count and time every request on every surface.
pub async fn track_requests(req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| ROUTE_UNMATCHED.to_string(), |m| m.as_str().to_string());
    let surface = surface_of(&route, req.headers().get(SOURCE_HEADER));
    let method = method_label(req.method());

    let _in_flight = InFlight::enter(surface);
    let started = Instant::now();
    let response = next.run(req).await;
    let elapsed = started.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    // `status` is on the counter but not the histogram: on the counter it is
    // four cheap labels, while on the histogram it multiplies by the bucket
    // count and the object route alone would be hundreds of series. Error
    // *rates* come from the counter; the histogram answers "how slow is this
    // route", which is the question a latency target is set from.
    metrics::counter!(
        name::HTTP_REQUESTS,
        label::SURFACE => surface,
        label::ROUTE => route.clone(),
        label::METHOD => method,
        label::STATUS => status,
    )
    .increment(1);
    metrics::histogram!(
        name::HTTP_REQUEST_DURATION,
        label::SURFACE => surface,
        label::ROUTE => route,
        label::METHOD => method,
    )
    .record(elapsed);

    response
}

#[cfg(test)]
mod tests {
    use super::{method_label, surface_of};
    use axum::http::{HeaderValue, Method};
    use notedthat_core::metrics::{ROUTE_UNMATCHED, surface};

    #[test]
    fn a_route_pattern_decides_the_surface() {
        for (route, expected) in [
            ("/webdav/{*path}", surface::WEBDAV),
            ("/webdav", surface::WEBDAV),
            ("/mcp", surface::MCP),
            ("/sse/{*path}", surface::MCP),
            ("/browse/{*path}", surface::BROWSE),
            (
                "/api/v1/knowledgebases/{kb_slug}/{*object_path}",
                surface::API,
            ),
            ("/healthz", surface::ROOT),
            ("/readyz", surface::ROOT),
            ("/llms.txt", surface::ROOT),
            (ROUTE_UNMATCHED, surface::ROOT),
        ] {
            assert_eq!(surface_of(route, None), expected, "for {route}");
        }
    }

    /// Without this, one MCP tool call is counted twice: once on `/mcp` and
    /// again on the loopback API call it makes to do the work.
    #[test]
    fn an_api_call_the_mcp_server_made_is_attributed_to_mcp() {
        let mcp = HeaderValue::from_static("mcp");
        assert_eq!(
            surface_of("/api/v1/knowledgebases", Some(&mcp)),
            surface::MCP
        );
    }

    /// The header is informational and any client may send it. It must be able
    /// to move a request between two constants and nothing more.
    #[test]
    fn the_source_header_cannot_move_another_surface_or_invent_one() {
        let mcp = HeaderValue::from_static("mcp");
        assert_eq!(surface_of("/webdav/{*path}", Some(&mcp)), surface::WEBDAV);
        assert_eq!(surface_of("/browse", Some(&mcp)), surface::BROWSE);

        let nonsense = HeaderValue::from_static("../../etc/passwd");
        assert_eq!(
            surface_of("/api/v1/knowledgebases", Some(&nonsense)),
            surface::API,
            "an unrecognised source is the default surface, never a label of its own"
        );
    }

    /// `Method` accepts arbitrary extension tokens, and this label is recorded
    /// before any 405, so an allow-list is the only thing bounding the series.
    #[test]
    fn an_invented_verb_shares_one_series() {
        let invented =
            Method::from_bytes(b"BREWCOFFEE").expect("an extension method is a valid token");
        assert_eq!(method_label(&invented), "other");
        assert_eq!(method_label(&Method::GET), "GET");
        assert_eq!(
            method_label(&Method::from_bytes(b"PROPFIND").expect("PROPFIND is a valid token")),
            "PROPFIND"
        );
    }
}

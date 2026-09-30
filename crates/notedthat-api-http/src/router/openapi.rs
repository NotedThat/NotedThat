//! The `OpenAPI` 3.1 document for `/api/v1` (D77), generated from the handlers.
//!
//! Each operation is declared by a `#[utoipa::path]` on its handler; this
//! module gathers them, adds what every operation shares — the reusable error
//! responses, the bearer scheme, the refusals of the request bounds — and
//! renders the document once. The router stays the one place routes are
//! registered: `tests/openapi.rs` holds the two to each other in both
//! directions, and holds `docs/openapi.json` to [`document_json`].

use std::sync::OnceLock;

use axum::http::header;
use axum::response::{IntoResponse, Response};
use notedthat_core::KbSlug;
use utoipa::openapi::path::{Operation, ParameterIn};
use utoipa::openapi::response::{Response as ApiResponse, ResponseBuilder};
use utoipa::openapi::schema::{Schema, SchemaType, Type};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::{Content, Header, Ref, RefOr};
use utoipa::{IntoParams, Modify, OpenApi, ToResponse};

use super::{events, index_health, index_reconcile, kbs, objects};
use crate::error::{ErrorBody, ErrorCode, RefusalBody, ReplaceAmbiguousBody};
use crate::search_route;

/// The document, as served and as committed: pretty-printed, one trailing
/// newline.
///
/// # Panics
///
/// If the document does not serialize to JSON. It is built from static
/// declarations, so this cannot happen at run time without also failing
/// `tests/openapi.rs`.
pub fn document_json() -> &'static str {
    static JSON: OnceLock<String> = OnceLock::new();
    JSON.get_or_init(|| {
        let mut json = ApiDoc::openapi()
            .to_pretty_json()
            .expect("the OpenAPI document serializes");
        json.push('\n');
        json
    })
}

/// The document as a value, for inspection.
pub fn document() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}

/// `GET /api/v1/openapi.json`: this document.
///
/// Public, like `/llms.txt`: it describes the routes and carries no data.
#[utoipa::path(
    get,
    path = "/openapi.json",
    tag = "meta",
    operation_id = "get_openapi",
    security(()),
    responses(
        (status = 200, description = "This document.", content_type = "application/json", body = Object),
    ),
)]
pub(crate) async fn openapi_json() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        document_json(),
    )
        .into_response()
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "NotedThat HTTP API",
        description = "The versioned machine API of a NotedThat server. Paths are relative to the \
            server URL, `/api/v1`.\n\nThis document is generated from the server's code and is \
            authoritative for paths, parameters, schemas and status codes. `docs/API.md` \
            explains behaviour; where the two disagree, `docs/API.md` is wrong.\n\nEvery \
            response carries `x-request-id`, echoing the request's when it sent one.",
        license(name = "MPL-2.0", identifier = "MPL-2.0"),
        // The version of the API this document describes, which is `v1` in
        // its paths — not the crate's. The crate version would change the
        // committed document on every release bump and tell anonymous callers
        // the exact build.
        version = "1",
    ),
    servers((url = "/api/v1")),
    paths(
        kbs::list_kbs,
        kbs::list_objects,
        search_route::search_kb,
        index_health::get_index_health,
        index_reconcile::post_index_reconcile,
        events::subscribe_events,
        objects::read::get_object,
        objects::read::head_object,
        objects::write::put_object,
        objects::write::delete_object,
        objects::patch::patch_object,
        objects::replace::post_object,
        openapi_json,
    ),
    components(
        schemas(ErrorBody, RefusalBody, ReplaceAmbiguousBody, ErrorCode, notedthat_core::ObjectEvent),
        responses(
            BadRequest,
            Unauthorized,
            Forbidden,
            NotFound,
            Conflict,
            Gone,
            PreconditionFailed,
            PayloadTooLarge,
            RangeNotSatisfiable,
            UnprocessableReplace,
            PreconditionRequired,
            InternalError,
            BackendUnavailable,
            NotModified,
            RequestTimeout,
            GatewayTimeout,
        ),
    ),
    modifiers(&Shared),
    tags(
        (name = "knowledgebases", description = "Which knowledge bases exist and what they hold."),
        (name = "objects", description = "Read and write one object's bytes."),
        (name = "search", description = "Hybrid search over a knowledge base."),
        (name = "index", description = "The search index's health, and on-demand reconciliation."),
        (name = "events", description = "A live stream of changes to a knowledge base."),
        (name = "meta", description = "This document."),
    ),
)]
struct ApiDoc;

/// The name of the one security scheme.
pub(crate) const BEARER: &str = "bearer";

/// What the attribute macros cannot say once for every operation.
struct Shared;

impl Modify for Shared {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            BEARER,
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("service token or OIDC access token (JWT)")
                    .description(Some(
                        "`Authorization: Bearer <token>`: the deployment's service token, or an \
                         access token from the configured OIDC issuer. A malformed or unverified \
                         credential is always `401`, never treated as anonymous.",
                    ))
                    .build(),
            ),
        );

        // The request bounds (D71) refuse before any handler runs, so every
        // bounded operation can answer these whatever its handler does. The
        // events stream is the one `/api/v1` route outside them.
        for (path, item) in &mut openapi.paths.paths {
            let bounded = !path.ends_with("/events");
            for operation in operations(item) {
                header_parameters_are_not_nullable(operation);
                if bounded {
                    add_response(operation, "408", RequestTimeout::response().0);
                    add_response(operation, "504", GatewayTimeout::response().0);
                    add_response(operation, "503", BackendUnavailable::response().0);
                }
            }
        }
    }
}

/// Every operation of one path item.
fn operations(item: &mut utoipa::openapi::path::PathItem) -> impl Iterator<Item = &mut Operation> {
    [
        item.get.as_mut(),
        item.put.as_mut(),
        item.post.as_mut(),
        item.delete.as_mut(),
        item.options.as_mut(),
        item.head.as_mut(),
        item.patch.as_mut(),
        item.trace.as_mut(),
    ]
    .into_iter()
    .flatten()
}

/// An optional header is absent, never `null`: HTTP has no null. utoipa
/// renders an `Option` header parameter as nullable, which a client generator
/// would type as `string | null`, so the `null` is taken back out.
fn header_parameters_are_not_nullable(operation: &mut Operation) {
    for parameter in operation.parameters.iter_mut().flatten() {
        let RefOr::T(parameter) = parameter else {
            continue;
        };
        if parameter.parameter_in != ParameterIn::Header {
            continue;
        }
        if let Some(RefOr::T(Schema::Object(object))) = &mut parameter.schema
            && let SchemaType::Array(types) = &object.schema_type
            && let [only] = types
                .iter()
                .filter(|kind| **kind != Type::Null)
                .collect::<Vec<_>>()[..]
        {
            object.schema_type = SchemaType::Type(only.clone());
        }
    }
}

/// Refer to the named response component unless the operation already
/// documents that status.
fn add_response(operation: &mut Operation, status: &str, component: &str) {
    operation
        .responses
        .responses
        .entry(status.to_string())
        .or_insert_with(|| RefOr::Ref(Ref::from_response_name(component)));
}

/// A string-valued response header.
fn string_header(description: &str) -> Header {
    let mut header = Header::default();
    header.description = Some(description.to_string());
    header
}

/// A response whose body is the named schema, as JSON.
fn json_error(description: &str, schema: &str) -> ResponseBuilder {
    ResponseBuilder::new().description(description).content(
        "application/json",
        Content::new(Some(Ref::from_schema_name(schema))),
    )
}

/// Declare reusable responses: each a unit type implementing [`ToResponse`],
/// registered by name under `components.responses`.
macro_rules! responses {
    ($($(#[$doc:meta])* $name:ident => $build:expr;)+) => {
        $(
            $(#[$doc])*
            pub(crate) struct $name;

            impl<'r> ToResponse<'r> for $name {
                fn response() -> (&'r str, RefOr<ApiResponse>) {
                    (stringify!($name), RefOr::T($build.build()))
                }
            }
        )+
    };
}

responses! {
    /// `400`.
    BadRequest => json_error(
        "The request is malformed: `invalid_request` or `malformed_range`. A query string the \
         server cannot parse at all is answered `400` with a plain-text body instead.",
        "Error",
    );
    /// `401`.
    Unauthorized => json_error(
        "`unauthorized`: no credential where one is required, or one that did not verify.",
        "Error",
    ).header(
        "WWW-Authenticate",
        string_header(
            "`Bearer resource_metadata=\"…\"`, when the deployment publishes OAuth protected \
             resource metadata.",
        ),
    );
    /// `403`.
    Forbidden => json_error(
        "`forbidden`: the credential is valid and the knowledge base's access policy does not \
         grant this operation. An anonymous caller is answered `404` instead.",
        "Error",
    );
    /// `404`.
    NotFound => json_error(
        "`not_found`: no such knowledge base or object — or one an anonymous caller may not see, \
         which reads identically.",
        "Error",
    );
    /// `409`.
    Conflict => json_error("`conflict`: the request conflicts with work in progress.", "Error");
    /// `410`.
    Gone => json_error(
        "`gone`: the events after `Last-Event-ID` are no longer retained. Resubscribe without it.",
        "Error",
    );
    /// `412`.
    PreconditionFailed => json_error(
        "`precondition_failed`: a conditional header's precondition did not hold.",
        "Error",
    );
    /// `413`.
    PayloadTooLarge => json_error("`payload_too_large`: the body exceeds the route's limit.", "Error");
    /// `416`, with no body (RFC 9110 §15.5.17).
    RangeNotSatisfiable => ResponseBuilder::new()
        .description("The range lies outside the object. No body.")
        .header(
            "Content-Range",
            string_header("`bytes */<size>`, or `lines */<line count>` for a line range."),
        )
        .header(
            "X-Content-Range-Bytes",
            string_header("`*/<size in bytes>`, on a line range only."),
        );
    /// `422` from a replace.
    UnprocessableReplace => ResponseBuilder::new()
        .description("`no_match`: `old_string` does not occur; or `ambiguous_match`: it occurs \
             more than once and `replace_all` is false.")
        .content(
            "application/json",
            Content::new(Some(
                utoipa::openapi::schema::AnyOfBuilder::new()
                    .item(Ref::from_schema_name("Error"))
                    .item(Ref::from_schema_name("AmbiguousMatchError")),
            )),
        );
    /// `428`.
    PreconditionRequired => json_error(
        "`precondition_required`: this write must be conditional; send `If-Match`.",
        "Error",
    );
    /// `500`.
    InternalError => json_error("`internal_error`: the server failed.", "Error");
    /// `503`.
    BackendUnavailable => ResponseBuilder::new()
        .description(
            "`backend_unavailable`: a backend or queue is unavailable or full, or the server is \
             at its limit of requests in flight. Retry after `Retry-After`. A write answered \
             this way may already be stored; repeat it.",
        )
        .content(
            "application/json",
            Content::new(Some(
                utoipa::openapi::schema::AnyOfBuilder::new()
                    .item(Ref::from_schema_name("Error"))
                    .item(Ref::from_schema_name("Refusal")),
            )),
        )
        .header("Retry-After", string_header("Seconds to wait: `5`."));
    /// `304`, with no body (RFC 9110 §15.4.5).
    NotModified => ResponseBuilder::new()
        .description("The object has not changed. No body.")
        .header("ETag", string_header("The object's current entity tag."))
        .header(
            "Last-Modified",
            string_header("The object's modification time, when it has no entity tag."),
        );
    /// `408` from the request bounds.
    RequestTimeout => json_error(
        "`request_timeout`: the request body stopped arriving for longer than the server's idle \
         limit.",
        "Refusal",
    );
    /// `504` from the request bounds.
    GatewayTimeout => json_error(
        "`request_timeout`: the request did not complete within the server's timeout.",
        "Refusal",
    );
}

/// The path parameter every knowledge-base route takes.
#[derive(IntoParams)]
#[into_params(parameter_in = Path)]
pub struct KbPath {
    /// The knowledge base.
    pub kb_slug: KbSlug,
}

/// The path parameters of an object route.
#[derive(IntoParams)]
#[into_params(parameter_in = Path)]
pub struct ObjectPathParams {
    /// The knowledge base.
    pub kb_slug: KbSlug,
    /// The object's path within the knowledge base, percent-encoded as one segment:
    /// `docs/rfc/7231.md` is sent as `docs%2Frfc%2F7231.md`. The server also accepts
    /// its `/` unencoded, which is how it writes `Location`.
    pub object_path: String,
}

/// The conditional request headers a read honours (RFC 9110 §13).
#[derive(IntoParams)]
#[into_params(parameter_in = Header)]
pub struct ReadConditions {
    /// Answer `412` unless the object's entity tag is one of these, or `*`.
    #[param(rename = "If-Match")]
    pub if_match: Option<String>,
    /// Answer `304` if the object's entity tag is one of these, or `*`.
    #[param(rename = "If-None-Match")]
    pub if_none_match: Option<String>,
    /// Answer `304` unless the object changed after this HTTP date.
    #[param(rename = "If-Modified-Since")]
    pub if_modified_since: Option<String>,
    /// Answer `412` if the object changed after this HTTP date.
    #[param(rename = "If-Unmodified-Since")]
    pub if_unmodified_since: Option<String>,
}

/// The conditional request headers a write or delete honours.
#[derive(IntoParams)]
#[into_params(parameter_in = Header)]
pub struct WriteConditions {
    /// Proceed only if the object's current entity tag is one of these, or `*` for any
    /// existing object; otherwise `412`.
    #[param(rename = "If-Match")]
    pub if_match: Option<String>,
    /// `*`: proceed only if the object does not exist; otherwise `412`.
    #[param(rename = "If-None-Match")]
    pub if_none_match: Option<String>,
}

#[cfg(test)]
mod tests {
    //! The document against the router it describes. axum cannot list a
    //! router's routes, so the router is asked instead: every documented
    //! operation must be answered by a handler, and every method the document
    //! leaves out must be `405`.

    use super::document;
    use crate::middleware::ANONYMOUS_REACHABLE;
    use crate::router::{API_ROUTES, API_V1_PREFIX, MATCHED_KB_OBJECT, OPENAPI_PATH, build_router};
    use crate::state::AppState;
    use crate::testing::InMemoryStorage;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode};
    use bytes::Bytes;
    use notedthat_core::{Authenticator, ConditionalHeaders, KbSlug, ObjectPath, Storage};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tower::ServiceExt;
    use utoipa::openapi::path::{HttpMethod, PathItem};

    const TOKEN: &str = "token";
    const REPLACE_PATH: &str = "/knowledgebases/{kb_slug}/replace/{object_path}";
    const OBJECT_PATH: &str = "/knowledgebases/{kb_slug}/{object_path}";
    const METHODS: [Method; 8] = [
        Method::GET,
        Method::HEAD,
        Method::PUT,
        Method::POST,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
        Method::TRACE,
    ];

    fn http_method(method: &Method) -> HttpMethod {
        match *method {
            Method::GET => HttpMethod::Get,
            Method::HEAD => HttpMethod::Head,
            Method::PUT => HttpMethod::Put,
            Method::POST => HttpMethod::Post,
            Method::PATCH => HttpMethod::Patch,
            Method::DELETE => HttpMethod::Delete,
            Method::OPTIONS => HttpMethod::Options,
            Method::TRACE => HttpMethod::Trace,
            _ => unreachable!("not in METHODS"),
        }
    }

    fn operation<'a>(
        item: &'a PathItem,
        method: &Method,
    ) -> Option<&'a utoipa::openapi::path::Operation> {
        match http_method(method) {
            HttpMethod::Get => item.get.as_ref(),
            HttpMethod::Head => item.head.as_ref(),
            HttpMethod::Put => item.put.as_ref(),
            HttpMethod::Post => item.post.as_ref(),
            HttpMethod::Patch => item.patch.as_ref(),
            HttpMethod::Delete => item.delete.as_ref(),
            HttpMethod::Options => item.options.as_ref(),
            HttpMethod::Trace => item.trace.as_ref(),
        }
    }

    /// Whether the document covers `method` on `item`. axum answers `HEAD`
    /// wherever it answers `GET`, so a documented `GET` covers `HEAD` too.
    fn documented(item: &PathItem, method: &Method) -> bool {
        operation(item, method).is_some()
            || (*method == Method::HEAD && operation(item, &Method::GET).is_some())
    }

    /// A router with one knowledge base holding one object, so a read finds
    /// something and a `404` can only be the router's own.
    async fn router() -> axum::Router {
        let kb = KbSlug::try_new("notes").expect("slug");
        let storage = InMemoryStorage::with_kbs([&kb]);
        storage
            .put_object(
                &kb,
                &ObjectPath::try_from_str("a.md").expect("path"),
                Bytes::from_static(b"# a\n"),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .expect("seed");
        let kbs = BTreeMap::from([("notes".to_string(), kb)]);
        build_router(AppState {
            authenticator: Arc::new(Authenticator::new(TOKEN)),
            max_body_size: 1024,
            max_patchable_size: 1024,
            ..AppState::for_tests(Arc::new(storage), kbs)
        })
    }

    /// The URL a documented path template names, with the test's slug and key.
    fn url(template: &str) -> String {
        let path = template
            .replace("{kb_slug}", "notes")
            .replace("{object_path}", "a.md");
        format!("{API_V1_PREFIX}{path}")
    }

    /// The status, and whether a body came with it.
    async fn call(method: &Method, url: &str) -> (StatusCode, bool) {
        let response = router()
            .await
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri(url)
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible");
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, !body.is_empty())
    }

    #[test]
    fn every_api_route_is_documented() {
        let document = document();
        for route in API_ROUTES {
            let path = route.replace("{*object_path}", "{object_path}");
            assert!(
                document.paths.paths.contains_key(&path),
                "{API_V1_PREFIX}{route} is routed but not in the OpenAPI document"
            );
        }
        let openapi = OPENAPI_PATH
            .strip_prefix(API_V1_PREFIX)
            .expect("under the API");
        assert!(document.paths.paths.contains_key(openapi));
    }

    #[tokio::test]
    async fn every_documented_operation_is_routed_and_no_other() {
        for (template, item) in &document().paths.paths {
            let url = url(template);
            for method in &METHODS {
                let (status, has_body) = call(method, &url).await;
                if documented(item, method) {
                    // A handler's `404` carries the error envelope; the
                    // router's own has no body. A `HEAD` never has one, so
                    // for it only the `405` tells.
                    let router_miss =
                        status == StatusCode::NOT_FOUND && !has_body && *method != Method::HEAD;
                    assert!(
                        status != StatusCode::METHOD_NOT_ALLOWED && !router_miss,
                        "{method} {template} is documented but not routed ({status})"
                    );
                    continue;
                }
                // `replace/<path>` is a key on the object route, which answers
                // every method; POST is the only one the document gives it. On
                // the object path itself POST is that same dispatcher, and any
                // key but `replace/…` is its `404`.
                let object_route = template == REPLACE_PATH
                    || (template == OBJECT_PATH && *method == Method::POST);
                if object_route {
                    continue;
                }
                assert_eq!(
                    status,
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {template} is routed but not documented"
                );
            }
        }
    }

    /// The document's security is `auth_middleware`'s table: an operation is
    /// open to anonymous callers exactly when the middleware lets them reach
    /// its handler.
    #[test]
    fn optional_security_is_the_anonymous_table() {
        let document = document();
        let openapi = OPENAPI_PATH
            .strip_prefix(API_V1_PREFIX)
            .expect("under the API");
        for (template, item) in &document.paths.paths {
            let matched = if template == REPLACE_PATH {
                MATCHED_KB_OBJECT.to_string()
            } else {
                format!("{API_V1_PREFIX}{template}").replace("{object_path}", "{*object_path}")
            };
            for method in &METHODS {
                let Some(operation) = operation(item, method) else {
                    continue;
                };
                let requirements = operation.security.clone().unwrap_or_default();
                let requirements = serde_json::to_value(requirements).expect("security");
                if template == openapi {
                    assert_eq!(
                        requirements,
                        serde_json::json!([{}]),
                        "{template} is public"
                    );
                    continue;
                }
                let anonymous = requirements
                    .as_array()
                    .expect("a list")
                    .iter()
                    .any(|requirement| requirement == &serde_json::json!({}));
                let reachable = ANONYMOUS_REACHABLE
                    .iter()
                    .any(|(m, route)| *m == method && *route == matched);
                assert_eq!(
                    anonymous, reachable,
                    "{method} {template}: the document says anonymous={anonymous}, \
                     auth_middleware says {reachable}"
                );
            }
        }
        // And every anonymous entry is in the document, `HEAD` by way of `GET`.
        for (method, route) in ANONYMOUS_REACHABLE {
            let template = route
                .strip_prefix(API_V1_PREFIX)
                .expect("under the API")
                .replace("{*object_path}", "{object_path}");
            let item = document.paths.paths.get(&template);
            assert!(
                item.is_some_and(|item| documented(item, method)),
                "{method} {route} is open to anonymous callers but not documented"
            );
        }
    }
}

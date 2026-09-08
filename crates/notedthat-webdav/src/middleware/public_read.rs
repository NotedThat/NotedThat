use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use notedthat_core::{PublicReadCapability, extract_basic_from_header, verify_basic_credentials};
use tower_http::request_id::RequestId;

use crate::{filesystem::DavTarget, state::WebDavState};

use super::path_validation::{parse_uri_path, validate_read_uri_path};

const AUTH_GUIDANCE: &str = "valid credentials are required; anonymous access is available only \
for configured public-read capabilities, and invalid credentials are not treated as anonymous.";

#[derive(Clone, Copy)]
pub(crate) struct AnonymousAccess {
    pub(crate) content: bool,
    pub(crate) propfind: bool,
}

fn challenge(request_id: &str) -> Response {
    let mut response = (StatusCode::UNAUTHORIZED, AUTH_GUIDANCE).into_response();
    response.headers_mut().insert(
        "www-authenticate",
        HeaderValue::from_static("Basic realm=\"NotedThat\""),
    );
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(request_id).unwrap_or(HeaderValue::from_static("unknown")),
    );
    response
}

fn anonymous_access(state: &WebDavState, target: &DavTarget) -> AnonymousAccess {
    match target {
        DavTarget::Root => {
            let allows = |capability| {
                state.declared_kbs.values().any(|kb| {
                    state
                        .public_read_policies
                        .get(kb.as_str())
                        .is_some_and(|policy| policy.allows(capability))
                })
            };
            AnonymousAccess {
                content: false,
                propfind: allows(PublicReadCapability::Discover),
            }
        }
        DavTarget::KbRoot(kb) | DavTarget::Object(kb, _) => {
            let policy = state.public_read_policies.get(kb.as_str());
            AnonymousAccess {
                content: policy.is_some_and(|value| value.allows(PublicReadCapability::Content)),
                propfind: policy.is_some_and(|value| value.allows(PublicReadCapability::Browse)),
            }
        }
        DavTarget::NonDeclaredKb => AnonymousAccess {
            content: false,
            propfind: false,
        },
    }
}

fn is_internal_target(target: &DavTarget) -> bool {
    matches!(
        target,
        DavTarget::Object(_, path)
            if path.as_str() == ".notedthat" || path.as_str().starts_with(".notedthat/")
    )
}

fn extract_request_id(req: &Request) -> String {
    req.extensions()
        .get::<RequestId>()
        .and_then(|id| id.header_value().to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

/// Authenticate or authorize one `WebDAV` request against its parsed target.
///
/// Supplied credentials always take precedence over anonymous capabilities, so
/// malformed or incorrect Basic credentials cannot silently downgrade access.
pub async fn basic_auth_middleware(
    State(state): State<WebDavState>,
    mut req: Request,
    next: Next,
) -> Response {
    let request_id = extract_request_id(&req);

    let mut auth_headers = req.headers().get_all("authorization").iter();
    let auth_header = auth_headers.next();
    if auth_headers.next().is_some() {
        return challenge(&request_id);
    }
    if let Some(auth_header) = auth_header {
        let authorized = auth_header
            .to_str()
            .ok()
            .and_then(extract_basic_from_header)
            .is_some_and(|(username, password)| {
                verify_basic_credentials(
                    &username,
                    &password,
                    state.username.as_str(),
                    state.password.as_str(),
                )
            });
        return if authorized {
            next.run(req).await
        } else {
            challenge(&request_id)
        };
    }

    if !matches!(
        req.method().as_str(),
        "OPTIONS" | "PROPFIND" | "GET" | "HEAD"
    ) {
        return challenge(&request_id);
    }
    if validate_read_uri_path(req.uri().path(), &state.declared_kbs).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let target_path = req
        .uri()
        .path()
        .strip_suffix('/')
        .filter(|path| !path.is_empty())
        .unwrap_or(req.uri().path());
    let Ok(target) = parse_uri_path(target_path, &state.declared_kbs) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if is_internal_target(&target) {
        return challenge(&request_id);
    }

    let access = anonymous_access(&state, &target);
    let root_has_webdav_capability = matches!(target, DavTarget::Root)
        && state.declared_kbs.values().any(|kb| {
            state
                .public_read_policies
                .get(kb.as_str())
                .is_some_and(|policy| {
                    policy.allows(PublicReadCapability::Browse)
                        || policy.allows(PublicReadCapability::Content)
                })
        });
    let authorized = match req.method().as_str() {
        "OPTIONS" => access.content || access.propfind || root_has_webdav_capability,
        "PROPFIND" => access.propfind,
        "GET" | "HEAD" => access.content,
        _ => false,
    };
    if !authorized {
        return challenge(&request_id);
    }

    req.extensions_mut().insert(access);
    next.run(req).await
}

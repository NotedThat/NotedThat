//! What only `phase3_access_e2e` probes for: the seeded bodies, the `PROPFIND`
//! request, and the answers the access layer is asserted to give. Kept apart
//! from the shared harness so `s3_reconcile_docker_e2e`, which includes that
//! harness but probes none of this, has no unused code.

use reqwest::{Method, StatusCode};

use super::access_wire::WireResponse;

pub const PUBLIC_BODY: &str = "# Public\n\nphase-three-public-needle";
pub const PRIVATE_BODY: &str = "# Private\n\nphase-three-private-needle";
pub const INTERNAL_BODY: &str = "phase-three-internal-needle";
pub const PROPFIND: &str =
    r#"<?xml version="1.0"?><D:propfind xmlns:D="DAV:"><D:allprop/></D:propfind>"#;

/// An anonymous caller the access rules refuse, on a route that names a
/// knowledge base.
///
/// `404`, byte-identical to an undeclared slug, so the status cannot be used to
/// discover which knowledge bases a deployment declares. Distinct from
/// [`assert_http_401`], which is the authentication layer's answer: a credential
/// missing where one is unconditionally required, or one that did not verify.
pub fn assert_http_concealed_404(response: &WireResponse) {
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    let json = response.json();
    assert_eq!(json["error"], "not_found");
    assert!(
        json["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("not found: KB '")
                && message.ends_with("' not declared")),
        "a denial must carry the undeclared-slug message, got {:?}",
        json["message"]
    );
}

pub fn assert_http_401(response: &WireResponse) {
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    let json = response.json();
    assert_eq!(json["error"], "unauthorized");
    assert!(json["message"].as_str().is_some_and(|message| {
        message.contains("valid Bearer token") && message.contains("Authorization")
    }));
}

pub fn method(value: &str) -> Method {
    Method::from_bytes(value.as_bytes()).expect("fixture HTTP method")
}

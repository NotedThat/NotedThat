//! Refuse legacy SSE and unsupported HTTP methods with 405 status and exact JSON body.
//!
//! Routes:
//! - GET /mcp → 405 with refusal JSON
//! - DELETE /mcp → 405 with refusal JSON
//! - POST /sse → 405 with refusal JSON
//! - GET /sse and /sse/* → 405 with refusal JSON
//!
//! The exact JSON body is: `{"error":"transport_not_supported","message":"Legacy SSE transport is not supported. Use streamable HTTP at POST /mcp"}`

/// The exact JSON body for SSE refusal responses.
const SSE_REFUSAL_BODY: &str = r#"{"error":"transport_not_supported","message":"Legacy SSE transport is not supported. Use streamable HTTP at POST /mcp"}"#;

/// Check if a request should be refused (legacy SSE or unsupported method on /mcp).
///
/// Returns true if the request matches one of the refusal patterns:
/// - GET /mcp
/// - DELETE /mcp
/// - POST /sse
/// - GET /sse
/// - Any path starting with /sse/
pub fn should_refuse_request(method: &str, path: &str) -> bool {
    match (method, path) {
        ("GET" | "DELETE", "/mcp") | ("POST" | "GET", "/sse") => true,
        (_, p) if p.starts_with("/sse/") => true,
        _ => false,
    }
}

/// Get the refusal response body as bytes.
pub fn refusal_body() -> &'static [u8] {
    SSE_REFUSAL_BODY.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_mcp_should_be_refused() {
        assert!(should_refuse_request("GET", "/mcp"));
    }

    #[test]
    fn delete_mcp_should_be_refused() {
        assert!(should_refuse_request("DELETE", "/mcp"));
    }

    #[test]
    fn post_sse_should_be_refused() {
        assert!(should_refuse_request("POST", "/sse"));
    }

    #[test]
    fn get_sse_should_be_refused() {
        assert!(should_refuse_request("GET", "/sse"));
    }

    #[test]
    fn sse_nested_path_should_be_refused() {
        assert!(should_refuse_request("GET", "/sse/events"));
    }

    #[test]
    fn sse_deeply_nested_path_should_be_refused() {
        assert!(should_refuse_request("GET", "/sse/v1/events/stream"));
    }

    #[test]
    fn post_mcp_should_not_be_refused() {
        assert!(!should_refuse_request("POST", "/mcp"));
    }

    #[test]
    fn other_paths_should_not_be_refused() {
        assert!(!should_refuse_request("GET", "/other"));
        assert!(!should_refuse_request("POST", "/other"));
        assert!(!should_refuse_request("DELETE", "/other"));
    }

    #[test]
    fn refusal_body_is_exact_json() {
        let body_str = std::str::from_utf8(refusal_body()).expect("body must be valid UTF-8");
        let body_json: serde_json::Value =
            serde_json::from_str(body_str).expect("body must be valid JSON");

        assert_eq!(
            body_json.get("error").and_then(|v| v.as_str()),
            Some("transport_not_supported")
        );
        assert_eq!(
            body_json.get("message").and_then(|v| v.as_str()),
            Some("Legacy SSE transport is not supported. Use streamable HTTP at POST /mcp")
        );
    }

    #[test]
    fn refusal_body_has_no_extra_keys() {
        let body_str = std::str::from_utf8(refusal_body()).expect("body must be valid UTF-8");
        let body_json: serde_json::Value =
            serde_json::from_str(body_str).expect("body must be valid JSON");

        let keys: Vec<&str> = body_json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();

        assert_eq!(keys.len(), 2, "body should have exactly 2 keys");
        assert!(keys.contains(&"error"));
        assert!(keys.contains(&"message"));
    }

    #[test]
    fn refusal_body_matches_exact_string() {
        let expected = r#"{"error":"transport_not_supported","message":"Legacy SSE transport is not supported. Use streamable HTTP at POST /mcp"}"#;
        let actual = std::str::from_utf8(refusal_body()).expect("body must be valid UTF-8");
        assert_eq!(actual, expected, "refusal body must match exact string");
    }
}

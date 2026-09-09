//! The exact 405 body served when a legacy SSE or unsupported method reaches
//! the MCP surface.
//!
//! Which requests are refused is decided by the routes registered in
//! `notedthat-server`'s `run::mcp_http` (GET/DELETE `/mcp`, GET/POST `/sse`,
//! any method under `/sse/`), so that one router is the single source of truth.
//! This module only owns the response body.

/// The exact JSON body for SSE refusal responses.
const SSE_REFUSAL_BODY: &str = r#"{"error":"transport_not_supported","message":"Legacy SSE transport is not supported. Use streamable HTTP at POST /mcp"}"#;

/// Get the refusal response body as bytes.
pub fn refusal_body() -> &'static [u8] {
    SSE_REFUSAL_BODY.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

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

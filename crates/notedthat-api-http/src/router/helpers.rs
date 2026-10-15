//! Shared helpers used across router handlers: path parsing, size limits,
//! percent-encoding, KB lookup, and PATCH/replace precondition parsing.

use super::API_V1_PREFIX;
use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::Request;
use axum::http::HeaderValue;
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, Error as CoreError, EventSource, KbSlug, ObjectPath,
    parse_line_range_header,
};
use notedthat_write::PatchMode;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// The header the MCP server sets on the API calls it makes on a caller's
/// behalf, so a write's event can say `mcp` rather than `http`. Informational:
/// any client may send it, and nothing is granted or refused on its account.
pub const SOURCE_HEADER: &str = "x-notedthat-source";

/// Which surface a write is attributed to in the events it publishes.
pub(crate) fn event_source(req: &Request) -> EventSource {
    match req.headers().get(SOURCE_HEADER).map(HeaderValue::as_bytes) {
        Some(b"mcp") => EventSource::Mcp,
        _ => EventSource::Http,
    }
}

pub(super) const REPLACE_IF_MATCH_ERROR: &str =
    "If-Match is required for POST replace and must be a single strong ETag";
pub(super) const REPLACE_IF_MATCH_REQUIRED: &str = "If-Match is required for POST replace";

pub(crate) fn lookup_kb(state: &AppState, slug: &str) -> Result<KbSlug, ApiError> {
    state
        .declared_kbs
        .get(slug)
        .cloned()
        .ok_or_else(|| kb_not_found(slug))
}

/// The `404` for a knowledge base an anonymous caller may not know about.
///
/// One constructor, because [`crate::authz::KbAccess::denial`] answers a
/// concealed knowledge base with this exact error and the concealment holds
/// only while the two are byte-identical. A different `message` on either side
/// would restore the slug-existence oracle that the shared status code closes.
pub(crate) fn kb_not_found(slug: &str) -> ApiError {
    ApiError::Core(CoreError::NotFound {
        resource: format!("KB '{slug}' not declared"),
    })
}

pub(super) fn parse_path(raw: &str) -> Result<ObjectPath, ApiError> {
    ObjectPath::try_object_key(raw).map_err(ApiError::Core)
}

pub(super) fn body_limit_usize(max_body_size: u64) -> usize {
    usize::try_from(max_body_size.saturating_add(1)).unwrap_or(usize::MAX)
}

/// Absolute path of an object under the API mount, as returned in `Location`
/// and `Content-Location` headers.
///
/// One definition so the write, patch and replace paths cannot drift from each
/// other or from the route they name.
pub(in crate::router) fn object_location(kb_slug: &str, object_path: &str) -> String {
    format!(
        "{API_V1_PREFIX}/knowledgebases/{kb_slug}/{}",
        percent_encode_path(object_path)
    )
}

/// Bytes [`percent_encode_path`] encodes: everything except the RFC 3986 §2.3
/// unreserved characters (`A-Z a-z 0-9 - . _ ~`) and `/`.
///
/// `/` is kept because the result is a multi-segment path under a route's
/// wildcard: each `/` of an object key must stay a path separator in the URL,
/// not collapse the key into one `%2F`-joined segment. Non-ASCII bytes are
/// always encoded, whatever the set says.
const PATH_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

pub(in crate::router) fn percent_encode_path(path: &str) -> String {
    utf8_percent_encode(path, PATH_ENCODE_SET).to_string()
}

pub(super) fn replace_conditionals(req: &Request) -> Result<ConditionalHeaders, ApiError> {
    let if_match = req
        .headers()
        .get(axum::http::header::IF_MATCH)
        .and_then(|value| value.to_str().ok());
    // RFC 6585 §3: a missing precondition is 428; a present but unusable one is 400.
    if !req.headers().contains_key(axum::http::header::IF_MATCH) {
        return Err(ApiError::PreconditionRequired {
            message: REPLACE_IF_MATCH_REQUIRED.into(),
        });
    }
    if if_match.is_none()
        || if_match == Some("*")
        || if_match.is_some_and(|value| value.contains(','))
    {
        return Err(ApiError::Core(CoreError::InvalidInput {
            message: REPLACE_IF_MATCH_ERROR.into(),
        }));
    }

    Ok(ConditionalHeaders::from_header_map(req.headers()))
}

pub(super) fn patch_mode_from_headers(
    nt_patch_mode: Option<&str>,
    content_range_header: Option<&str>,
    body_bytes: Bytes,
) -> Result<PatchMode, CoreError> {
    match (nt_patch_mode, content_range_header) {
        (Some("append"), None) => Ok(PatchMode::Append { body: body_bytes }),
        (Some("append"), Some(_)) => Err(CoreError::InvalidInput {
            message: "NT-Patch-Mode: append is mutually exclusive with Content-Range".into(),
        }),
        (None, Some(content_range)) => parse_patch_content_range(content_range, body_bytes)
            .map_err(|message| CoreError::InvalidInput { message }),
        (None, None) => Err(CoreError::InvalidInput {
            message: "PATCH requires either Content-Range or NT-Patch-Mode: append".into(),
        }),
        (Some(mode), _) => Err(CoreError::InvalidInput {
            message: format!("Unknown NT-Patch-Mode value: {mode}; only 'append' is supported"),
        }),
    }
}

fn parse_patch_content_range(content_range: &str, body: Bytes) -> Result<PatchMode, String> {
    let (unit, range_part) = content_range
        .split_once(' ')
        .ok_or_else(|| format!("malformed Content-Range: {content_range}"))?;
    let (range_str, _total) = range_part
        .split_once('/')
        .ok_or_else(|| format!("malformed Content-Range: {content_range}"))?;

    match unit {
        "bytes" => {
            let (start, end) = range_str
                .split_once('-')
                .ok_or_else(|| format!("malformed Content-Range bytes range: {range_str}"))?;
            let first = start
                .parse::<u64>()
                .map_err(|_| format!("invalid byte range start: {start}"))?;
            let last = end
                .parse::<u64>()
                .map_err(|_| format!("invalid byte range end: {end}"))?;
            // Reversed is a syntax error, as on `Range: bytes=` and `lines`; bytes have no insert form.
            if last < first {
                return Err(format!(
                    "malformed Content-Range bytes range: {range_str}: last precedes first"
                ));
            }
            Ok(PatchMode::Bytes {
                range: ByteRange::FromStart { first, last },
                body,
            })
        }
        "lines" => {
            let line_range = parse_line_range_header(&format!("lines={range_str}"))
                .map_err(|e| format!("malformed Content-Range lines range: {range_str}: {e}"))?;
            Ok(PatchMode::Lines {
                range: line_range,
                body,
            })
        }
        other => Err(format!(
            "Content-Range unit must be 'bytes' or 'lines', got: {other}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::percent_encode_path;

    #[test]
    fn percent_encode_path_keeps_unreserved_and_slash() {
        assert_eq!(
            percent_encode_path("AZaz09-._~/docs/a.md"),
            "AZaz09-._~/docs/a.md"
        );
    }

    #[test]
    fn percent_encode_path_encodes_punctuation_and_space_as_uppercase_hex() {
        assert_eq!(
            percent_encode_path(" !\"#$%&'()*+,:;<=>?@[\\]^`{|}"),
            "%20%21%22%23%24%25%26%27%28%29%2A%2B%2C%3A%3B%3C%3D%3E%3F%40%5B%5C%5D%5E%60%7B%7C%7D"
        );
        assert_eq!(percent_encode_path("\u{0}\t\u{7f}"), "%00%09%7F");
    }

    #[test]
    fn percent_encode_path_encodes_every_byte_of_multibyte_utf8() {
        assert_eq!(
            percent_encode_path("caf\u{e9}/\u{1f600}"),
            "caf%C3%A9/%F0%9F%98%80"
        );
    }
}

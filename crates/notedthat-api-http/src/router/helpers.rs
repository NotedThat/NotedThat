//! Shared helpers used across router handlers: path parsing, size limits,
//! percent-encoding, KB lookup, and PATCH/replace precondition parsing.

use crate::error::ApiError;
use crate::state::AppState;
use axum::extract::Request;
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, Error as CoreError, KbSlug, ObjectPath, parse_line_range_header,
};
use notedthat_write::PatchMode;
use std::fmt::Write;

pub(super) const REPLACE_IF_MATCH_ERROR: &str =
    "If-Match is required for POST replace and must be a single strong ETag";

pub(crate) fn lookup_kb(state: &AppState, slug: &str) -> Result<KbSlug, ApiError> {
    state.declared_kbs.get(slug).cloned().ok_or_else(|| {
        ApiError::Core(CoreError::NotFound {
            resource: format!("KB '{slug}' not declared"),
        })
    })
}

pub(super) fn parse_path(raw: &str) -> Result<ObjectPath, ApiError> {
    ObjectPath::try_from_str(raw).map_err(ApiError::Core)
}

pub(super) fn body_limit_usize(max_body_size: u64) -> usize {
    usize::try_from(max_body_size.saturating_add(1)).unwrap_or(usize::MAX)
}

pub(super) fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for &byte in path.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                encoded.push(char::from(byte));
            }
            _ => {
                let _ = write!(&mut encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

pub(super) fn replace_conditionals(req: &Request) -> Result<ConditionalHeaders, ApiError> {
    let if_match = req
        .headers()
        .get(axum::http::header::IF_MATCH)
        .and_then(|value| value.to_str().ok());
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
            Ok(PatchMode::Bytes {
                range: ByteRange::FromStart { first, last },
                body,
            })
        }
        "lines" => {
            let line_range = parse_line_range_header(&format!("lines={range_str}"))
                .map_err(|_| format!("malformed Content-Range lines range: {range_str}"))?;
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

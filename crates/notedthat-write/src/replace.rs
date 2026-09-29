//! Shared exact-substring replacement primitive for object writes.

use bytes::{Bytes, BytesMut};
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, Storage};

use crate::commit::{CasRewrite, cas_rewrite};
use crate::sinks::WriteSinks;
use crate::{ReplaceOutcome, WriteError};

/// Request data for one optimistic replace operation.
pub struct ReplaceRequest<'a> {
    /// Knowledge base containing the object.
    pub kb: &'a KbSlug,
    /// Object path to replace within.
    pub path: &'a ObjectPath,
    /// Exact UTF-8 substring to search for in the object body.
    pub old_string: &'a str,
    /// Replacement UTF-8 string to splice into the object body.
    pub new_string: &'a str,
    /// Whether to replace every non-overlapping match instead of exactly one match.
    pub replace_all: bool,
    /// Caller-supplied conditional headers.
    pub caller_conditionals: ConditionalHeaders,
    /// Maximum replaceable object size in bytes.
    pub max_patchable_size: u64,
    /// Caller-supplied content type, if any.
    pub caller_content_type: Option<&'a str>,
}

/// Replace exact occurrences of a UTF-8 substring in an object body.
///
/// # Errors
/// Returns [`crate::WriteError`] when storage access, preconditions, size limits, or
/// match-count requirements fail.
pub async fn replace(
    storage: &dyn Storage,
    sinks: &WriteSinks<'_>,
    request: ReplaceRequest<'_>,
) -> Result<ReplaceOutcome, WriteError> {
    let ReplaceRequest {
        kb,
        path,
        old_string,
        new_string,
        replace_all,
        caller_conditionals,
        max_patchable_size,
        caller_content_type,
    } = request;

    if old_string.is_empty() {
        return Err(WriteError::PatchInvalidRange {
            message: "old_string must be non-empty (would match every byte position)".into(),
        });
    }
    crate::patch::require_strong_if_match(&caller_conditionals)?;

    let target = CasRewrite {
        kb,
        path,
        caller_if_match: caller_conditionals.if_match.as_deref(),
        max_size: max_patchable_size,
        caller_content_type,
    };
    let (put_outcome, match_count) = cas_rewrite(storage, sinks, target, |src| {
        replace_in(
            src,
            old_string.as_bytes(),
            new_string.as_bytes(),
            replace_all,
            max_patchable_size,
        )
    })
    .await?;
    Ok(ReplaceOutcome {
        put_outcome,
        match_count,
    })
}

/// Replace `needle` in `haystack` — once, or everywhere with `replace_all` — and return the new
/// bytes with the number of matches replaced.
fn replace_in(
    haystack: &[u8],
    needle: &[u8],
    replacement: &[u8],
    replace_all: bool,
    max_patchable_size: u64,
) -> Result<(Bytes, u64), WriteError> {
    let matches = find_non_overlapping_matches(haystack, needle);
    let count = u64::try_from(matches.len()).map_err(|_| WriteError::PatchInvalidRange {
        message: "replace: match count exceeds u64".into(),
    })?;
    match count {
        0 => return Err(WriteError::ReplaceNoMatch),
        2.. if !replace_all => return Err(WriteError::ReplaceAmbiguous { count }),
        _ => {}
    }

    let match_bound = if replace_all { matches.len() } else { 1 };
    let new_bytes = splice_replacement(&ReplacementSplice {
        haystack,
        needle,
        replacement,
        matches: &matches,
        match_bound,
        max_patchable_size,
    })?;
    let match_count = if replace_all { count } else { 1 };
    Ok((new_bytes, match_count))
}

fn find_non_overlapping_matches(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut matches = Vec::new();
    let mut cursor = 0usize;
    while cursor + needle.len() <= haystack.len() {
        if &haystack[cursor..cursor + needle.len()] == needle {
            matches.push(cursor);
            cursor += needle.len();
        } else {
            cursor += 1;
        }
    }
    matches
}

struct ReplacementSplice<'a> {
    haystack: &'a [u8],
    needle: &'a [u8],
    replacement: &'a [u8],
    matches: &'a [usize],
    match_bound: usize,
    max_patchable_size: u64,
}

fn splice_replacement(splice: &ReplacementSplice<'_>) -> Result<Bytes, WriteError> {
    let replaced_bytes = splice.match_bound * splice.needle.len();
    let added_bytes = splice.match_bound * splice.replacement.len();
    let new_len = splice
        .haystack
        .len()
        .checked_sub(replaced_bytes)
        .and_then(|n| n.checked_add(added_bytes))
        .ok_or_else(|| WriteError::PatchInvalidRange {
            message: "replace: length arithmetic overflow".into(),
        })?;
    let new_len_u64 = u64::try_from(new_len).map_err(|_| WriteError::PatchInvalidRange {
        message: "replace: new_len exceeds u64".into(),
    })?;
    if new_len_u64 > splice.max_patchable_size {
        return Err(WriteError::PatchTooLarge {
            size: new_len_u64,
            limit: splice.max_patchable_size,
        });
    }

    let mut result = BytesMut::with_capacity(new_len);
    let mut prev_end = 0usize;
    for &matched_at in splice.matches.iter().take(splice.match_bound) {
        result.extend_from_slice(&splice.haystack[prev_end..matched_at]);
        result.extend_from_slice(splice.replacement);
        prev_end = matched_at + splice.needle.len();
    }
    result.extend_from_slice(&splice.haystack[prev_end..]);
    Ok(result.freeze())
}

#[cfg(test)]
mod tests;

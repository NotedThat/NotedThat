//! MIME type selection for write operations.

use notedthat_core::{ObjectPath, StagedBody};
use tokio::io::AsyncReadExt as _;

/// Whether `content_type` is text a charset applies to: `text/*`, JSON and XML,
/// including the `+json` and `+xml` suffixes.
fn is_textual(content_type: &mime::Mime) -> bool {
    let subtype = content_type.subtype().as_str();
    let suffix = content_type.suffix().map(|suffix| suffix.as_str());
    content_type.type_() == mime::TEXT
        || (content_type.type_() == mime::APPLICATION
            && (matches!(subtype, "json" | "xml") || matches!(suffix, Some("json" | "xml"))))
}

/// `content_type` with its charset decided by the bytes it labels (#483): a
/// textual type without a charset gains `charset=utf-8` when `utf8` holds, and
/// loses a `charset=utf-8` it carries when it does not — an append can make a
/// UTF-8 note stop being one. Any other charset, and every non-textual type, is
/// left exactly as given.
///
/// The charset is read and removed on the original string, so every other
/// parameter keeps its bytes (quotes included).
pub(crate) fn label_charset(content_type: &str, utf8: bool) -> String {
    if !content_type
        .parse::<mime::Mime>()
        .is_ok_and(|parsed| is_textual(&parsed))
    {
        return content_type.to_string();
    }
    let segments = split_params(content_type);
    match segments.iter().skip(1).find_map(|segment| charset(segment)) {
        None if utf8 => format!("{content_type}; charset=utf-8"),
        Some(value) if value.eq_ignore_ascii_case("utf-8") && !utf8 => segments
            .iter()
            .enumerate()
            .filter(|(index, segment)| *index == 0 || charset(segment).is_none())
            .map(|(_, segment)| *segment)
            .collect::<Vec<_>>()
            .join(";"),
        _ => content_type.to_string(),
    }
}

/// The value of `segment` when it is the charset parameter, unquoted.
fn charset(segment: &str) -> Option<&str> {
    segment
        .split_once('=')
        .filter(|(name, _)| name.trim().eq_ignore_ascii_case("charset"))
        .map(|(_, value)| value.trim().trim_matches('"'))
}

/// `content_type` split at each `;` outside a quoted string, segments untrimmed.
/// A quoted string ends at the next `"`, backslash or not, as the mime crate
/// that gates [`label_charset`] parses it.
fn split_params(content_type: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (index, byte) in content_type.bytes().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b';' if !quoted => {
                segments.push(&content_type[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    segments.push(&content_type[start..]);
    segments
}

/// The stored type for `body` written as `content_type`: [`label_charset`],
/// reading the body only when the type is textual.
pub(crate) async fn label_staged_charset(
    content_type: String,
    body: &StagedBody,
) -> Result<String, std::io::Error> {
    if !content_type
        .parse::<mime::Mime>()
        .is_ok_and(|parsed| is_textual(&parsed))
    {
        return Ok(content_type);
    }
    Ok(label_charset(&content_type, is_utf8(body).await?))
}

/// Whether `body` is valid UTF-8, read in chunks so a body staged to disk is
/// never loaded whole. A character split across two chunks is carried over.
async fn is_utf8(body: &StagedBody) -> Result<bool, std::io::Error> {
    const CHUNK: usize = 64 * 1024;
    let mut reader = body.open().await?;
    let mut buffer = vec![0; CHUNK];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            // A character still incomplete at the end is not one.
            return Ok(pending.is_empty());
        }
        pending.extend_from_slice(&buffer[..read]);
        match std::str::from_utf8(&pending) {
            Ok(_) => pending.clear(),
            // Incomplete at the end of the chunk: keep the tail for the next one.
            Err(error) if error.error_len().is_none() => {
                pending.drain(..error.valid_up_to());
            }
            Err(_) => return Ok(false),
        }
    }
}

/// Select a content type for an object write.
///
/// An explicit caller-provided content type wins unless it is
/// `application/octet-stream`, in which case the type is inferred from the key's
/// extension: `md` and `markdown` give `text/markdown`, `txt` gives `text/plain`,
/// and anything else stays `application/octet-stream`.
pub fn sniff_content_type(caller: Option<&str>, path: &ObjectPath) -> String {
    match caller {
        Some(ct) if ct != "application/octet-stream" => ct.to_string(),
        _ => {
            let ext = path
                .as_str()
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            match ext.as_str() {
                "md" | "markdown" => "text/markdown".to_string(),
                "txt" => "text/plain".to_string(),
                _ => "application/octet-stream".to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_textual_type_is_labelled_utf_8_only_for_utf_8_bytes() {
        assert_eq!(label_charset("text/csv", true), "text/csv; charset=utf-8");
        assert_eq!(label_charset("text/csv", false), "text/csv");
        assert_eq!(
            label_charset("application/ld+json", true),
            "application/ld+json; charset=utf-8"
        );
        assert_eq!(label_charset("image/svg+xml", false), "image/svg+xml");
        // Non-textual types are never labelled.
        assert_eq!(label_charset("image/png", true), "image/png");
        assert_eq!(
            label_charset("application/octet-stream", true),
            "application/octet-stream"
        );
    }

    #[test]
    fn a_stated_charset_is_kept_unless_it_claims_utf_8_for_bytes_that_are_not() {
        assert_eq!(
            label_charset("text/plain; charset=iso-8859-1", false),
            "text/plain; charset=iso-8859-1"
        );
        assert_eq!(
            label_charset("text/plain; charset=utf-8", true),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            label_charset("text/plain; charset=UTF-8", false),
            "text/plain"
        );
        assert_eq!(
            label_charset("text/markdown; charset=utf-8; variant=GFM", false),
            "text/markdown; variant=GFM"
        );
        // Other parameters keep their bytes, quotes and order.
        assert_eq!(
            label_charset("text/plain; charset=utf-8; title=\"My notes\"", false),
            "text/plain; title=\"My notes\""
        );
        assert_eq!(
            label_charset("text/plain; a=1; charset=utf-8; b=\"x;y\"", false),
            "text/plain; a=1; b=\"x;y\""
        );
        // A quoted charset followed by another parameter.
        assert_eq!(
            label_charset("text/plain; charset=\"utf-8\"; x=y", false),
            "text/plain; x=y"
        );
        assert_eq!(
            label_charset("text/plain; charset=\"utf-8\"; x=y", true),
            "text/plain; charset=\"utf-8\"; x=y"
        );
        // A backslash does not escape the closing quote, as in the mime crate.
        assert_eq!(
            label_charset("text/plain; x=\"a\\\"; charset=utf-8", false),
            "text/plain; x=\"a\\\""
        );
        assert_eq!(
            label_charset("text/plain; x=\"a\\\"; charset=utf-8", true),
            "text/plain; x=\"a\\\"; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn utf_8_is_checked_across_chunk_boundaries_and_on_disk() {
        // "é" is two bytes; put its first byte last in the first 64 KiB chunk.
        let mut split = vec![b'a'; 64 * 1024 - 1];
        split.extend_from_slice("é and more".as_bytes());
        let staged = |bytes: Vec<u8>| StagedBody::from_bytes(bytes::Bytes::from(bytes));
        assert!(is_utf8(&staged(split.clone())).await.unwrap());
        assert!(!is_utf8(&staged(b"caf\xe9".to_vec())).await.unwrap());
        // A character cut off at the very end is not valid.
        assert!(!is_utf8(&staged(b"caf\xc3".to_vec())).await.unwrap());
        assert!(is_utf8(&staged(Vec::new())).await.unwrap());

        let on_disk = StagedBody::stage_stream_to_file(
            futures::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from(split))]),
            None,
            u64::MAX,
            &notedthat_core::StagingConfig::default(),
        )
        .await
        .unwrap();
        assert!(on_disk.file_path().is_some(), "staged to disk");
        assert!(is_utf8(&on_disk).await.unwrap());
    }

    fn path(value: &str) -> ObjectPath {
        ObjectPath::try_from_str(value).expect("valid path")
    }

    #[test]
    fn caller_explicit_wins() {
        assert_eq!(
            sniff_content_type(Some("text/plain"), &path("file.md")),
            "text/plain"
        );
    }

    #[test]
    fn caller_octet_stream_overridden_for_md() {
        assert_eq!(
            sniff_content_type(Some("application/octet-stream"), &path("file.md")),
            "text/markdown"
        );
    }

    #[test]
    fn caller_none_inferred_from_md() {
        assert_eq!(
            sniff_content_type(None, &path("notes/test.md")),
            "text/markdown"
        );
    }

    #[test]
    fn caller_none_inferred_from_markdown() {
        assert_eq!(
            sniff_content_type(None, &path("test.markdown")),
            "text/markdown"
        );
    }

    /// A `.txt` sent with no type, or the generic one, is plain text: as
    /// `application/octet-stream` the indexer would drop it from search (#345).
    #[test]
    fn a_txt_without_a_type_is_plain_text() {
        assert_eq!(
            sniff_content_type(None, &path("notes/todo.txt")),
            "text/plain"
        );
        assert_eq!(
            sniff_content_type(Some("application/octet-stream"), &path("notes/TODO.TXT")),
            "text/plain"
        );
    }

    #[test]
    fn caller_none_unknown_ext_falls_back_to_octet_stream() {
        assert_eq!(
            sniff_content_type(None, &path("test.rs")),
            "application/octet-stream"
        );
    }

    #[test]
    fn caller_none_no_extension() {
        assert_eq!(
            sniff_content_type(None, &path("README")),
            "application/octet-stream"
        );
    }

    // Regression: extension match must be case-insensitive, mirroring
    // is_indexable() in notedthat-indexer::worker — otherwise `.MD` files
    // sniff as octet-stream and are silently dropped from the index.
    #[test]
    fn caller_none_uppercase_md_inferred_as_markdown() {
        assert_eq!(
            sniff_content_type(None, &path("README.MD")),
            "text/markdown"
        );
    }

    #[test]
    fn caller_none_uppercase_markdown_inferred_as_markdown() {
        assert_eq!(
            sniff_content_type(None, &path("notes/DRAFT.MARKDOWN")),
            "text/markdown"
        );
    }

    #[test]
    fn caller_none_mixed_case_md_inferred_as_markdown() {
        assert_eq!(
            sniff_content_type(None, &path("notes/Notes.Md")),
            "text/markdown"
        );
    }

    #[test]
    fn caller_octet_stream_uppercase_md_overridden_for_markdown() {
        assert_eq!(
            sniff_content_type(Some("application/octet-stream"), &path("file.MD")),
            "text/markdown"
        );
    }
}

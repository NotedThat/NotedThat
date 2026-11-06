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
pub(crate) fn label_charset(content_type: &str, utf8: bool) -> String {
    let Ok(parsed) = content_type.parse::<mime::Mime>() else {
        return content_type.to_string();
    };
    if !is_textual(&parsed) {
        return content_type.to_string();
    }
    match parsed.get_param(mime::CHARSET) {
        None if utf8 => format!("{content_type}; charset=utf-8"),
        Some(charset) if charset == mime::UTF_8 && !utf8 => parsed
            .params()
            .filter(|(name, _)| *name != mime::CHARSET)
            .fold(parsed.essence_str().to_string(), |label, (name, value)| {
                format!("{label}; {name}={value}")
            }),
        _ => content_type.to_string(),
    }
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
/// `application/octet-stream`, in which case markdown extensions are inferred.
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

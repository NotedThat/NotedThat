//! MIME type selection for write operations.

use notedthat_core::ObjectPath;

/// Select a content type for an object write.
///
/// An explicit caller-provided content type wins unless it is
/// `application/octet-stream`, in which case markdown extensions are inferred.
pub fn sniff_content_type(caller: Option<&str>, path: &ObjectPath) -> String {
    match caller {
        Some(ct) if ct != "application/octet-stream" => ct.to_string(),
        _ => {
            let ext = path.as_str().rsplit('.').next().unwrap_or("");
            match ext {
                "md" | "markdown" => "text/markdown".to_string(),
                _ => "application/octet-stream".to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

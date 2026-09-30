//! Media-type normalisation shared by indexing and search.

/// The media type of a `Content-Type` value: parameters dropped, trimmed,
/// ASCII-lowercased (`"Text/Markdown; charset=utf-8"` → `"text/markdown"`).
///
/// Indexability, the stored `mime` payload and the `mime` search filter all go
/// through this, so an object is found by the same type it was indexed as —
/// whatever parameters or casing the client that wrote it happened to send.
pub(crate) fn essence(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::essence;

    #[test]
    fn drops_parameters() {
        assert_eq!(essence("text/markdown; charset=utf-8"), "text/markdown");
        assert_eq!(essence("text/plain;charset=UTF-8"), "text/plain");
    }

    #[test]
    fn trims_and_lowercases() {
        assert_eq!(essence("  Text/Markdown  ; q=1"), "text/markdown");
    }

    #[test]
    fn empty_stays_empty() {
        assert_eq!(essence(""), "");
    }
}

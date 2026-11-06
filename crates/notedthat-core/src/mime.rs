//! The one table that names a content type from an object key's extension (#344).

/// The content type an object key's extension names, or `None` when it names none.
///
/// Case-insensitive, and only the last path segment counts, so a dot in a folder
/// name is not an extension. Each caller decides what an unknown extension means:
/// the write path stores `application/octet-stream`, the indexer reads only the
/// text types, and so on.
#[must_use]
pub fn from_key(key: &str) -> Option<&'static str> {
    let name = key.rsplit('/').next().unwrap_or(key);
    let (_, extension) = name.rsplit_once('.')?;
    match extension.to_ascii_lowercase().as_str() {
        "md" | "markdown" => Some("text/markdown"),
        "txt" => Some("text/plain"),
        "json" => Some("application/json"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::from_key;

    #[test]
    fn every_extension_names_its_type_in_any_case() {
        for (key, expected) in [
            ("a.md", Some("text/markdown")),
            ("notes/Notes.MD", Some("text/markdown")),
            ("Draft.Markdown", Some("text/markdown")),
            ("todo.txt", Some("text/plain")),
            ("TODO.TXT", Some("text/plain")),
            ("data.json", Some("application/json")),
            ("img/a.png", Some("image/png")),
            ("img/a.JPG", Some("image/jpeg")),
            ("img/a.jpeg", Some("image/jpeg")),
            ("archive.tar.gz", None),
            ("README", None),
            ("notes.v2/README", None),
            ("", None),
        ] {
            assert_eq!(from_key(key), expected, "{key}");
        }
    }
}

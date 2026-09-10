//! Building the two kinds of URL a browse page emits, and the argument that
//! neither can leave the knowledge base it belongs to.
//!
//! # Why a generated link cannot escape
//!
//! 1. Both builders start with a literal `/` and a literal segment, so no
//!    dynamic value can reach the first character. There is no `//host`
//!    authority injection and no scheme injection.
//! 2. Every dynamic part goes through [`percent_encode_path`], which encodes
//!    every byte outside `A-Za-z0-9-._~/` as `%XX`. `?`, `#`, `\`, whitespace
//!    and all non-ASCII bytes are neutralised — none can become a delimiter.
//! 3. `kb_slug` is a [`notedthat_core::KbSlug`] (`[a-z0-9-]{1,40}`, D24), so it
//!    contributes no separator and no dot segment.
//! 4. A folder name is a rollup segment — by construction the text before the
//!    first `/` of a relative key — so it cannot add a path level.
//! 5. A prefix has already been through `ObjectPath::try_from_str`, which
//!    rejects `.` and `..` segments.
//! 6. **`percent_encode_path` leaves `.` unencoded**, so a *stored* key
//!    containing a `..` segment would survive into an href and a browser would
//!    resolve `/api/v1/knowledgebases/notes/a/../../docs/x` into a different
//!    knowledge base. Our write path rejects such keys, but S3 accepts them and
//!    a bucket can be filled out of band — so [`super::listing`] drops any key
//!    that fails `ObjectPath::try_from_str` before it can reach a link. That
//!    filter is load-bearing, not defence in depth.

use super::super::BROWSE_PREFIX;
use super::super::helpers::{object_location, percent_encode_path};

/// The browse URL for a directory: `/browse/{kb}/{prefix}`.
///
/// `prefix` is empty or ends with `/`; the result always ends with `/`, which is
/// the canonical form for a directory page.
pub(super) fn directory_href(kb_slug: &str, prefix: &str) -> String {
    format!(
        "{BROWSE_PREFIX}/{}/{}",
        percent_encode_path(kb_slug),
        without_dot_segments(&percent_encode_path(prefix))
    )
}

/// Percent-encode any `.` or `..` segment so a browser cannot resolve it away.
///
/// [`percent_encode_path`] passes `.` through, which is right for the machine
/// API's `Location` headers and wrong for an href a browser will resolve. Keys
/// containing dot segments are already dropped before they reach a link, so this
/// should never fire — but the guarantee belongs where the URL is built, not two
/// modules away in whichever caller happens to be filtering today.
fn without_dot_segments(encoded_path: &str) -> String {
    encoded_path
        .split('/')
        .map(|segment| match segment {
            "." => "%2E",
            ".." => "%2E%2E",
            other => other,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The URL of an object's existing representation, on the machine API.
///
/// Reusing [`object_location`] verbatim is what keeps the acceptance criterion
/// honest: the browse page hands the client the API's own URL rather than
/// growing a second download path.
pub(super) fn object_href(kb_slug: &str, key: &str) -> String {
    without_dot_segments(&object_location(kb_slug, key))
}

/// The browse URL of the index of knowledge bases.
pub(super) fn root_href() -> String {
    format!("{BROWSE_PREFIX}/")
}

/// The directory one level above `prefix`, or the index at a knowledge-base root.
pub(super) fn parent_href(kb_slug: &str, prefix: &str) -> String {
    let trimmed = prefix.strip_suffix('/').unwrap_or(prefix);
    match trimmed.rsplit_once('/') {
        Some((parent, _)) => directory_href(kb_slug, &format!("{parent}/")),
        None if trimmed.is_empty() => root_href(),
        None => directory_href(kb_slug, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever a name contains, the href stays inside its knowledge base.
    ///
    /// These are rollup *segments*, which by construction contain no `/`. A
    /// stored key that does contain a `..` segment is dropped in
    /// [`super::super::listing`] before it can reach a link — see the module
    /// docs for why that filter is load-bearing rather than belt-and-braces.
    #[test]
    fn no_generated_link_leaves_the_selected_knowledge_base() {
        // Given — names chosen to break out if anything were left unencoded.
        let hostile = [
            "..",
            ".",
            "a b",
            "a#b",
            "a?b",
            "a%2Fb",
            "\\windows",
            "naïve",
            "a\"b",
            "..evil",
        ];

        // When / Then
        for name in hostile {
            let folder = directory_href("notes", &format!("{name}/"));
            let object = object_href("notes", name);
            for href in [&folder, &object] {
                assert!(
                    href.starts_with("/browse/notes/")
                        || href.starts_with("/api/v1/knowledgebases/notes/"),
                    "`{name}` produced `{href}`"
                );
                assert!(
                    !href.contains("/../") && !href.ends_with("/.."),
                    "`{name}` produced a dot-segment href: `{href}`"
                );
                assert!(!href.contains("/./"), "`{name}` -> `{href}`");
                assert!(
                    !href.contains('#') && !href.contains('?'),
                    "`{name}` -> `{href}`"
                );
                assert!(!href.contains(' '), "`{name}` -> `{href}`");
            }
        }
    }

    #[test]
    fn a_directory_href_is_always_the_canonical_trailing_slash_form() {
        assert_eq!(directory_href("notes", ""), "/browse/notes/");
        assert_eq!(directory_href("notes", "docs/"), "/browse/notes/docs/");
        assert_eq!(
            directory_href("notes", "docs/deep/"),
            "/browse/notes/docs/deep/"
        );
    }

    #[test]
    fn an_object_href_matches_the_machine_api_url_for_the_same_key() {
        // Given / When / Then — one definition, so the browse page and the API
        // cannot drift apart about where an object lives.
        assert_eq!(
            object_href("notes", "docs/a b.md"),
            crate::router::helpers::object_location("notes", "docs/a b.md")
        );
        // They diverge only for a key that cannot exist, which is the point.
        assert_eq!(
            object_href("notes", "a/../b"),
            "/api/v1/knowledgebases/notes/a/%2E%2E/b"
        );
    }

    #[test]
    fn the_parent_of_a_knowledge_base_root_is_the_index() {
        assert_eq!(parent_href("notes", ""), "/browse/");
    }

    #[test]
    fn the_parent_of_a_nested_directory_is_one_level_up() {
        assert_eq!(parent_href("notes", "docs/"), "/browse/notes/");
        assert_eq!(parent_href("notes", "docs/deep/"), "/browse/notes/docs/");
    }
}

//! Synthesising one directory level from a flat object listing.
//!
//! Storage is flat: [`crate::Storage::list_objects`] takes a prefix and returns
//! keys, with no delimiter and no common-prefix rollup, and only object bytes
//! are stored — directories are virtual prefixes (D40). Every surface that shows
//! a caller something directory-shaped therefore has to synthesise that shape
//! itself, from the keys.
//!
//! This is the shared half of that work. What a surface does with a folder and
//! an object of the same name is a presentation choice and stays with the
//! surface: `WebDAV` must let the folder win, because a filesystem name is
//! either a file or a directory and the protocol cannot say both.

use crate::kb::ObjectMeta;
use std::collections::{BTreeMap, BTreeSet};

/// One directory level synthesised from a flat key listing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rollup {
    /// Names of synthesised subdirectories, lexicographic, without a trailing `/`.
    pub folders: BTreeSet<String>,
    /// Objects at this level, keyed by their name relative to the prefix.
    pub files: BTreeMap<String, ObjectMeta>,
}

impl Rollup {
    /// Returns whether this level has neither subdirectories nor objects.
    pub fn is_empty(&self) -> bool {
        self.folders.is_empty() && self.files.is_empty()
    }
}

/// Roll a flat listing up into the single directory level directly under `prefix`.
///
/// `prefix` is either empty (the knowledge-base root) or ends with `/`. Keys that
/// do not start with `prefix` are skipped, as is a key equal to `prefix` itself —
/// an object stored at a directory's own name is a folder marker, not a child.
///
/// A key with one remaining segment becomes a file; a key with more contributes
/// its first segment as a folder. Both collections are ordered, so a caller gets
/// a stable listing without sorting.
pub fn roll_up(objects: impl IntoIterator<Item = ObjectMeta>, prefix: &str) -> Rollup {
    let mut rollup = Rollup::default();

    for meta in objects {
        let Some(relative) = meta.key.strip_prefix(prefix) else {
            continue;
        };
        if relative.is_empty() {
            continue;
        }

        if let Some((folder, _)) = relative.split_once('/') {
            rollup.folders.insert(folder.to_string());
        } else {
            rollup.files.insert(relative.to_string(), meta);
        }
    }

    rollup
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(key: &str) -> ObjectMeta {
        ObjectMeta {
            key: key.to_string(),
            size: 1,
            last_modified: None,
            content_type: None,
            etag: None,
        }
    }

    #[test]
    fn a_root_listing_splits_into_folders_and_files() {
        // Given / When
        let rollup = roll_up(
            ["a.md", "docs/b.md", "docs/deep/c.md", "z.md"].map(meta),
            "",
        );

        // Then
        assert_eq!(
            rollup
                .folders
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["docs"],
            "a folder is contributed once however many keys sit under it"
        );
        assert_eq!(
            rollup.files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["a.md", "z.md"]
        );
    }

    #[test]
    fn a_nested_listing_shows_only_its_own_level() {
        // Given / When
        let rollup = roll_up(
            [
                "docs/b.md",
                "docs/deep/c.md",
                "docs/deep/deeper/d.md",
                "a.md",
            ]
            .map(meta),
            "docs/",
        );

        // Then
        assert_eq!(
            rollup
                .folders
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["deep"]
        );
        assert_eq!(
            rollup.files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["b.md"],
            "keys outside the prefix must not leak into the level"
        );
    }

    #[test]
    fn an_object_stored_at_the_prefix_itself_is_not_a_child_of_it() {
        // Given — a folder-marker object, which some clients create.
        let rollup = roll_up(["docs", "docs/b.md"].map(meta), "docs/");

        // Then
        assert!(rollup.folders.is_empty());
        assert_eq!(
            rollup.files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["b.md"]
        );
    }

    #[test]
    fn a_folder_and_an_object_of_the_same_name_are_both_reported() {
        // Given / When — resolving this collision is the caller's choice, so
        // both survive the rollup and neither is silently dropped here.
        let rollup = roll_up(["collide", "collide/inner.md"].map(meta), "");

        // Then
        assert!(rollup.folders.contains("collide"));
        assert!(rollup.files.contains_key("collide"));
    }

    #[test]
    fn an_empty_listing_rolls_up_to_nothing() {
        // Given / When / Then
        assert!(roll_up(Vec::new(), "").is_empty());
        assert!(roll_up(["other/a.md"].map(meta), "docs/").is_empty());
    }
}

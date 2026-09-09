//! Walking a bucket directory in object-key order, and the pagination cursor.
//!
//! # The ordering rule
//!
//! `list_objects` must return keys in byte-lexicographic order, and directory traversal
//! order is not that. Sorting each directory's entries by their *names* is not it either.
//! Take the keys `a-b` and `a/c`: `-` is `0x2D` and `/` is `0x2F`, so `a-b` sorts first —
//! but comparing the names `a` and `a-b` visits directory `a` first and emits `a/c`
//! before `a-b`.
//!
//! The fix is to sort each directory's entries by the name with `/` appended for
//! directories, so the comparison is against `a/` rather than `a`. A depth-first walk
//! over per-directory sorts built that way yields exactly key order.

use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use notedthat_core::StorageError;
use serde::{Deserialize, Serialize};

use crate::layout::TEMP_PREFIX;

/// One file found by the walk.
pub(crate) struct Found {
    pub(crate) key: String,
    pub(crate) size: u64,
    pub(crate) last_modified: Option<i64>,
}

/// A pagination cursor.
///
/// Opaque and self-describing rather than a bare key. `InMemoryStorage` historically used
/// the last key and rejected a cursor whose key had since been deleted — but `WebDAV`
/// pages through a whole knowledge base in a loop, so a concurrent delete of the cursor
/// key would break an in-flight listing. Resuming from "the first key after this string"
/// needs no such key to exist, matching how an S3 continuation token behaves. Carrying
/// the prefix additionally catches a client reusing a cursor under a different one.
#[derive(Serialize, Deserialize)]
struct Cursor {
    v: u8,
    after: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
}

pub(crate) fn encode_cursor(after: &str, prefix: Option<&str>) -> String {
    let cursor = Cursor {
        v: 1,
        after: after.to_string(),
        prefix: prefix.map(str::to_string),
    };
    let json = serde_json::to_vec(&cursor).unwrap_or_default();
    URL_SAFE_NO_PAD.encode(json)
}

pub(crate) fn decode_cursor(token: &str, prefix: Option<&str>) -> Result<String, StorageError> {
    let invalid = || StorageError::BackendUnavailable {
        message: "invalid or expired cursor".to_string(),
    };

    let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid())?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if cursor.v != 1 || cursor.prefix.as_deref() != prefix {
        return Err(invalid());
    }
    Ok(cursor.after)
}

/// Depth-first walk of a bucket directory, yielding keys in byte-lexicographic order.
pub(crate) struct OrderedWalk {
    bucket_dir: PathBuf,
    /// Pending entries, deepest level last; each level is held in reverse sorted order so
    /// that popping yields ascending order.
    stack: Vec<Vec<Entry>>,
}

struct Entry {
    path: PathBuf,
    key: String,
    is_dir: bool,
}

impl OrderedWalk {
    pub(crate) fn new(bucket_dir: PathBuf) -> Self {
        let level = read_level(&bucket_dir, &bucket_dir);
        Self {
            bucket_dir,
            stack: vec![level],
        }
    }

    /// Advance to the next file whose key is greater than `after` and starts with
    /// `prefix`, skipping subtrees that cannot contain a match.
    pub(crate) fn next_match(
        &mut self,
        after: Option<&str>,
        prefix: Option<&str>,
    ) -> Option<Found> {
        loop {
            let level = self.stack.last_mut()?;
            let Some(entry) = level.pop() else {
                self.stack.pop();
                continue;
            };

            if entry.is_dir {
                // A directory can only hold keys that extend its own key with `/…`, so
                // it is worth descending only if some such key could still match.
                if subtree_may_match(&entry.key, after, prefix) {
                    let level = read_level(&self.bucket_dir, &entry.path);
                    self.stack.push(level);
                }
                continue;
            }

            if after.is_some_and(|after| entry.key.as_str() <= after) {
                continue;
            }
            if prefix.is_some_and(|prefix| !entry.key.starts_with(prefix)) {
                continue;
            }

            let metadata = std::fs::symlink_metadata(&entry.path).ok()?;
            return Some(Found {
                key: entry.key,
                size: metadata.len(),
                last_modified: metadata
                    .modified()
                    .ok()
                    .map(notedthat_core::unix_seconds_i64),
            });
        }
    }
}

/// Could any key under the directory `dir_key` still match?
///
/// Keys below it all begin `dir_key/`, so the subtree is worth entering when that
/// stem is compatible with the prefix and the subtree can still hold a key above
/// `after`.
fn subtree_may_match(dir_key: &str, after: Option<&str>, prefix: Option<&str>) -> bool {
    let stem = format!("{dir_key}/");

    if let Some(prefix) = prefix
        && !stem.starts_with(prefix)
        && !prefix.starts_with(&stem)
    {
        return false;
    }

    // Every key inside sorts at or above the stem, so a subtree is exhausted only when
    // even its largest possible key is behind the cursor. `after >= stem` alone is not
    // enough — `after` may itself lie inside this subtree.
    if let Some(after) = after
        && after >= stem.as_str()
        && !after.starts_with(&stem)
    {
        return false;
    }

    true
}

/// Read one directory into reverse key order, so popping yields ascending order.
fn read_level(bucket_dir: &Path, dir: &Path) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut level: Vec<(String, Entry)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };

        // Symlinks and anything that is not a regular file or directory are not objects.
        if metadata.file_type().is_symlink() {
            continue;
        }
        let is_dir = metadata.is_dir();
        if !is_dir && !metadata.is_file() {
            continue;
        }

        // A name that is not valid UTF-8 cannot be an ObjectPath, and a crash-leftover
        // temp file was never committed.
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with(TEMP_PREFIX) {
            continue;
        }

        let Some(key) = crate::layout::key_from_path(bucket_dir, &path) else {
            continue;
        };

        // Sort directories as `name/`, so `a/` compares against `a-b` the way the keys
        // `a/c` and `a-b` actually compare.
        let sort_key = if is_dir { format!("{name}/") } else { name };
        level.push((sort_key, Entry { path, key, is_dir }));
    }

    level.sort_by(|(left, _), (right, _)| right.cmp(left));
    level.into_iter().map(|(_, entry)| entry).collect()
}

#[cfg(test)]
mod tests {
    use super::{OrderedWalk, decode_cursor, encode_cursor};
    use std::path::Path;

    /// Note there is no bare `a`: a filesystem cannot hold `a` as a file and `a/c` as a
    /// key at the same time. That divergence from S3 is covered in the adapter's own
    /// tests; here it would just make the fixture unbuildable.
    fn corpus(root: &Path) {
        for key in ["a.md", "a-b", "a/c", "a0/d", "b.md"] {
            let path = root.join(key.replace('/', std::path::MAIN_SEPARATOR_STR));
            if key.contains('/') {
                std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            }
            std::fs::write(&path, b"x").expect("write");
        }
    }

    fn walk(root: &Path, prefix: Option<&str>) -> Vec<String> {
        let mut walk = OrderedWalk::new(root.to_path_buf());
        let mut keys = Vec::new();
        while let Some(found) = walk.next_match(None, prefix) {
            keys.push(found.key);
        }
        keys
    }

    /// `a/c` must sort after `a-b`, because `/` (0x2F) is above `-` (0x2D). Sorting the
    /// directory `a0` by its bare name would also put it before `a.md`, which is wrong.
    #[test]
    fn keys_come_back_in_byte_order_not_traversal_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `a` is a file here, so use a corpus where no name is both file and directory.
        for key in ["a.md", "a-b", "a/c", "a0/d", "b.md"] {
            let path = dir.path().join(key);
            if key.contains('/') {
                std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            }
            std::fs::write(&path, b"x").expect("write");
        }

        let keys = walk(dir.path(), None);
        let mut expected = keys.clone();
        expected.sort();
        assert_eq!(keys, expected, "walk order must equal byte order");
        assert_eq!(keys, vec!["a-b", "a.md", "a/c", "a0/d", "b.md"]);
    }

    #[test]
    fn a_prefix_is_a_string_prefix_not_a_path_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("dir")).expect("dirs");
        std::fs::write(dir.path().join("dir/one.md"), b"x").expect("write");
        std::fs::write(dir.path().join("dinner.md"), b"x").expect("write");

        // "di" must reach into the `dir/` subtree, not just match top-level names.
        let keys = walk(dir.path(), Some("di"));
        assert_eq!(keys, vec!["dinner.md", "dir/one.md"]);
    }

    #[test]
    fn resuming_after_a_key_skips_everything_at_or_below_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        corpus(dir.path());

        let mut walk = OrderedWalk::new(dir.path().to_path_buf());
        let mut keys = Vec::new();
        while let Some(found) = walk.next_match(Some("a/c"), None) {
            keys.push(found.key);
        }
        assert_eq!(keys, vec!["a0/d", "b.md"]);
    }

    #[test]
    fn resuming_inside_a_subtree_still_yields_its_later_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("d")).expect("dirs");
        for key in ["d/1.md", "d/2.md", "d/3.md"] {
            std::fs::write(dir.path().join(key), b"x").expect("write");
        }

        let mut walk = OrderedWalk::new(dir.path().to_path_buf());
        let mut keys = Vec::new();
        while let Some(found) = walk.next_match(Some("d/1.md"), None) {
            keys.push(found.key);
        }
        assert_eq!(keys, vec!["d/2.md", "d/3.md"]);
    }

    #[test]
    fn temp_leftovers_and_symlinks_are_not_objects() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("real.md"), b"x").expect("write");
        std::fs::write(dir.path().join(".notedthat-tmp-abc"), b"x").expect("write");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("real.md"), dir.path().join("link.md"))
            .expect("symlink");

        assert_eq!(walk(dir.path(), None), vec!["real.md"]);
    }

    #[test]
    fn a_cursor_round_trips_and_rejects_a_changed_prefix() {
        let token = encode_cursor("notes/a.md", Some("notes/"));
        assert_eq!(
            decode_cursor(&token, Some("notes/")).expect("decode"),
            "notes/a.md"
        );
        assert!(decode_cursor(&token, None).is_err());
        assert!(decode_cursor(&token, Some("other/")).is_err());
        assert!(decode_cursor("not-a-cursor", None).is_err());
    }
}

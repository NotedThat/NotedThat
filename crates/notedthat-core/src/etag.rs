//! Content-derived `ETag` generation for backends that must produce their own.
//!
//! S3 hands back an `ETag` of its own making, so `notedthat-storage-s3` never calls
//! anything here. A backend with no such server — the filesystem adapter, and the
//! in-memory substitute — has to derive one, and it must derive it the same way, or
//! two backends disagree about object identity and the indexer re-indexes the world
//! after a backend switch.
//!
//! The value is a quoted lowercase SHA-256 hex digest, matching RFC 7232 §2.3's
//! requirement that an `ETag` be a quoted string. It is content-derived and therefore
//! stable across restarts, which is what [`crate::storage::Storage`] callers rely on.

use sha2::{Digest, Sha256};

/// SHA-256 `ETag` for `bytes`, in `"<hex>"` form.
#[must_use]
pub fn compute_etag(bytes: &[u8]) -> String {
    let mut hasher = EtagHasher::new();
    hasher.update(bytes);
    hasher.finish()
}

/// Incremental form of [`compute_etag`], for bodies that must not be buffered whole.
///
/// A filesystem backend streams an object to disk once and hashes it as it goes; without
/// this it would have to read every uploaded object back to learn its `ETag`.
#[derive(Debug, Default)]
pub struct EtagHasher(Sha256);

impl EtagHasher {
    /// Start a new hash.
    #[must_use]
    pub fn new() -> Self {
        Self(Sha256::new())
    }

    /// Feed the next chunk of the body.
    pub fn update(&mut self, chunk: &[u8]) {
        self.0.update(chunk);
    }

    /// Finish, returning the quoted hex digest.
    #[must_use]
    pub fn finish(self) -> String {
        format!("\"{}\"", hex::encode(self.0.finalize()))
    }
}

#[cfg(test)]
mod tests {
    use super::{EtagHasher, compute_etag};

    #[test]
    fn etag_is_quoted_lowercase_hex() {
        let etag = compute_etag(b"hello");
        assert!(etag.starts_with('"') && etag.ends_with('"'));
        let inner = &etag[1..etag.len() - 1];
        assert_eq!(inner.len(), 64);
        assert!(
            inner
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
    }

    #[test]
    fn etag_is_deterministic_and_content_derived() {
        assert_eq!(compute_etag(b"same"), compute_etag(b"same"));
        assert_ne!(compute_etag(b"same"), compute_etag(b"other"));
    }

    #[test]
    fn incremental_hashing_matches_one_shot() {
        let mut hasher = EtagHasher::new();
        hasher.update(b"hel");
        hasher.update(b"lo");
        assert_eq!(hasher.finish(), compute_etag(b"hello"));
    }

    #[test]
    fn empty_body_has_an_etag() {
        assert_eq!(compute_etag(b""), EtagHasher::new().finish());
    }
}

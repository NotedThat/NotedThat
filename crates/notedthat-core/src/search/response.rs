use serde::{Deserialize, Serialize};
use super::SearchHit;

/// Response from `POST /v1/knowledgebases/{kb_slug}/search`.
///
/// `#[non_exhaustive]` establishes a new convention in `notedthat-core`:
/// response types get `non_exhaustive` so post-v1 additions (e.g. `next_cursor`)
/// are non-breaking. This convention does NOT retro-apply to existing types.
///
/// Because this struct is `#[non_exhaustive]`, callers outside this crate
/// must use the provided constructors (`new` / `empty`) rather than struct literals.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResponse {
    /// The ranked list of search hits (may be empty).
    #[serde(default)]
    pub hits: Vec<SearchHit>,
}

impl SearchResponse {
    /// Construct a response with the given hits.
    pub fn new(hits: Vec<SearchHit>) -> Self {
        Self { hits }
    }

    /// Construct an empty response (no hits).
    pub fn empty() -> Self {
        Self { hits: Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_serializes_to_hits_array() {
        let r = SearchResponse::empty();
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"hits":[]}"#);
    }

    #[test]
    fn empty_deserializes_from_empty_hits() {
        let r: SearchResponse = serde_json::from_str(r#"{"hits":[]}"#).unwrap();
        assert!(r.hits.is_empty());
    }

    #[test]
    fn deserializes_from_empty_object() {
        // #[serde(default)] on hits allows {} to deserialize as empty
        let r: SearchResponse = serde_json::from_str("{}").unwrap();
        assert!(r.hits.is_empty());
    }

    #[test]
    fn new_constructor_sets_hits() {
        // We can't construct SearchHit without ObjectKey, but we can test the constructor path
        let r = SearchResponse::new(vec![]);
        assert!(r.hits.is_empty());
        let r2 = SearchResponse::empty();
        assert_eq!(r, r2);
    }

    #[test]
    fn send_sync_clone_debug() {
        fn assert<T: Send + Sync + Clone + std::fmt::Debug>() {}
        assert::<SearchResponse>();
    }
}

use thiserror::Error;

/// Errors returned by the search subsystem.
///
/// Maps to HTTP status codes via `From<SearchError> for Error` in `error.rs`:
/// - `InvalidInput` → 400
/// - `UnknownKb` → 404
/// - `BackendUnavailable` → 503
/// - `Internal` → 500
#[derive(Debug, Error)]
pub enum SearchError {
    /// Search request input is malformed or semantically invalid.
    #[error("invalid input: {message}")]
    InvalidInput {
        /// Human-readable description of the invalid input.
        message: String,
    },

    /// The requested knowledge base is unknown to the search subsystem.
    #[error("knowledge base not found: {slug}")]
    UnknownKb {
        /// The missing knowledge base slug.
        slug: String,
    },

    /// The search backend is unavailable or temporarily unable to serve requests.
    #[error("search backend unavailable: {message}")]
    BackendUnavailable {
        /// Human-readable backend failure detail.
        message: String,
    },

    /// An unexpected search subsystem error occurred.
    #[error("internal error: {message}")]
    Internal {
        /// Human-readable internal failure detail.
        message: String,
    },
}

impl SearchError {
    /// Convenience constructor.
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::InvalidInput {
            message: message.into(),
        }
    }

    /// Convenience constructor.
    pub fn unknown_kb(slug: impl Into<String>) -> Self {
        Self::UnknownKb { slug: slug.into() }
    }

    /// Convenience constructor.
    pub fn backend_unavailable(message: impl Into<String>) -> Self {
        Self::BackendUnavailable {
            message: message.into(),
        }
    }

    /// Convenience constructor.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn invalid_input_display() {
        let e = SearchError::invalid_input("query must not be empty");
        assert!(e.to_string().contains("query must not be empty"));
    }

    #[test]
    fn unknown_kb_display() {
        let e = SearchError::unknown_kb("notes");
        assert!(e.to_string().contains("notes"));
    }

    #[test]
    fn backend_unavailable_display() {
        let e = SearchError::backend_unavailable("connection refused");
        assert!(e.to_string().contains("connection refused"));
    }

    #[test]
    fn internal_display() {
        let e = SearchError::internal("unexpected state");
        assert!(e.to_string().contains("unexpected state"));
    }

    #[test]
    fn maps_invalid_input_to_error_invalid_input() {
        let e = SearchError::invalid_input("bad query");
        let mapped: Error = e.into();
        assert!(matches!(mapped, Error::InvalidInput { .. }));
    }

    #[test]
    fn maps_unknown_kb_to_error_not_found() {
        let e = SearchError::unknown_kb("my-kb");
        let mapped: Error = e.into();
        assert!(matches!(mapped, Error::NotFound { .. }));
    }

    #[test]
    fn maps_backend_unavailable_to_error_storage() {
        let e = SearchError::backend_unavailable("qdrant down");
        let mapped: Error = e.into();
        // Should map to StorageError::BackendUnavailable → Error::Storage(...)
        assert!(matches!(mapped, Error::Storage(_)));
    }

    #[test]
    fn maps_internal_to_error_config() {
        let e = SearchError::internal("unexpected state");
        let mapped: Error = e.into();
        // Maps to Error::Config (existing 500-mapped variant)
        assert!(matches!(mapped, Error::Config { .. }));
    }

    #[test]
    fn send_sync_bounds() {
        fn assert_send_sync<T: Send + Sync + std::error::Error>() {}
        assert_send_sync::<SearchError>();
    }
}

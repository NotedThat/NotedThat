//! Domain error types: `Error` and `StorageError`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_invalid_input_display() {
        let e = Error::InvalidInput { message: "bad".into() };
        let s = e.to_string();
        assert!(s.contains("bad"), "Display should contain the message: {s}");
    }

    #[test]
    fn test_error_payload_too_large_display() {
        let e = Error::PayloadTooLarge {
            size: 17_000_000_u64,
            limit: 16_777_216_u64,
        };
        let s = e.to_string();
        assert!(
            s.contains("17000000") || s.contains("17_000_000"),
            "Display should contain size: {s}"
        );
        assert!(
            s.contains("16777216") || s.contains("16_777_216"),
            "Display should contain limit: {s}"
        );
    }

    #[test]
    fn test_storage_error_not_found_display() {
        let e = StorageError::NotFound { key: "foo".into() };
        let s = e.to_string();
        assert!(s.contains("foo"), "Display should contain the key: {s}");
    }

    #[test]
    fn test_storage_error_is_not_found_true_for_not_found() {
        let e = StorageError::NotFound { key: "bar".into() };
        assert!(e.is_not_found());
    }

    #[test]
    fn test_storage_error_is_not_found_true_for_bucket_not_found() {
        let e = StorageError::BucketNotFound { bucket: "my-bucket".into() };
        assert!(e.is_not_found());
    }

    #[test]
    fn test_storage_error_is_not_found_false_for_backend_unavailable() {
        let e = StorageError::BackendUnavailable { source: "connection refused".into() };
        assert!(!e.is_not_found());
    }

    #[test]
    fn test_from_storage_error_for_error() {
        let se = StorageError::NotFound { key: "obj".into() };
        let e: Error = Error::from(se);
        assert!(matches!(e, Error::Storage(_)));
    }

    #[test]
    fn test_error_implements_std_error() {
        fn assert_std_error<T: std::error::Error>(_: &T) {}
        let e = Error::InvalidInput { message: "test".into() };
        assert_std_error(&e);
    }

    #[test]
    fn test_storage_error_implements_std_error() {
        fn assert_std_error<T: std::error::Error>(_: &T) {}
        let e = StorageError::BucketNotFound { bucket: "b".into() };
        assert_std_error(&e);
    }

    #[test]
    fn test_error_not_found_display() {
        let e = Error::NotFound { resource: "kb:my-notes".into() };
        let s = e.to_string();
        assert!(s.contains("my-notes"), "Display should contain resource: {s}");
    }

    #[test]
    fn test_error_bucket_name_too_long_fields() {
        let e = Error::BucketNameTooLong {
            name: "nt-toolong-name".into(),
            len: 15_usize,
        };
        if let Error::BucketNameTooLong { name, len } = &e {
            assert_eq!(name, "nt-toolong-name");
            assert_eq!(*len, 15);
        } else {
            panic!("Wrong variant");
        }
    }
}

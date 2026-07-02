//! `Storage` trait — object store abstraction.

#[cfg(test)]
mod tests {
    fn assert_send_sync<T: Send + Sync + ?Sized>() {}

    #[test]
    fn storage_is_dyn_compatible_and_send_sync() {
        assert_send_sync::<dyn crate::storage::Storage>();
    }
}

//! Compile smoke tests for the `notedthat-write` public API.

#[test]
fn exported_items_are_reachable() {
    let _ = notedthat_write::MAX_UPLOAD_BYTES;
    notedthat_write::check_size(0, notedthat_write::MAX_UPLOAD_BYTES).expect("size ok");
}

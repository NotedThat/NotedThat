use super::*;

#[tokio::test]
async fn put_body_read_failure_returns_400_without_storage_write() {
    let storage = Arc::new(MockStorage::default());
    let body = Body::from_stream(futures::stream::once(async {
        Err::<Bytes, std::io::Error>(std::io::Error::other("client disconnected"))
    }));
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/interrupted.md")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(!storage.calls().contains(&"put_object"));
}

#[tokio::test]
async fn put_shorter_than_content_length_returns_400_without_storage_write() {
    let storage = Arc::new(MockStorage::default());
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/truncated.md")
                .header("content-length", "10")
                .body(Body::from("short"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(!storage.calls().contains(&"put_object"));
}

#[tokio::test]
async fn put_malformed_content_length_returns_400_before_storage() {
    let storage = Arc::new(MockStorage::default());
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/file.md")
                .header("content-length", "many")
                .body(Body::from("body"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(storage.calls().is_empty());
}

#[tokio::test]
async fn put_duplicate_content_length_returns_400_before_storage() {
    let storage = Arc::new(MockStorage::default());
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/file.md")
                .header("content-length", "4")
                .header("content-length", "5")
                .body(Body::from("body"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(storage.calls().is_empty());
}

#[tokio::test]
async fn put_accepts_17_mib_body_via_staging_file() {
    let storage = Arc::new(MockStorage::default());
    let body = vec![b'x'; 17 * 1024 * 1024];
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/large.bin")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(storage.staged_lengths(), vec![17 * 1024 * 1024]);
    assert!(storage.get_stored("notes", "large.bin").is_some());
    let staged_paths = storage.staged_paths();
    assert_eq!(staged_paths.len(), 1);
    assert!(!staged_paths[0].exists());
}

#[tokio::test]
#[ignore = "5 GiB upload stress scenario"]
async fn stress_put_accepts_actual_5_gib_without_content_length() {
    let storage = Arc::new(MockStorage::default());
    let chunk = Bytes::from(vec![b'x'; 1024 * 1024]);
    let chunk_len = u64::try_from(chunk.len()).unwrap();
    let chunks = usize::try_from(notedthat_write::MAX_UPLOAD_BYTES / chunk_len).unwrap();
    let body = Body::from_stream(futures::stream::iter(
        (0..chunks).map(move |_| Ok::<_, std::io::Error>(chunk.clone())),
    ));
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/five-gib.bin")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        storage.staged_lengths(),
        vec![notedthat_write::MAX_UPLOAD_BYTES]
    );
    assert!(storage.staged_paths().iter().all(|path| !path.exists()));
}

#[tokio::test]
#[ignore = "5 GiB observed oversize stress scenario"]
async fn stress_put_rejects_actual_body_over_5_gib_and_removes_staging_file() {
    use futures::StreamExt as _;

    let storage = Arc::new(MockStorage::default());
    let directory = std::env::temp_dir().join(format!(
        "notedthat-overflow-staging-{}",
        uuid::Uuid::now_v7()
    ));
    std::fs::create_dir(&directory).unwrap();
    let chunk = Bytes::from(vec![b'x'; 1024 * 1024]);
    let chunk_len = u64::try_from(chunk.len()).unwrap();
    let chunks = usize::try_from(notedthat_write::MAX_UPLOAD_BYTES / chunk_len).unwrap();
    let generated = futures::stream::iter(
        (0..chunks).map(move |_| Ok::<_, std::io::Error>(chunk.clone())),
    )
    .chain(futures::stream::once(async {
        Ok::<_, std::io::Error>(Bytes::from_static(b"x"))
    }));
    let resp = app_with_staging_config(
        storage.clone(),
        notedthat_core::StagingConfig::new(directory.clone()),
    )
    .oneshot(
        HttpRequest::builder()
            .method("PUT")
            .uri("/webdav/notes/too-large.bin")
            .body(Body::from_stream(generated))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!storage.calls().contains(&"put_object"));
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    std::fs::remove_dir(directory).unwrap();
}

#[tokio::test]
async fn put_staging_disk_failure_returns_507_without_storage_write() {
    let storage = Arc::new(MockStorage::default());
    let missing = std::env::temp_dir().join(format!(
        "notedthat-missing-staging-{}",
        uuid::Uuid::now_v7()
    ));
    let staging_config = notedthat_core::StagingConfig::new(missing);
    let body = vec![b'x'; 17 * 1024 * 1024];
    let resp = app_with_staging_config(storage.clone(), staging_config)
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/webdav/notes/large.bin")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INSUFFICIENT_STORAGE);
    assert!(!storage.calls().contains(&"put_object"));
}

async fn self_copy_or_move(method: &'static str, error_name: &str) {
    let storage = Arc::new(MockStorage::default());
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method(method)
                .uri("/webdav/notes/my%7Efile.md")
                .header("destination", "/webdav/notes/my~file.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(response_body(resp).await.contains(error_name));
    assert!(storage.calls().is_empty());
}

#[tokio::test]
async fn copy_to_same_decoded_object_returns_403_before_storage() {
    self_copy_or_move("COPY", "cannot-copy-resource").await;
}

#[tokio::test]
async fn move_to_same_decoded_object_returns_403_before_storage() {
    self_copy_or_move("MOVE", "cannot-move-resource").await;
}

#[tokio::test]
async fn copy_overwrite_false_existing_destination_returns_412() {
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "src.md", Bytes::from_static(b"source"), "\"src\"");
    storage.insert("notes", "dst.md", Bytes::from_static(b"old"), "\"dst\"");
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("COPY")
                .uri("/webdav/notes/src.md")
                .header("destination", "/webdav/notes/dst.md")
                .header("overwrite", "F")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        storage.get_stored("notes", "dst.md").unwrap().bytes,
        Bytes::from_static(b"old")
    );
    assert_eq!(
        storage.copy_options()[0]
            .destination_if_none_match
            .as_deref(),
        Some("*")
    );
}

#[tokio::test]
async fn copy_status_follows_the_backend_when_the_destination_appears_mid_request() {
    // The destination is written between the request's arrival and the copy; the
    // copy replaces it, so the answer is 204 (RFC 4918 §9.8.5), not 201.
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "src.md", Bytes::from_static(b"source"), "\"src\"");
    storage.race_destination_before_copy();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("COPY")
                .uri("/webdav/notes/src.md")
                .header("destination", "/webdav/notes/dst.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        storage.get_stored("notes", "dst.md").unwrap().bytes,
        Bytes::from_static(b"source")
    );
}

#[tokio::test]
async fn copy_or_move_of_a_missing_source_is_404_despite_if_match() {
    // Without its preconditions the request is a 404, so they are ignored
    // (RFC 9110 §13.2.1).
    for method in ["COPY", "MOVE"] {
        let storage = Arc::new(MockStorage::default());
        let resp = app(storage.clone())
            .oneshot(
                HttpRequest::builder()
                    .method(method)
                    .uri("/webdav/notes/missing.md")
                    .header("destination", "/webdav/notes/dst.md")
                    .header("if-match", "*")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method}");
        assert!(!storage.calls().contains(&"copy_object"), "{method}");
    }
}

#[tokio::test]
async fn copy_overwrite_false_protects_destination_creation_race() {
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "src.md", Bytes::from_static(b"source"), "\"src\"");
    storage.race_destination_before_copy();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("COPY")
                .uri("/webdav/notes/src.md")
                .header("destination", "/webdav/notes/dst.md")
                .header("overwrite", "F")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        storage.get_stored("notes", "dst.md").unwrap().bytes,
        Bytes::from_static(b"racing writer")
    );
}

#[tokio::test]
async fn copy_source_change_before_native_copy_returns_412_without_destination() {
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "src.md", Bytes::from_static(b"source"), "\"src\"");
    storage.change_source_before_copy();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("COPY")
                .uri("/webdav/notes/src.md")
                .header("destination", "/webdav/notes/dst.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    assert!(storage.get_stored("notes", "dst.md").is_none());
    assert_eq!(
        storage.copy_options()[0].source_if_match.as_deref(),
        Some("\"src\"")
    );
}

#[tokio::test]
async fn copy_overwrite_true_replaces_destination_and_preserves_mime() {
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "src.md", Bytes::from_static(b"source"), "\"src\"");
    storage.insert("notes", "dst.md", Bytes::from_static(b"old"), "\"dst\"");
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("COPY")
                .uri("/webdav/notes/src.md")
                .header("destination", "/webdav/notes/dst.md")
                .header("overwrite", "T")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let copied = storage.get_stored("notes", "dst.md").unwrap();
    assert_eq!(copied.bytes, Bytes::from_static(b"source"));
    assert_eq!(copied.content_type.as_deref(), Some("text/markdown"));
    let options = storage.copy_options();
    assert_eq!(options[0].source_if_match.as_deref(), Some("\"src\""));
    assert_eq!(options[0].destination_if_none_match, None);
}

#[tokio::test]
async fn copy_infers_markdown_mime_from_destination_path() {
    // Given
    let storage = Arc::new(MockStorage::default());
    storage.insert_with_content_type(
        "notes",
        "src.tmp",
        Bytes::from_static(b"# source"),
        "\"src\"",
        "application/octet-stream",
    );

    // When
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("COPY")
                .uri("/webdav/notes/src.tmp")
                .header("destination", "/webdav/notes/dst.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Then
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        storage
            .get_stored("notes", "dst.md")
            .expect("COPY creates the destination")
            .content_type
            .as_deref(),
        Some("text/markdown")
    );
}

#[tokio::test]
async fn copy_invalid_overwrite_returns_400_before_storage() {
    let storage = Arc::new(MockStorage::default());
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("COPY")
                .uri("/webdav/notes/src.md")
                .header("destination", "/webdav/notes/dst.md")
                .header("overwrite", "true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(storage.calls().is_empty());
}

#[tokio::test]
async fn copy_duplicate_overwrite_returns_400_before_storage() {
    let storage = Arc::new(MockStorage::default());
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("COPY")
                .uri("/webdav/notes/src.md")
                .header("destination", "/webdav/notes/dst.md")
                .header("overwrite", "T")
                .header("overwrite", "F")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(storage.calls().is_empty());
}

#[tokio::test]
async fn move_source_change_returns_412_with_partial_completion_body() {
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "src.md", Bytes::from_static(b"source"), "\"src\"");
    storage.change_source_after_copy();
    let resp = app(storage.clone())
        .oneshot(
            HttpRequest::builder()
                .method("MOVE")
                .uri("/webdav/notes/src.md")
                .header("destination", "/webdav/notes/dst.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    let body = response_body(resp).await;
    assert!(body.contains("partially completed"));
    assert!(body.contains("queued for indexing"));
    assert!(body.contains("source changed before deletion"));
    assert!(storage.get_stored("notes", "src.md").is_some());
    assert!(storage.get_stored("notes", "dst.md").is_some());
}

/// COPY or MOVE `src.md` (`ETag` `"src"`) onto `dst.md` (`ETag` `"dst"`) with `headers`.
async fn copy_or_move_with(
    method: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, Arc<MockStorage>) {
    let storage = Arc::new(MockStorage::default());
    storage.insert("notes", "src.md", Bytes::from_static(b"source"), "\"src\"");
    storage.insert("notes", "dst.md", Bytes::from_static(b"old"), "\"dst\"");
    let mut builder = HttpRequest::builder()
        .method(method)
        .uri("/webdav/notes/src.md")
        .header("destination", "/webdav/notes/dst.md");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let resp = app(storage.clone())
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    (resp.status(), storage)
}

/// RFC 9110 §13.2.1: the source's If-Match is the client's, not only our own pin.
#[tokio::test]
async fn copy_with_stale_if_match_returns_412_without_copying() {
    let (status, storage) = copy_or_move_with("COPY", &[("if-match", "\"stale\"")]).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(storage.copy_options().is_empty());
    assert_eq!(
        storage.get_stored("notes", "dst.md").unwrap().bytes,
        Bytes::from_static(b"old")
    );
}

#[tokio::test]
async fn move_with_current_if_match_succeeds() {
    let (status, storage) = copy_or_move_with("MOVE", &[("if-match", "\"src\"")]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(storage.get_stored("notes", "src.md").is_none());
}

#[tokio::test]
async fn move_with_matching_if_none_match_returns_412() {
    let (status, storage) = copy_or_move_with("MOVE", &[("if-none-match", "*")]).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(storage.get_stored("notes", "src.md").is_some());
}

/// RFC 4918 §10.4: an untagged `If` list applies to the source.
#[tokio::test]
async fn copy_honours_the_if_header() {
    let (status, _) = copy_or_move_with("COPY", &[("if", "([\"src\"])")]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, storage) = copy_or_move_with("COPY", &[("if", "([\"stale\"])")]).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(storage.copy_options().is_empty());
}

/// There are no locks, so a list asserting a lock token cannot hold.
#[tokio::test]
async fn move_with_a_lock_token_in_the_if_header_returns_412() {
    let (status, storage) = copy_or_move_with(
        "MOVE",
        &[("if", "(<opaquelocktoken:a515cfa4-5da4-22e1-f5bf-00a0451e6bf7>)")],
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(storage.get_stored("notes", "src.md").is_some());

    let (status, _) = copy_or_move_with("MOVE", &[("if", "(Not <DAV:no-lock>)")]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// A tagged list naming the destination is judged against the destination's `ETag`.
#[tokio::test]
async fn copy_evaluates_a_tagged_list_against_the_destination() {
    let (status, _) =
        copy_or_move_with("COPY", &[("if", "</webdav/notes/dst.md> ([\"dst\"])")]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = copy_or_move_with(
        "COPY",
        &[("if", "<http://localhost/webdav/notes/dst.md> ([\"src\"])")],
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn copy_with_a_malformed_if_header_returns_400() {
    let (status, storage) = copy_or_move_with("COPY", &[("if", "[\"src\"]")]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(storage.copy_options().is_empty());
}

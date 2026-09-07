//! Integration coverage for staged request bodies.

use bytes::Bytes;
use futures::{StreamExt, stream};
use notedthat_core::{StageError, StagedBody, StagingConfig};
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn staged_body_spills_only_after_memory_threshold() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let config = StagingConfig::new(dir.path().to_path_buf());
    let exact = Bytes::from(vec![b'a'; StagedBody::MEMORY_THRESHOLD]);
    let over = Bytes::from(vec![b'b'; StagedBody::MEMORY_THRESHOLD + 1]);

    let memory = StagedBody::stage_stream(
        stream::iter([Ok::<_, std::io::Error>(exact)]),
        None,
        u64::MAX,
        &config,
    )
    .await
    .expect("threshold body stages");
    let file = StagedBody::stage_stream(
        stream::iter([Ok::<_, std::io::Error>(over)]),
        None,
        u64::MAX,
        &config,
    )
    .await
    .expect("over-threshold body stages");

    assert!(!memory.is_file());
    assert!(file.is_file());
}

#[tokio::test]
async fn staged_body_rejects_mismatched_declared_length() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let config = StagingConfig::new(dir.path().to_path_buf());
    let result = StagedBody::stage_stream(
        stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"abc"))]),
        Some(4),
        10,
        &config,
    )
    .await;

    assert!(matches!(
        result,
        Err(StageError::LengthMismatch {
            expected: 4,
            actual: 3
        })
    ));
}

#[tokio::test]
async fn staged_body_removes_private_file_after_read_failure() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let config = StagingConfig::new(dir.path().to_path_buf());
    let chunks = stream::iter([
        Ok(Bytes::from(vec![0; StagedBody::MEMORY_THRESHOLD + 1])),
        Err(std::io::Error::other("source failed")),
    ]);

    let result = StagedBody::stage_stream(chunks, None, u64::MAX, &config).await;

    assert!(matches!(result, Err(StageError::Read { .. })));
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("read staging dir")
            .count(),
        0
    );
}

#[tokio::test]
async fn staged_body_reports_unusable_staging_directory() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let missing = dir.path().join("missing");
    let config = StagingConfig::new(missing);

    let result = StagedBody::stage_stream(
        stream::iter([Ok::<_, std::io::Error>(Bytes::from(vec![
            0;
            StagedBody::MEMORY_THRESHOLD
                + 1
        ]))]),
        None,
        u64::MAX,
        &config,
    )
    .await;

    assert!(matches!(result, Err(StageError::Write { .. })));
}

#[tokio::test]
async fn staging_config_validation_writes_and_removes_probe() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let config = StagingConfig::new(dir.path().to_path_buf());

    config
        .validate()
        .await
        .expect("staging directory is usable");

    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("read staging directory")
            .count(),
        0
    );
}

#[tokio::test]
async fn cancelling_staging_removes_private_file() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let config = StagingConfig::new(dir.path().to_path_buf());
    let stream = stream::iter([Ok::<_, std::io::Error>(Bytes::from(vec![
        0;
        StagedBody::MEMORY_THRESHOLD
            + 1
    ]))])
    .chain(stream::pending());
    let task =
        tokio::spawn(
            async move { StagedBody::stage_stream(stream, None, u64::MAX, &config).await },
        );
    for _ in 0..100 {
        if std::fs::read_dir(dir.path())
            .expect("read staging dir")
            .next()
            .is_some()
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    task.abort();
    let Err(cancelled) = task.await else {
        panic!("task unexpectedly completed");
    };

    assert!(cancelled.is_cancelled());
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("read staging dir")
            .count(),
        0
    );
}

#[tokio::test]
async fn staged_file_reopens_from_start_without_materializing() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let config = StagingConfig::new(dir.path().to_path_buf());
    let expected = Bytes::from(vec![b'x'; StagedBody::MEMORY_THRESHOLD + 1]);
    let staged = StagedBody::stage_stream(
        stream::iter([Ok::<_, std::io::Error>(expected.clone())]),
        Some(expected.len() as u64),
        u64::MAX,
        &config,
    )
    .await
    .expect("body stages");

    let mut first = staged.open().await.expect("first reader");
    let mut second = staged.open().await.expect("second reader");
    let mut first_bytes = Vec::new();
    let mut second_bytes = Vec::new();
    first
        .read_to_end(&mut first_bytes)
        .await
        .expect("first read");
    second
        .read_to_end(&mut second_bytes)
        .await
        .expect("second read");

    assert_eq!(first_bytes, expected);
    assert_eq!(second_bytes, expected);
}

//! Wire-level proof that file-backed request bodies survive an SDK retry.

use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use bytes::Bytes;
use futures::stream;
use notedthat_core::{
    ConditionalHeaders, CopyObjectOptions, KbSlug, ObjectPath, StagedBody, StagingConfig, Storage,
    TenantSlug,
};
use notedthat_storage_s3::S3Storage;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn file_backed_put_replays_identical_body_after_retry() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/nt-test-notes/large.bin"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/nt-test-notes/large.bin"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"stored\""))
        .expect(1)
        .with_priority(2)
        .mount(&server)
        .await;
    let sdk_config = aws_sdk_s3::config::Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .endpoint_url(server.uri())
        .force_path_style(true)
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("key", "secret", None, None, "test"))
        .retry_config(RetryConfig::standard().with_max_attempts(2))
        .build();
    let storage = S3Storage::new(
        aws_sdk_s3::Client::from_conf(sdk_config),
        TenantSlug::try_new("test").expect("tenant slug"),
    );
    let directory = tempfile::tempdir().expect("staging directory");
    let staging = StagingConfig::new(directory.path().to_path_buf());
    let body = Bytes::from(vec![b'x'; StagedBody::MEMORY_THRESHOLD + 1]);
    let payload_len = body.len();
    let staged = StagedBody::stage_stream(
        stream::iter([Ok::<_, std::io::Error>(body)]),
        None,
        u64::MAX,
        &staging,
    )
    .await
    .expect("staged body");

    let outcome = storage
        .put_staged_object(
            &KbSlug::try_new("notes").expect("kb slug"),
            &ObjectPath::try_from_str("large.bin").expect("object path"),
            staged,
            Some("application/octet-stream"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("retried upload succeeds");

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 2);
    let expected_length = payload_len.to_string();
    for request in &requests {
        assert_eq!(
            request
                .headers
                .get("x-amz-decoded-content-length")
                .expect("decoded content length"),
            expected_length.as_str()
        );
    }
    assert_eq!(requests[0].body.len(), requests[1].body.len());
    assert_eq!(outcome.etag.as_deref(), Some("\"stored\""));
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("read staging directory")
            .count(),
        0
    );
}

#[tokio::test]
async fn native_copy_sends_source_and_destination_preconditions() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/nt-test-notes/destination.md"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/xml")
                .set_body_raw(
                    "<CopyObjectResult><ETag>&quot;copied&quot;</ETag></CopyObjectResult>",
                    "application/xml",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let sdk_config = aws_sdk_s3::config::Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .endpoint_url(server.uri())
        .force_path_style(true)
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("key", "secret", None, None, "test"))
        .build();
    let storage = S3Storage::new(
        aws_sdk_s3::Client::from_conf(sdk_config),
        TenantSlug::try_new("test").expect("tenant slug"),
    );

    let outcome = storage
        .copy_object(
            &KbSlug::try_new("notes").expect("kb slug"),
            &ObjectPath::try_from_str("source file.md").expect("source path"),
            &ObjectPath::try_from_str("destination.md").expect("destination path"),
            CopyObjectOptions {
                source_if_match: Some("\"source-etag\"".into()),
                destination_if_none_match: Some("*".into()),
                content_type: Some("text/markdown".into()),
            },
        )
        .await
        .expect("native copy succeeds");

    let requests = server.received_requests().await.expect("recorded request");
    let headers = &requests[0].headers;
    assert_eq!(
        headers.get("x-amz-copy-source").expect("copy source"),
        "nt-test-notes/source%20file.md"
    );
    assert_eq!(
        headers
            .get("x-amz-copy-source-if-match")
            .expect("source match"),
        "\"source-etag\""
    );
    assert_eq!(
        headers.get("if-none-match").expect("destination non-match"),
        "*"
    );
    assert_eq!(
        headers.get("content-type").expect("content type"),
        "text/markdown"
    );
    assert_eq!(
        headers
            .get("x-amz-metadata-directive")
            .expect("metadata directive"),
        "REPLACE"
    );
    assert_eq!(outcome.etag.as_deref(), Some("\"copied\""));
}

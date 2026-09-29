//! Wire-level proof that the `HEAD` behind [`PutOutcome::created`] never fails a write.
//!
//! S3's `PutObject` does not say whether it replaced an object, so the adapter asks with a
//! `HEAD` first. That answer only picks `201` or `204`; a `HEAD` the backend refuses must
//! leave the write itself to succeed or fail on its own.

use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use bytes::Bytes;
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, Storage, TenantSlug};
use notedthat_storage_s3::S3Storage;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn a_failed_head_before_put_does_not_fail_the_put() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"stored\""))
        .expect(1)
        .mount(&server)
        .await;
    let sdk_config = aws_sdk_s3::config::Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .endpoint_url(server.uri())
        .force_path_style(true)
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("key", "secret", None, None, "test"))
        .retry_config(RetryConfig::disabled())
        .build();
    let storage = S3Storage::new(
        aws_sdk_s3::Client::from_conf(sdk_config),
        TenantSlug::try_new("test").expect("tenant slug"),
    );

    let outcome = storage
        .put_object(
            &KbSlug::try_new("notes").expect("kb slug"),
            &ObjectPath::try_from_str("a.md").expect("object path"),
            Bytes::from_static(b"body"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("the PUT succeeds although the HEAD before it failed");

    assert_eq!(outcome.etag.as_deref(), Some("\"stored\""));
    assert!(
        !outcome.created,
        "an unanswered HEAD is read as an existing object"
    );
}

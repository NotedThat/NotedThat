//! Wire-level proof that S3's `NoSuchBucket` becomes [`StorageError::BucketNotFound`].
//!
//! A declared knowledge base whose bucket was removed after provisioning is a `404`
//! for every surface (D43), not an internal error. The shared storage integration
//! suite proves the same against `SeaweedFS`; this pins the SDK error-code mapping
//! without a container, including the one shape a real backend cannot produce on
//! demand: a `HeadObject` refusal, which carries no body at all.

use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use bytes::Bytes;
use notedthat_core::{
    ConditionalHeaders, CopyObjectOptions, KbSlug, ObjectPath, Storage, StorageError, TenantSlug,
};
use notedthat_storage_s3::S3Storage;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const NO_SUCH_BUCKET: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist</Message>\
<BucketName>nt-test-notes</BucketName></Error>";

async fn storage_answering(
    http_method: &str,
    template: ResponseTemplate,
) -> (MockServer, S3Storage) {
    let server = MockServer::start().await;
    Mock::given(method(http_method))
        .respond_with(template)
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
    (server, storage)
}

fn no_such_bucket() -> ResponseTemplate {
    ResponseTemplate::new(404).set_body_raw(NO_SUCH_BUCKET, "application/xml")
}

fn kb() -> KbSlug {
    KbSlug::try_new("notes").expect("kb slug")
}

fn key(s: &str) -> ObjectPath {
    ObjectPath::try_from_str(s).expect("object path")
}

fn assert_bucket_not_found<T>(result: Result<T, StorageError>, op: &str) {
    match result {
        Err(StorageError::BucketNotFound { bucket }) => assert_eq!(bucket, "nt-test-notes"),
        Err(other) => panic!("{op} on a missing bucket should be BucketNotFound, got {other}"),
        Ok(_) => panic!("{op} on a missing bucket should fail"),
    }
}

#[tokio::test]
async fn get_object_on_a_missing_bucket_is_bucket_not_found() {
    let (_server, storage) = storage_answering("GET", no_such_bucket()).await;
    let result = storage
        .get_object(&kb(), &key("a.md"), None, ConditionalHeaders::default())
        .await;
    assert_bucket_not_found(result, "get_object");
}

#[tokio::test]
async fn put_object_on_a_missing_bucket_is_bucket_not_found() {
    let (_server, storage) = storage_answering("PUT", no_such_bucket()).await;
    let result = storage
        .put_object(
            &kb(),
            &key("a.md"),
            Bytes::from_static(b"hello"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await;
    assert_bucket_not_found(result, "put_object");
}

#[tokio::test]
async fn delete_object_on_a_missing_bucket_is_bucket_not_found() {
    let (_server, storage) = storage_answering("DELETE", no_such_bucket()).await;
    let result = storage
        .delete_object(&kb(), &key("a.md"), ConditionalHeaders::default())
        .await;
    assert_bucket_not_found(result, "delete_object");
}

#[tokio::test]
async fn copy_object_on_a_missing_bucket_is_bucket_not_found() {
    let (_server, storage) = storage_answering("PUT", no_such_bucket()).await;
    let result = storage
        .copy_object(
            &kb(),
            &key("a.md"),
            &key("b.md"),
            CopyObjectOptions::default(),
        )
        .await;
    assert_bucket_not_found(result, "copy_object");
}

#[tokio::test]
async fn list_objects_on_a_missing_bucket_is_bucket_not_found() {
    let (_server, storage) = storage_answering("GET", no_such_bucket()).await;
    let result = storage.list_objects(&kb(), None, 10, None).await;
    assert_bucket_not_found(result, "list_objects");
}

#[tokio::test]
async fn read_manifest_on_a_missing_bucket_is_bucket_not_found() {
    let (_server, storage) = storage_answering("GET", no_such_bucket()).await;
    let result = storage.read_manifest(&kb()).await;
    assert_bucket_not_found(result, "read_manifest");
}

/// `HeadObject` answers `404` with an empty body, so the SDK reports `NotFound` and
/// the adapter cannot tell a missing bucket from a missing key. Both are a `404` to
/// every consumer; this pins that the answer is the object-level variant.
#[tokio::test]
async fn head_object_on_a_missing_bucket_is_a_bare_not_found() {
    let (_server, storage) = storage_answering("HEAD", ResponseTemplate::new(404)).await;
    let result = storage
        .head_object(&kb(), &key("a.md"), ConditionalHeaders::default())
        .await;
    match result {
        Err(StorageError::NotFound { key }) => assert_eq!(key, "a.md"),
        other => panic!("head_object should be NotFound, got {other:?}"),
    }
}

//! Wire-level proof of how [`S3Storage::list_objects`] reads the two malformed page
//! shapes a real backend cannot be made to produce on demand.
//!
//! `IsTruncated=true` without a `NextContinuationToken` must fail closed: ending the walk
//! there would silently drop every later key from PROPFIND and from reconciliation.
//! `IsTruncated=false` carrying a token is the opposite slip, and the flag wins: the page
//! is the last one and no cursor is handed out.

use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use notedthat_core::{KbSlug, Storage, StorageError, TenantSlug};
use notedthat_storage_s3::S3Storage;
use wiremock::matchers::{method, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A well-formed `ListObjectsV2` page holding one object, with `tail` spliced in where
/// `IsTruncated` and `NextContinuationToken` go.
fn list_page(tail: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
<Name>nt-test-notes</Name><Prefix></Prefix><KeyCount>1</KeyCount><MaxKeys>10</MaxKeys>\
{tail}\
<Contents><Key>a.md</Key><LastModified>2026-01-01T00:00:00.000Z</LastModified>\
<ETag>&quot;0cc175b9c0f1b6a831c399e269772661&quot;</ETag><Size>1</Size></Contents>\
</ListBucketResult>"
    )
}

async fn storage_listing(tail: &str) -> (MockServer, S3Storage) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("list-type", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(list_page(tail), "application/xml"))
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

fn kb() -> KbSlug {
    KbSlug::try_new("notes").expect("kb slug")
}

#[tokio::test]
async fn truncated_page_without_a_token_fails_closed() {
    let (_server, storage) = storage_listing("<IsTruncated>true</IsTruncated>").await;
    match storage.list_objects(&kb(), None, 10, None).await {
        Err(StorageError::BackendUnavailable { message }) => {
            assert!(message.contains("NextContinuationToken"), "{message}");
        }
        other => panic!("a truncated page without a token should fail closed, got {other:?}"),
    }
}

#[tokio::test]
async fn untruncated_page_with_a_token_is_the_last_page() {
    let (_server, storage) = storage_listing(
        "<IsTruncated>false</IsTruncated><NextContinuationToken>x</NextContinuationToken>",
    )
    .await;
    let page = storage
        .list_objects(&kb(), None, 10, None)
        .await
        .expect("an untruncated page with a stray token is still a page");
    assert!(!page.truncated);
    assert_eq!(page.next_cursor, None);
    assert_eq!(page.objects.len(), 1);
    assert_eq!(page.objects[0].key, "a.md");
}

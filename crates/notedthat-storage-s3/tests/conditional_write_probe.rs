//! Wire-level proof of what [`S3Storage::check_conditional_writes`] reads from a backend.
//!
//! The one backend in the test suite, `SeaweedFS` 4.18, enforces both preconditions, so
//! a backend that parses them and stores the object anyway (`SPECIFICATIONS.md` §8.1)
//! can only be shown here: each fake below answers a conditional `PUT` the way one kind
//! of backend does, and every test also pins that the scratch object is deleted.

use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use notedthat_core::{KbSlug, StorageError, TenantSlug};
use notedthat_storage_s3::{ConditionalWrites, PROBE_KEY_PREFIX, S3Storage};
use wiremock::matchers::{header_exists, method};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const BUCKET_PREFIX: &str = "/nt-test-notes/";

fn storage_for(server: &MockServer) -> S3Storage {
    let sdk_config = aws_sdk_s3::config::Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .endpoint_url(server.uri())
        .force_path_style(true)
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("key", "secret", None, None, "test"))
        .retry_config(RetryConfig::disabled())
        .build();
    S3Storage::new(
        aws_sdk_s3::Client::from_conf(sdk_config),
        TenantSlug::try_new("test").expect("tenant slug"),
    )
}

/// A backend answering `status` to any `PUT` carrying `header`. Mounted ahead of the
/// catch-all in [`backend`], so an unconditional `PUT` still lands on the catch-all.
async fn answer_put_with(server: &MockServer, header: &'static str, status: u16) {
    Mock::given(method("PUT"))
        .and(header_exists(header))
        .respond_with(ResponseTemplate::new(status))
        .with_priority(1)
        .mount(server)
        .await;
}

/// The part every fake shares: a `PUT` without a precondition succeeds and a `DELETE`
/// answers `204`, as S3 does.
async fn backend() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"0123abcd\""))
        .with_priority(5)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    server
}

fn kb() -> KbSlug {
    KbSlug::try_new("notes").expect("kb slug")
}

fn scratch_key(request: &Request) -> &str {
    let path = request.url.path();
    path.strip_prefix(BUCKET_PREFIX)
        .unwrap_or_else(|| panic!("request outside the knowledge base's bucket: {path}"))
}

/// Every request touched one scratch key under `.notedthat/`, and exactly one of them
/// deleted it.
async fn assert_scratch_object_deleted(server: &MockServer) {
    let requests = server.received_requests().await.expect("request recording");
    let first = requests.first().expect("the check sent no request");
    let key = scratch_key(first).to_string();
    assert!(
        key.starts_with(PROBE_KEY_PREFIX),
        "scratch key {key} is not under {PROBE_KEY_PREFIX}"
    );
    for request in &requests {
        assert_eq!(scratch_key(request), key, "the check touched a second key");
    }
    let deletes = requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::DELETE)
        .count();
    assert_eq!(
        deletes, 1,
        "the scratch object must be deleted exactly once"
    );
    assert_eq!(
        requests.last().map(|r| r.method.clone()),
        Some(wiremock::http::Method::DELETE),
        "the delete must come last"
    );
}

#[tokio::test]
async fn a_backend_that_refuses_both_preconditions_enforces_them() {
    let server = backend().await;
    answer_put_with(&server, "if-none-match", 412).await;
    answer_put_with(&server, "if-match", 412).await;

    let found = storage_for(&server)
        .check_conditional_writes(&kb())
        .await
        .expect("check");

    assert_eq!(found, ConditionalWrites::Enforced);
    assert_scratch_object_deleted(&server).await;
}

#[tokio::test]
async fn a_backend_that_stores_every_put_enforces_neither() {
    let server = backend().await;

    let found = storage_for(&server)
        .check_conditional_writes(&kb())
        .await
        .expect("check");

    assert_eq!(
        found,
        ConditionalWrites::NotEnforced {
            if_match: true,
            if_none_match: true,
        }
    );
    assert_scratch_object_deleted(&server).await;
}

#[tokio::test]
async fn a_backend_that_enforces_only_if_match_is_reported_for_if_none_match() {
    let server = backend().await;
    answer_put_with(&server, "if-match", 412).await;

    let found = storage_for(&server)
        .check_conditional_writes(&kb())
        .await
        .expect("check");

    assert_eq!(
        found,
        ConditionalWrites::NotEnforced {
            if_match: false,
            if_none_match: true,
        }
    );
    assert_scratch_object_deleted(&server).await;
}

#[tokio::test]
async fn a_backend_that_enforces_only_if_none_match_is_reported_for_if_match() {
    let server = backend().await;
    answer_put_with(&server, "if-none-match", 412).await;

    let found = storage_for(&server)
        .check_conditional_writes(&kb())
        .await
        .expect("check");

    assert_eq!(
        found,
        ConditionalWrites::NotEnforced {
            if_match: true,
            if_none_match: false,
        }
    );
    assert_scratch_object_deleted(&server).await;
}

#[tokio::test]
async fn a_backend_that_answers_not_implemented_does_not_support_them() {
    let server = backend().await;
    answer_put_with(&server, "if-none-match", 501).await;
    answer_put_with(&server, "if-match", 501).await;

    let found = storage_for(&server)
        .check_conditional_writes(&kb())
        .await
        .expect("check");

    assert_eq!(found, ConditionalWrites::Unsupported);
    assert_scratch_object_deleted(&server).await;
}

#[tokio::test]
async fn a_failing_conditional_put_is_an_error_and_still_deletes_the_scratch_object() {
    let server = backend().await;
    answer_put_with(&server, "if-none-match", 500).await;

    let result = storage_for(&server).check_conditional_writes(&kb()).await;

    assert!(
        matches!(result, Err(StorageError::Other { .. })),
        "a 500 is not an answer about preconditions, got {result:?}"
    );
    assert_scratch_object_deleted(&server).await;
}

#[tokio::test]
async fn a_delete_that_fails_does_not_change_the_answer() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(header_exists("if-none-match"))
        .respond_with(ResponseTemplate::new(412))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(header_exists("if-match"))
        .respond_with(ResponseTemplate::new(412))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .with_priority(5)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let found = storage_for(&server)
        .check_conditional_writes(&kb())
        .await
        .expect("a failed cleanup is logged, not returned");

    assert_eq!(found, ConditionalWrites::Enforced);
}

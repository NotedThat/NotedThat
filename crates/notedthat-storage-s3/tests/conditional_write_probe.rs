//! Wire-level proof of what [`S3Storage::check_conditional_writes`] reads from a backend.
//!
//! The one backend in the test suite, `SeaweedFS` 4.18, enforces both preconditions, so
//! a backend that parses them and stores the object anyway (`SPECIFICATIONS.md` §8.1)
//! can only be shown here: each fake below answers a conditional `PUT` the way one kind
//! of backend does, and every test also pins that the scratch object is deleted.
//!
//! The probe sends three overwrites, and the fakes are written in those terms:
//! an `If-Match` naming the `ETag` the first `PUT` returned (which must be *stored*),
//! the same `ETag` with one hex digit altered (which must be *refused*), and
//! `If-None-Match: *` over the object that now exists (also refused). Asking only the
//! two that cannot hold is what let a backend which refuses every conditional write look
//! identical to one that enforces them.

use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use notedthat_core::{KbSlug, StorageError, TenantSlug};
use notedthat_storage_s3::{ConditionalWrites, PROBE_KEY_PREFIX, S3Storage};
use wiremock::matchers::{header, header_exists, method};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const BUCKET_PREFIX: &str = "/nt-test-notes/";

/// What [`backend`] returns from the unconditional `PUT`.
const REAL_ETAG: &str = "\"0123abcd\"";
/// [`REAL_ETAG`] with its first hex digit altered, which is what the probe derives and
/// sends as the `If-Match` that must not match. Spelled out rather than computed, so a
/// change to that derivation fails here rather than silently agreeing with itself.
const ALTERED_ETAG: &str = "\"1123abcd\"";

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

/// A backend answering `status` to the `PUT` carrying exactly this `If-Match` value.
async fn answer_if_match(server: &MockServer, value: &str, status: u16) {
    Mock::given(method("PUT"))
        .and(header("if-match", value))
        .respond_with(ResponseTemplate::new(status))
        .with_priority(1)
        .mount(server)
        .await;
}

/// A backend answering `status` to any `PUT` carrying `header`. Mounted ahead of the
/// catch-all in [`backend`], so an unconditional `PUT` still lands on the catch-all.
async fn answer_put_with(server: &MockServer, name: &'static str, status: u16) {
    Mock::given(method("PUT"))
        .and(header_exists(name))
        .respond_with(ResponseTemplate::new(status))
        .with_priority(2)
        .mount(server)
        .await;
}

/// The part every fake shares: a `PUT` without a precondition succeeds and returns an
/// `ETag`, and a `DELETE` answers `204`, as S3 does.
async fn backend() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", REAL_ETAG))
        .with_priority(5)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    server
}

/// A backend that honours a precondition that holds: the `If-Match` naming the real
/// `ETag` is stored. Every fake below mounts this unless it is the thing under test,
/// because a backend that refuses it is [`ConditionalWrites::AlwaysRefused`] and would
/// otherwise swallow the case the test means to make.
async fn honours_a_matching_if_match(server: &MockServer) {
    answer_if_match(server, REAL_ETAG, 200).await;
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
/// deleted it. The unversioned case: one `DELETE`, last.
async fn assert_scratch_object_deleted(server: &MockServer) {
    let deletes = assert_scratch_key_and_count_deletes(server).await;
    assert_eq!(
        deletes, 1,
        "the scratch object must be deleted exactly once on an unversioned bucket"
    );
}

/// The shared half: one key, under the reserved prefix, and the deletes come last.
/// Returns how many `DELETE`s there were.
async fn assert_scratch_key_and_count_deletes(server: &MockServer) -> usize {
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
    assert!(deletes >= 1, "the scratch object was never deleted");
    assert!(
        requests
            .iter()
            .rev()
            .take(deletes)
            .all(|r| r.method == wiremock::http::Method::DELETE),
        "every delete must come after the last write"
    );
    deletes
}

#[tokio::test]
async fn a_backend_that_honours_and_refuses_the_right_preconditions_enforces_them() {
    let server = backend().await;
    honours_a_matching_if_match(&server).await;
    answer_if_match(&server, ALTERED_ETAG, 412).await;
    answer_put_with(&server, "if-none-match", 412).await;

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
    honours_a_matching_if_match(&server).await;
    answer_if_match(&server, ALTERED_ETAG, 412).await;

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
    honours_a_matching_if_match(&server).await;
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

/// The combination that made the old three-way collapse report the opposite of the
/// truth: `501` to one header while the other is parsed and ignored. This is the old
/// AWS S3 behaviour several S3-compatibles copied, and an operator told "every write
/// carrying If-Match or If-None-Match would fail" would go looking for the wrong
/// problem while `If-Match` writes are being lost.
#[tokio::test]
async fn a_backend_that_ignores_one_header_and_rejects_the_other_reports_the_ignored_one() {
    let server = backend().await;
    answer_put_with(&server, "if-none-match", 501).await;

    let found = storage_for(&server)
        .check_conditional_writes(&kb())
        .await
        .expect("check");

    assert_eq!(
        found,
        ConditionalWrites::NotEnforced {
            if_match: true,
            if_none_match: false,
        },
        "a stored write outranks a 501: losing a write in silence is the worse finding"
    );
    assert_scratch_object_deleted(&server).await;
}

/// The case only a precondition that *holds* can find: every conditional `PUT` is
/// refused, including the `If-Match` naming the `ETag` the backend itself returned.
/// Answering `412` to everything is indistinguishable from enforcement unless something
/// sends a precondition that ought to succeed.
#[tokio::test]
async fn a_backend_that_refuses_even_a_matching_if_match_can_never_write_conditionally() {
    let server = backend().await;
    answer_put_with(&server, "if-match", 412).await;
    answer_put_with(&server, "if-none-match", 412).await;

    let found = storage_for(&server)
        .check_conditional_writes(&kb())
        .await
        .expect("check");

    assert_eq!(found, ConditionalWrites::AlwaysRefused);
    assert_scratch_object_deleted(&server).await;
}

#[tokio::test]
async fn a_failing_conditional_put_is_an_error_and_still_deletes_the_scratch_object() {
    let server = backend().await;
    honours_a_matching_if_match(&server).await;
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
        .and(header("if-match", REAL_ETAG))
        .respond_with(ResponseTemplate::new(200))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(header_exists("if-none-match"))
        .respond_with(ResponseTemplate::new(412))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(header_exists("if-match"))
        .respond_with(ResponseTemplate::new(412))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).insert_header("etag", REAL_ETAG))
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

/// On a versioned bucket a plain `DeleteObject` only adds a delete marker, so every
/// version the probe wrote would stay stored — billed, listed by `ListObjectVersions`,
/// and swept up by backup tooling — once per knowledge base per restart, under a key
/// that is never reused. Each one is removed by id, and so is the marker.
#[tokio::test]
async fn on_a_versioned_bucket_every_version_the_probe_wrote_is_deleted() {
    let server = MockServer::start().await;
    // Each stored PUT reports a version id; the probe stores two here (the
    // unconditional one and the matching `If-Match`).
    Mock::given(method("PUT"))
        .and(header("if-match", REAL_ETAG))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", REAL_ETAG)
                .insert_header("x-amz-version-id", "v2"),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(header_exists("if-match"))
        .respond_with(ResponseTemplate::new(412))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(header_exists("if-none-match"))
        .respond_with(ResponseTemplate::new(412))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", REAL_ETAG)
                .insert_header("x-amz-version-id", "v1"),
        )
        .with_priority(5)
        .mount(&server)
        .await;
    // The unversioned delete adds a marker, which has a version of its own.
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204).insert_header("x-amz-version-id", "marker"))
        .mount(&server)
        .await;

    let found = storage_for(&server)
        .check_conditional_writes(&kb())
        .await
        .expect("check");
    assert_eq!(found, ConditionalWrites::Enforced);

    let deletes = assert_scratch_key_and_count_deletes(&server).await;
    assert_eq!(deletes, 4, "the marker-adding delete, then one per version");

    let requests = server.received_requests().await.expect("request recording");
    let removed: Vec<String> = requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::DELETE)
        .filter_map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "versionId")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    assert_eq!(
        removed,
        ["v1", "v2", "marker"],
        "every version the probe wrote, and the delete marker, removed by id"
    );
}

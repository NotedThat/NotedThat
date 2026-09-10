//! The storage integration suite, written once and run against every `Storage` backend.
//!
//! Each scenario here is an *absolute* assertion about one `Storage` implementation:
//! "a wrong `If-Match` is a `PreconditionFailed`", "a range read reports an inclusive
//! `Content-Range`". That is the difference between this file and its neighbour
//! `conformance.rs`, which only asserts that two backends *agree* — two backends can
//! agree on the wrong answer, and this is what says they do not.
//!
//! Every scenario takes `&dyn Storage` and a knowledge base of its own, so the same body
//! runs unchanged over a real filesystem, over a real S3 server, and over the in-memory
//! substitute. The three expansions live in `storage_integration_local.rs`,
//! `storage_integration_s3.rs` and `storage_integration_memory.rs`; none of them contains
//! a test body, only a fixture and one macro call.
//!
//! # Adding a scenario
//!
//! Write the `async fn` here, then add its name to [`storage_integration_scenarios!`].
//! Every backend picks it up; nothing else needs editing. The name becomes the knowledge
//! base slug, so keep it to 40 characters and `[a-z0-9_]` — [`KbSlug`] enforces that, and
//! [`kb_for`] converts the underscores.
//!
//! # `ETag`s are never compared across backends
//!
//! S3 returns an MD5-shaped digest and the filesystem backend a SHA-256 one, so a
//! scenario may assert an `ETag`'s *shape*, or that two of them are equal *within one
//! backend*, but never a literal value.
//!
//! A single-shot PUT *does* return one on every backend here, `SeaweedFS` included —
//! [`put_returns_etag`] is what says so, and it is an absolute assertion like the rest.
//! Scenarios that need an existing object's `ETag` still read it back through
//! [`etag_of`], but only because the [`put`] helper they seed with discards its
//! `PutOutcome`; it is not a hedge against a backend withholding one.

#![allow(dead_code)]

use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, CopyObjectOptions, KbManifest, KbSlug, ObjectPath, Storage,
    StorageError, TenantSlug,
};

/// Every scenario in this file, expanded by whatever `$emit` the caller supplies.
///
/// The list is the single place a backend's test binary learns what to run, which is what
/// keeps the expansions from drifting apart: a scenario cannot be added to one backend
/// and forgotten on the others.
macro_rules! storage_integration_scenarios {
    ($emit:ident) => {
        $emit!(round_trip_put_get_head_delete_list);
        $emit!(ensure_bucket_is_idempotent);
        $emit!(manifest_read_write);
        $emit!(get_full_returns_etag);
        $emit!(get_range_returns_partial);
        $emit!(put_returns_etag);
        $emit!(put_if_match_wrong_fails);
        $emit!(put_if_none_match_star_conflict_fails);
        $emit!(get_if_none_match_not_modified);
        $emit!(get_range_unsatisfiable);
        $emit!(delete_if_match_wrong_fails);
        $emit!(get_malformed_date_rejected);
        $emit!(get_suffix_range_larger_than_object);
        $emit!(head_returns_etag);
        $emit!(head_if_none_match_not_modified);
        $emit!(put_if_match_correct_succeeds);
        $emit!(put_if_none_match_star_creates);
        $emit!(delete_if_match_correct_succeeds);
        $emit!(content_range_reflects_object_size);
        $emit!(copy_enforces_both_preconditions);
        $emit!(list_objects_pagination_walks_cursor);
    };
}

pub(crate) use storage_integration_scenarios;

/// The knowledge base a scenario runs in, derived from its own name.
///
/// One knowledge base per scenario is what lets the S3 expansion share a single container
/// between tests that run concurrently: they never touch each other's bucket.
///
/// # Panics
///
/// If the scenario name is not a valid [`KbSlug`] once underscores become hyphens —
/// in practice, if it exceeds 40 characters.
pub fn kb_for(scenario: &str) -> KbSlug {
    let slug = scenario.replace('_', "-");
    KbSlug::try_new(slug).unwrap_or_else(|error| {
        panic!("scenario name `{scenario}` is not a usable knowledge base slug: {error}")
    })
}

fn path(key: &str) -> ObjectPath {
    ObjectPath::try_from_str(key).expect("test key is a valid ObjectPath")
}

/// Assert the shape every backend's `ETag` must have: a quoted lowercase hex digest.
///
/// The digest itself differs per backend (S3 hands one back, the filesystem derives a
/// SHA-256), so the shape is all that is portable — and it is what RFC 7232 §2.3
/// requires callers to be able to echo back verbatim.
fn assert_quoted_lower_hex_etag(etag: &str) {
    let Some(hex) = etag.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        panic!("ETag should match ^\"[0-9a-f]+\"$: {etag}");
    };
    assert!(!hex.is_empty(), "ETag hex payload should not be empty");
    assert!(
        hex.bytes()
            .all(|byte| byte.is_ascii_hexdigit()
                && (byte.is_ascii_digit() || byte.is_ascii_lowercase())),
        "ETag should match ^\"[0-9a-f]+\"$: {etag}"
    );
}

/// The `ETag` of an object, read back through HEAD.
///
/// Not a fallback: [`put`] throws its `PutOutcome` away, so HEAD is where the `ETag` of
/// an already-seeded object comes from. Whether PUT reports one is a separate question,
/// and [`put_returns_etag`] is the scenario that answers it.
async fn etag_of(store: &dyn Storage, kb: &KbSlug, key: &str) -> String {
    store
        .head_object(kb, &path(key), ConditionalHeaders::default())
        .await
        .expect("head for etag")
        .etag
        .expect("backend should report an ETag on HEAD")
}

async fn put(store: &dyn Storage, kb: &KbSlug, key: &str, body: &'static [u8], content_type: &str) {
    store
        .put_object(
            kb,
            &path(key),
            Bytes::from_static(body),
            Some(content_type),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put_object");
}

// ─── Scenarios ──────────────────────────────────────────────────────────────

pub async fn round_trip_put_get_head_delete_list(store: &dyn Storage, kb: &KbSlug) {
    let key = "hello.md";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    put(store, kb, key, b"# Hello", "text/markdown").await;

    let meta = store
        .head_object(kb, &path(key), ConditionalHeaders::default())
        .await
        .expect("head_object");
    assert_eq!(meta.size, 7);
    assert_eq!(meta.key, key);

    let read = store
        .get_object(kb, &path(key), None, ConditionalHeaders::default())
        .await
        .expect("get_object");
    assert_eq!(&read.bytes[..], b"# Hello");

    let list = store
        .list_objects(kb, None, 10, None)
        .await
        .expect("list_objects");
    assert!(
        list.objects.iter().any(|object| object.key == key),
        "{key} should appear in list"
    );

    store
        .delete_object(kb, &path(key), ConditionalHeaders::default())
        .await
        .expect("delete_object");
    store
        .delete_object(kb, &path(key), ConditionalHeaders::default())
        .await
        .expect("delete_object is idempotent");

    assert!(
        matches!(
            store
                .head_object(kb, &path(key), ConditionalHeaders::default())
                .await,
            Err(StorageError::NotFound { .. })
        ),
        "HEAD after DELETE should be NotFound"
    );
}

pub async fn ensure_bucket_is_idempotent(store: &dyn Storage, kb: &KbSlug) {
    store.ensure_bucket(kb).await.expect("first ensure_bucket");
    store
        .ensure_bucket(kb)
        .await
        .expect("second ensure_bucket is idempotent");
}

pub async fn manifest_read_write(store: &dyn Storage, kb: &KbSlug) {
    use std::time::{SystemTime, UNIX_EPOCH};

    store.ensure_bucket(kb).await.expect("ensure_bucket");

    assert!(
        matches!(
            store.read_manifest(kb).await,
            Err(StorageError::NotFound { .. })
        ),
        "manifest should not exist before first write"
    );

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let created_at = i64::try_from(secs).unwrap_or(i64::MAX);
    let manifest = KbManifest::new_v1(&TenantSlug::default(), kb, "Manifest Test KB", created_at);
    store
        .write_manifest(kb, &manifest)
        .await
        .expect("write_manifest");

    let read = store.read_manifest(kb).await.expect("read_manifest");
    assert_eq!(read.kb_slug.as_str(), kb.as_str());
    assert_eq!(read.manifest_version, 1);
}

pub async fn get_full_returns_etag(store: &dyn Storage, kb: &KbSlug) {
    let key = "full-etag.bin";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    put(
        store,
        kb,
        key,
        b"0123456789012345678901",
        "application/octet-stream",
    )
    .await;

    let read = store
        .get_object(kb, &path(key), None, ConditionalHeaders::default())
        .await
        .expect("get_object");

    assert!(read.meta.etag.is_some(), "GET should populate ETag");
    assert_eq!(read.content_range, None);
    assert_eq!(read.bytes.len(), 22);
}

pub async fn get_range_returns_partial(store: &dyn Storage, kb: &KbSlug) {
    let key = "range.bin";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    store
        .put_object(
            kb,
            &path(key),
            Bytes::from((0_u8..100).collect::<Vec<_>>()),
            Some("application/octet-stream"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put_object");

    let read = store
        .get_object(
            kb,
            &path(key),
            Some(vec![ByteRange::FromStart {
                first: 10,
                last: 19,
            }]),
            ConditionalHeaders::default(),
        )
        .await
        .expect("range get_object");

    assert_eq!(read.content_range, Some("bytes 10-19/100".to_string()));
    assert_eq!(read.bytes.len(), 10);
    assert!(read.meta.etag.is_some(), "range GET should populate ETag");
}

pub async fn put_returns_etag(store: &dyn Storage, kb: &KbSlug) {
    let key = "put-etag.bin";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    let outcome = store
        .put_object(
            kb,
            &path(key),
            Bytes::from_static(b"etag please"),
            Some("application/octet-stream"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put_object");

    let etag = outcome.etag.expect("PUT should return ETag");
    assert_quoted_lower_hex_etag(&etag);
}

pub async fn put_if_match_wrong_fails(store: &dyn Storage, kb: &KbSlug) {
    let key = "conditional.txt";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    put(store, kb, key, b"initial", "text/plain").await;

    let error = store
        .put_object(
            kb,
            &path(key),
            Bytes::from_static(b"replacement"),
            Some("text/plain"),
            ConditionalHeaders {
                if_match: Some("\"wrong-etag\"".to_string()),
                ..ConditionalHeaders::default()
            },
        )
        .await
        .expect_err("wrong If-Match should fail");

    assert!(matches!(error, StorageError::PreconditionFailed));

    let kept = store
        .get_object(kb, &path(key), None, ConditionalHeaders::default())
        .await
        .expect("read after refused write");
    assert_eq!(
        &kept.bytes[..],
        b"initial",
        "a refused write must not have replaced the body"
    );
}

pub async fn put_if_none_match_star_conflict_fails(store: &dyn Storage, kb: &KbSlug) {
    let key = "conditional.txt";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    put(store, kb, key, b"initial", "text/plain").await;

    let error = store
        .put_object(
            kb,
            &path(key),
            Bytes::from_static(b"replacement"),
            Some("text/plain"),
            ConditionalHeaders {
                if_none_match: Some("*".to_string()),
                ..ConditionalHeaders::default()
            },
        )
        .await
        .expect_err("If-None-Match: * should fail when object exists");

    assert!(matches!(error, StorageError::PreconditionFailed));
}

pub async fn get_if_none_match_not_modified(store: &dyn Storage, kb: &KbSlug) {
    let key = "conditional.txt";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    put(store, kb, key, b"etag me", "text/plain").await;
    let etag = etag_of(store, kb, key).await;

    let Err(error) = store
        .get_object(
            kb,
            &path(key),
            None,
            ConditionalHeaders {
                if_none_match: Some(etag),
                ..ConditionalHeaders::default()
            },
        )
        .await
    else {
        panic!("matching If-None-Match should return NotModified");
    };

    assert!(matches!(error, StorageError::NotModified));
}

pub async fn get_range_unsatisfiable(store: &dyn Storage, kb: &KbSlug) {
    let key = "range.bin";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    store
        .put_object(
            kb,
            &path(key),
            Bytes::from(vec![b'x'; 100]),
            Some("application/octet-stream"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");

    let Err(error) = store
        .get_object(
            kb,
            &path(key),
            Some(vec![ByteRange::FromStart {
                first: 200,
                last: 300,
            }]),
            ConditionalHeaders::default(),
        )
        .await
    else {
        panic!("unsatisfiable range should be refused");
    };

    assert!(matches!(
        error,
        StorageError::RangeNotSatisfiable {
            complete_length: 100
        }
    ));
}

pub async fn delete_if_match_wrong_fails(store: &dyn Storage, kb: &KbSlug) {
    let key = "conditional.txt";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    put(store, kb, key, b"initial", "text/plain").await;

    let error = store
        .delete_object(
            kb,
            &path(key),
            ConditionalHeaders {
                if_match: Some("\"wrong-etag\"".to_string()),
                ..ConditionalHeaders::default()
            },
        )
        .await
        .expect_err("wrong If-Match should fail delete");

    assert!(matches!(error, StorageError::PreconditionFailed));
    assert!(
        store
            .head_object(kb, &path(key), ConditionalHeaders::default())
            .await
            .is_ok(),
        "a refused delete must leave the object in place"
    );
}

pub async fn get_malformed_date_rejected(store: &dyn Storage, kb: &KbSlug) {
    let key = "conditional.txt";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    put(store, kb, key, b"initial", "text/plain").await;

    let Err(error) = store
        .get_object(
            kb,
            &path(key),
            None,
            ConditionalHeaders {
                if_modified_since: Some("not-a-date".to_string()),
                ..ConditionalHeaders::default()
            },
        )
        .await
    else {
        panic!("malformed HTTP-date should be refused");
    };

    assert!(matches!(error, StorageError::Other { .. }));
}

pub async fn get_suffix_range_larger_than_object(store: &dyn Storage, kb: &KbSlug) {
    let key = "suffix.bin";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    store
        .put_object(
            kb,
            &path(key),
            Bytes::from(vec![b'z'; 50]),
            Some("application/octet-stream"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");

    // A suffix longer than the object may be clamped to the whole body or refused;
    // both are allowed. What must not happen is a panic or a different error variant.
    match store
        .get_object(
            kb,
            &path(key),
            Some(vec![ByteRange::Suffix { length: 9999 }]),
            ConditionalHeaders::default(),
        )
        .await
    {
        Ok(read) => assert_eq!(
            read.bytes.len(),
            50,
            "suffix beyond file end should return all bytes"
        ),
        Err(StorageError::RangeNotSatisfiable {
            complete_length: 50,
        }) => {}
        Err(error) => panic!("unexpected error on oversized suffix range: {error:?}"),
    }
}

pub async fn head_returns_etag(store: &dyn Storage, kb: &KbSlug) {
    let key = "head-etag.txt";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    put(store, kb, key, b"head etag test", "text/plain").await;

    let meta = store
        .head_object(kb, &path(key), ConditionalHeaders::default())
        .await
        .expect("head_object");

    let etag = meta.etag.expect("HEAD should populate ETag");
    assert_quoted_lower_hex_etag(&etag);
}

pub async fn head_if_none_match_not_modified(store: &dyn Storage, kb: &KbSlug) {
    let key = "head-304.txt";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    put(store, kb, key, b"conditional head", "text/plain").await;
    let etag = etag_of(store, kb, key).await;

    let Err(error) = store
        .head_object(
            kb,
            &path(key),
            ConditionalHeaders {
                if_none_match: Some(etag),
                ..ConditionalHeaders::default()
            },
        )
        .await
    else {
        panic!("matching If-None-Match on HEAD should return NotModified");
    };

    assert!(matches!(error, StorageError::NotModified));
}

pub async fn put_if_match_correct_succeeds(store: &dyn Storage, kb: &KbSlug) {
    let key = "conditional-put.txt";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    put(store, kb, key, b"original content", "text/plain").await;
    let etag = etag_of(store, kb, key).await;

    let outcome = store
        .put_object(
            kb,
            &path(key),
            Bytes::from_static(b"updated content differs"),
            Some("text/plain"),
            ConditionalHeaders {
                if_match: Some(etag),
                ..ConditionalHeaders::default()
            },
        )
        .await
        .expect("conditional PUT with correct If-Match should succeed");

    assert!(
        outcome.etag.is_some(),
        "successful conditional PUT should return ETag"
    );
    let read = store
        .get_object(kb, &path(key), None, ConditionalHeaders::default())
        .await
        .expect("read after conditional write");
    assert_eq!(&read.bytes[..], b"updated content differs");
}

pub async fn put_if_none_match_star_creates(store: &dyn Storage, kb: &KbSlug) {
    let key = "new-object.txt";
    store.ensure_bucket(kb).await.expect("ensure_bucket");

    // The object does not exist yet, so `If-None-Match: *` means "create only".
    let outcome = store
        .put_object(
            kb,
            &path(key),
            Bytes::from_static(b"brand new object"),
            Some("text/plain"),
            ConditionalHeaders {
                if_none_match: Some("*".to_string()),
                ..ConditionalHeaders::default()
            },
        )
        .await
        .expect("If-None-Match: * on a new object should succeed");

    assert!(outcome.etag.is_some(), "successful PUT should return ETag");
}

pub async fn delete_if_match_correct_succeeds(store: &dyn Storage, kb: &KbSlug) {
    let key = "delete-conditional.txt";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    put(store, kb, key, b"to be deleted", "text/plain").await;
    let etag = etag_of(store, kb, key).await;

    store
        .delete_object(
            kb,
            &path(key),
            ConditionalHeaders {
                if_match: Some(etag),
                ..ConditionalHeaders::default()
            },
        )
        .await
        .expect("DELETE with correct If-Match should succeed");

    assert!(
        matches!(
            store
                .head_object(kb, &path(key), ConditionalHeaders::default())
                .await,
            Err(StorageError::NotFound { .. })
        ),
        "object should be absent after conditional delete"
    );
}

pub async fn content_range_reflects_object_size(store: &dyn Storage, kb: &KbSlug) {
    let key = "large.bin";
    store.ensure_bucket(kb).await.expect("ensure_bucket");
    store
        .put_object(
            kb,
            &path(key),
            Bytes::from((0_u8..=255).cycle().take(1000).collect::<Vec<_>>()),
            Some("application/octet-stream"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put 1000-byte object");

    let read = store
        .get_object(
            kb,
            &path(key),
            Some(vec![ByteRange::FromStart {
                first: 100,
                last: 199,
            }]),
            ConditionalHeaders::default(),
        )
        .await
        .expect("range GET 100-199");

    assert_eq!(
        read.content_range,
        Some("bytes 100-199/1000".to_string()),
        "Content-Range should match the requested range over the total object size"
    );
    assert_eq!(
        read.bytes.len(),
        100,
        "range response should contain exactly 100 bytes"
    );
}

/// Server-side copy, with a precondition on each end and a key neither backend can
/// store verbatim: a space and a non-ASCII character, which S3 must percent-encode in
/// its copy-source header and the filesystem must write as-is.
pub async fn copy_enforces_both_preconditions(store: &dyn Storage, kb: &KbSlug) {
    let source = path("folder/source file.md");
    let copied = path("copied/文 copy.md");
    let existing = path("existing.md");
    let rejected = path("wrong-source-etag.md");

    store.ensure_bucket(kb).await.expect("ensure bucket");
    store
        .put_object(
            kb,
            &source,
            Bytes::from_static(b"# encoded source\n"),
            Some("text/plain"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put encoded source");
    let source_etag = etag_of(store, kb, "folder/source file.md").await;

    let copied_outcome = store
        .copy_object(
            kb,
            &source,
            &copied,
            CopyObjectOptions {
                source_if_match: Some(source_etag.clone()),
                destination_if_none_match: Some("*".into()),
                content_type: Some("text/plain".into()),
            },
        )
        .await
        .expect("conditional native copy");
    let copied_etag = copied_outcome.etag.expect("copied etag");
    assert_quoted_lower_hex_etag(&copied_etag);
    assert_eq!(
        copied_etag, source_etag,
        "a copy holds the same bytes, so it must carry the same ETag"
    );
    let copied_read = store
        .get_object(kb, &copied, None, ConditionalHeaders::default())
        .await
        .expect("read encoded destination");
    assert_eq!(copied_read.bytes, Bytes::from_static(b"# encoded source\n"));
    assert_eq!(copied_read.meta.content_type.as_deref(), Some("text/plain"));

    store
        .put_object(
            kb,
            &existing,
            Bytes::from_static(b"preserve destination"),
            Some("text/plain"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put existing destination");
    let existing_error = store
        .copy_object(
            kb,
            &source,
            &existing,
            CopyObjectOptions {
                source_if_match: Some(source_etag.clone()),
                destination_if_none_match: Some("*".into()),
                content_type: Some("text/plain".into()),
            },
        )
        .await
        .expect_err("existing destination must reject create-only copy");
    assert!(matches!(existing_error, StorageError::PreconditionFailed));
    let preserved = store
        .get_object(kb, &existing, None, ConditionalHeaders::default())
        .await
        .expect("read preserved destination");
    assert_eq!(preserved.bytes, Bytes::from_static(b"preserve destination"));
    assert_eq!(preserved.meta.content_type.as_deref(), Some("text/plain"));

    let source_error = store
        .copy_object(
            kb,
            &source,
            &rejected,
            CopyObjectOptions {
                source_if_match: Some("\"wrong-etag\"".into()),
                destination_if_none_match: Some("*".into()),
                content_type: Some("text/plain".into()),
            },
        )
        .await
        .expect_err("wrong source etag must reject copy");
    assert!(matches!(source_error, StorageError::PreconditionFailed));
    assert!(matches!(
        store
            .head_object(kb, &rejected, ConditionalHeaders::default())
            .await,
        Err(StorageError::NotFound { .. })
    ));
}

/// Walking a truncated listing must yield every key exactly once, in order.
///
/// This is the absolute half of the pagination contract, and it is the one no single
/// backend can be excused from: `truncated` and `next_cursor` have to agree, a cursor has
/// to resume *after* the key it names, and the pages have to compose back into the seeded
/// set with nothing dropped and nothing repeated. `S3Storage` derives all of that from
/// `is_truncated` and `NextContinuationToken`, `FsStorage` from an encoded walk position
/// and `InMemoryStorage` from a key comparison, so agreeing is not the same as being
/// right — `storage_conformance_*.rs` compares the three, this pins them.
pub async fn list_objects_pagination_walks_cursor(store: &dyn Storage, kb: &KbSlug) {
    const SEEDED: u32 = 25;
    const PAGE: u32 = 10;

    store.ensure_bucket(kb).await.expect("ensure_bucket");

    let expected: Vec<String> = (0..SEEDED).map(|i| format!("page/doc-{i:04}.md")).collect();
    for key in &expected {
        store
            .put_object(
                kb,
                &path(key),
                Bytes::from_static(b"x"),
                Some("text/markdown"),
                ConditionalHeaders::default(),
            )
            .await
            .expect("seed a page key");
    }

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages: u32 = 0;
    loop {
        let page = store
            .list_objects(kb, Some("page/"), PAGE, cursor.as_deref())
            .await
            .expect("list a page");
        assert_eq!(
            page.truncated,
            page.next_cursor.is_some(),
            "truncated must mean a cursor and a cursor must mean truncated"
        );
        let returned = u32::try_from(page.objects.len()).expect("a page fits in u32");
        assert!(
            returned <= PAGE,
            "a page must not exceed the requested limit: {returned} > {PAGE}"
        );
        seen.extend(page.objects.into_iter().map(|object| object.key));
        pages += 1;
        assert!(pages <= SEEDED, "pagination did not terminate");
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    assert!(
        pages > 1,
        "a {PAGE}-key page over {SEEDED} keys should have truncated at least once"
    );
    assert_eq!(
        seen, expected,
        "the pages should compose back into the seeded keys, in order, exactly once each"
    );

    // A limit that covers the whole prefix must not claim a further page. The wrong answer
    // here is the one `S3Storage` fails closed on — `is_truncated=true` with no
    // `NextContinuationToken` is a `BackendUnavailable`, not an empty final page.
    let whole = store
        .list_objects(kb, Some("page/"), SEEDED, None)
        .await
        .expect("list the whole prefix");
    assert!(
        !whole.truncated,
        "a page covering every key is not truncated"
    );
    assert_eq!(whole.next_cursor, None);
    assert_eq!(whole.objects.len(), expected.len());

    // The cursor names a key, not an index: deleting that key must not break a listing
    // already in flight. WebDAV pages a whole knowledge base in a loop, so a concurrent
    // delete has to leave the walk resumable.
    let first = store
        .list_objects(kb, Some("page/"), 2, None)
        .await
        .expect("first page of two");
    let cursor = first.next_cursor.expect("two of twenty-five is truncated");
    let named = first.objects.last().expect("a second key").key.clone();
    store
        .delete_object(kb, &path(&named), ConditionalHeaders::default())
        .await
        .expect("delete the key the cursor names");
    let resumed = store
        .list_objects(kb, Some("page/"), 2, Some(&cursor))
        .await
        .expect("a cursor survives deletion of the key it names");
    assert_eq!(
        resumed
            .objects
            .iter()
            .map(|o| o.key.as_str())
            .collect::<Vec<_>>(),
        [expected[2].as_str(), expected[3].as_str()],
        "resuming must continue after the deleted key, not restart"
    );
}

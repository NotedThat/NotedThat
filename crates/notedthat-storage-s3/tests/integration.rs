//! Integration tests for `S3Storage` against a real `SeaweedFS` instance.
//!
//! These tests are marked `#[ignore]` because they require Docker. Run with:
//! ```sh
//! cargo test -p notedthat-storage-s3 --locked -- --include-ignored
//! ```
#![allow(missing_docs)]

use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, KbManifest, KbSlug, ObjectPath, Storage, StorageError,
    TenantSlug,
};
use notedthat_storage_s3::{S3Config, S3Storage};
use testcontainers::{
    GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

async fn start_seaweedfs() -> (impl std::any::Any, String) {
    let container = GenericImage::new("chrislusf/seaweedfs", "4.18")
        .with_exposed_port(8333_u16.tcp())
        .with_wait_for(WaitFor::seconds(5))
        .with_cmd(["server", "-s3", "-filer"])
        .start()
        .await
        .expect("failed to start SeaweedFS testcontainer");
    let port = container
        .get_host_port_ipv4(8333_u16)
        .await
        .expect("failed to get SeaweedFS port");
    (container, format!("http://127.0.0.1:{port}"))
}

fn make_config(endpoint: &str) -> S3Config {
    S3Config {
        endpoint_url: Some(endpoint.to_string()),
        region: "us-east-1".to_string(),
        access_key_id: "any".to_string(),
        secret_access_key: "any".to_string(),
        force_path_style: true,
    }
}

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

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn integration_round_trip_put_get_head_delete_list() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("integration-test").unwrap();
    let path = ObjectPath::try_from_str("hello.md").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");

    storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"# Hello"),
            Some("text/markdown"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put_object");

    let meta = storage
        .head_object(&kb, &path, ConditionalHeaders::default())
        .await
        .expect("head_object");
    assert_eq!(meta.size, 7);
    assert_eq!(meta.key, "hello.md");

    let read = storage
        .get_object(&kb, &path, None, ConditionalHeaders::default())
        .await
        .expect("get_object");
    assert_eq!(&read.bytes[..], b"# Hello");

    let list = storage
        .list_objects(&kb, None, 10)
        .await
        .expect("list_objects");
    assert!(
        list.objects.iter().any(|o| o.key == "hello.md"),
        "hello.md should appear in list"
    );

    storage
        .delete_object(&kb, &path, ConditionalHeaders::default())
        .await
        .expect("delete_object");
    storage
        .delete_object(&kb, &path, ConditionalHeaders::default())
        .await
        .expect("delete_object is idempotent");

    assert!(
        storage
            .head_object(&kb, &path, ConditionalHeaders::default())
            .await
            .is_err(),
        "HEAD after DELETE should return an error"
    );
}

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn integration_ensure_bucket_idempotent() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("idempotent-test").unwrap();

    storage
        .ensure_bucket(&kb)
        .await
        .expect("first ensure_bucket");
    storage
        .ensure_bucket(&kb)
        .await
        .expect("second ensure_bucket is idempotent");
}

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn integration_manifest_read_write() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let (_container, endpoint) = start_seaweedfs().await;
    let tenant = TenantSlug::default();
    let storage = S3Storage::new(make_config(&endpoint).build_client(), tenant.clone());
    let kb = KbSlug::try_new("manifest-test").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");

    assert!(
        storage.read_manifest(&kb).await.is_err(),
        "manifest should not exist before first write"
    );

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let created_at = i64::try_from(secs).unwrap_or(i64::MAX);
    let manifest = KbManifest::new_v1(&tenant, &kb, "Manifest Test KB", created_at);
    storage
        .write_manifest(&kb, &manifest)
        .await
        .expect("write_manifest");

    let read = storage.read_manifest(&kb).await.expect("read_manifest");
    assert_eq!(read.kb_slug.as_str(), "manifest-test");
    assert_eq!(read.manifest_version, 1);
}

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn m3_get_object_full_returns_etag() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-full-get-etag").unwrap();
    let path = ObjectPath::try_from_str("full-etag.bin").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"0123456789012345678901"),
            Some("application/octet-stream"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put_object");

    let read = storage
        .get_object(&kb, &path, None, ConditionalHeaders::default())
        .await
        .expect("get_object");

    assert!(read.meta.etag.is_some(), "GET should populate ETag");
    assert_eq!(read.content_range, None);
    assert_eq!(read.bytes.len(), 22);
}

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn m3_get_object_range_returns_partial() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-range-get").unwrap();
    let path = ObjectPath::try_from_str("range.bin").unwrap();
    let bytes = Bytes::from((0_u8..100).collect::<Vec<_>>());

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    storage
        .put_object(
            &kb,
            &path,
            bytes,
            Some("application/octet-stream"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put_object");

    let read = storage
        .get_object(
            &kb,
            &path,
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

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn m3_put_object_returns_etag() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-put-etag").unwrap();
    let path = ObjectPath::try_from_str("put-etag.bin").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    let outcome = storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"etag please"),
            Some("application/octet-stream"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put_object");

    let etag = outcome.etag.expect("PUT should return ETag");
    assert_quoted_lower_hex_etag(&etag);
}

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn m3_put_if_match_wrong_412() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-put-if-match").unwrap();
    let path = ObjectPath::try_from_str("conditional.txt").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"initial"),
            Some("text/plain"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("initial put");

    let err = storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"replacement"),
            Some("text/plain"),
            ConditionalHeaders {
                if_match: Some("\"wrong-etag\"".to_string()),
                ..ConditionalHeaders::default()
            },
        )
        .await
        .expect_err("wrong If-Match should fail");

    assert!(matches!(err, StorageError::PreconditionFailed));
}

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn m3_put_if_none_match_wildcard_conflict_412() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-put-if-none-match").unwrap();
    let path = ObjectPath::try_from_str("conditional.txt").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"initial"),
            Some("text/plain"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("initial put");

    let err = storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"replacement"),
            Some("text/plain"),
            ConditionalHeaders {
                if_none_match: Some("*".to_string()),
                ..ConditionalHeaders::default()
            },
        )
        .await
        .expect_err("If-None-Match: * should fail when object exists");

    assert!(matches!(err, StorageError::PreconditionFailed));
}

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn m3_get_if_none_match_304() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-get-if-none-match").unwrap();
    let path = ObjectPath::try_from_str("conditional.txt").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    let put = storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"etag me"),
            Some("text/plain"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");
    let etag = if let Some(etag) = put.etag {
        etag
    } else {
        storage
            .head_object(&kb, &path, ConditionalHeaders::default())
            .await
            .expect("head for etag")
            .etag
            .expect("etag from head")
    };

    let Err(err) = storage
        .get_object(
            &kb,
            &path,
            None,
            ConditionalHeaders {
                if_none_match: Some(etag),
                ..ConditionalHeaders::default()
            },
        )
        .await
    else {
        panic!("matching If-None-Match should return 304");
    };

    assert!(matches!(err, StorageError::NotModified));
}

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn m3_get_range_416() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-get-range-416").unwrap();
    let path = ObjectPath::try_from_str("range.bin").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    storage
        .put_object(
            &kb,
            &path,
            Bytes::from(vec![b'x'; 100]),
            Some("application/octet-stream"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");

    let Err(err) = storage
        .get_object(
            &kb,
            &path,
            Some(vec![ByteRange::FromStart {
                first: 200,
                last: 300,
            }]),
            ConditionalHeaders::default(),
        )
        .await
    else {
        panic!("unsatisfiable range should return 416");
    };

    assert!(matches!(
        err,
        StorageError::RangeNotSatisfiable {
            complete_length: 100
        }
    ));
}

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn m3_delete_if_match_wrong_412() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-delete-if-match").unwrap();
    let path = ObjectPath::try_from_str("conditional.txt").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"initial"),
            Some("text/plain"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");

    let err = storage
        .delete_object(
            &kb,
            &path,
            ConditionalHeaders {
                if_match: Some("\"wrong-etag\"".to_string()),
                ..ConditionalHeaders::default()
            },
        )
        .await
        .expect_err("wrong If-Match should fail delete");

    assert!(matches!(err, StorageError::PreconditionFailed));
}

#[tokio::test]
#[ignore = "requires SeaweedFS testcontainer"]
async fn m3_get_bad_date() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-get-bad-date").unwrap();
    let path = ObjectPath::try_from_str("conditional.txt").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"initial"),
            Some("text/plain"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");

    let Err(err) = storage
        .get_object(
            &kb,
            &path,
            None,
            ConditionalHeaders {
                if_modified_since: Some("not-a-date".to_string()),
                ..ConditionalHeaders::default()
            },
        )
        .await
    else {
        panic!("malformed HTTP-date should return StorageError::Other");
    };

    assert!(matches!(err, StorageError::Other { .. }));
}

#[tokio::test]
#[ignore = "requires SeaweedFS 4.18 in Docker"]
async fn m3_get_object_range_suffix_clamped() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-range-suffix").unwrap();
    let path = ObjectPath::try_from_str("suffix.bin").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    storage
        .put_object(
            &kb,
            &path,
            Bytes::from(vec![b'z'; 50]),
            Some("application/octet-stream"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");

    // Suffix range much larger than object — S3 may return all bytes (clamped) or 416.
    // Either is acceptable; what must NOT happen is a panic or unexpected error variant.
    match storage
        .get_object(
            &kb,
            &path,
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
        }) => {
            // Also acceptable per S3 implementation semantics
        }
        Err(e) => panic!("unexpected error on oversized suffix range: {e:?}"),
    }
}

#[tokio::test]
#[ignore = "requires SeaweedFS 4.18 in Docker"]
async fn m3_head_object_returns_etag() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-head-etag").unwrap();
    let path = ObjectPath::try_from_str("head-etag.txt").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"head etag test"),
            Some("text/plain"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");

    let meta = storage
        .head_object(&kb, &path, ConditionalHeaders::default())
        .await
        .expect("head_object");

    assert!(meta.etag.is_some(), "HEAD should populate ETag");
}

#[tokio::test]
#[ignore = "requires SeaweedFS 4.18 in Docker"]
async fn m3_head_if_none_match_304() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-head-304").unwrap();
    let path = ObjectPath::try_from_str("head-304.txt").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    let put = storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"conditional head"),
            Some("text/plain"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");

    let etag = if let Some(e) = put.etag {
        e
    } else {
        storage
            .head_object(&kb, &path, ConditionalHeaders::default())
            .await
            .expect("head for etag")
            .etag
            .expect("etag from head")
    };

    let Err(err) = storage
        .head_object(
            &kb,
            &path,
            ConditionalHeaders {
                if_none_match: Some(etag),
                ..ConditionalHeaders::default()
            },
        )
        .await
    else {
        panic!("matching If-None-Match on HEAD should return NotModified");
    };

    assert!(matches!(err, StorageError::NotModified));
}

#[tokio::test]
#[ignore = "requires SeaweedFS 4.18 in Docker"]
async fn m3_put_if_match_correct_201() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-put-if-match-ok").unwrap();
    let path = ObjectPath::try_from_str("conditional-put.txt").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    let put1 = storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"original content"),
            Some("text/plain"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("initial put");

    let etag = if let Some(e) = put1.etag {
        e
    } else {
        storage
            .head_object(&kb, &path, ConditionalHeaders::default())
            .await
            .expect("head for etag")
            .etag
            .expect("etag from head")
    };

    let put2 = storage
        .put_object(
            &kb,
            &path,
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
        put2.etag.is_some(),
        "successful conditional PUT should return ETag"
    );
}

#[tokio::test]
#[ignore = "requires SeaweedFS 4.18 in Docker"]
async fn m3_put_if_none_match_wildcard_new() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-put-inm-new").unwrap();
    let path = ObjectPath::try_from_str("new-object.txt").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");

    // Object does not exist yet — If-None-Match: * means "create only", should succeed.
    let outcome = storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"brand new object"),
            Some("text/plain"),
            ConditionalHeaders {
                if_none_match: Some("*".to_string()),
                ..ConditionalHeaders::default()
            },
        )
        .await
        .expect("If-None-Match: * on non-existent object should succeed");

    assert!(outcome.etag.is_some(), "successful PUT should return ETag");
}

#[tokio::test]
#[ignore = "requires SeaweedFS 4.18 in Docker"]
async fn m3_delete_if_match_correct_204() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-del-if-match-ok").unwrap();
    let path = ObjectPath::try_from_str("delete-conditional.txt").unwrap();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    let put = storage
        .put_object(
            &kb,
            &path,
            Bytes::from_static(b"to be deleted"),
            Some("text/plain"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put");

    let etag = if let Some(e) = put.etag {
        e
    } else {
        storage
            .head_object(&kb, &path, ConditionalHeaders::default())
            .await
            .expect("head for etag")
            .etag
            .expect("etag from head")
    };

    storage
        .delete_object(
            &kb,
            &path,
            ConditionalHeaders {
                if_match: Some(etag),
                ..ConditionalHeaders::default()
            },
        )
        .await
        .expect("DELETE with correct If-Match should succeed");

    assert!(
        storage
            .head_object(&kb, &path, ConditionalHeaders::default())
            .await
            .is_err(),
        "object should be absent after conditional delete"
    );
}

#[tokio::test]
#[ignore = "requires SeaweedFS 4.18 in Docker"]
async fn m3_content_range_extracted_correctly() {
    let (_container, endpoint) = start_seaweedfs().await;
    let storage = S3Storage::new(make_config(&endpoint).build_client(), TenantSlug::default());
    let kb = KbSlug::try_new("m3-content-range").unwrap();
    let path = ObjectPath::try_from_str("large.bin").unwrap();
    let data: Vec<u8> = (0_u8..=255).cycle().take(1000).collect();

    storage.ensure_bucket(&kb).await.expect("ensure_bucket");
    storage
        .put_object(
            &kb,
            &path,
            Bytes::from(data),
            Some("application/octet-stream"),
            ConditionalHeaders::default(),
        )
        .await
        .expect("put 1000-byte object");

    let read = storage
        .get_object(
            &kb,
            &path,
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
        "Content-Range header should match requested range over total object size"
    );
    assert_eq!(
        read.bytes.len(),
        100,
        "range response should contain exactly 100 bytes"
    );
}

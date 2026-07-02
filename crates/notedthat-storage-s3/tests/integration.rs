//! Integration tests for `S3Storage` against a real `SeaweedFS` instance.
//!
//! These tests are marked `#[ignore]` because they require Docker. Run with:
//! ```sh
//! cargo test -p notedthat-storage-s3 --locked -- --include-ignored
//! ```
#![allow(missing_docs)]

use bytes::Bytes;
use notedthat_core::{ConditionalHeaders, KbManifest, KbSlug, ObjectPath, Storage, TenantSlug};
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

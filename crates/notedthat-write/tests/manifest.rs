//! The manifest is checked on every write path that can store it (#98).
//!
//! `.notedthat/manifest.json` is an ordinary object to the surfaces; what makes
//! it the manifest is this crate, so the check lives here and each entry point
//! is exercised: a full write, a patch, a replace, and a copy onto the key.

#![allow(missing_docs)]

use bytes::Bytes;
use notedthat_core::testing::InMemoryStorage;
use notedthat_core::{
    ConditionalHeaders, CopyObjectOptions, KbManifest, KbSlug, LineRange, ObjectPath, Storage,
};
use notedthat_indexer::IndexEvent;
use notedthat_write::{
    PatchMode, ReplaceRequest, WriteError, WriteSinks, commit, commit_copy, patch, replace,
};
use tokio::sync::mpsc;

const KB: &str = "notes";

fn kb() -> KbSlug {
    KbSlug::try_new(KB).unwrap()
}

fn manifest_key() -> ObjectPath {
    ObjectPath::try_from_str(KbManifest::KEY).unwrap()
}

fn manifest(description: &str) -> String {
    serde_json::json!({
        "notedthat_version": "0.7.2",
        "manifest_version": 1,
        "tenant_slug": "default",
        "kb_slug": KB,
        "display_name": "Notes",
        "description": description,
        "created_at": 1_700_000_000,
        "access": [{ "who": "signed-in", "may": ["list", "read", "write", "delete", "search"] }]
    })
    .to_string()
}

async fn storage() -> InMemoryStorage {
    let storage = InMemoryStorage::default();
    storage.ensure_bucket(&kb()).await.unwrap();
    storage
}

async fn stored(storage: &InMemoryStorage, path: &ObjectPath) -> Option<String> {
    storage
        .get_object(&kb(), path, None, ConditionalHeaders::default())
        .await
        .ok()
        .map(|read| String::from_utf8(read.bytes.to_vec()).unwrap())
}

fn is_invalid_manifest(error: &WriteError, naming: &str) -> bool {
    matches!(error, WriteError::InvalidManifest { message } if message.contains(naming))
}

#[tokio::test]
async fn a_full_write_of_the_manifest_is_checked_and_an_ordinary_key_is_not() {
    let storage = storage().await;
    let (tx, _rx) = mpsc::channel::<IndexEvent>(8);
    let sinks = WriteSinks::indexer_only(&tx);

    // An ordinary key takes anything.
    let note = ObjectPath::try_from_str("a.md").unwrap();
    commit(
        &storage,
        &sinks,
        &kb(),
        &note,
        Bytes::from_static(b"# not json"),
        None,
        ConditionalHeaders::default(),
    )
    .await
    .expect("an ordinary object is not a manifest");

    // The manifest refuses what startup would refuse, with its message …
    for (body, naming) in [
        (manifest("two\nlines"), "description"),
        ("# not json".to_string(), "not a valid manifest document"),
        (
            manifest("fine").replace(r#""kb_slug":"notes""#, r#""kb_slug":"other""#),
            "'other'",
        ),
    ] {
        let error = commit(
            &storage,
            &sinks,
            &kb(),
            &manifest_key(),
            Bytes::from(body),
            None,
            ConditionalHeaders::default(),
        )
        .await
        .expect_err("startup would refuse this manifest");
        assert!(is_invalid_manifest(&error, naming), "{error}");
    }
    assert_eq!(stored(&storage, &manifest_key()).await, None);

    // … and stores what it would accept.
    commit(
        &storage,
        &sinks,
        &kb(),
        &manifest_key(),
        Bytes::from(manifest("Engineering notes and ADRs.")),
        None,
        ConditionalHeaders::default(),
    )
    .await
    .expect("the manifest startup would accept");
    assert!(stored(&storage, &manifest_key()).await.is_some());
}

#[tokio::test]
async fn a_patch_that_breaks_the_manifest_is_refused_before_storage() {
    let storage = storage().await;
    let (tx, _rx) = mpsc::channel::<IndexEvent>(8);
    let sinks = WriteSinks::indexer_only(&tx);
    let good = manifest("fine");
    let etag = storage
        .put_object(
            &kb(),
            &manifest_key(),
            Bytes::from(good.clone()),
            Some("application/json"),
            ConditionalHeaders::default(),
        )
        .await
        .unwrap()
        .etag;
    let if_match = ConditionalHeaders {
        if_match: etag,
        ..ConditionalHeaders::default()
    };

    // An append lands bytes after the closing brace: no longer JSON.
    let error = patch(
        &storage,
        &sinks,
        notedthat_write::patch::PatchRequest {
            kb: &kb(),
            path: &manifest_key(),
            patch_mode: PatchMode::Lines {
                range: LineRange::Insert { before: 2 },
                body: Bytes::from_static(b"\ntrailing"),
            },
            caller_conditionals: if_match,
            max_patchable_size: 1 << 20,
            caller_content_type: None,
        },
    )
    .await
    .expect_err("the spliced body is not a manifest");
    assert!(
        is_invalid_manifest(&error, "not a valid manifest document"),
        "{error}"
    );
    assert_eq!(
        stored(&storage, &manifest_key()).await.as_deref(),
        Some(good.as_str())
    );
}

#[tokio::test]
async fn a_replace_that_breaks_the_manifest_is_refused_before_storage() {
    let storage = storage().await;
    let (tx, _rx) = mpsc::channel::<IndexEvent>(8);
    let sinks = WriteSinks::indexer_only(&tx);
    let good = manifest("fine");
    let etag = storage
        .put_object(
            &kb(),
            &manifest_key(),
            Bytes::from(good.clone()),
            Some("application/json"),
            ConditionalHeaders::default(),
        )
        .await
        .unwrap()
        .etag;
    let if_match = ConditionalHeaders {
        if_match: etag,
        ..ConditionalHeaders::default()
    };

    let error = replace(
        &storage,
        &sinks,
        ReplaceRequest {
            kb: &kb(),
            path: &manifest_key(),
            old_string: "\"description\":\"fine\"",
            new_string: "\"description\":\"two\\nlines\"",
            replace_all: false,
            caller_conditionals: if_match,
            max_patchable_size: 1 << 20,
            caller_content_type: None,
        },
    )
    .await;
    let Err(error) = error else {
        panic!("the replaced description is outside the limits");
    };
    assert!(is_invalid_manifest(&error, "description"), "{error}");
    assert_eq!(
        stored(&storage, &manifest_key()).await.as_deref(),
        Some(good.as_str())
    );
}

#[tokio::test]
async fn a_copy_onto_the_manifest_key_is_checked_against_the_source() {
    let storage = storage().await;
    let (tx, _rx) = mpsc::channel::<IndexEvent>(8);
    let sinks = WriteSinks::indexer_only(&tx);
    let bad = ObjectPath::try_from_str("manifest.bad.json").unwrap();
    let good = ObjectPath::try_from_str("manifest.new.json").unwrap();
    for (path, body) in [(&bad, manifest("two\nlines")), (&good, manifest("fine"))] {
        storage
            .put_object(
                &kb(),
                path,
                Bytes::from(body),
                Some("application/json"),
                ConditionalHeaders::default(),
            )
            .await
            .unwrap();
    }

    let error = commit_copy(
        &storage,
        &sinks,
        &kb(),
        &bad,
        &manifest_key(),
        CopyObjectOptions::default(),
    )
    .await
    .expect_err("the source is not a manifest startup would accept");
    assert!(is_invalid_manifest(&error, "description"), "{error}");
    assert_eq!(stored(&storage, &manifest_key()).await, None);

    commit_copy(
        &storage,
        &sinks,
        &kb(),
        &good,
        &manifest_key(),
        CopyObjectOptions::default(),
    )
    .await
    .expect("a valid source copies into place");
    assert!(stored(&storage, &manifest_key()).await.is_some());
}

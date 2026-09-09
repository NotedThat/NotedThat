//! One scenario set, run through two `Storage` implementations, asserted to agree.
//!
//! Every surface — the HTTP API, `WebDAV`, MCP, the write path, the indexer — holds storage
//! behind `Arc<dyn Storage>`. So a difference between two adapters is a behaviour change
//! an operator gets for free by flipping `NOTEDTHAT_STORAGE_BACKEND`, and the E2E suites,
//! which run one backend at a time, cannot see it. This is what can.
//!
//! Modelled on `crates/notedthat-indexer/tests/vector_store_conformance.rs`.
//!
//! # What is compared, and what cannot be
//!
//! Three values are meaningless to compare across backends and are never rendered
//! literally:
//!
//! - **`ETag` values.** S3 returns an MD5-shaped digest, the filesystem backend a
//!   SHA-256 one. They are rendered as *aliases* — `E1`, `E2` in first-seen order — so
//!   "unchanged after a re-read", "changed after a write" and "preserved across a copy"
//!   are all still comparable, while the value is not.
//! - **`last_modified`.** Wall-clock. Rendered as presence and as an ordering relation
//!   within one backend.
//! - **Cursors.** Opaque, and structurally different between backends. Pagination is
//!   observed as page count, key order, and the absence of gaps and duplicates.
//!
//! `ETag`s are always resolved through `head_object`, never from `PutOutcome`: `SeaweedFS`
//! does not always return one on PUT, which is a backend artifact rather than a contract.

#![allow(dead_code)]
// Each scenario group is a declarative table of cases. Splitting them to satisfy a line
// count would scatter one readable table across several functions for no benefit.
#![allow(clippy::too_many_lines)]

use std::collections::BTreeMap;

use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, CopyObjectOptions, KbManifest, KbSlug, ObjectPath, Storage,
    StorageError, TenantSlug,
};

/// One named observation, produced identically by every conforming backend.
pub type Observations = Vec<(&'static str, String)>;

/// Which implementation produced a set of observations, and where its code lives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    S3,
    Fs,
    Memory,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Self::S3 => "S3Storage",
            Self::Fs => "FsStorage",
            Self::Memory => "InMemoryStorage",
        }
    }

    pub fn source_file(self) -> &'static str {
        match self {
            Self::S3 => "crates/notedthat-storage-s3/src/storage.rs",
            Self::Fs => "crates/notedthat-storage-fs/src/storage.rs",
            Self::Memory => "crates/notedthat-core/src/testing.rs",
        }
    }
}

/// Renders `ETag`s as stable aliases, so two backends are comparable without comparing
/// values. Reset per scenario group, so `E1` means "the first distinct tag this group saw".
#[derive(Default)]
struct EtagLedger {
    order: Vec<String>,
}

impl EtagLedger {
    fn alias(&mut self, etag: Option<&str>) -> String {
        let Some(etag) = etag else {
            return "absent".to_string();
        };
        if let Some(index) = self.order.iter().position(|seen| seen == etag) {
            return format!("E{}", index + 1);
        }
        self.order.push(etag.to_string());
        format!("E{}", self.order.len())
    }
}

/// Collapse an error to its kind, so comparison does not depend on backend wording.
fn error_kind(error: &StorageError) -> String {
    match error {
        StorageError::NotFound { .. } => "NotFound".to_string(),
        StorageError::BucketNotFound { .. } => "BucketNotFound".to_string(),
        StorageError::BackendUnavailable { .. } => "BackendUnavailable".to_string(),
        StorageError::Other { .. } => "Other".to_string(),
        StorageError::NotModified => "NotModified".to_string(),
        StorageError::PreconditionFailed => "PreconditionFailed".to_string(),
        // Kept: this becomes `Content-Range: bytes */N`, a number both must agree on.
        StorageError::RangeNotSatisfiable { complete_length } => {
            format!("RangeNotSatisfiable({complete_length})")
        }
    }
}

fn outcome<T>(result: &Result<T, StorageError>) -> String {
    match result {
        Ok(_) => "ok".to_string(),
        Err(error) => error_kind(error),
    }
}

/// Length plus a digest prefix. Length alone would let two different bodies look equal.
fn render_bytes(bytes: &Bytes) -> String {
    let digest = notedthat_core::compute_etag(bytes);
    format!("len={} sha={}", bytes.len(), &digest[1..9])
}

fn path(key: &str) -> ObjectPath {
    ObjectPath::try_from_str(key).expect("test key is a valid ObjectPath")
}

/// Content type every seed write declares.
const MARKDOWN: Option<&str> = Some("text/markdown");

async fn put(store: &dyn Storage, kb: &KbSlug, key: &str, body: &'static str) {
    store
        .put_object(
            kb,
            &path(key),
            Bytes::from_static(body.as_bytes()),
            MARKDOWN,
            ConditionalHeaders::default(),
        )
        .await
        .expect("seed write");
}

/// Resolve an `ETag` the same way for every backend.
///
/// Never read from `PutOutcome`: `SeaweedFS` omits it on some writes, which five tests in
/// the S3 container suite already work around.
async fn etag_of(store: &dyn Storage, kb: &KbSlug, key: &str) -> Option<String> {
    store
        .head_object(kb, &path(key), ConditionalHeaders::default())
        .await
        .ok()
        .and_then(|meta| meta.etag)
}

async fn http_date_of(store: &dyn Storage, kb: &KbSlug, key: &str, offset: i64) -> String {
    let seconds = store
        .head_object(kb, &path(key), ConditionalHeaders::default())
        .await
        .ok()
        .and_then(|meta| meta.last_modified)
        .unwrap_or(0)
        + offset;
    let stamp = if seconds < 0 {
        std::time::SystemTime::UNIX_EPOCH
    } else {
        std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(u64::try_from(seconds).unwrap_or(0))
    };
    httpdate::fmt_http_date(stamp)
}

// ─── Scenario groups ────────────────────────────────────────────────────────

pub async fn observe_round_trip(store: &dyn Storage, kb: &KbSlug) -> Observations {
    let mut ledger = EtagLedger::default();
    let mut out = Observations::new();

    put(store, kb, "hello.md", "first").await;

    let head = store
        .head_object(kb, &path("hello.md"), ConditionalHeaders::default())
        .await
        .expect("head after write");
    out.push(("head_key", head.key.clone()));
    out.push(("head_size", head.size.to_string()));
    out.push((
        "head_content_type",
        head.content_type.clone().unwrap_or_else(|| "absent".into()),
    ));
    out.push((
        "head_last_modified",
        if head.last_modified.is_some() {
            "present".into()
        } else {
            "absent".into()
        },
    ));
    out.push(("head_etag_alias", ledger.alias(head.etag.as_deref())));

    let read = store
        .get_object(kb, &path("hello.md"), None, ConditionalHeaders::default())
        .await
        .expect("get after write");
    out.push(("get_body", render_bytes(&read.bytes)));
    out.push(("get_size", read.meta.size.to_string()));
    out.push((
        "get_content_range",
        read.content_range
            .clone()
            .unwrap_or_else(|| "absent".into()),
    ));
    out.push(("get_etag_alias", ledger.alias(read.meta.etag.as_deref())));

    // Identical bytes: a content-derived validator must not move.
    put(store, kb, "hello.md", "first").await;
    let same = etag_of(store, kb, "hello.md").await;
    out.push((
        "rewrite_same_bytes_etag_alias",
        ledger.alias(same.as_deref()),
    ));

    put(store, kb, "hello.md", "second").await;
    let changed = etag_of(store, kb, "hello.md").await;
    out.push((
        "rewrite_new_bytes_etag_alias",
        ledger.alias(changed.as_deref()),
    ));

    out.push((
        "head_missing",
        outcome(
            &store
                .head_object(kb, &path("absent.md"), ConditionalHeaders::default())
                .await,
        ),
    ));
    out.push((
        "get_missing",
        outcome(
            &store
                .get_object(kb, &path("absent.md"), None, ConditionalHeaders::default())
                .await,
        ),
    ));

    // Empty objects, and keys with a space and non-ASCII characters.
    store
        .put_object(
            kb,
            &path("empty.md"),
            Bytes::new(),
            MARKDOWN,
            ConditionalHeaders::default(),
        )
        .await
        .expect("empty write");
    let empty = store
        .get_object(kb, &path("empty.md"), None, ConditionalHeaders::default())
        .await
        .expect("empty read");
    out.push(("empty_size", empty.meta.size.to_string()));
    out.push(("empty_body", render_bytes(&empty.bytes)));

    put(store, kb, "dossier/文 note.md", "unicode").await;
    let unicode = store
        .get_object(
            kb,
            &path("dossier/文 note.md"),
            None,
            ConditionalHeaders::default(),
        )
        .await
        .expect("unicode read");
    out.push(("unicode_key_body", render_bytes(&unicode.bytes)));
    out.push(("unicode_key", unicode.meta.key.clone()));

    out
}

pub async fn observe_conditional_writes(store: &dyn Storage, kb: &KbSlug) -> Observations {
    let mut out = Observations::new();
    put(store, kb, "a.md", "base").await;
    let etag = etag_of(store, kb, "a.md").await.expect("an ETag");

    let cases: Vec<(&'static str, ConditionalHeaders)> = vec![
        ("unconditional", ConditionalHeaders::default()),
        (
            "if_match_current",
            ConditionalHeaders {
                if_match: Some(etag.clone()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_match_wrong",
            ConditionalHeaders {
                if_match: Some("\"nope\"".into()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_match_star",
            ConditionalHeaders {
                if_match: Some("*".into()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_match_list_containing_current",
            ConditionalHeaders {
                if_match: Some(format!("\"other\", {etag}")),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_match_list_without_current",
            ConditionalHeaders {
                if_match: Some("\"one\", \"two\"".into()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_none_match_star_over_existing",
            ConditionalHeaders {
                if_none_match: Some("*".into()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_unmodified_since_far_past",
            ConditionalHeaders {
                if_unmodified_since: Some(httpdate::fmt_http_date(
                    std::time::SystemTime::UNIX_EPOCH,
                )),
                ..ConditionalHeaders::default()
            },
        ),
    ];

    for (name, conditionals) in cases {
        // The `if_match_current` row consumes the tag, so re-read it each time.
        let fresh = etag_of(store, kb, "a.md").await.unwrap_or_default();
        let conditionals = if conditionals.if_match.as_deref() == Some(etag.as_str()) {
            ConditionalHeaders {
                if_match: Some(fresh),
                ..conditionals
            }
        } else {
            conditionals
        };
        let result = store
            .put_object(
                kb,
                &path("a.md"),
                Bytes::from_static(b"written"),
                MARKDOWN,
                conditionals,
            )
            .await;
        out.push((leak(format!("put/{name}")), outcome(&result)));
        let body = store
            .get_object(kb, &path("a.md"), None, ConditionalHeaders::default())
            .await
            .expect("read back");
        out.push((leak(format!("put/{name}/body")), render_bytes(&body.bytes)));
    }

    // Create-only on a key that does not exist yet.
    out.push((
        "if_none_match_star_creates",
        outcome(
            &store
                .put_object(
                    kb,
                    &path("fresh.md"),
                    Bytes::from_static(b"new"),
                    MARKDOWN,
                    ConditionalHeaders {
                        if_none_match: Some("*".into()),
                        ..ConditionalHeaders::default()
                    },
                )
                .await,
        ),
    ));
    out.push((
        "if_match_star_on_missing",
        outcome(
            &store
                .put_object(
                    kb,
                    &path("still-absent.md"),
                    Bytes::from_static(b"new"),
                    MARKDOWN,
                    ConditionalHeaders {
                        if_match: Some("*".into()),
                        ..ConditionalHeaders::default()
                    },
                )
                .await,
        ),
    ));

    out
}

pub async fn observe_conditional_reads(store: &dyn Storage, kb: &KbSlug) -> Observations {
    let mut out = Observations::new();
    put(store, kb, "a.md", "body").await;
    let etag = etag_of(store, kb, "a.md").await.expect("an ETag");
    let at_mtime = http_date_of(store, kb, "a.md", 0).await;
    let before_mtime = http_date_of(store, kb, "a.md", -1).await;

    let cases: Vec<(&'static str, ConditionalHeaders)> = vec![
        ("plain", ConditionalHeaders::default()),
        (
            "if_none_match_current",
            ConditionalHeaders {
                if_none_match: Some(etag.clone()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_none_match_other",
            ConditionalHeaders {
                if_none_match: Some("\"other\"".into()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_none_match_star",
            ConditionalHeaders {
                if_none_match: Some("*".into()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_match_current",
            ConditionalHeaders {
                if_match: Some(etag.clone()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_match_wrong",
            ConditionalHeaders {
                if_match: Some("\"nope\"".into()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_modified_since_at_mtime",
            ConditionalHeaders {
                if_modified_since: Some(at_mtime.clone()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_modified_since_before_mtime",
            ConditionalHeaders {
                if_modified_since: Some(before_mtime.clone()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_unmodified_since_at_mtime",
            ConditionalHeaders {
                if_unmodified_since: Some(at_mtime.clone()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "if_unmodified_since_before_mtime",
            ConditionalHeaders {
                if_unmodified_since: Some(before_mtime.clone()),
                ..ConditionalHeaders::default()
            },
        ),
        // RFC 7232 §6: If-Match is evaluated first, so this is a 412 and not a 304.
        (
            "if_match_wrong_and_if_none_match_current",
            ConditionalHeaders {
                if_match: Some("\"nope\"".into()),
                if_none_match: Some(etag.clone()),
                ..ConditionalHeaders::default()
            },
        ),
        (
            "malformed_date",
            ConditionalHeaders {
                if_modified_since: Some("not-a-date".into()),
                ..ConditionalHeaders::default()
            },
        ),
    ];

    for (name, conditionals) in cases {
        out.push((
            leak(format!("get/{name}")),
            outcome(
                &store
                    .get_object(kb, &path("a.md"), None, conditionals.clone())
                    .await,
            ),
        ));
        out.push((
            leak(format!("head/{name}")),
            outcome(&store.head_object(kb, &path("a.md"), conditionals).await),
        ));
    }

    // A missing object is a 404 before it is anything else.
    out.push((
        "get_missing_with_if_none_match_star",
        outcome(
            &store
                .get_object(
                    kb,
                    &path("absent.md"),
                    None,
                    ConditionalHeaders {
                        if_none_match: Some("*".into()),
                        ..ConditionalHeaders::default()
                    },
                )
                .await,
        ),
    ));

    out
}

pub async fn observe_ranges(store: &dyn Storage, kb: &KbSlug) -> Observations {
    let mut out = Observations::new();
    put(store, kb, "a.md", "0123456789").await;
    store
        .put_object(
            kb,
            &path("empty.md"),
            Bytes::new(),
            MARKDOWN,
            ConditionalHeaders::default(),
        )
        .await
        .expect("empty write");

    let cases: Vec<(&'static str, &str, Option<Vec<ByteRange>>)> = vec![
        ("no_range", "a.md", None),
        ("empty_range_vec", "a.md", Some(vec![])),
        (
            "from_start",
            "a.md",
            Some(vec![ByteRange::FromStart { first: 2, last: 5 }]),
        ),
        (
            "single_byte",
            "a.md",
            Some(vec![ByteRange::FromStart { first: 0, last: 0 }]),
        ),
        (
            "open_ended",
            "a.md",
            Some(vec![ByteRange::FromStartOpen { first: 7 }]),
        ),
        (
            "suffix",
            "a.md",
            Some(vec![ByteRange::Suffix { length: 3 }]),
        ),
        (
            "past_eof",
            "a.md",
            Some(vec![ByteRange::FromStart {
                first: 50,
                last: 60,
            }]),
        ),
        (
            "start_beyond_eof_open",
            "a.md",
            Some(vec![ByteRange::FromStartOpen { first: 99 }]),
        ),
        (
            "zero_length_suffix",
            "a.md",
            Some(vec![ByteRange::Suffix { length: 0 }]),
        ),
        (
            "empty_object_any_range",
            "empty.md",
            Some(vec![ByteRange::FromStart { first: 0, last: 3 }]),
        ),
    ];

    for (name, key, range) in cases {
        let result = store
            .get_object(kb, &path(key), range, ConditionalHeaders::default())
            .await;
        out.push((leak(format!("range/{name}")), outcome(&result)));
        if let Ok(read) = result {
            out.push((
                leak(format!("range/{name}/body")),
                render_bytes(&read.bytes),
            ));
            out.push((
                leak(format!("range/{name}/content_range")),
                read.content_range.unwrap_or_else(|| "absent".into()),
            ));
            // S3 reports the length served, not the object length.
            out.push((
                leak(format!("range/{name}/size")),
                read.meta.size.to_string(),
            ));
        }
    }

    out
}

pub async fn observe_listing(store: &dyn Storage, kb: &KbSlug) -> Observations {
    let mut out = Observations::new();
    // No name here is both a file and a prefix: a filesystem cannot hold that, and the
    // divergence is pinned in the adapter's own tests rather than here.
    for key in [
        "UPPER.md",
        "_underscore.md",
        "a.md",
        "a-b.md",
        "a0/deep.md",
        "b.md",
        "dinner.md",
        "dir/one.md",
        "dir/nested/two.md",
        "dossier/文 note.md",
        "with space.md",
    ] {
        put(store, kb, key, "x").await;
    }

    let cases: Vec<(&'static str, Option<&str>)> = vec![
        ("all", None),
        ("prefix_dir_slash", Some("dir/")),
        // A string prefix, not a path prefix — it must reach into `dir/`.
        ("prefix_partial_word", Some("di")),
        ("prefix_exact_key", Some("a.md")),
        ("prefix_no_match", Some("zzz")),
    ];

    for (name, prefix) in cases {
        let response = store
            .list_objects(kb, prefix, 1000, None)
            .await
            .expect("list");
        let keys: Vec<&str> = response
            .objects
            .iter()
            .map(|object| object.key.as_str())
            .collect();
        out.push((leak(format!("list/{name}")), keys.join(",")));
        out.push((
            leak(format!("list/{name}/truncated")),
            response.truncated.to_string(),
        ));
    }

    let page = store
        .list_objects(kb, None, 1000, None)
        .await
        .expect("list");
    let entry = page.objects.first().expect("at least one object");
    out.push((
        "entry_etag",
        entry.etag.clone().unwrap_or_else(|| "absent".into()),
    ));
    out.push((
        "entry_content_type",
        entry
            .content_type
            .clone()
            .unwrap_or_else(|| "absent".into()),
    ));
    out.push((
        "entry_last_modified",
        if entry.last_modified.is_some() {
            "present".into()
        } else {
            "absent".into()
        },
    ));
    out.push(("entry_size", entry.size.to_string()));

    // limit = 0 must still satisfy `truncated == next_cursor.is_some()`.
    let zero = store.list_objects(kb, None, 0, None).await.expect("list");
    out.push(("limit_zero_count", zero.objects.len().to_string()));
    out.push((
        "limit_zero_invariant_holds",
        (zero.truncated == zero.next_cursor.is_some()).to_string(),
    ));

    out.push((
        "garbage_cursor",
        outcome(&store.list_objects(kb, None, 10, Some("not-a-cursor")).await),
    ));

    out
}

pub async fn observe_pagination(store: &dyn Storage, kb: &KbSlug) -> Observations {
    let mut out = Observations::new();
    for index in 0..25 {
        let key = format!("page/doc-{index:04}.md");
        store
            .put_object(
                kb,
                &path(&key),
                Bytes::from_static(b"x"),
                MARKDOWN,
                ConditionalHeaders::default(),
            )
            .await
            .expect("seed");
    }

    for (name, limit) in [("limit_10", 10_u32), ("limit_5_divides_evenly", 5)] {
        let mut keys = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let response = store
                .list_objects(kb, Some("page/"), limit, cursor.as_deref())
                .await
                .expect("page");
            assert_eq!(
                response.truncated,
                response.next_cursor.is_some(),
                "{name}: ListResponse invariant violated"
            );
            keys.extend(response.objects.into_iter().map(|object| object.key));
            pages += 1;
            cursor = response.next_cursor;
            assert!(pages < 50, "{name}: pagination did not terminate");
            if cursor.is_none() {
                break;
            }
        }
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        out.push((leak(format!("{name}/pages")), pages.to_string()));
        out.push((leak(format!("{name}/count")), keys.len().to_string()));
        out.push((
            leak(format!("{name}/sorted_gapless_unique")),
            (sorted == keys).to_string(),
        ));
    }

    // A cursor must survive deletion of the key it names: WebDAV pages a whole knowledge
    // base in a loop, and a concurrent delete must not break that.
    let first = store
        .list_objects(kb, Some("page/"), 2, None)
        .await
        .expect("first page");
    let cursor = first.next_cursor.clone().expect("more pages");
    let last_key = first.objects.last().expect("a key").key.clone();
    store
        .delete_object(kb, &path(&last_key), ConditionalHeaders::default())
        .await
        .expect("delete the cursor key");
    let resumed = store
        .list_objects(kb, Some("page/"), 2, Some(&cursor))
        .await;
    out.push(("resume_after_deleting_cursor_key", outcome(&resumed)));
    if let Ok(resumed) = resumed {
        let keys: Vec<&str> = resumed
            .objects
            .iter()
            .map(|object| object.key.as_str())
            .collect();
        out.push(("resume_after_deleting_cursor_key/keys", keys.join(",")));
    }

    out
}

pub async fn observe_copy(store: &dyn Storage, kb: &KbSlug) -> Observations {
    let mut ledger = EtagLedger::default();
    let mut out = Observations::new();

    put(store, kb, "source file.md", "shared").await;
    let source_etag = etag_of(store, kb, "source file.md").await;
    out.push(("source_etag_alias", ledger.alias(source_etag.as_deref())));

    out.push((
        "copy_plain",
        outcome(
            &store
                .copy_object(
                    kb,
                    &path("source file.md"),
                    &path("copied/文 copy.md"),
                    CopyObjectOptions {
                        source_if_match: source_etag.clone(),
                        destination_if_none_match: Some("*".into()),
                        content_type: None,
                    },
                )
                .await,
        ),
    ));
    let copied_etag = etag_of(store, kb, "copied/文 copy.md").await;
    out.push(("copy_dest_etag_alias", ledger.alias(copied_etag.as_deref())));
    let copied = store
        .get_object(
            kb,
            &path("copied/文 copy.md"),
            None,
            ConditionalHeaders::default(),
        )
        .await
        .expect("read copy");
    out.push(("copy_dest_body", render_bytes(&copied.bytes)));
    out.push((
        "copy_dest_content_type",
        copied.meta.content_type.unwrap_or_else(|| "absent".into()),
    ));

    out.push((
        "copy_over_existing_with_if_none_match_star",
        outcome(
            &store
                .copy_object(
                    kb,
                    &path("source file.md"),
                    &path("copied/文 copy.md"),
                    CopyObjectOptions {
                        destination_if_none_match: Some("*".into()),
                        ..CopyObjectOptions::default()
                    },
                )
                .await,
        ),
    ));

    out.push((
        "copy_with_wrong_source_if_match",
        outcome(
            &store
                .copy_object(
                    kb,
                    &path("source file.md"),
                    &path("elsewhere.md"),
                    CopyObjectOptions {
                        source_if_match: Some("\"stale\"".into()),
                        ..CopyObjectOptions::default()
                    },
                )
                .await,
        ),
    ));
    out.push((
        "destination_absent_after_failed_copy",
        outcome(
            &store
                .head_object(kb, &path("elsewhere.md"), ConditionalHeaders::default())
                .await,
        ),
    ));

    out.push((
        "copy_missing_source",
        outcome(
            &store
                .copy_object(
                    kb,
                    &path("absent.md"),
                    &path("nowhere.md"),
                    CopyObjectOptions::default(),
                )
                .await,
        ),
    ));

    out.push((
        "copy_replacing_content_type",
        outcome(
            &store
                .copy_object(
                    kb,
                    &path("source file.md"),
                    &path("typed.md"),
                    CopyObjectOptions {
                        content_type: Some("text/plain".into()),
                        ..CopyObjectOptions::default()
                    },
                )
                .await,
        ),
    ));
    out.push((
        "copy_replaced_content_type",
        store
            .head_object(kb, &path("typed.md"), ConditionalHeaders::default())
            .await
            .ok()
            .and_then(|meta| meta.content_type)
            .unwrap_or_else(|| "absent".into()),
    ));

    out
}

pub async fn observe_delete_and_lifecycle(store: &dyn Storage, kb: &KbSlug) -> Observations {
    let mut out = Observations::new();

    out.push((
        "ensure_bucket_again",
        outcome(&store.ensure_bucket(kb).await),
    ));
    out.push((
        "ensure_bucket_third_time",
        outcome(&store.ensure_bucket(kb).await),
    ));

    put(store, kb, "a.md", "body").await;
    let etag = etag_of(store, kb, "a.md").await.expect("an ETag");

    out.push((
        "delete_if_match_wrong",
        outcome(
            &store
                .delete_object(
                    kb,
                    &path("a.md"),
                    ConditionalHeaders {
                        if_match: Some("\"nope\"".into()),
                        ..ConditionalHeaders::default()
                    },
                )
                .await,
        ),
    ));
    out.push((
        "object_survives_failed_delete",
        outcome(
            &store
                .head_object(kb, &path("a.md"), ConditionalHeaders::default())
                .await,
        ),
    ));

    // The three headers S3 cannot express on a DELETE are ignored, not honoured — which
    // means the object is gone afterwards. Pinned here so the contract lives somewhere
    // other than a `debug!` line.
    put(store, kb, "ignored.md", "body").await;
    out.push((
        "delete_if_none_match_star_is_ignored",
        outcome(
            &store
                .delete_object(
                    kb,
                    &path("ignored.md"),
                    ConditionalHeaders {
                        if_none_match: Some("*".into()),
                        if_unmodified_since: Some(httpdate::fmt_http_date(
                            std::time::SystemTime::UNIX_EPOCH,
                        )),
                        ..ConditionalHeaders::default()
                    },
                )
                .await,
        ),
    ));
    out.push((
        "object_gone_after_ignored_conditions",
        outcome(
            &store
                .head_object(kb, &path("ignored.md"), ConditionalHeaders::default())
                .await,
        ),
    ));

    out.push((
        "delete_if_match_current",
        outcome(
            &store
                .delete_object(
                    kb,
                    &path("a.md"),
                    ConditionalHeaders {
                        if_match: Some(etag),
                        ..ConditionalHeaders::default()
                    },
                )
                .await,
        ),
    ));
    out.push((
        "head_after_delete",
        outcome(
            &store
                .head_object(kb, &path("a.md"), ConditionalHeaders::default())
                .await,
        ),
    ));
    out.push((
        "delete_again_is_idempotent",
        outcome(
            &store
                .delete_object(kb, &path("a.md"), ConditionalHeaders::default())
                .await,
        ),
    ));
    out.push((
        "delete_never_existed",
        outcome(
            &store
                .delete_object(kb, &path("never.md"), ConditionalHeaders::default())
                .await,
        ),
    ));

    put(store, kb, "a.md", "again").await;
    out.push((
        "key_reusable_after_delete",
        outcome(
            &store
                .head_object(kb, &path("a.md"), ConditionalHeaders::default())
                .await,
        ),
    ));

    out
}

pub async fn observe_manifest(store: &dyn Storage, kb: &KbSlug) -> Observations {
    let mut out = Observations::new();

    out.push(("read_before_write", outcome(&store.read_manifest(kb).await)));

    let manifest = KbManifest::new_v1(&TenantSlug::default(), kb, "Conformance", 1_700_000_000);
    out.push(("write", outcome(&store.write_manifest(kb, &manifest).await)));

    let read = store.read_manifest(kb).await.expect("manifest round trip");
    out.push(("read_kb_slug", read.kb_slug.as_str().to_string()));
    out.push(("read_version", read.manifest_version.to_string()));
    out.push(("read_display_name", read.display_name.clone()));

    // The manifest is an ordinary object: readable as one, and visible to a listing.
    out.push((
        "readable_as_an_object",
        outcome(
            &store
                .get_object(
                    kb,
                    &path(".notedthat/manifest.json"),
                    None,
                    ConditionalHeaders::default(),
                )
                .await,
        ),
    ));
    let listed = store
        .list_objects(kb, None, 1000, None)
        .await
        .expect("list");
    out.push((
        "visible_in_listing",
        listed
            .objects
            .iter()
            .any(|object| object.key == ".notedthat/manifest.json")
            .to_string(),
    ));

    out
}

pub async fn observe_streaming(store: &dyn Storage, kb: &KbSlug) -> Observations {
    let mut ledger = EtagLedger::default();
    let mut out = Observations::new();
    put(store, kb, "a.md", "0123456789").await;

    let stream = store
        .get_object_stream(kb, &path("a.md"), None, ConditionalHeaders::default())
        .await
        .expect("stream");
    out.push((
        "stream_etag_alias",
        ledger.alias(stream.meta.etag.as_deref()),
    ));
    out.push((
        "stream_content_range",
        stream
            .content_range
            .clone()
            .unwrap_or_else(|| "absent".into()),
    ));
    let body = collect(stream.chunks).await;
    out.push(("stream_body", render_bytes(&body)));

    let ranged = store
        .get_object_stream(
            kb,
            &path("a.md"),
            Some(vec![ByteRange::FromStart { first: 2, last: 5 }]),
            ConditionalHeaders::default(),
        )
        .await
        .expect("ranged stream");
    out.push((
        "stream_range_content_range",
        ranged
            .content_range
            .clone()
            .unwrap_or_else(|| "absent".into()),
    ));
    out.push((
        "stream_range_body",
        render_bytes(&collect(ranged.chunks).await),
    ));

    out.push((
        "stream_missing",
        outcome(
            &store
                .get_object_stream(kb, &path("absent.md"), None, ConditionalHeaders::default())
                .await,
        ),
    ));

    let etag = etag_of(store, kb, "a.md").await.expect("an ETag");
    out.push((
        "stream_if_none_match_current",
        outcome(
            &store
                .get_object_stream(
                    kb,
                    &path("a.md"),
                    None,
                    ConditionalHeaders {
                        if_none_match: Some(etag),
                        ..ConditionalHeaders::default()
                    },
                )
                .await,
        ),
    ));

    out
}

async fn collect(chunks: notedthat_core::ObjectChunkStream) -> Bytes {
    use futures::TryStreamExt;
    let collected: Vec<u8> = chunks
        .try_fold(Vec::new(), |mut acc, chunk| async move {
            acc.extend_from_slice(&chunk);
            Ok(acc)
        })
        .await
        .expect("stream body");
    Bytes::from(collected)
}

/// Every group, each against its own knowledge base, names prefixed by the group.
///
/// `provision` is called once per knowledge base, so a backend that needs its container
/// created gets one.
/// `suffix` keeps knowledge-base slugs unique across runs while staying **identical**
/// between the two backends being compared — some observations echo the slug back, so a
/// per-call nonce would make them disagree for no reason.
pub async fn observe_all<F, Fut>(store: &dyn Storage, suffix: &str, provision: F) -> Observations
where
    F: Fn(KbSlug) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut all = Observations::new();

    macro_rules! group {
        ($name:literal, $observe:path) => {{
            let kb =
                KbSlug::try_new(&format!("conf-{}-{suffix}", $name)).expect("conformance KB slug");
            provision(kb.clone()).await;
            for (key, value) in $observe(store, &kb).await {
                all.push((leak(format!("{}/{}", $name, key)), value));
            }
        }};
    }

    group!("roundtrip", observe_round_trip);
    group!("condwrite", observe_conditional_writes);
    group!("condread", observe_conditional_reads);
    group!("ranges", observe_ranges);
    group!("listing", observe_listing);
    group!("paging", observe_pagination);
    group!("copy", observe_copy);
    group!("lifecycle", observe_delete_and_lifecycle);
    group!("manifest", observe_manifest);
    group!("stream", observe_streaming);

    all
}

/// Observation names are `&'static str` to match the `VectorStore` suite's shape; the
/// table-driven rows build theirs at runtime, and a suite lives for the process.
fn leak(name: String) -> &'static str {
    Box::leak(name.into_boxed_str())
}

/// Guard the premises the comparisons rest on.
///
/// Without this, a backend that returned no `ETag` at all would make every alias
/// `absent`, every conditional row agree vacuously, and the suite pass while proving
/// nothing.
pub fn assert_premises(backend: Backend, observed: &Observations) {
    let map: BTreeMap<&str, &str> = observed
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let get = |name: &str| {
        *map.get(name)
            .unwrap_or_else(|| panic!("{}: missing observation {name}", backend.label()))
    };

    assert_eq!(
        get("roundtrip/head_etag_alias"),
        "E1",
        "{}: returned no ETag, so every alias collapses and the conditional rows below \
         would agree for the wrong reason",
        backend.label()
    );
    assert_eq!(
        get("roundtrip/rewrite_new_bytes_etag_alias"),
        "E2",
        "{}: writing different bytes did not change the ETag, so nothing below can \
         distinguish a stale validator from a fresh one",
        backend.label()
    );
    assert_eq!(
        get("listing/list/all/truncated"),
        "false",
        "{}: the unfiltered listing did not return the whole corpus, so the prefix rows \
         prove nothing",
        backend.label()
    );
}

/// Compare two backends' observations, naming both source files on a mismatch.
pub fn assert_agree(
    reference: Backend,
    from_reference: &Observations,
    candidate: Backend,
    from_candidate: &Observations,
) {
    assert_eq!(
        from_reference.len(),
        from_candidate.len(),
        "{} produced {} observations and {} produced {}; both must run the same scenarios",
        reference.label(),
        from_reference.len(),
        candidate.label(),
        from_candidate.len()
    );

    for ((name, expected), (candidate_name, actual)) in from_reference.iter().zip(from_candidate) {
        assert_eq!(
            name,
            candidate_name,
            "observation order must match between {} and {}",
            reference.label(),
            candidate.label()
        );
        assert_eq!(
            expected,
            actual,
            "'{name}': {} and {} disagree.\n  {}: [{expected}]\n  {}: [{actual}]\n\
             One of {} or {} is wrong. Every surface holds storage behind Arc<dyn Storage>, \
             so this is a behaviour change an operator gets for free by flipping \
             NOTEDTHAT_STORAGE_BACKEND, and the E2E suites run one backend at a time and \
             cannot see it. If the difference is intentional, do not weaken this \
             assertion — decide which side is right and record the other as a documented \
             divergence.",
            reference.label(),
            candidate.label(),
            reference.label(),
            candidate.label(),
            reference.source_file(),
            candidate.source_file(),
        );
    }
}

//! [`S3Storage`]: implements the `notedthat_core::Storage` trait against `aws-sdk-s3`.

use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder;
use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_runtime_api::http::Response as HttpResponse;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use bytes::Bytes;
use notedthat_core::{
    ByteRange, ConditionalHeaders, CopyObjectOptions, KbManifest, KbSlug, ListResponse, ObjectMeta,
    ObjectPath, ObjectRead, ObjectStream, PutOutcome, StagedBody, Storage, StorageError,
    TenantSlug, derive_bucket_name,
};
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use tracing::{debug, info, warn};

const MANIFEST_KEY: &str = ".notedthat/manifest.json";
/// Where [`S3Storage::check_conditional_writes`] puts its scratch object. Under
/// `.notedthat/` so that a copy left behind by a failed delete is private (D48) and
/// never indexed.
pub const PROBE_KEY_PREFIX: &str = ".notedthat/conditional-write-probe-";
const PROBE_BODY: &[u8] = b"conditional-write probe";
/// A quoted `ETag` no object can carry: S3 `ETag`s are hex digests, and this is not hex.
const PROBE_UNMATCHABLE_ETAG: &str = "\"notedthat-conditional-write-probe\"";
const COPY_SOURCE_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// Production `Storage` implementation backed by Amazon S3 (or S3-compatible backends
/// such as `SeaweedFS` 4.18+).
///
/// Uses `force_path_style(true)` when configured for compatibility with non-AWS backends.
/// The client is constructed by [`crate::S3Config::build_client`].
pub struct S3Storage {
    client: Client,
    tenant: TenantSlug,
}

impl S3Storage {
    /// Construct a new [`S3Storage`].
    ///
    /// `client` must already be configured with credentials and endpoint URL.
    /// The `tenant` is used to derive bucket names via `nt-{tenant}-{kb}`.
    #[must_use]
    pub fn new(client: Client, tenant: TenantSlug) -> Self {
        Self { client, tenant }
    }

    fn bucket_name(&self, kb: &KbSlug) -> String {
        derive_bucket_name(&self.tenant, kb)
    }

    /// Find out whether `kb`'s bucket enforces the preconditions `NotedThat` forwards on a
    /// conditional `PUT` (D70).
    ///
    /// Some S3-compatible backends parse `If-Match` and `If-None-Match` and then store
    /// the object anyway (`SPECIFICATIONS.md` §8.1), so a conditional write that should
    /// be `412` answers `200` and one of two concurrent writers is silently lost. Nothing
    /// in a normal request can tell, so this asks directly.
    ///
    /// It stores a scratch object under `.notedthat/` and then overwrites it three times,
    /// because enforcement is a claim in **both** directions and only asking half of it
    /// cannot tell a backend that compares correctly from one that never matches at all:
    ///
    /// | Overwrite | Expected |
    /// |---|---|
    /// | `If-Match` naming the `ETag` the first `PUT` returned | stored |
    /// | `If-Match` naming that `ETag` with one hex digit altered | `412` |
    /// | `If-None-Match: *` over the object that now exists | `412` |
    ///
    /// The first is what catches an `ETag` representation mismatch — a backend that
    /// returns an unquoted or weak `ETag` while comparing against the strong quoted form
    /// refuses every legitimate conditional write, which is as broken as ignoring the
    /// header and looks identical to enforcement if you only ever send values that cannot
    /// match. Altering the real `ETag` rather than inventing a literal also keeps the
    /// value syntactically valid, so a backend that rejects a malformed `ETag` with `400`
    /// no longer fails the check.
    ///
    /// The scratch object is deleted whatever the answers were, including every version
    /// it wrote on a versioned bucket.
    ///
    /// This is a write, unlike [`Storage::probe`], and calls the client directly so it
    /// is not counted as a storage operation. One writer cannot provoke a failure that
    /// only appears under contention; what it catches is a backend that does not enforce
    /// the header at all.
    ///
    /// # Errors
    ///
    /// Any answer other than success, `412` or `501` to a conditional `PUT`, and any
    /// failure of the unconditional one, is returned as the [`StorageError`] the rest of
    /// the adapter would report for it.
    pub async fn check_conditional_writes(
        &self,
        kb: &KbSlug,
    ) -> Result<ConditionalWrites, StorageError> {
        let bucket = self.bucket_name(kb);
        let key = format!("{PROBE_KEY_PREFIX}{}", uuid::Uuid::now_v7());

        let mut wrote = Vec::new();
        let outcome = self.probe_preconditions(&bucket, &key, &mut wrote).await;
        self.remove_probe(&bucket, &key, &wrote).await;
        outcome
    }

    /// Delete the scratch object, and on a versioned bucket every version of it.
    ///
    /// A plain `DeleteObject` on a versioned bucket only adds a delete marker, so each
    /// version the probe wrote would stay stored — billed, listed by
    /// `ListObjectVersions`, and picked up by lifecycle and backup tooling — under a key
    /// that is never reused, once per knowledge base per restart. So every version id a
    /// `PUT` returned is removed by id, and so is the delete marker's own.
    ///
    /// On an unversioned bucket no `PUT` returns a version id and `wrote` is empty, which
    /// leaves exactly the single unconditional delete this did before.
    ///
    /// Failures are logged, never returned: the probe's answer is what the caller asked
    /// for, and the key is private (D48) and never indexed.
    async fn remove_probe(&self, bucket: &str, key: &str, wrote: &[String]) {
        let mut versions: Vec<&str> = wrote.iter().map(String::as_str).collect();
        let marker;
        match self
            .client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
        {
            Ok(deleted) => {
                marker = deleted.version_id().map(str::to_owned);
                if let Some(id) = marker.as_deref().filter(|id| *id != "null") {
                    versions.push(id);
                }
            }
            Err(e) => {
                warn!(
                    bucket = %bucket,
                    key = %key,
                    error = %e,
                    "could not delete the conditional-write probe object; it is private and never indexed"
                );
            }
        }
        for version in versions {
            if let Err(e) = self
                .client
                .delete_object()
                .bucket(bucket)
                .key(key)
                .version_id(version)
                .send()
                .await
            {
                warn!(
                    bucket = %bucket,
                    key = %key,
                    version_id = %version,
                    error = %e,
                    "could not delete a version of the conditional-write probe object; on a \
                     versioned bucket it remains stored until a lifecycle rule removes it"
                );
            }
        }
    }

    /// The three overwrites, and the one judgement made from all of them.
    ///
    /// Every version id a `PUT` returns is pushed onto `wrote`, so the caller can remove
    /// them even when this returns early with an error.
    async fn probe_preconditions(
        &self,
        bucket: &str,
        key: &str,
        wrote: &mut Vec<String>,
    ) -> Result<ConditionalWrites, StorageError> {
        let stored = self
            .client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(PROBE_BODY))
            .send()
            .await
            .map_err(|e| map_put_error(&e, bucket))?;
        record_version(wrote, stored.version_id());
        let etag = stored.e_tag().unwrap_or_default().to_owned();
        let altered = altered_etag(&etag);

        // A precondition that holds. Asked first, because if this is refused nothing
        // below distinguishes enforcement from a backend that never matches.
        let matching = self
            .conditional_put(bucket, key, wrote, |req| req.if_match(&etag))
            .await?;
        let mismatched = self
            .conditional_put(bucket, key, wrote, |req| req.if_match(&altered))
            .await?;
        let existing = self
            .conditional_put(bucket, key, wrote, |req| req.if_none_match("*"))
            .await?;

        // Order matters, and it is the order of how badly each outcome ends.
        //
        // A precondition that was ignored comes first: that is the silent one, the
        // case where a write is lost and nobody is told, and an operator reading
        // `Unsupported` for it would be told the opposite of the risk they have. A
        // backend answering `501` to one header while storing the other does exist —
        // it is the old S3 behaviour several S3-compatibles copied — so this is not a
        // theoretical ordering.
        let if_match = mismatched == Answer::Stored;
        let if_none_match = existing == Answer::Stored;
        Ok(if if_match || if_none_match {
            ConditionalWrites::NotEnforced {
                if_match,
                if_none_match,
            }
        } else if matching == Answer::Unsupported
            || mismatched == Answer::Unsupported
            || existing == Answer::Unsupported
        {
            ConditionalWrites::Unsupported
        } else if matching == Answer::Refused {
            ConditionalWrites::AlwaysRefused
        } else {
            ConditionalWrites::Enforced
        })
    }

    /// Overwrite the probe object under one precondition, and say what the backend
    /// made of it.
    async fn conditional_put(
        &self,
        bucket: &str,
        key: &str,
        wrote: &mut Vec<String>,
        condition: impl FnOnce(PutObjectFluentBuilder) -> PutObjectFluentBuilder,
    ) -> Result<Answer, StorageError> {
        let req = self
            .client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from_static(PROBE_BODY));
        match condition(req).send().await {
            Ok(stored) => {
                record_version(wrote, stored.version_id());
                Ok(Answer::Stored)
            }
            Err(SdkError::ServiceError(e)) if e.raw().status().as_u16() == 412 => {
                Ok(Answer::Refused)
            }
            Err(SdkError::ServiceError(e)) if e.raw().status().as_u16() == 501 => {
                Ok(Answer::Unsupported)
            }
            Err(e) => Err(map_put_error(&e, bucket)),
        }
    }
}

/// Keep a version id worth deleting later. `"null"` is what an unversioned bucket
/// reports for the only version there is, and deleting that by id is not the same
/// request as deleting the object.
fn record_version(wrote: &mut Vec<String>, version_id: Option<&str>) {
    if let Some(id) = version_id.filter(|id| *id != "null") {
        wrote.push(id.to_owned());
    }
}

/// The same `ETag` with its first hex digit changed: syntactically whatever the backend
/// handed us, and guaranteed not to match the object.
///
/// Falls back to [`PROBE_UNMATCHABLE_ETAG`] when there is no hex digit to alter, which
/// covers an absent or unrecognisable `ETag` — better a value that cannot match than one
/// that might, since a value equal to the real `ETag` would be stored and misread as the
/// backend ignoring the header.
fn altered_etag(etag: &str) -> String {
    let mut altered = String::with_capacity(etag.len());
    let mut changed = false;
    for c in etag.chars() {
        if !changed && c.is_ascii_hexdigit() {
            altered.push(if c == '0' { '1' } else { '0' });
            changed = true;
        } else {
            altered.push(c);
        }
    }
    if changed {
        altered
    } else {
        PROBE_UNMATCHABLE_ETAG.to_string()
    }
}

/// What a backend does with the preconditions on a conditional `PUT`, as
/// [`S3Storage::check_conditional_writes`] found it.
///
/// Only [`Self::Enforced`] lets the server start without the operator saying so: the
/// other three all mean a conditional write does not do what the API promises, and they
/// are kept apart because the operator's diagnosis differs even where the decision does
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionalWrites {
    /// A precondition that holds is honoured, and one that does not is refused `412`,
    /// for both `If-Match` and `If-None-Match: *`.
    Enforced,
    /// At least one precondition was accepted and the object stored anyway. Each field
    /// is `true` when that header was ignored.
    ///
    /// The one outcome that loses a write in silence, and so the one reported whenever
    /// it is found — including alongside a `501` to the *other* header, which is a real
    /// combination rather than a theoretical one.
    NotEnforced {
        /// A mismatched `If-Match` was stored rather than refused.
        if_match: bool,
        /// `If-None-Match: *` over an existing object was stored rather than refused.
        if_none_match: bool,
    },
    /// The backend answered `501 Not Implemented` to a conditional `PUT`: it refuses the
    /// header outright, so every conditional write would fail rather than be lost.
    Unsupported,
    /// The backend refused `412` even for an `If-Match` naming the `ETag` it had just
    /// returned, so no conditional write can ever succeed.
    ///
    /// Distinct from [`Self::Unsupported`] only in how it is spelled on the wire — the
    /// remedy is the same — but a backend that answers `412` looks exactly like one that
    /// enforces correctly unless something sends a precondition that ought to hold. The
    /// usual cause is an `ETag` representation mismatch: returned unquoted or weak,
    /// compared as strong and quoted, or the reverse.
    AlwaysRefused,
}

/// What one conditional `PUT` was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// `2xx` — the object was written under this precondition.
    Stored,
    /// `412` — the precondition was evaluated and did not hold.
    Refused,
    /// `501` — the backend does not implement the header.
    Unsupported,
}

/// Convert an HTTP-date string (e.g., "Thu, 01 Jan 1970 00:00:00 GMT") to an
/// `aws_smithy_types::DateTime` for use with conditional request builders.
///
/// Returns [`StorageError::Other`] on parse failure (never panics).
fn parse_http_date_to_smithy(header: &str) -> Result<aws_smithy_types::DateTime, StorageError> {
    let system_time = httpdate::parse_http_date(header)
        .map_err(|e| storage_other(format!("invalid HTTP-date '{header}': {e}")))?;
    Ok(aws_smithy_types::DateTime::from(system_time))
}

/// Extract the complete object length from a `Content-Range: bytes */NNN` header value.
fn extract_complete_length_from_content_range(raw: &HttpResponse) -> Option<u64> {
    let s = raw.headers().get("content-range")?;
    let after_slash = s.split('/').nth(1)?;
    after_slash.trim().parse::<u64>().ok()
}

fn map_get_error(
    err: &SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
    bucket: &str,
    key: &str,
) -> StorageError {
    if let SdkError::ServiceError(inner) = err {
        let raw = inner.raw();
        match raw.status().as_u16() {
            304 => return StorageError::NotModified,
            412 => return StorageError::PreconditionFailed,
            416 => {
                let complete_length = extract_complete_length_from_content_range(raw).unwrap_or(0);
                return StorageError::RangeNotSatisfiable { complete_length };
            }
            _ => {}
        }
    }

    if is_no_such_bucket_sdk(err) {
        return bucket_not_found(bucket);
    }
    if is_not_found_sdk(err) {
        StorageError::NotFound {
            key: key.to_string(),
        }
    } else {
        storage_other(format!("S3 get_object error: {err}"))
    }
}

/// `HeadObject` carries no error body, so a missing bucket and a missing key both
/// arrive as a bare `404 NotFound` and are reported as [`StorageError::NotFound`].
/// The `NoSuchBucket` check is kept for a backend that does say which it was.
fn map_head_error(
    err: &SdkError<aws_sdk_s3::operation::head_object::HeadObjectError>,
    bucket: &str,
    key: &str,
) -> StorageError {
    if let SdkError::ServiceError(inner) = err {
        match inner.raw().status().as_u16() {
            304 => return StorageError::NotModified,
            412 => return StorageError::PreconditionFailed,
            _ => {}
        }
    }

    if is_no_such_bucket_sdk(err) {
        return bucket_not_found(bucket);
    }
    if is_not_found_sdk(err) {
        StorageError::NotFound {
            key: key.to_string(),
        }
    } else {
        storage_other(format!("S3 head_object error: {err}"))
    }
}

fn map_put_error(
    err: &SdkError<aws_sdk_s3::operation::put_object::PutObjectError>,
    bucket: &str,
) -> StorageError {
    if let SdkError::ServiceError(inner) = err
        && inner.raw().status().as_u16() == 412
    {
        return StorageError::PreconditionFailed;
    }
    if is_no_such_bucket_sdk(err) {
        return bucket_not_found(bucket);
    }

    storage_other(format!("S3 put_object error: {err}"))
}

fn map_delete_error(
    err: &SdkError<aws_sdk_s3::operation::delete_object::DeleteObjectError>,
    bucket: &str,
) -> StorageError {
    if let SdkError::ServiceError(inner) = err
        && inner.raw().status().as_u16() == 412
    {
        return StorageError::PreconditionFailed;
    }
    if is_no_such_bucket_sdk(err) {
        return bucket_not_found(bucket);
    }

    storage_other(format!("S3 delete_object error: {err}"))
}

fn map_copy_error(
    err: &SdkError<aws_sdk_s3::operation::copy_object::CopyObjectError>,
    bucket: &str,
    source_key: &str,
) -> StorageError {
    if let SdkError::ServiceError(inner) = err
        && inner.raw().status().as_u16() == 412
    {
        return StorageError::PreconditionFailed;
    }
    if is_no_such_bucket_sdk(err) {
        return bucket_not_found(bucket);
    }
    // A missing copy source is a 404, not a 500. Without this the surfaces above
    // reported an internal error for a client asking to copy something that is not
    // there, which is the one thing they could have corrected on their own.
    if is_not_found_sdk(err) {
        return StorageError::NotFound {
            key: source_key.to_string(),
        };
    }
    storage_other(format!("S3 copy_object error: {err}"))
}

fn storage_other(message: String) -> StorageError {
    StorageError::Other {
        source: Box::new(std::io::Error::other(message)),
    }
}

/// The knowledge base's bucket is gone: a `404` for every surface, never a `5xx`
/// (D43). Reachable for a declared KB whose bucket was removed after provisioning.
fn bucket_not_found(bucket: &str) -> StorageError {
    StorageError::BucketNotFound {
        bucket: bucket.to_string(),
    }
}

fn normalize_etag(etag: &str) -> String {
    if etag.starts_with('"') && etag.ends_with('"') {
        etag.to_string()
    } else {
        format!("\"{etag}\"")
    }
}

#[async_trait]
impl Storage for S3Storage {
    async fn ensure_bucket(&self, kb: &KbSlug) -> Result<(), StorageError> {
        let bucket = self.bucket_name(kb);
        info!(bucket = %bucket, kb = %kb.as_str(), "ensuring bucket exists");
        match self.client.create_bucket().bucket(&bucket).send().await {
            Ok(_) => {
                info!(bucket = %bucket, "bucket created");
                Ok(())
            }
            Err(SdkError::ServiceError(e))
                if e.err().is_bucket_already_owned_by_you()
                    || e.err().is_bucket_already_exists() =>
            {
                info!(bucket = %bucket, "bucket already exists (owned by us)");
                Ok(())
            }
            Err(e) => Err(StorageError::BackendUnavailable {
                message: format!("create_bucket failed for {bucket}: {e}"),
            }),
        }
    }

    async fn probe(&self, kb: &KbSlug) -> Result<(), StorageError> {
        let bucket = self.bucket_name(kb);
        match self.client.head_bucket().bucket(&bucket).send().await {
            Ok(_) => Ok(()),
            // `HeadBucket` carries no body, so a missing bucket is a bare `NotFound`;
            // a store that does attach `NoSuchBucket` is read the same way every
            // other operation reads it, so readiness and the request path agree.
            Err(e) if is_not_found_sdk(&e) || is_no_such_bucket_sdk(&e) => {
                Err(bucket_not_found(&bucket))
            }
            Err(e) => Err(StorageError::BackendUnavailable {
                message: format!("head_bucket failed for {bucket}: {e}"),
            }),
        }
    }

    async fn read_manifest(&self, kb: &KbSlug) -> Result<KbManifest, StorageError> {
        let bucket = self.bucket_name(kb);
        let resp = self
            .client
            .get_object()
            .bucket(&bucket)
            .key(MANIFEST_KEY)
            .send()
            .await
            .map_err(|e| {
                if is_no_such_bucket_sdk(&e) {
                    bucket_not_found(&bucket)
                } else if is_not_found_sdk(&e) {
                    StorageError::NotFound {
                        key: MANIFEST_KEY.into(),
                    }
                } else {
                    StorageError::BackendUnavailable {
                        message: format!("get_object(manifest) failed for {bucket}: {e}"),
                    }
                }
            })?;

        let body = resp
            .body
            .collect()
            .await
            .map_err(|e| StorageError::BackendUnavailable {
                message: format!("reading manifest body from {bucket}: {e}"),
            })?;
        let bytes = body.into_bytes();

        let manifest: KbManifest =
            serde_json::from_slice(&bytes).map_err(|e| StorageError::BackendUnavailable {
                message: format!("deserializing manifest from {bucket}: {e}"),
            })?;

        manifest
            .validate()
            .map_err(|e| StorageError::BackendUnavailable {
                message: format!("manifest validation failed for {bucket}: {e}"),
            })?;

        Ok(manifest)
    }

    async fn write_manifest(&self, kb: &KbSlug, manifest: &KbManifest) -> Result<(), StorageError> {
        let bucket = self.bucket_name(kb);
        let json =
            serde_json::to_vec_pretty(manifest).map_err(|e| StorageError::BackendUnavailable {
                message: format!("serializing manifest for {bucket}: {e}"),
            })?;

        self.client
            .put_object()
            .bucket(&bucket)
            .key(MANIFEST_KEY)
            .content_type("application/json")
            .body(ByteStream::from(Bytes::from(json)))
            .send()
            .await
            .map_err(|e| {
                if is_no_such_bucket_sdk(&e) {
                    bucket_not_found(&bucket)
                } else {
                    StorageError::BackendUnavailable {
                        message: format!("put_object(manifest) failed for {bucket}: {e}"),
                    }
                }
            })?;

        info!(bucket = %bucket, kb = %kb.as_str(), "manifest written");
        Ok(())
    }

    async fn head_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectMeta, StorageError> {
        let bucket = self.bucket_name(kb);
        let key = path.as_str();

        let mut req = self.client.head_object().bucket(&bucket).key(key);

        if let Some(v) = conditionals.if_match {
            req = req.if_match(v);
        }
        if let Some(v) = conditionals.if_none_match {
            req = req.if_none_match(v);
        }
        if let Some(v) = &conditionals.if_modified_since {
            req = req.if_modified_since(parse_http_date_to_smithy(v)?);
        }
        if let Some(v) = &conditionals.if_unmodified_since {
            req = req.if_unmodified_since(parse_http_date_to_smithy(v)?);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| map_head_error(&e, &bucket, key))?;

        Ok(ObjectMeta {
            key: key.to_string(),
            size: u64::try_from(resp.content_length().unwrap_or(0)).unwrap_or(0),
            last_modified: resp.last_modified().map(aws_smithy_types::DateTime::secs),
            content_type: resp.content_type().map(str::to_string),
            etag: resp.e_tag().map(str::to_string),
        })
    }

    async fn get_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<ByteRange>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        let bucket = self.bucket_name(kb);
        let key = path.as_str();

        let mut req = self.client.get_object().bucket(&bucket).key(key);

        if let Some(range) = &range {
            req = req.range(range.to_http_string());
        }
        if let Some(v) = conditionals.if_match {
            req = req.if_match(v);
        }
        if let Some(v) = conditionals.if_none_match {
            req = req.if_none_match(v);
        }
        if let Some(v) = &conditionals.if_modified_since {
            req = req.if_modified_since(parse_http_date_to_smithy(v)?);
        }
        if let Some(v) = &conditionals.if_unmodified_since {
            req = req.if_unmodified_since(parse_http_date_to_smithy(v)?);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| map_get_error(&e, &bucket, key))?;

        let content_type = resp.content_type().map(str::to_string);
        let last_modified = resp.last_modified().map(aws_smithy_types::DateTime::secs);
        let content_range = resp.content_range().map(str::to_string);
        let etag = resp.e_tag().map(str::to_string);
        let size = u64::try_from(resp.content_length().unwrap_or(0)).unwrap_or(0);

        let body = resp
            .body
            .collect()
            .await
            .map_err(|e| StorageError::BackendUnavailable {
                message: format!("reading body for {key} from {bucket}: {e}"),
            })?;
        let bytes = body.into_bytes();

        Ok(ObjectRead {
            bytes,
            meta: ObjectMeta {
                key: key.to_string(),
                size,
                last_modified,
                content_type,
                etag,
            },
            content_range,
        })
    }

    async fn get_object_stream(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        range: Option<ByteRange>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectStream, StorageError> {
        let bucket = self.bucket_name(kb);
        let key = path.as_str();
        let mut req = self.client.get_object().bucket(&bucket).key(key);
        if let Some(range) = &range {
            req = req.range(range.to_http_string());
        }
        if let Some(value) = conditionals.if_match {
            req = req.if_match(value);
        }
        if let Some(value) = conditionals.if_none_match {
            req = req.if_none_match(value);
        }
        if let Some(value) = &conditionals.if_modified_since {
            req = req.if_modified_since(parse_http_date_to_smithy(value)?);
        }
        if let Some(value) = &conditionals.if_unmodified_since {
            req = req.if_unmodified_since(parse_http_date_to_smithy(value)?);
        }
        let resp = req
            .send()
            .await
            .map_err(|error| map_get_error(&error, &bucket, key))?;
        let meta = ObjectMeta {
            key: key.to_string(),
            size: u64::try_from(resp.content_length().unwrap_or(0)).unwrap_or(0),
            last_modified: resp.last_modified().map(aws_smithy_types::DateTime::secs),
            content_type: resp.content_type().map(str::to_string),
            etag: resp.e_tag().map(str::to_string),
        };
        let content_range = resp.content_range().map(str::to_string);
        let chunks = futures::stream::unfold(resp.body, |mut body| async move {
            body.next().await.map(|result| {
                let mapped = result.map_err(|error| StorageError::BackendUnavailable {
                    message: format!("reading streamed S3 body failed: {error}"),
                });
                (mapped, body)
            })
        });
        Ok(ObjectStream {
            chunks: Box::pin(chunks),
            meta,
            content_range,
        })
    }

    async fn put_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        bytes: Bytes,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        let bucket = self.bucket_name(kb);
        let key = path.as_str();

        let mut req = self
            .client
            .put_object()
            .bucket(&bucket)
            .key(key)
            .body(ByteStream::from(bytes));

        if let Some(ct) = content_type {
            req = req.content_type(ct);
        }
        if let Some(v) = conditionals.if_match {
            req = req.if_match(v);
        }
        if let Some(v) = conditionals.if_none_match {
            req = req.if_none_match(v);
        }
        if conditionals.if_modified_since.is_some() {
            debug!("if_modified_since ignored on PUT (not supported by S3 API)");
        }
        if conditionals.if_unmodified_since.is_some() {
            debug!("if_unmodified_since ignored on PUT (not supported by S3 API)");
        }

        let resp = req.send().await.map_err(|e| map_put_error(&e, &bucket))?;

        info!(bucket = %bucket, key = %key, "object stored");
        Ok(PutOutcome {
            etag: resp.e_tag().map(str::to_string),
        })
    }

    async fn put_staged_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        body: StagedBody,
        content_type: Option<&str>,
        conditionals: ConditionalHeaders,
    ) -> Result<PutOutcome, StorageError> {
        let bucket = self.bucket_name(kb);
        let key = path.as_str();
        let byte_stream = match (body.memory_bytes(), body.file_path()) {
            (Some(bytes), None) => ByteStream::from(bytes.clone()),
            (None, Some(path)) => ByteStream::read_from()
                .path(path)
                .length(aws_smithy_types::byte_stream::Length::Exact(body.len()))
                .build()
                .await
                .map_err(|error| {
                    storage_other(format!("opening staged body for upload: {error}"))
                })?,
            _ => return Err(storage_other("invalid staged body storage".into())),
        };
        let mut req = self
            .client
            .put_object()
            .bucket(&bucket)
            .key(key)
            .body(byte_stream);
        if let Some(value) = content_type {
            req = req.content_type(value);
        }
        if let Some(value) = conditionals.if_match {
            req = req.if_match(value);
        }
        if let Some(value) = conditionals.if_none_match {
            req = req.if_none_match(value);
        }
        let resp = req
            .send()
            .await
            .map_err(|error| map_put_error(&error, &bucket))?;
        Ok(PutOutcome {
            etag: resp.e_tag().map(str::to_string),
        })
    }

    async fn copy_object(
        &self,
        kb: &KbSlug,
        source: &ObjectPath,
        destination: &ObjectPath,
        options: CopyObjectOptions,
    ) -> Result<PutOutcome, StorageError> {
        let bucket = self.bucket_name(kb);
        let encoded_source = source
            .as_str()
            .split('/')
            .map(|segment| utf8_percent_encode(segment, COPY_SOURCE_ENCODE_SET).to_string())
            .collect::<Vec<_>>()
            .join("/");
        let copy_source = format!("{bucket}/{encoded_source}");
        let mut req = self
            .client
            .copy_object()
            .bucket(&bucket)
            .key(destination.as_str())
            .copy_source(copy_source);
        if let Some(value) = options.source_if_match {
            req = req.copy_source_if_match(value);
        }
        if let Some(value) = options.destination_if_none_match {
            req = req.if_none_match(value);
        }
        if let Some(value) = options.content_type {
            req = req
                .content_type(value)
                .metadata_directive(aws_sdk_s3::types::MetadataDirective::Replace);
        }
        let resp = req
            .send()
            .await
            .map_err(|error| map_copy_error(&error, &bucket, source.as_str()))?;
        Ok(PutOutcome {
            etag: resp
                .copy_object_result()
                .and_then(|result| result.e_tag())
                .map(normalize_etag),
        })
    }

    async fn delete_object(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
        conditionals: ConditionalHeaders,
    ) -> Result<(), StorageError> {
        let bucket = self.bucket_name(kb);
        let key = path.as_str();

        let mut req = self.client.delete_object().bucket(&bucket).key(key);

        if let Some(v) = conditionals.if_match {
            req = req.if_match(v);
        }
        if conditionals.if_none_match.is_some() {
            debug!("if_none_match ignored on DELETE (not supported by S3 API)");
        }
        if conditionals.if_modified_since.is_some() {
            debug!("if_modified_since ignored on DELETE (not supported by S3 API)");
        }
        if conditionals.if_unmodified_since.is_some() {
            debug!("if_unmodified_since ignored on DELETE (not supported by S3 API)");
        }

        match req.send().await {
            Ok(_) => {
                info!(bucket = %bucket, key = %key, "object deleted");
                Ok(())
            }
            // S3 delete is idempotent — not-found is OK per Metis directive.
            Err(e) if is_not_found_sdk(&e) => {
                info!(bucket = %bucket, key = %key, "delete_object: object not found (idempotent Ok)");
                Ok(())
            }
            Err(e) => Err(map_delete_error(&e, &bucket)),
        }
    }

    /// # Coverage
    ///
    /// The cursor walk is asserted by `list_objects_pagination_walks_cursor` in
    /// `notedthat-storage-fs/tests/support/integration_scenarios.rs`: it seeds 25 keys,
    /// pages through them ten at a time, and requires the pages to compose back into the
    /// seeded set exactly once and in order. That scenario runs against `S3Storage` over
    /// `SeaweedFS`, `FsStorage` over a directory tree and `InMemoryStorage`, so what it
    /// pins is the contract rather than one backend's reading of it. The `ETag` carried
    /// by each entry is asserted against `HEAD`'s by `list_reports_an_etag_that_matches_head`
    /// in the same file.
    ///
    /// The two malformed responses handled below have no coverage, because a real backend
    /// cannot be made to produce either on demand: `is_truncated=false` carrying a
    /// `NextContinuationToken` (warned about and ignored) and `is_truncated=true` carrying
    /// none (failed closed as [`StorageError::BackendUnavailable`], rather than silently
    /// ending the walk). Asserting them needs a stubbed S3 response; `wiremock` can serve
    /// one, as `tests/missing_bucket.rs` and `tests/conditional_write_probe.rs` do, but
    /// nobody has written these two yet.
    async fn list_objects(
        &self,
        kb: &KbSlug,
        prefix: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<ListResponse, StorageError> {
        let bucket = self.bucket_name(kb);
        let max_keys = i32::try_from(limit.min(1000)).unwrap_or(1000);

        let mut req = self
            .client
            .list_objects_v2()
            .bucket(&bucket)
            .max_keys(max_keys);

        if let Some(p) = prefix {
            req = req.prefix(p);
        }

        if let Some(token) = cursor {
            req = req.continuation_token(token);
        }

        let resp = req.send().await.map_err(|e| {
            if is_no_such_bucket_sdk(&e) {
                bucket_not_found(&bucket)
            } else {
                StorageError::BackendUnavailable {
                    message: format!("list_objects_v2 failed for {bucket}: {e}"),
                }
            }
        })?;

        let truncated = resp.is_truncated().unwrap_or(false);
        let next_cursor = if truncated {
            resp.next_continuation_token().map(str::to_string)
        } else {
            if resp.next_continuation_token().is_some() {
                tracing::warn!(
                    "backend returned NextContinuationToken with is_truncated=false; ignoring"
                );
            }
            None
        };
        if truncated && next_cursor.is_none() {
            return Err(StorageError::BackendUnavailable {
                message: "backend returned is_truncated=true without NextContinuationToken".into(),
            });
        }

        let objects = resp
            .contents()
            .iter()
            .map(|obj| {
                let key = obj.key().unwrap_or("").to_string();
                let size = u64::try_from(obj.size().unwrap_or(0)).unwrap_or(0);
                let last_modified = obj.last_modified().map(aws_smithy_types::DateTime::secs);
                // `ListObjectsV2` reports each object's `ETag` — the same string
                // `HEAD` returns for it, quotes included — and no content type. The
                // reconciliation walk (`notedthat_core::reconcile::walk_etags`) reads the
                // stamp from here so a pass never has to `HEAD` an unchanged object.
                let etag = obj.e_tag().map(str::to_string);
                ObjectMeta {
                    key,
                    size,
                    last_modified,
                    content_type: None,
                    etag,
                }
            })
            .collect();

        Ok(ListResponse {
            objects,
            truncated,
            next_cursor,
        })
    }
}

/// Check whether an SDK error is a "not found" / "no such key" error.
fn is_not_found_sdk<E, R>(err: &SdkError<E, R>) -> bool
where
    E: ProvideErrorMetadata,
    R: std::fmt::Debug,
{
    matches!(err.code(), Some("NoSuchKey" | "NotFound"))
}

/// Check whether an SDK error says the bucket itself does not exist.
fn is_no_such_bucket_sdk<E, R>(err: &SdkError<E, R>) -> bool
where
    E: ProvideErrorMetadata,
    R: std::fmt::Debug,
{
    matches!(err.code(), Some("NoSuchBucket"))
}

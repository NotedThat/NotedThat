//! [`S3Storage`]: implements the `notedthat_core::Storage` trait against `aws-sdk-s3`.

use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::SdkError;
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
use tracing::{debug, info};

const MANIFEST_KEY: &str = ".notedthat/manifest.json";
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

    if is_not_found_sdk(err) {
        StorageError::NotFound {
            key: key.to_string(),
        }
    } else {
        storage_other(format!("S3 get_object error: {err}"))
    }
}

fn map_head_error(
    err: &SdkError<aws_sdk_s3::operation::head_object::HeadObjectError>,
    key: &str,
) -> StorageError {
    if let SdkError::ServiceError(inner) = err {
        match inner.raw().status().as_u16() {
            304 => return StorageError::NotModified,
            412 => return StorageError::PreconditionFailed,
            _ => {}
        }
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
) -> StorageError {
    if let SdkError::ServiceError(inner) = err
        && inner.raw().status().as_u16() == 412
    {
        return StorageError::PreconditionFailed;
    }

    storage_other(format!("S3 put_object error: {err}"))
}

fn map_delete_error(
    err: &SdkError<aws_sdk_s3::operation::delete_object::DeleteObjectError>,
) -> StorageError {
    if let SdkError::ServiceError(inner) = err
        && inner.raw().status().as_u16() == 412
    {
        return StorageError::PreconditionFailed;
    }

    storage_other(format!("S3 delete_object error: {err}"))
}

fn map_copy_error(
    err: &SdkError<aws_sdk_s3::operation::copy_object::CopyObjectError>,
    source_key: &str,
) -> StorageError {
    if let SdkError::ServiceError(inner) = err
        && inner.raw().status().as_u16() == 412
    {
        return StorageError::PreconditionFailed;
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

fn normalize_etag(etag: &str) -> String {
    if etag.starts_with('"') && etag.ends_with('"') {
        etag.to_string()
    } else {
        format!("\"{etag}\"")
    }
}

/// Build the `Range` header for a read, honouring only the first requested range.
///
/// [`ByteRange::to_http_string`] already emits the `bytes=` prefix, so joining several
/// of them produced `bytes=0-9, bytes=20-29` — not a valid `Range` value. S3 answered a
/// malformed header by ignoring it and returning the whole object with a 200, while
/// every other backend returned a 206 of the first range, for the same client request.
///
/// Only the first range is served. [`notedthat_core::ObjectRead`] carries a single
/// `content_range`, so no surface above this one can render `multipart/byteranges`
/// anyway; serving the first range is what the rest of the stack already assumes.
fn range_header(ranges: &[ByteRange]) -> Option<String> {
    let first = ranges.first()?;
    if ranges.len() > 1 {
        tracing::debug!(
            requested = ranges.len(),
            "multi-range read: serving only the first range"
        );
    }
    Some(first.to_http_string())
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
                if is_not_found_sdk(&e) {
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
            .map_err(|e| StorageError::BackendUnavailable {
                message: format!("put_object(manifest) failed for {bucket}: {e}"),
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

        let resp = req.send().await.map_err(|e| map_head_error(&e, key))?;

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
        range: Option<Vec<ByteRange>>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectRead, StorageError> {
        let bucket = self.bucket_name(kb);
        let key = path.as_str();

        let mut req = self.client.get_object().bucket(&bucket).key(key);

        if let Some(ranges) = &range
            && let Some(header) = range_header(ranges)
        {
            req = req.range(header);
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

        let resp = req.send().await.map_err(|e| map_get_error(&e, key))?;

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
        range: Option<Vec<ByteRange>>,
        conditionals: ConditionalHeaders,
    ) -> Result<ObjectStream, StorageError> {
        let bucket = self.bucket_name(kb);
        let key = path.as_str();
        let mut req = self.client.get_object().bucket(&bucket).key(key);
        if let Some(ranges) = &range
            && let Some(header) = range_header(ranges)
        {
            req = req.range(header);
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
            .map_err(|error| map_get_error(&error, key))?;
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

        let resp = req.send().await.map_err(|e| map_put_error(&e))?;

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
        let resp = req.send().await.map_err(|error| map_put_error(&error))?;
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
            .map_err(|error| map_copy_error(&error, source.as_str()))?;
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
            Err(e) => Err(map_delete_error(&e)),
        }
    }

    /// # Coverage
    ///
    /// The cursor walk is asserted by `list_objects_pagination_walks_cursor` in
    /// `notedthat-storage-fs/tests/support/integration_scenarios.rs`: it seeds 25 keys,
    /// pages through them ten at a time, and requires the pages to compose back into the
    /// seeded set exactly once and in order. That scenario runs against `S3Storage` over
    /// `SeaweedFS`, `FsStorage` over a directory tree and `InMemoryStorage`, so what it
    /// pins is the contract rather than one backend's reading of it.
    ///
    /// The two malformed responses handled below have no coverage, because a real backend
    /// cannot be made to produce either on demand: `is_truncated=false` carrying a
    /// `NextContinuationToken` (warned about and ignored) and `is_truncated=true` carrying
    /// none (failed closed as [`StorageError::BackendUnavailable`], rather than silently
    /// ending the walk). Asserting them needs a stubbed S3 response, which this crate has
    /// no harness for.
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

        let resp = req
            .send()
            .await
            .map_err(|e| StorageError::BackendUnavailable {
                message: format!("list_objects_v2 failed for {bucket}: {e}"),
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
                ObjectMeta {
                    key,
                    size,
                    last_modified,
                    content_type: None,
                    etag: None,
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

//! Shared domain types, traits, and pure validators for `NotedThat`.
//! See SPECIFICATIONS.md §6.6–6.12 for the source of truth.
#![deny(missing_docs)]

pub mod access;
pub mod auth;
pub mod bucket_name;
pub mod conditional;
pub mod error;
pub mod etag;
pub mod kb;
pub mod object_path;
pub mod preconditions;
mod public_read;
pub mod range;
pub mod search;
pub mod setting;
pub mod slug;
pub mod staging;
pub mod storage;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;

pub use access::KeyPattern;
pub use auth::{
    extract_basic_from_header, extract_bearer_from_header, verify_basic_credentials,
    verify_bearer_token,
};
pub use bucket_name::{
    BUCKET_NAME_MAX, BUCKET_NAME_PREFIX, derive_bucket_name, validate_bucket_name,
};
pub use conditional::ConditionalHeaders;
pub use error::{Error, StorageError};
pub use etag::{EtagHasher, compute_etag};
pub use kb::{Kb, KbManifest, ManifestEmbedding, ObjectMeta};
pub use object_path::{ObjectPath, is_internal_path};
pub use preconditions::{
    ObjectState, evaluate_read_preconditions, evaluate_write_preconditions, matches_if_match,
    matches_if_none_match, parse_http_date_or_err, resolve_range, unix_seconds, unix_seconds_i64,
};
pub use public_read::{PublicReadCapability, PublicReadPolicy};
pub use range::{
    ByteRange, LineIndex, LineRange, ParsedRanges, RangeParseError, parse_line_range_header,
    parse_range_header,
};
pub use setting::{flag_for, setting};
pub use slug::{KbSlug, TenantSlug};
pub use staging::{
    AsyncReadSeek, ReadSeek, StageError, StagedBody, StagingConfig, UPLOAD_TMP_DIR_ENV,
};
pub use storage::{
    CopyObjectOptions, ListResponse, ObjectChunkStream, ObjectRead, ObjectStream, PutOutcome,
    Storage,
};

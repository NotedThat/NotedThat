//! Shared domain types, traits, and pure validators for `NotedThat`.
//! See SPECIFICATIONS.md §6.6–6.12 for the source of truth.
#![deny(missing_docs)]

pub mod auth;
pub mod bucket_name;
pub mod conditional;
pub mod error;
pub mod kb;
pub mod object_path;
pub mod range;
pub mod search;
pub mod slug;
pub mod staging;
pub mod storage;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;

pub use auth::{
    extract_basic_from_header, extract_bearer_from_header, verify_basic_credentials,
    verify_bearer_token,
};
pub use bucket_name::{
    BUCKET_NAME_MAX, BUCKET_NAME_PREFIX, derive_bucket_name, validate_bucket_name,
};
pub use conditional::ConditionalHeaders;
pub use error::{Error, StorageError};
pub use kb::{Kb, KbManifest, ManifestEmbedding, ObjectMeta};
pub use object_path::ObjectPath;
pub use range::{
    ByteRange, LineIndex, LineRange, ParsedRanges, RangeParseError, parse_line_range_header,
    parse_range_header,
};
pub use slug::{KbSlug, TenantSlug};
pub use staging::{
    AsyncReadSeek, ReadSeek, StageError, StagedBody, StagingConfig, UPLOAD_TMP_DIR_ENV,
};
pub use storage::{
    CopyObjectOptions, ListResponse, ObjectChunkStream, ObjectRead, ObjectStream, PutOutcome,
    Storage,
};

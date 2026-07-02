//! Shared domain types, traits, and pure validators for `NotedThat`.
//! See SPECIFICATIONS.md §6.6–6.12 for the source of truth.
#![deny(missing_docs)]

pub mod auth;
pub mod bucket_name;
pub mod error;
pub mod kb;
pub mod object_path;
pub mod slug;
pub mod storage;

pub use auth::{extract_bearer_from_header, verify_bearer_token};
pub use bucket_name::{derive_bucket_name, validate_bucket_name, BUCKET_NAME_MAX, BUCKET_NAME_PREFIX};
pub use error::{Error, StorageError};
pub use kb::{Kb, KbManifest, ObjectMeta};
pub use object_path::ObjectPath;
pub use slug::{KbSlug, TenantSlug};
pub use storage::{ListResponse, ObjectRead, Storage};

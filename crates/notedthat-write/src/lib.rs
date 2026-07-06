//! Shared write path for `NotedThat` protocol surfaces.

mod commit;
mod error;
mod mime;
pub mod patch;

pub use commit::{MAX_UPLOAD_BYTES, check_size, commit, commit_delete};
pub use error::WriteError;
pub use mime::sniff_content_type;
pub use patch::{PatchMode, patch};

//! Shared write path for `NotedThat` protocol surfaces.

use notedthat_core::PutOutcome;

mod commit;
mod error;
mod mime;
pub mod patch;
pub mod replace;

pub use commit::{MAX_UPLOAD_BYTES, check_size, commit, commit_delete};
pub use error::WriteError;
pub use mime::sniff_content_type;
pub use patch::{PatchMode, patch};
pub use replace::{ReplaceRequest, replace};

/// Outcome of a replace write operation.
pub struct ReplaceOutcome {
    /// Result of storing the replaced object body.
    pub put_outcome: PutOutcome,
    /// Number of old-string matches found in the original object body.
    pub match_count: u64,
}

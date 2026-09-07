//! Object-level CRUD + PATCH + POST replace handlers on
//! `/v1/knowledgebases/{kb_slug}/{*object_path}`.

mod patch;
mod read;
mod replace;
mod write;

pub(super) use patch::patch_object;
pub(super) use read::{get_object, head_object};
pub(super) use replace::post_object;
pub(super) use write::{delete_object, put_object};

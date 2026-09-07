//! Opt-in maintenance of the reserved OKF files, `index.md` and `log.md`.
//!
//! This is the one part of `NotedThat` that **rewrites files the user authored**,
//! so it is off unless an operator switches it on per knowledge base, and it is
//! best-effort in the strongest sense: it can never fail, delay, or retry a
//! user's write.
//!
//! # How recursion is prevented
//!
//! Three independent guards, all of them:
//!
//! 1. The producer ([`notedthat_indexer::worker::IndexerWorker`]) refuses to
//!    enqueue an event for a reserved file.
//! 2. This worker writes through [`notedthat_core::Storage::put_object`]
//!    directly rather than through [`crate::commit`], so it structurally cannot
//!    enqueue a maintenance event; it enqueues an `IndexEvent` explicitly
//!    afterwards so search still sees the updated listing.
//! 3. It filters reserved keys again on the way in.
//!
//! Given (1) and (2) no depth or origin field is needed on the event, and adding
//! one would only invite someone to relax (1) later.
//!
//! See SPECIFICATIONS.md D48.

mod worker;

pub use worker::{
    COALESCE_WINDOW, MAX_ATTEMPTS, OkfMaintenanceConfig, OkfMaintenanceWorker, log_bullet,
};

//! Open Knowledge Format (OKF) v0.2 wire types.
//!
//! This module holds only the types that cross the API boundary — what a search
//! hit carries and what a search filter accepts — plus the pure, time-invariant
//! derivations behind them. Parsing lives in the `notedthat-okf` crate, so
//! `notedthat-core` gains no YAML dependency.
//!
//! See SPECIFICATIONS.md D48, which amends D33.

mod actor;
pub use actor::{Actor, ActorKind};

mod annotation;
pub use annotation::OkfAnnotation;

mod instant;
pub use instant::OkfInstant;

mod trust;
pub use trust::{OkfStatus, OkfTrust};

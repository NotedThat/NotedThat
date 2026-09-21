//! What `/readyz` reports about the backends it depends on.
//!
//! The server probes the storage backend and the vector store in the
//! background and publishes a [`ReadinessSnapshot`] through a
//! [`tokio::sync::watch`] channel; the route reads the latest value and never
//! probes anything itself, so a flood of readiness requests costs the backends
//! nothing. The event backend is not part of the snapshot: its readiness is a
//! connection-state read the route makes inline (D55).
//!
//! A backend's own error message never reaches the response. The route is
//! unauthenticated, and a client error can quote an endpoint or a
//! credential-bearing URL, so a check reports one of a fixed set of
//! [`Unready`] reasons and the server logs the detail once per transition.
//!
//! Readiness is about the backend, not the data in it: a probe that the
//! backend *answered* with "not found" leaves the replica ready, because it is
//! up and every other knowledge base keeps serving. The check still says so
//! (`degraded`, `not_found`), for the operator who deleted a bucket.

use serde::Serialize;

/// Why a check is not `ok`. Closed set: the response body can say nothing
/// else about a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unready {
    /// The probe did not answer within its deadline.
    Timeout,
    /// The backend answered with an error, or could not be reached at all.
    Unreachable,
    /// The backend answered, and the thing probed for is gone: a knowledge
    /// base's bucket or directory deleted after startup. Reported, but not a
    /// readiness failure — the backend is up.
    NotFound,
}

impl Unready {
    /// Whether this reason means the backend itself is unavailable, as opposed
    /// to answering that the probed thing is gone.
    #[must_use]
    pub fn is_outage(self) -> bool {
        match self {
            Self::Timeout | Self::Unreachable => true,
            Self::NotFound => false,
        }
    }

    /// The `reason` value the route renders.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Unreachable => "unreachable",
            Self::NotFound => "not_found",
        }
    }
}

/// One backend's latest probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// The selector value the backend answers to (`s3`, `fs`, `qdrant`), so a
    /// reader can tell which deployment choice is failing.
    pub backend: &'static str,
    /// `Ok` when the last probe succeeded.
    pub outcome: Result<(), Unready>,
}

impl Check {
    /// A check whose last probe succeeded.
    #[must_use]
    pub fn ok(backend: &'static str) -> Self {
        Self {
            backend,
            outcome: Ok(()),
        }
    }

    /// A check whose last probe failed for `reason`.
    #[must_use]
    pub fn unready(backend: &'static str, reason: Unready) -> Self {
        Self {
            backend,
            outcome: Err(reason),
        }
    }

    /// Whether this check leaves the replica ready: the last probe succeeded,
    /// or failed in a way that is not the backend's outage.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        match self.outcome {
            Ok(()) => true,
            Err(reason) => !reason.is_outage(),
        }
    }

    /// The `{"backend", "status", "reason"?}` object the route renders:
    /// `ok`, `degraded` (answered, but the probed thing is gone) or
    /// `unavailable`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self.outcome {
            Ok(()) => serde_json::json!({ "backend": self.backend, "status": "ok" }),
            Err(reason) => serde_json::json!({
                "backend": self.backend,
                "status": if reason.is_outage() { "unavailable" } else { "degraded" },
                "reason": reason.as_str(),
            }),
        }
    }
}

/// The latest probe of every backend the poller covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessSnapshot {
    /// The object store: `s3` or `fs`.
    pub storage: Check,
    /// The vector store behind search and indexing.
    pub search: Check,
}

impl ReadinessSnapshot {
    /// Every check ok — what the poller publishes before its first probe,
    /// since startup provisioning has just reached both backends.
    #[must_use]
    pub fn ok(storage_backend: &'static str, search_backend: &'static str) -> Self {
        Self {
            storage: Check::ok(storage_backend),
            search: Check::ok(search_backend),
        }
    }

    /// Whether every check leaves the replica ready (see [`Check::is_ready`]).
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.storage.is_ready() && self.search.is_ready()
    }
}

/// The route's end of the channel the poller publishes on.
///
/// `borrow()` keeps answering with the last value after the sender is gone,
/// so a test can hand a state a fixed snapshot without running a poller.
pub type ReadinessReceiver = tokio::sync::watch::Receiver<ReadinessSnapshot>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ok_check_renders_without_a_reason() {
        assert_eq!(
            Check::ok("s3").to_json(),
            serde_json::json!({ "backend": "s3", "status": "ok" })
        );
    }

    #[test]
    fn an_unready_check_renders_its_reason_and_nothing_else() {
        let json = Check::unready("qdrant", Unready::Timeout).to_json();
        assert_eq!(
            json,
            serde_json::json!({ "backend": "qdrant", "status": "unavailable", "reason": "timeout" })
        );
    }

    #[test]
    fn reasons_are_the_documented_vocabulary() {
        for (reason, expected) in [
            (Unready::Timeout, "timeout"),
            (Unready::Unreachable, "unreachable"),
            (Unready::NotFound, "not_found"),
        ] {
            assert_eq!(reason.as_str(), expected);
            assert_eq!(serde_json::to_value(reason).unwrap(), expected);
        }
    }

    #[test]
    fn a_not_found_check_is_degraded_and_still_ready() {
        let check = Check::unready("fs", Unready::NotFound);
        assert!(check.is_ready());
        assert_eq!(
            check.to_json(),
            serde_json::json!({ "backend": "fs", "status": "degraded", "reason": "not_found" })
        );
    }

    #[test]
    fn ready_unless_a_backend_is_out() {
        assert!(ReadinessSnapshot::ok("fs", "qdrant").is_ready());
        let witness_gone = ReadinessSnapshot {
            storage: Check::unready("fs", Unready::NotFound),
            search: Check::ok("qdrant"),
        };
        assert!(witness_gone.is_ready(), "the backend answered; it is up");
        for reason in [Unready::Timeout, Unready::Unreachable] {
            let storage_down = ReadinessSnapshot {
                storage: Check::unready("s3", reason),
                search: Check::ok("qdrant"),
            };
            assert!(!storage_down.is_ready());
            let search_down = ReadinessSnapshot {
                storage: Check::ok("fs"),
                search: Check::unready("qdrant", reason),
            };
            assert!(!search_down.is_ready());
        }
    }
}

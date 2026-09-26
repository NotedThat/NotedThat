//! Metric names and label vocabularies, shared by every crate that records one
//! (D69).
//!
//! The names live here, next to the domain types they describe, for the same
//! reason the log codes are constants: a metric is a contract with whoever
//! wrote the dashboard, and a typo in a string literal renames it silently. The
//! exporter is not here — it is `notedthat-server`'s alone, and this module
//! pulls in nothing to record with, so `notedthat-core` keeps its place at the
//! bottom of the graph (D28).
//!
//! # What may never be a label
//!
//! No label value may carry an object key, a principal, a bearer token or a
//! subscriber's address. Every series here is labelled from a closed set of
//! literals or from a knowledge base slug declared at startup, so the
//! cardinality is bounded by configuration rather than by traffic, and a
//! scrape leaks nothing the caller did not already have to know. The
//! `metrics_carry_no_identifiers` suite in `notedthat-server` is the backstop.

use crate::StorageError;

/// Metric names.
///
/// Prometheus conventions: a counter ends `_total`, a duration is in seconds
/// and says so, and a base unit is never prefixed.
pub mod name {
    /// Requests served, on every surface.
    pub const HTTP_REQUESTS: &str = "notedthat_http_requests_total";
    /// How long a request took to produce a response head.
    pub const HTTP_REQUEST_DURATION: &str = "notedthat_http_request_duration_seconds";
    /// Requests in flight right now.
    pub const HTTP_IN_FLIGHT: &str = "notedthat_http_requests_in_flight";

    /// Searches answered.
    pub const SEARCH_REQUESTS: &str = "notedthat_search_requests_total";
    /// How long a search took, end to end.
    pub const SEARCH_DURATION: &str = "notedthat_search_duration_seconds";
    /// Hits a search returned, after every access filter.
    pub const SEARCH_HITS: &str = "notedthat_search_hits";

    /// Calls to the embedding endpoint.
    pub const EMBEDDING_REQUESTS: &str = "notedthat_embedding_requests_total";
    /// How long an embedding call took.
    pub const EMBEDDING_DURATION: &str = "notedthat_embedding_duration_seconds";
    /// Embedding calls that failed, by the kind of failure.
    pub const EMBEDDING_ERRORS: &str = "notedthat_embedding_errors_total";
    /// Texts handed to the embedding endpoint in one call.
    pub const EMBEDDING_TEXTS: &str = "notedthat_embedding_texts";

    /// Events waiting in the indexing queue (D38).
    pub const INDEX_QUEUE_DEPTH: &str = "notedthat_index_queue_depth";
    /// How many the indexing queue holds.
    pub const INDEX_QUEUE_CAPACITY: &str = "notedthat_index_queue_capacity";
    /// Indexing events accepted onto the queue.
    pub const INDEX_EVENTS_ENQUEUED: &str = "notedthat_index_events_enqueued_total";
    /// Writes refused because the queue was full (D38).
    pub const INDEX_EVENTS_REFUSED: &str = "notedthat_index_events_refused_total";
    /// Indexing events the worker finished with, by outcome.
    pub const INDEX_EVENTS_COMPLETED: &str = "notedthat_index_events_completed_total";
    /// Knowledge bases marked as possibly behind the tree (D50).
    pub const INDEX_STALE_MARKS: &str = "notedthat_index_stale_marks_total";
    /// `1` while the indexer worker's loop is running, `0` once it has ended.
    pub const INDEX_WORKER_ALIVE: &str = "notedthat_index_worker_alive";

    /// Vector-store calls.
    pub const VECTOR_STORE_OPERATIONS: &str = "notedthat_vector_store_operations_total";
    /// How long a vector-store call took.
    pub const VECTOR_STORE_DURATION: &str = "notedthat_vector_store_operation_duration_seconds";
    /// Vector-store calls that failed, by the kind of failure.
    pub const VECTOR_STORE_ERRORS: &str = "notedthat_vector_store_errors_total";

    /// Object change events handed to the log (D55).
    pub const EVENTS_PUBLISHED: &str = "notedthat_events_published_total";
    /// Object change events the log refused.
    pub const EVENTS_PUBLISH_FAILED: &str = "notedthat_events_publish_failed_total";
    /// Subscribers on the events route right now.
    pub const EVENTS_SUBSCRIBERS: &str = "notedthat_events_subscribers";
    /// Subscribers turned away because their position is no longer retained.
    pub const EVENTS_REPLAY_GONE: &str = "notedthat_events_replay_gone_total";

    /// Filesystem watches lost at runtime (D50).
    pub const FS_WATCH_LOST: &str = "notedthat_fs_watch_lost_total";

    /// Reconciliation passes run, by what asked for them and how they ended.
    ///
    /// Backend-neutral: `fs` reconciles from its watcher (D50) and `s3` at
    /// startup and on demand (D67), and an operator wants one family for both.
    pub const RECONCILE_PASSES: &str = "notedthat_reconcile_passes_total";
    /// How long a reconciliation pass took.
    pub const RECONCILE_DURATION: &str = "notedthat_reconcile_duration_seconds";
    /// What reconciliation passes found, by class.
    pub const RECONCILE_OBJECTS: &str = "notedthat_reconcile_objects_total";
    /// Objects the last whole-knowledge-base pass found in the backend.
    pub const RECONCILE_OBJECTS_ON_DISK: &str = "notedthat_reconcile_objects_on_disk";

    /// Storage backend calls.
    pub const STORAGE_OPERATIONS: &str = "notedthat_storage_operations_total";
    /// How long a storage backend call took.
    pub const STORAGE_DURATION: &str = "notedthat_storage_operation_duration_seconds";
    /// Storage calls that failed, by the kind of failure.
    pub const STORAGE_ERRORS: &str = "notedthat_storage_errors_total";
    /// `1` when a knowledge base's bucket refused a `PUT` whose precondition did not
    /// hold, `0` when it stored it anyway; set once at startup on `s3` (D70).
    pub const STORAGE_CONDITIONAL_WRITES_ENFORCED: &str =
        "notedthat_storage_conditional_writes_enforced";

    /// Always `1`; carries what this build is as labels.
    pub const BUILD_INFO: &str = "notedthat_build_info";
}

/// Label keys.
pub mod label {
    /// Which surface served the request.
    pub const SURFACE: &str = "surface";
    /// The matched route *pattern*, never the request's path.
    pub const ROUTE: &str = "route";
    /// The HTTP method.
    pub const METHOD: &str = "method";
    /// The HTTP status code.
    pub const STATUS: &str = "status";
    /// The knowledge base slug.
    pub const KB: &str = "kb";
    /// What became of the operation.
    pub const OUTCOME: &str = "outcome";
    /// The operation's name within its backend.
    pub const OP: &str = "op";
    /// Which storage backend answered.
    pub const BACKEND: &str = "backend";
    /// Which side of the system asked for an embedding.
    pub const PHASE: &str = "phase";
    /// Why a watch was lost.
    pub const REASON: &str = "reason";
    /// The kind of object change event.
    pub const KIND: &str = "kind";
    /// What asked for a reconciliation pass.
    pub const CAUSE: &str = "cause";
    /// Which class of a reconciliation report this count is.
    pub const CLASS: &str = "class";
    /// The kind of failure, from an error type's own variants.
    pub const ERROR_KIND: &str = "error_kind";
    /// The release version.
    pub const VERSION: &str = "version";
    /// The build's revision, when the build was told one.
    pub const REVISION: &str = "revision";
}

/// The `route` label for a request no route matched.
///
/// A 404 for an unrouted path still deserves counting — it is how a
/// misconfigured client shows up — but its path is attacker-chosen and
/// unbounded, so every one of them shares this series.
pub const ROUTE_UNMATCHED: &str = "<unmatched>";

/// Values for the `surface` label.
///
/// Closed, and deliberately not [`crate::EventSource`]: that records who
/// *wrote* an object, while this records which family of routes served a
/// request, and `browse` and `root` have no writes to attribute.
pub mod surface {
    /// The versioned machine API under `/api/v1`.
    pub const API: &str = "api";
    /// The `WebDAV` surface under `/webdav`.
    pub const WEBDAV: &str = "webdav";
    /// The MCP endpoint, and the API calls MCP makes on a caller's behalf.
    pub const MCP: &str = "mcp";
    /// The human-facing pages under `/browse`.
    pub const BROWSE: &str = "browse";
    /// The unversioned root routes: the probes, `/llms.txt`, the metadata.
    pub const ROOT: &str = "root";
}

/// Values for the `outcome` label on a storage or vector-store call.
pub mod outcome {
    /// The call did what was asked.
    pub const OK: &str = "ok";
    /// The object or bucket is not there. Ordinary control flow, not a fault.
    pub const NOT_FOUND: &str = "not_found";
    /// A conditional request was refused, or the object was unmodified.
    /// Ordinary control flow: it is how a caller's `If-Match` does its job.
    pub const PRECONDITION: &str = "precondition";
    /// The backend could not be reached, or answered that it could not serve.
    /// This is the one an operator should alert on.
    pub const UNAVAILABLE: &str = "unavailable";
    /// Anything else the backend refused.
    pub const ERROR: &str = "error";
    /// The caller gave up before the call answered, so there is no outcome to
    /// report — an abandoned search, a dropped connection mid-`GET`.
    ///
    /// Recorded from `Drop` rather than after the await, because nothing after
    /// the await runs once the future is dropped. Without it the operation and
    /// duration families would simply disagree, and disagree *worst* on slow
    /// calls, which are the ones a caller gives up on: the histogram would go
    /// quiet under exactly the condition it exists to reveal.
    pub const CANCELLED: &str = "cancelled";
}

/// Values for the `phase` label on an embedding call.
///
/// The same [`crate`]-level embedder serves both, and they have entirely
/// different shapes: indexing sends batches and tolerates latency, a query
/// sends one string and a caller is waiting on it.
pub mod phase {
    /// Embedding an object's chunks for the index.
    pub const INDEX: &str = "index";
    /// Embedding a search query.
    pub const QUERY: &str = "query";
}

/// Values for the `class` label on a reconciliation report (D50, D67).
pub mod reconcile_class {
    /// Objects already indexed at the version the backend holds.
    pub const UNCHANGED: &str = "unchanged";
    /// Objects whose bytes the index has not caught up with.
    pub const CHANGED: &str = "changed";
    /// Objects the index holds that the backend no longer has.
    pub const ORPHANED: &str = "orphaned";
}

/// Values for the `outcome` label on an indexing event the worker finished.
pub mod index_outcome {
    /// The worker indexed it.
    pub const SUCCEEDED: &str = "succeeded";
    /// The worker could not index it.
    pub const FAILED: &str = "failed";
}

/// Values for the `reason` label on a lost filesystem watch (D50).
pub mod watch_lost_reason {
    /// The kernel's per-user watch limit was reached.
    pub const MAX_FILES_WATCH: &str = "max_files_watch";
    /// Any other watcher failure.
    pub const ERROR: &str = "error";
}

/// Every histogram in the catalogue, with the buckets it is rendered in.
///
/// The exporter renders a histogram as a *summary* with per-process quantiles
/// unless buckets are registered for it, and a summary's quantiles cannot be
/// aggregated across replicas — which is exactly what §7.3's search P95 needs.
/// So the exporter registers this table in one loop at startup, and
/// `every_histogram_has_buckets` below fails the build if a histogram is added
/// without an entry, because the failure mode is otherwise silent: the metric
/// still appears, just in a shape no `histogram_quantile` can read.
pub const HISTOGRAM_BUCKETS: &[(&str, &[f64])] = &[
    // Time to response head, across every surface. A `/healthz` is tens of
    // microseconds; a 16 MiB `PUT` to S3 is seconds.
    (
        name::HTTP_REQUEST_DURATION,
        &[
            0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
        ],
    ),
    // One remote embedding call plus one Qdrant query. Geometric ×2 from 10 ms
    // puts four bucket edges inside 50–400 ms, so the P95 interpolation band is
    // at most twice as wide as the answer — a coarser ladder would report P95
    // as "somewhere in 0.1–1.0 s", which no latency target can be set from.
    (
        name::SEARCH_DURATION,
        &[0.01, 0.025, 0.05, 0.1, 0.2, 0.4, 0.8, 1.6, 3.2, 6.4],
    ),
    // Counts, not seconds. The `0` bucket is the point: "how often does a
    // search come back empty" is the question behind most reports that the
    // index is broken, and `le="0"` answers it exactly.
    (
        name::SEARCH_HITS,
        &[0.0, 1.0, 2.0, 3.0, 5.0, 10.0, 20.0, 50.0, 100.0],
    ),
    // Both phases share this: an index-phase call embeds a whole chunk batch, a
    // query-phase call one short string, and `phase` separates them. The 60 s
    // top edge is there because a rate-limited or cold endpoint genuinely takes
    // that long, and that must be distinguishable from `+Inf`.
    (
        name::EMBEDDING_DURATION,
        &[0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0],
    ),
    (
        name::EMBEDDING_TEXTS,
        &[1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0],
    ),
    // The widest ladder, because one family spans two backends: an `fs`
    // `head_object` is a `stat` at ~100 µs while an `s3` staged put of 16 MiB
    // is seconds. Starting at 500 µs keeps `fs` out of a single first bucket;
    // the `backend` label keeps the two distributions apart.
    (
        name::STORAGE_DURATION,
        &[
            0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
        ],
    ),
    // Qdrant over the network, so no sub-millisecond bucket: nothing here is local.
    (
        name::VECTOR_STORE_DURATION,
        &[
            0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
        ],
    ),
    // Minutes, not milliseconds: a pass lists a whole bucket or walks a whole
    // tree and reads the whole index. The 900 s top edge is so that a large
    // startup pass is a number an operator can watch grow rather than `+Inf`.
    (
        name::RECONCILE_DURATION,
        &[0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0],
    ),
];

/// The `outcome` label for a storage call that returned `result`.
///
/// Three of the [`StorageError`] variants are ordinary control flow rather than
/// faults, and counting them as errors would make the error rate track client
/// behaviour: [`StorageError::NotFound`] is how an idempotent delete and every
/// conditional read work, and `NotModified` / `PreconditionFailed` /
/// `RangeNotSatisfiable` are HTTP conditional semantics the backend is
/// answering correctly. Only the remaining variants mean something is wrong,
/// which is what makes an alert on `outcome="error"` worth waking someone for.
#[must_use]
pub fn storage_outcome<T>(result: &Result<T, StorageError>) -> &'static str {
    match result {
        Ok(_) => outcome::OK,
        Err(StorageError::NotFound { .. }) => outcome::NOT_FOUND,
        Err(
            StorageError::NotModified
            | StorageError::PreconditionFailed
            | StorageError::RangeNotSatisfiable { .. },
        ) => outcome::PRECONDITION,
        // Its own value, not the catch-all: this is the one an operator alerts
        // on, and `docs/CONFIGURATION.md` names the series. Folded into
        // `error` it would be a documented alert that can never fire.
        Err(StorageError::BackendUnavailable { .. }) => outcome::UNAVAILABLE,
        Err(_) => outcome::ERROR,
    }
}

/// The `error_kind` label for a storage failure, or `None` when `error` is
/// ordinary control flow and belongs to no error series.
///
/// The variant's name and nothing else: `NotFound` carries a key,
/// `BackendUnavailable` a backend message that can quote an endpoint, and
/// `Other` a boxed source — none of which may reach a label.
#[must_use]
pub fn storage_error_kind(error: &StorageError) -> Option<&'static str> {
    match error {
        StorageError::NotFound { .. }
        | StorageError::NotModified
        | StorageError::PreconditionFailed
        | StorageError::RangeNotSatisfiable { .. } => None,
        StorageError::BucketNotFound { .. } => Some("bucket_not_found"),
        StorageError::BackendUnavailable { .. } => Some("backend_unavailable"),
        StorageError::Other { .. } => Some("other"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HISTOGRAM_BUCKETS, StorageError, label, name, outcome, storage_error_kind, storage_outcome,
    };

    /// Every metric name in the catalogue.
    const ALL_NAMES: &[&str] = &[
        name::HTTP_REQUESTS,
        name::HTTP_REQUEST_DURATION,
        name::HTTP_IN_FLIGHT,
        name::SEARCH_REQUESTS,
        name::SEARCH_DURATION,
        name::SEARCH_HITS,
        name::EMBEDDING_REQUESTS,
        name::EMBEDDING_DURATION,
        name::EMBEDDING_ERRORS,
        name::EMBEDDING_TEXTS,
        name::INDEX_QUEUE_DEPTH,
        name::INDEX_QUEUE_CAPACITY,
        name::INDEX_EVENTS_ENQUEUED,
        name::INDEX_EVENTS_REFUSED,
        name::INDEX_EVENTS_COMPLETED,
        name::INDEX_STALE_MARKS,
        name::INDEX_WORKER_ALIVE,
        name::VECTOR_STORE_OPERATIONS,
        name::VECTOR_STORE_DURATION,
        name::VECTOR_STORE_ERRORS,
        name::EVENTS_PUBLISHED,
        name::EVENTS_PUBLISH_FAILED,
        name::EVENTS_SUBSCRIBERS,
        name::EVENTS_REPLAY_GONE,
        name::FS_WATCH_LOST,
        name::RECONCILE_PASSES,
        name::RECONCILE_DURATION,
        name::RECONCILE_OBJECTS,
        name::RECONCILE_OBJECTS_ON_DISK,
        name::STORAGE_OPERATIONS,
        name::STORAGE_DURATION,
        name::STORAGE_ERRORS,
        name::STORAGE_CONDITIONAL_WRITES_ENFORCED,
        name::BUILD_INFO,
    ];

    /// Prometheus reads `_total` as a counter's suffix and the unit from the
    /// name's tail. A metric renamed into the wrong shape breaks a dashboard
    /// quietly, so the convention is asserted rather than trusted.
    #[test]
    fn every_name_is_prefixed_and_well_formed() {
        for metric in ALL_NAMES {
            assert!(
                metric.starts_with("notedthat_"),
                "{metric} does not carry the project prefix"
            );
            assert!(
                metric
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{metric} is not a valid Prometheus metric name"
            );
            assert!(
                !(metric.ends_with("_total") && metric.contains("_duration_")),
                "{metric} is spelled as both a counter and a duration"
            );
        }
    }

    #[test]
    fn no_two_metrics_share_a_name() {
        let mut seen = ALL_NAMES.to_vec();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "two metrics share a name");
    }

    /// A histogram with no registered buckets is rendered as a summary, whose
    /// quantiles cannot be aggregated across replicas. It still appears, so
    /// nothing fails at runtime — which is why it has to fail here.
    #[test]
    fn every_histogram_has_buckets() {
        for (metric, buckets) in HISTOGRAM_BUCKETS {
            assert!(
                ALL_NAMES.contains(metric),
                "{metric} has buckets but is not in the catalogue"
            );
            assert!(!buckets.is_empty(), "{metric} has an empty bucket set");
            assert!(
                buckets.windows(2).all(|w| w[0] < w[1]),
                "{metric}'s buckets must be strictly ascending"
            );
        }
        // Every `_seconds` metric is a histogram here, so the table must cover
        // each one. `notedthat_search_hits` and `..._texts` are histograms too
        // and are asserted by name.
        for metric in ALL_NAMES.iter().filter(|m| m.ends_with("_seconds")) {
            assert!(
                HISTOGRAM_BUCKETS.iter().any(|(name, _)| name == metric),
                "{metric} is a duration histogram with no buckets registered"
            );
        }
        for metric in [name::SEARCH_HITS, name::EMBEDDING_TEXTS] {
            assert!(
                HISTOGRAM_BUCKETS.iter().any(|(name, _)| *name == metric),
                "{metric} is a histogram with no buckets registered"
            );
        }
    }

    #[test]
    fn label_keys_are_lowercase_words() {
        for key in [
            label::SURFACE,
            label::ROUTE,
            label::METHOD,
            label::STATUS,
            label::KB,
            label::OUTCOME,
            label::OP,
            label::BACKEND,
            label::PHASE,
            label::CAUSE,
            label::CLASS,
            label::ERROR_KIND,
            label::REASON,
            label::KIND,
            label::VERSION,
            label::REVISION,
        ] {
            assert!(
                key.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{key} is not a valid Prometheus label name"
            );
        }
    }

    /// The classification that decides what an operator is paged for. A missing
    /// object is how an idempotent delete works; it is not an outage.
    #[test]
    fn ordinary_control_flow_is_not_counted_as_a_storage_error() {
        for error in [
            StorageError::NotFound {
                key: "some/key.md".to_string(),
            },
            StorageError::NotModified,
            StorageError::PreconditionFailed,
            StorageError::RangeNotSatisfiable { complete_length: 9 },
        ] {
            let expected = if matches!(error, StorageError::NotFound { .. }) {
                outcome::NOT_FOUND
            } else {
                outcome::PRECONDITION
            };
            let result: Result<(), StorageError> = Err(error);
            assert_eq!(storage_outcome(&result), expected);
            assert_eq!(
                storage_error_kind(result.as_ref().unwrap_err()),
                None,
                "a conditional answer is the backend working, not failing"
            );
        }
    }

    #[test]
    fn a_failing_backend_is_counted_as_an_error() {
        for (error, kind, expected) in [
            (
                StorageError::BackendUnavailable {
                    message: "https://seaweed.internal:8333 refused the connection".to_string(),
                },
                "backend_unavailable",
                outcome::UNAVAILABLE,
            ),
            (
                StorageError::BucketNotFound {
                    bucket: "notedthat-notes".to_string(),
                },
                "bucket_not_found",
                outcome::ERROR,
            ),
        ] {
            let result: Result<(), StorageError> = Err(error);
            assert_eq!(storage_outcome(&result), expected);
            assert_eq!(storage_error_kind(result.as_ref().unwrap_err()), Some(kind));
        }
    }

    /// `docs/CONFIGURATION.md` tells operators to alert on
    /// `outcome="unavailable"`. A documented alert that can never fire is worse
    /// than no alert, so the value has to be reachable from a real failure.
    #[test]
    fn an_unreachable_backend_is_its_own_outcome_not_the_catch_all() {
        let result: Result<(), StorageError> = Err(StorageError::BackendUnavailable {
            message: "connection refused".to_string(),
        });
        assert_eq!(storage_outcome(&result), outcome::UNAVAILABLE);
    }

    /// The error kinds are variant names, never the variants' contents — those
    /// carry a key, a bucket name and a backend message that can quote an
    /// endpoint.
    #[test]
    fn an_error_kind_never_quotes_the_error() {
        let error = StorageError::BackendUnavailable {
            message: "https://seaweed.internal:8333 refused".to_string(),
        };
        let kind = storage_error_kind(&error).expect("a backend failure has a kind");
        assert!(!kind.contains("seaweed"), "{kind} quotes the backend");
        assert!(!kind.contains(':'), "{kind} looks like it carries a URL");
    }

    #[test]
    fn ok_is_ok() {
        assert_eq!(storage_outcome(&Ok::<_, StorageError>(())), outcome::OK);
    }
}

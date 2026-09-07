//! The OKF index-maintenance worker.

use notedthat_core::{ConditionalHeaders, KbSlug, ObjectMeta, ObjectPath, Storage, StorageError};
use notedthat_indexer::{IndexEvent, OkfMaintenanceEvent};
use notedthat_okf::{INDEX_FILE, IndexEntry, LOG_FILE};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Retry budget for one read-modify-write cycle.
///
/// Matches `replace.rs`, which faces the same compare-and-swap race.
pub const MAX_ATTEMPTS: usize = 3;

/// How long the worker waits to gather events before applying a batch.
///
/// Coalescing is a requirement, not an optimisation: without it, importing 200
/// concepts into one directory is 200 serialised read-modify-writes of a file
/// that grows with each one — quadratic in bytes written.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(250);

/// Maximum events gathered into one batch.
pub const COALESCE_BATCH: usize = 64;

/// Which knowledge bases get their listings maintained, and how far.
#[derive(Debug, Clone, Default)]
pub struct OkfMaintenanceConfig {
    /// Knowledge bases opted in. Empty means every declared KB, but only when
    /// the feature is switched on at all.
    pub kbs: BTreeSet<String>,
    /// Whether to maintain `log.md` as well as `index.md`.
    ///
    /// Separate switch, because `log.md` grows without bound.
    pub maintain_log: bool,
}

impl OkfMaintenanceConfig {
    /// Whether this knowledge base is opted in.
    #[must_use]
    pub fn covers(&self, kb: &KbSlug) -> bool {
        self.kbs.is_empty() || self.kbs.contains(kb.as_str())
    }
}

/// Drains [`OkfMaintenanceEvent`]s and keeps directory listings in step.
pub struct OkfMaintenanceWorker {
    storage: Arc<dyn Storage>,
    indexer_tx: mpsc::Sender<IndexEvent>,
    rx: mpsc::Receiver<OkfMaintenanceEvent>,
    shutdown: CancellationToken,
    config: OkfMaintenanceConfig,
}

impl OkfMaintenanceWorker {
    /// Build a worker around shared dependencies and an event receiver.
    pub fn new(
        storage: Arc<dyn Storage>,
        indexer_tx: mpsc::Sender<IndexEvent>,
        rx: mpsc::Receiver<OkfMaintenanceEvent>,
        shutdown: CancellationToken,
        config: OkfMaintenanceConfig,
    ) -> Self {
        Self {
            storage,
            indexer_tx,
            rx,
            shutdown,
            config,
        }
    }

    /// Drain the queue until shutdown, applying coalesced batches.
    pub async fn run(mut self) {
        loop {
            let next = tokio::select! {
                event = self.rx.recv() => event,
                () = self.shutdown.cancelled() => None,
            };
            let Some(first) = next else {
                break;
            };

            let mut batch = vec![first];
            let deadline = tokio::time::Instant::now() + COALESCE_WINDOW;
            while batch.len() < COALESCE_BATCH {
                match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                    Ok(Some(event)) => batch.push(event),
                    Ok(None) | Err(_) => break,
                }
            }

            self.apply_batch(batch).await;
        }
    }

    /// Apply one coalesced batch, one read-modify-write per directory.
    async fn apply_batch(&self, batch: Vec<OkfMaintenanceEvent>) {
        let mut by_directory: BTreeMap<(String, String), Vec<OkfMaintenanceEvent>> =
            BTreeMap::new();

        for event in batch {
            if !self.config.covers(event.kb()) {
                continue;
            }
            let key = event.object_key().as_str();
            // Guard three: never maintain a listing in response to a listing.
            if notedthat_okf::is_reserved(key) {
                continue;
            }
            let dir = notedthat_okf::dir_of(key).to_string();
            by_directory
                .entry((event.kb().as_str().to_string(), dir))
                .or_default()
                .push(event);
        }

        for ((kb_slug, dir), events) in by_directory {
            let Ok(kb) = KbSlug::try_new(kb_slug) else {
                continue;
            };
            if let Err(err) = self.maintain_index(&kb, &dir, &events).await {
                tracing::warn!(
                    target: "notedthat::okf",
                    kb = %kb.as_str(),
                    dir = %dir,
                    error = %err,
                    "OKF_MAINTENANCE_FAILED"
                );
            }
            if self.config.maintain_log
                && let Err(err) = self.maintain_log(&kb, &dir, &events).await
            {
                tracing::warn!(
                    target: "notedthat::okf",
                    kb = %kb.as_str(),
                    dir = %dir,
                    error = %err,
                    "OKF_MAINTENANCE_FAILED"
                );
            }
        }
    }

    async fn maintain_index(
        &self,
        kb: &KbSlug,
        dir: &str,
        events: &[OkfMaintenanceEvent],
    ) -> Result<(), StorageError> {
        self.rewrite(kb, dir, INDEX_FILE, |current| {
            let mut next = current.to_string();
            for event in events {
                next = match event {
                    OkfMaintenanceEvent::Upserted {
                        object_key,
                        concept_type,
                        title,
                        description,
                        ..
                    } => {
                        let name = file_name(object_key.as_str());
                        notedthat_okf::upsert_entry(
                            &next,
                            concept_type,
                            &IndexEntry {
                                title: title.clone(),
                                url: format!("./{name}"),
                                description: description.clone(),
                                is_directory: false,
                                line: 0,
                            },
                        )
                    }
                    OkfMaintenanceEvent::Deleted { object_key, .. } => {
                        let name = file_name(object_key.as_str());
                        notedthat_okf::remove_entry(&next, &format!("./{name}"))
                    }
                };
            }
            next
        })
        .await
    }

    async fn maintain_log(
        &self,
        kb: &KbSlug,
        dir: &str,
        events: &[OkfMaintenanceEvent],
    ) -> Result<(), StorageError> {
        let today = today_utc();
        self.rewrite(kb, dir, LOG_FILE, |current| {
            let mut next = current.to_string();
            for event in events {
                next = notedthat_okf::append_log_entry(&next, &today, &log_bullet(event));
            }
            next
        })
        .await
    }

    /// Read-modify-write one reserved file under compare-and-swap.
    ///
    /// `If-Match` on the write, `If-None-Match: *` on creation, and a bounded
    /// retry — so two writers racing on the same directory cannot silently
    /// clobber each other. On exhausting the budget the attempt is abandoned and
    /// logged; the user's own write already succeeded and is never affected.
    async fn rewrite(
        &self,
        kb: &KbSlug,
        dir: &str,
        file: &str,
        edit: impl Fn(&str) -> String,
    ) -> Result<(), StorageError> {
        let key = format!("{dir}{file}");
        let Ok(path) = ObjectPath::try_from_str(&key) else {
            return Ok(());
        };

        for attempt in 1..=MAX_ATTEMPTS {
            let existing = self.read(kb, &path).await?;
            let current = existing.as_ref().map_or("", |(text, _)| text.as_str());
            let next = edit(current);
            if next == current {
                return Ok(());
            }

            let conditionals = match &existing {
                Some((_, etag)) => ConditionalHeaders {
                    if_match: Some(etag.clone()),
                    ..ConditionalHeaders::default()
                },
                None => ConditionalHeaders {
                    if_none_match: Some("*".to_string()),
                    ..ConditionalHeaders::default()
                },
            };

            // Written through `put_object`, never `commit`: that is what makes it
            // structurally impossible for this write to enqueue another
            // maintenance event.
            match self
                .storage
                .put_object(
                    kb,
                    &path,
                    bytes::Bytes::from(next),
                    Some("text/markdown"),
                    conditionals,
                )
                .await
            {
                Ok(outcome) => {
                    // Search still needs to see the updated listing.
                    let _ = self.indexer_tx.try_send(IndexEvent::Upsert {
                        kb: kb.clone(),
                        object_key: path.clone(),
                        etag: outcome.etag.clone().unwrap_or_default(),
                        mtime: 0,
                    });
                    return Ok(());
                }
                Err(StorageError::PreconditionFailed) if attempt < MAX_ATTEMPTS => {}
                Err(StorageError::PreconditionFailed) => {
                    tracing::warn!(
                        target: "notedthat::okf",
                        kb = %kb.as_str(),
                        path = %path.as_str(),
                        attempts = MAX_ATTEMPTS,
                        "OKF_MAINTENANCE_CONFLICT; giving up without failing the user's write"
                    );
                    return Ok(());
                }
                Err(err) => return Err(err),
            }
        }

        Ok(())
    }

    async fn read(
        &self,
        kb: &KbSlug,
        path: &ObjectPath,
    ) -> Result<Option<(String, String)>, StorageError> {
        match self
            .storage
            .get_object(kb, path, None, ConditionalHeaders::default())
            .await
        {
            Ok(read) => {
                let ObjectMeta { etag, .. } = read.meta;
                Ok(String::from_utf8(read.bytes.to_vec())
                    .ok()
                    .map(|text| (text, etag.unwrap_or_default())))
            }
            Err(StorageError::NotFound { .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }
}

/// The `log.md` bullet for one event, in the shape OKF's example uses.
#[must_use]
pub fn log_bullet(event: &OkfMaintenanceEvent) -> String {
    match event {
        OkfMaintenanceEvent::Upserted {
            object_key, title, ..
        } => {
            let name = file_name(object_key.as_str());
            format!("**Update**: Updated [{title}](./{name}).")
        }
        OkfMaintenanceEvent::Deleted { object_key, .. } => {
            let name = file_name(object_key.as_str());
            format!("**Update**: Removed [{name}](./{name}).")
        }
    }
}

fn file_name(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/// Today's date in UTC as `YYYY-MM-DD`.
fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let days = secs.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Howard Hinnant's `civil_from_days`, so a date needs no extra dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> KbSlug {
        KbSlug::try_new("notes").unwrap()
    }

    fn upserted(key: &str) -> OkfMaintenanceEvent {
        OkfMaintenanceEvent::Upserted {
            kb: kb(),
            object_key: ObjectPath::try_from_str(key).unwrap(),
            concept_type: "BigQuery Table".into(),
            title: "Customers".into(),
            description: Some("Customer master".into()),
        }
    }

    #[test]
    fn an_empty_kb_set_covers_every_declared_kb() {
        let config = OkfMaintenanceConfig::default();
        assert!(config.covers(&kb()));
    }

    #[test]
    fn a_populated_kb_set_is_an_allowlist() {
        let config = OkfMaintenanceConfig {
            kbs: ["other".to_string()].into_iter().collect(),
            maintain_log: false,
        };
        assert!(!config.covers(&kb()));
    }

    #[test]
    fn file_name_takes_the_last_segment() {
        assert_eq!(file_name("a/b/c.md"), "c.md");
        assert_eq!(file_name("c.md"), "c.md");
    }

    #[test]
    fn log_bullets_match_the_spec_shape() {
        assert_eq!(
            log_bullet(&upserted("tables/customers.md")),
            "**Update**: Updated [Customers](./customers.md)."
        );
        assert_eq!(
            log_bullet(&OkfMaintenanceEvent::Deleted {
                kb: kb(),
                object_key: ObjectPath::try_from_str("tables/orders.md").unwrap(),
            }),
            "**Update**: Removed [orders.md](./orders.md)."
        );
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // A leap day, to catch an off-by-one in the era arithmetic.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn today_has_the_iso_shape_the_log_parser_expects() {
        assert!(notedthat_okf::is_iso_date(&today_utc()), "{}", today_utc());
    }
}

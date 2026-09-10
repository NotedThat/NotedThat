//! Turning what the filesystem watcher notices into indexing work.
//!
//! The watcher reports that something under a knowledge base's directory needs looking at
//! again; the indexer takes one object at a time. This is the join between them, and the
//! only place in the server where the two vocabularies meet.
//!
//! A whole knowledge base or a subtree is resolved by [`reconcile`], which needs to know
//! what the index currently holds — so this is also where the vector store is consulted on
//! behalf of a storage adapter that deliberately knows nothing about it.

use std::sync::Arc;

use notedthat_core::KbSlug;
use notedthat_indexer::{IndexEvent, VectorStore};
use notedthat_storage_fs::{
    FsChange, FsConfig, FsSignal, FsStorage, FsWatchConfig, FsWatcher, IndexedEtag, reconcile,
};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// How many changes one reconciliation pass holds while they are forwarded.
const RECONCILE_BUFFER: usize = 256;

/// A running watch and the task draining it.
pub(super) struct FsWatch {
    watcher: FsWatcher,
    bridge: tokio::task::JoinHandle<()>,
}

impl FsWatch {
    /// Stop watching and wait until nothing more can be enqueued.
    ///
    /// Ordering matters at shutdown: this holds a sender on the indexing queue, so draining
    /// that queue while this is still running would be chasing a live producer, and the
    /// queue would never be seen to close.
    pub(super) async fn stop(self) {
        self.watcher.stop().await;
        // The watcher owned the only sender, so the bridge's loop ends on its own.
        let _ = self.bridge.await;
    }
}

/// Start watching every declared knowledge base, and reconcile each one once.
///
/// Returns `Ok(None)` when watching is switched off.
///
/// # Errors
///
/// Propagates the watcher's own failure, which is fatal by design: a watch that cannot be
/// established would leave the search index quietly drifting from a store people are being
/// invited to edit. The diagnostic names the way out.
pub(super) fn start(
    config: &FsConfig,
    tenant: notedthat_core::TenantSlug,
    kbs: Vec<KbSlug>,
    store: Arc<dyn VectorStore>,
    indexer_tx: mpsc::Sender<IndexEvent>,
) -> anyhow::Result<Option<FsWatch>> {
    if !config.watch {
        info!(
            "filesystem watching is off; only writes through NotedThat will update the \
             search index"
        );
        return Ok(None);
    }

    // A second adapter over the same root, rather than the one serving requests.
    //
    // Safe because nothing on this path writes object content. The only write it can cause
    // is a metadata repair, and one of those is derived from the bytes it just hashed and
    // stamped against that same file — so if the file changes underneath it, the stamp no
    // longer matches and the record is repaired again on the next read. It corrects itself
    // rather than depending on a lock shared with the writer.
    let storage = FsStorage::new(config, config.root.clone(), tenant);

    let (signals_tx, signals_rx) = mpsc::channel::<FsSignal>(1024);
    let watcher =
        notedthat_storage_fs::watch_kbs(&storage, &kbs, FsWatchConfig::from(config), signals_tx)?;

    info!(
        knowledgebases = kbs.len(),
        debounce_ms = u64::try_from(config.watch_debounce.as_millis()).unwrap_or(u64::MAX),
        "watching the storage tree for changes made outside NotedThat"
    );

    let bridge = tokio::spawn(run_bridge(storage, store, indexer_tx, signals_rx, kbs));

    Ok(Some(FsWatch { watcher, bridge }))
}

/// Drain signals until the watcher stops, reconciling everything once first.
async fn run_bridge(
    storage: FsStorage,
    store: Arc<dyn VectorStore>,
    indexer_tx: mpsc::Sender<IndexEvent>,
    mut signals: mpsc::Receiver<FsSignal>,
    kbs: Vec<KbSlug>,
) {
    // Changes made while the server was not running are invisible to a watcher, so every
    // knowledge base is compared against the index once at startup. On a store that has
    // not moved this reads no file content and embeds nothing.
    for kb in &kbs {
        reconcile_into(&storage, store.as_ref(), &indexer_tx, kb, None, "startup").await;
    }

    while let Some(signal) = signals.recv().await {
        match signal {
            FsSignal::Changed { kb, key } => {
                if indexer_tx
                    .send(IndexEvent::Refresh {
                        kb,
                        object_key: key,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            FsSignal::Prefix { kb, prefix } => {
                reconcile_into(
                    &storage,
                    store.as_ref(),
                    &indexer_tx,
                    &kb,
                    Some(prefix.as_str()),
                    "subtree changed",
                )
                .await;
            }
            FsSignal::Kb { kb } => {
                reconcile_into(&storage, store.as_ref(), &indexer_tx, &kb, None, "rescan").await;
            }
        }
    }
}

/// Compare one knowledge base, or one subtree of it, and enqueue what differs.
async fn reconcile_into(
    storage: &FsStorage,
    store: &dyn VectorStore,
    indexer_tx: &mpsc::Sender<IndexEvent>,
    kb: &KbSlug,
    prefix: Option<&str>,
    cause: &str,
) {
    let indexed = match store.indexed_objects(kb, prefix).await {
        Ok(indexed) => indexed,
        Err(error) => {
            // Without knowing what is indexed there is nothing to compare against, and
            // guessing would mean either re-embedding everything or silently doing nothing.
            error!(
                target: "notedthat::watch",
                kb = %kb.as_str(),
                prefix = prefix.unwrap_or(""),
                %error,
                "FS_WATCH_RESCAN: could not read the index, so this pass was skipped"
            );
            return;
        }
    };
    let indexed: Vec<IndexedEtag> = indexed
        .into_iter()
        .map(|object| IndexedEtag {
            key: object.object_key,
            etag: object.etag,
        })
        .collect();

    let (changes_tx, mut changes_rx) = mpsc::channel::<FsChange>(RECONCILE_BUFFER);
    let forwarder = {
        let indexer_tx = indexer_tx.clone();
        tokio::spawn(async move {
            while let Some(FsChange { kb, key }) = changes_rx.recv().await {
                if indexer_tx
                    .send(IndexEvent::Refresh {
                        kb,
                        object_key: key,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        })
    };

    let report = reconcile(storage, kb, prefix, &indexed, &changes_tx).await;
    drop(changes_tx);
    let _ = forwarder.await;

    match report {
        Ok(report) if report.is_clean() => info!(
            target: "notedthat::watch",
            kb = %kb.as_str(),
            prefix = prefix.unwrap_or(""),
            cause,
            objects = report.objects_on_disk,
            "already in step with the index"
        ),
        Ok(report) => info!(
            target: "notedthat::watch",
            kb = %kb.as_str(),
            prefix = prefix.unwrap_or(""),
            cause,
            objects = report.objects_on_disk,
            changed = report.changed,
            orphaned = report.orphaned,
            "enqueued objects whose index entries are out of date"
        ),
        Err(error) => warn!(
            target: "notedthat::watch",
            kb = %kb.as_str(),
            prefix = prefix.unwrap_or(""),
            cause,
            %error,
            "FS_WATCH_RESCAN: could not read the tree, so this pass was incomplete"
        ),
    }
}

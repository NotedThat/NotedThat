//! Resource subscriptions for one MCP session (D66).
//!
//! rmcp creates one handler per session, so this is per-session state: which
//! keys the session subscribed to, per knowledge base, and whether it wants
//! `list_changed`. Each knowledge base with any interest has one **forwarder**
//! task consuming `GET /api/v1/knowledgebases/{kb}/events` over the loopback
//! API as the subscriber — bearer or nothing — so the D51 filter, the
//! concealment and the `anyone` rules are the API's, and the MCP layer holds
//! no evaluator (D59). The forwarder turns `object.written` and
//! `object.deleted` for a subscribed key into `notifications/resources/updated`
//! and any change on a watched knowledge base into a coalesced
//! `notifications/resources/list_changed`. `object.indexed` and
//! `object.index_failed` (D65) are ignored: the resource's text did not change.
//!
//! A forwarder runs as the credential that opened it, and an OIDC client
//! rotates that credential underneath its session. When the events route
//! refuses an expired bearer (`401`), the keys it fed are **stranded**, not
//! dropped: the client is sent `notifications/resources/updated` for each, and
//! they move onto a watch for the newest credential the session presents —
//! at once if it already presented one, otherwise on its next request — after
//! each key is probed again as that credential. A `403` or `404` means the
//! credential lost access rather than expired, so its keys are dropped, with
//! the same notice.
//!
//! Everything here dies with the session: dropping the [`Subscriptions`] —
//! which rmcp does when the session ends — cancels every task. Nothing is
//! replayed and nothing persists.
//!
//! Two more tasks per subscribed session: a **keeper** that pings the client
//! once a minute, because rmcp's idle timeout counts messages, not an open
//! `GET` leg, and a session that only listens would otherwise be closed after
//! five minutes; and the **coalescer** behind `list_changed`. The keeper runs
//! only while something is subscribed — it starts on the first
//! `resources/subscribe` and stops once the last one goes, so it never holds a
//! session open for a client with nothing to receive.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rmcp::ErrorData as McpError;
use rmcp::model::{PingRequest, ResourceUpdatedNotificationParam, ServerRequest};
use rmcp::service::{Peer, PeerRequestOptions, RoleServer};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::client::NotedThatClient;
use crate::error::McpToolError;
use crate::sse::SseParser;

/// How often the keeper pings a session with live subscriptions, and how long
/// it waits for the answer. Comfortably inside rmcp's five-minute idle
/// timeout; a client that cannot answer in 30 s has no working notification
/// leg either.
const KEEPER_INTERVAL: Duration = Duration::from_secs(60);
const KEEPER_TIMEOUT: Duration = Duration::from_secs(30);
/// The `list_changed` window: one notification at once, at most one more
/// after this long, however many changes arrived in between.
const LIST_CHANGED_WINDOW: Duration = Duration::from_secs(1);
/// Reconnect backoff for a forwarder whose stream ended or failed.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How many pages of a prefix listing the subscribe-time probe reads before
/// giving up on finding the exact key.
const PROBE_PAGE_CAP: usize = 10;

/// How long `subscribe` waits for the knowledge base's stream to be open
/// before answering, so that a change made right after the answer is seen.
const OPEN_WAIT: Duration = Duration::from_secs(5);

/// The event kinds that mean the resource's bytes changed.
const CHANGE_EVENTS: [&str; 2] = ["object.written", "object.deleted"];

/// Where a forwarder's connection to the events route stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Connecting,
    Open,
    /// The route refused the caller with this status; the watch is dead.
    Refused(u16),
}

/// What one session subscribed to. Held in an `Arc` by the handler; the
/// forwarders hold only a `Weak`, so the session's end is the last strong
/// reference going away.
pub(crate) struct Subscriptions {
    /// A child of the service's shutdown token; cancelled in `Drop`.
    cancel: CancellationToken,
    /// Set when the keeper gave up on the session: its notification leg is not
    /// working, so nothing subscribed to it can be delivered. `subscribe` says
    /// so rather than answering `Ok` and going quiet.
    gone: AtomicBool,
    inner: Mutex<Inner>,
    list_changed: Arc<Coalescer>,
}

/// What a forwarder is keyed on: the knowledge base, and the credential it
/// consumes the events route as.
///
/// The credential is part of the key because a forwarder runs as whoever
/// opened it, for its whole life, while `probe_listable` runs as whoever is
/// subscribing *now*. Keying on the knowledge base alone made those two
/// different callers whenever they differed — and the ordinary way they differ
/// is not an attacker but OIDC token refresh: a client keeps one MCP session
/// across its whole connection, which is the point of the stateful transport,
/// and rotates its access token underneath it every few minutes. The second
/// `subscribe` would then be probed as the new token, served by the old
/// token's stream, and lose every subscription on that knowledge base when the
/// old token expired and the route answered `401`. The keys the old token
/// subscribed still need a stream once it expires; they are stranded and
/// re-homed onto the newest credential (see [`Subscriptions::note_caller`]). The anonymous case is worse
/// and quieter: one request that arrives without an `Authorization` header
/// opens the stream as the anonymous caller, and every authenticated
/// subscription afterwards is fed by a stream D51 filters as anonymous — so
/// the private key's events never arrive and `subscribe` still answered `Ok`.
///
/// The bearer is held verbatim, as the client already holds it, and is never
/// logged or rendered: it is a map key inside one session's state.
type WatchKey = (String, Option<String>);

#[derive(Default)]
struct Inner {
    kbs: HashMap<WatchKey, KbWatch>,
    /// What a watch the route refused with `401` was feeding, waiting for a
    /// newer credential to be re-homed onto.
    stranded: HashMap<WatchKey, Stranded>,
    /// The credential this session presented last; where stranded keys go.
    /// Held verbatim like [`WatchKey`]'s and never logged.
    latest: Option<NotedThatClient>,
    /// The session's peer, so a re-home started from any request can open a
    /// forwarder. Set by the first watch.
    peer: Option<Peer<RoleServer>>,
    /// A re-home task is running; the next request need not start another.
    rehoming: bool,
    keeper_started: bool,
    coalescer_started: bool,
}

/// A refused watch's interest, kept until it is re-homed or given up on.
#[derive(Default)]
struct Stranded {
    /// Object key to the URI it was subscribed under, as in [`KbWatch`].
    keys: HashMap<String, String>,
    list_changed: bool,
}

impl Stranded {
    fn wanted(&self) -> bool {
        self.list_changed || !self.keys.is_empty()
    }
}

/// One knowledge base's forwarder, shared between the handler and the task.
#[derive(Clone)]
struct KbWatch {
    /// Subscribed object keys, each with the URI the client subscribed under,
    /// which is the URI the notification must carry.
    keys: Arc<Mutex<HashMap<String, String>>>,
    /// Whether this knowledge base feeds `list_changed`.
    list_changed: Arc<AtomicBool>,
    /// Cleared by the forwarder when the route refused the caller; a later
    /// subscribe replaces the watch instead of adding to a dead one.
    alive: Arc<AtomicBool>,
    /// Set by the forwarder once its stream is open, so a subscribe can wait
    /// for it and a change made right after the answer is not missed.
    link: tokio::sync::watch::Sender<Link>,
    cancel: CancellationToken,
}

impl KbWatch {
    fn wanted(&self) -> bool {
        self.list_changed.load(Ordering::SeqCst)
            || !self.keys.lock().expect("subscription keys").is_empty()
    }
}

impl Drop for Subscriptions {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Subscriptions {
    /// Per-session state whose tasks stop with `shutdown`.
    pub(crate) fn new(shutdown: &CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            cancel: shutdown.child_token(),
            gone: AtomicBool::new(false),
            inner: Mutex::new(Inner::default()),
            list_changed: Arc::new(Coalescer::default()),
        })
    }

    /// Register `key` in `kb` under `uri`; the caller has already been
    /// probed. Starts the knowledge base's forwarder if it is not running,
    /// and answers once its stream is open — or with the route's refusal.
    pub(crate) async fn subscribe(
        self: &Arc<Self>,
        kb: &str,
        key: &str,
        uri: &str,
        client: &NotedThatClient,
        peer: &Peer<RoleServer>,
    ) -> Result<(), McpError> {
        self.still_deliverable()?;
        let link = {
            let mut inner = self.inner.lock().expect("subscriptions");
            let watch = self.ensure_watch(&mut inner, kb, client, peer);
            watch
                .keys
                .lock()
                .expect("subscription keys")
                .insert(key.to_owned(), uri.to_owned());
            // The keeper starts here and nowhere else. A subscription is the
            // only thing a session has that is worth holding the session open
            // for; starting it from `resources/list` would keep every
            // connected client that listed once alive for as long as it
            // answered pings, and `MAX_SESSIONS` would never be reclaimed.
            self.ensure_keeper(&mut inner, peer);
            self.ensure_coalescer(&mut inner, peer);
            watch.link.subscribe()
        };
        let state = wait_open(link).await;
        // Re-checked after the wait: the keeper can give up while it runs, and
        // a `Connecting` link on a session whose token is cancelled will never
        // open.
        self.still_deliverable()?;
        match state {
            Link::Refused(403) => Err(McpToolError::Forbidden.into()),
            Link::Refused(401) => Err(McpToolError::Unauthorized.into()),
            Link::Refused(_) => Err(McpToolError::NotFound("object".to_owned()).into()),
            Link::Open | Link::Connecting => Ok(()),
        }
    }

    /// Refuse a subscription this session could never deliver.
    ///
    /// `Ok` from `subscribe` is a promise that notifications will arrive. Once
    /// the keeper has given up — the client stopped answering `ping`, or
    /// answered it with an error, while its session stayed otherwise usable —
    /// every forwarder is cancelled and nothing restarts them, so the promise
    /// is one this session cannot keep.
    fn still_deliverable(&self) -> Result<(), McpError> {
        if self.gone.load(Ordering::SeqCst) || self.cancel.is_cancelled() {
            return Err(McpToolError::BackendUnavailable.into());
        }
        Ok(())
    }

    /// Forget `key` in `kb`; idempotent. A forwarder nobody wants any more is
    /// stopped.
    pub(crate) fn unsubscribe(&self, kb: &str, key: &str) {
        let mut inner = self.inner.lock().expect("subscriptions");
        // Every watch on this knowledge base, whatever credential opened it:
        // `resources/unsubscribe` carries a URI and nothing else, so the client
        // cannot name which of its credentials registered the key, and after a
        // token refresh that is not the one it is presenting now.
        let mut spent = Vec::new();
        for (watch_key, watch) in &inner.kbs {
            if watch_key.0 != kb {
                continue;
            }
            watch.keys.lock().expect("subscription keys").remove(key);
            if !watch.wanted() {
                watch.cancel.cancel();
                spent.push(watch_key.clone());
            }
        }
        for watch_key in spent {
            inner.kbs.remove(&watch_key);
        }
        // And every stranded copy, or a re-home would bring it back.
        for (watch_key, stranded) in &mut inner.stranded {
            if watch_key.0 == kb {
                stranded.keys.remove(key);
            }
        }
        inner.stranded.retain(|_, stranded| stranded.wanted());
    }

    /// Record the credential the session just presented, and re-home anything
    /// stranded under another one onto it.
    ///
    /// Called for every request that reaches the handler, so a client whose
    /// old token was refused while it sat idle gets its subscriptions back on
    /// its next call — which the `resources/updated` sent at the refusal
    /// invites, since a client re-reads a resource it is told changed.
    pub(crate) fn note_caller(self: &Arc<Self>, client: &NotedThatClient) {
        let mut inner = self.inner.lock().expect("subscriptions");
        if inner
            .latest
            .as_ref()
            .is_none_or(|latest| latest.credential() != client.credential())
        {
            inner.latest = Some(client.clone());
        }
        self.rehome_if_due(&mut inner);
    }

    /// Start a re-home onto the latest credential if anything is stranded
    /// under a different one and no re-home is already running.
    fn rehome_if_due(self: &Arc<Self>, inner: &mut Inner) {
        if inner.rehoming {
            return;
        }
        let Some(latest) = inner.latest.clone() else {
            return;
        };
        let credential = latest.credential();
        if inner
            .stranded
            .keys()
            .any(|watch_key| watch_key.1 != credential)
        {
            inner.rehoming = true;
            tokio::spawn(rehome(Arc::downgrade(self), latest));
        }
    }

    /// A forwarder was refused with `status`: take what its watch was feeding,
    /// and keep it for a re-home when the refusal is `401` — the bearer
    /// expired, which is what a token refresh leaves behind. Returns the URIs
    /// the client must be told about and whether the watch fed `list_changed`.
    ///
    /// Must run before [`Self::drop_watch`], which clears the watch.
    fn strand(&self, watch_key: &WatchKey, watch: &KbWatch, status: u16) -> (Vec<String>, bool) {
        let keys = watch.keys.lock().expect("subscription keys").clone();
        let list_changed = watch.list_changed.load(Ordering::SeqCst);
        let uris = keys.values().cloned().collect();
        if status == 401 && (list_changed || !keys.is_empty()) {
            let mut inner = self.inner.lock().expect("subscriptions");
            let stranded = inner.stranded.entry(watch_key.clone()).or_default();
            stranded.keys.extend(keys);
            stranded.list_changed |= list_changed;
        }
        (uris, list_changed)
    }

    /// Watch every knowledge base in `kbs` for list changes, as the caller
    /// whose listing named them. Waits for the streams to open, briefly, so
    /// a write right after the listing counts; a knowledge base whose stream
    /// the route refuses is simply not watched.
    ///
    /// Deliberately does **not** start the keeper. `resources/list` is what
    /// most clients call on connect, so a session that listed once would
    /// otherwise be pinged forever and never idle out — rmcp's idle timer
    /// counts messages — and `MAX_SESSIONS` would fill with sessions nothing
    /// reclaims. A `list_changed` watch has no pending notification a client
    /// is blocked on; a subscription does, and that is where the keeper
    /// starts.
    pub(crate) async fn watch_list_changes(
        self: &Arc<Self>,
        kbs: &[String],
        client: &NotedThatClient,
        peer: &Peer<RoleServer>,
    ) {
        let links: Vec<_> = {
            let mut inner = self.inner.lock().expect("subscriptions");
            let links = kbs
                .iter()
                .map(|kb| {
                    let watch = self.ensure_watch(&mut inner, kb, client, peer);
                    watch.list_changed.store(true, Ordering::SeqCst);
                    watch.link.subscribe()
                })
                .collect();
            if !kbs.is_empty() {
                self.ensure_coalescer(&mut inner, peer);
            }
            links
        };
        // One budget for all of them, not one each. Awaiting in turn made the
        // first `resources/list` of a session cost up to `OPEN_WAIT × kbs` —
        // 50 s for ten knowledge bases with a slow or unreachable events route,
        // inside the call most clients make at connect time. The waits share
        // nothing, and what this wait is for — a write in the next moment not
        // being missed — one shared budget covers.
        let _ = tokio::time::timeout(
            OPEN_WAIT,
            futures::future::join_all(links.into_iter().map(wait_open)),
        )
        .await;
    }

    /// The live watch for `kb` as this caller, started if missing or dead.
    fn ensure_watch(
        self: &Arc<Self>,
        inner: &mut Inner,
        kb: &str,
        client: &NotedThatClient,
        peer: &Peer<RoleServer>,
    ) -> KbWatch {
        if inner.peer.is_none() {
            inner.peer = Some(peer.clone());
        }
        let watch_key: WatchKey = (kb.to_owned(), client.credential());
        // `alive` alone is not enough: it is cleared only where a forwarder
        // exits through `drop_watch`, and a cancelled token is the other way a
        // watch stops having a task behind it. Reusing one would hand back a
        // watch whose `link` still reads `Open` from before, so `subscribe`
        // would answer `Ok` for a subscription nothing can ever feed.
        if let Some(watch) = inner.kbs.get(&watch_key)
            && watch.alive.load(Ordering::SeqCst)
            && !watch.cancel.is_cancelled()
        {
            return watch.clone();
        }
        let watch = KbWatch {
            keys: Arc::new(Mutex::new(HashMap::new())),
            list_changed: Arc::new(AtomicBool::new(false)),
            alive: Arc::new(AtomicBool::new(true)),
            link: tokio::sync::watch::Sender::new(Link::Connecting),
            cancel: self.cancel.child_token(),
        };
        inner.kbs.insert(watch_key.clone(), watch.clone());
        tokio::spawn(forward(Forwarder {
            session: Arc::downgrade(self),
            watch_key,
            client: client.clone(),
            peer: peer.clone(),
            watch: watch.clone(),
            coalescer: self.list_changed.clone(),
        }));
        watch
    }

    /// Start the keeper once, on the session's first subscription; it runs
    /// until the session's token is cancelled or its last subscription goes.
    ///
    /// Separate from [`Self::ensure_coalescer`] on purpose. The coalescer only
    /// forwards what a watch produced and costs nothing when nothing does; the
    /// keeper holds the *session* open against rmcp's idle timer, which is a
    /// thing to do only for a client with a notification it is waiting on.
    fn ensure_keeper(self: &Arc<Self>, inner: &mut Inner, peer: &Peer<RoleServer>) {
        if inner.keeper_started {
            return;
        }
        inner.keeper_started = true;
        tokio::spawn(keep_alive(
            Arc::downgrade(self),
            peer.clone(),
            self.cancel.clone(),
        ));
    }

    /// Whether any knowledge base still holds a subscribed key.
    ///
    /// What the keeper asks before each ping. `list_changed` watches do not
    /// count: they are the case the keeper deliberately does not hold a session
    /// open for.
    #[cfg(test)]
    fn has_subscriptions(&self) -> bool {
        any_subscribed(&self.inner.lock().expect("subscriptions"))
    }

    /// Stand the keeper down if nothing is subscribed any more, so a later
    /// `subscribe` starts a fresh one; `true` when it did.
    ///
    /// The test and the release are deliberately one lock acquisition. Split
    /// across two, a `subscribe` landing in between finds `keeper_started`
    /// still set, declines to spawn a keeper of its own, and is then left
    /// without one when this clears the flag. The subscription is live and the
    /// forwarder delivers, but nothing pings, so rmcp's idle timer closes the
    /// session a few minutes later and takes the subscription with it — and
    /// nothing restarts the keeper, because the client has no reason to
    /// subscribe again.
    fn stand_down_if_idle(&self) -> bool {
        let mut inner = self.inner.lock().expect("subscriptions");
        let idle = !any_subscribed(&inner);
        if idle {
            inner.keeper_started = false;
        }
        idle
    }

    /// Start the `list_changed` coalescer once, on the session's first list
    /// watch; it runs until the session's token is cancelled.
    fn ensure_coalescer(self: &Arc<Self>, inner: &mut Inner, peer: &Peer<RoleServer>) {
        if inner.coalescer_started {
            return;
        }
        inner.coalescer_started = true;
        tokio::spawn(
            self.list_changed
                .clone()
                .run(peer.clone(), self.cancel.clone()),
        );
    }

    /// A forwarder is stopping: mark its watch dead so no later `subscribe`
    /// joins it, and forget it if it is still the one registered.
    ///
    /// Called on **every** exit from [`forward`], not only the route-refusal
    /// one. A watch left `alive` with no task behind it is the worst shape this
    /// module has: `ensure_watch` hands it back, its `link` still reads `Open`,
    /// and `subscribe` answers `Ok` for something that will never fire.
    ///
    /// The registered watch is compared by identity before it is removed,
    /// because `unsubscribe` may already have dropped this one and a later
    /// `subscribe` put a fresh watch under the same key.
    fn drop_watch(&self, watch_key: &WatchKey, watch: &KbWatch) {
        watch.alive.store(false, Ordering::SeqCst);
        watch.list_changed.store(false, Ordering::SeqCst);
        watch.keys.lock().expect("subscription keys").clear();
        // Only this watch's token, never the one a replacement under the same
        // key holds. Redundant for the task that just returned; it is what
        // makes the deadness visible to `ensure_watch`'s own check.
        watch.cancel.cancel();
        let mut inner = self.inner.lock().expect("subscriptions");
        if inner
            .kbs
            .get(watch_key)
            .is_some_and(|registered| Arc::ptr_eq(&registered.keys, &watch.keys))
        {
            inner.kbs.remove(watch_key);
        }
    }
}

/// The forwarder's link once it left `Connecting`, or `Connecting` still if
/// that took longer than [`OPEN_WAIT`] — a subscription is then live from
/// whenever the stream does open, which is the best the API allows.
async fn wait_open(mut link: tokio::sync::watch::Receiver<Link>) -> Link {
    let _ =
        tokio::time::timeout(OPEN_WAIT, link.wait_for(|state| *state != Link::Connecting)).await;
    *link.borrow()
}

/// Whether the caller may `list` `key` in `kb` — the predicate the events
/// route applies per event, so a subscription that passes is one that can
/// fire. Probed as the caller with a prefix listing: a knowledge base the
/// caller may not list is `forbidden` (or the concealed `not_found` for an
/// anonymous caller — the API decides), and a key it cannot see, or that does
/// not exist, is `not_found`, matching `resources/read`.
pub(crate) async fn probe_listable(
    client: &NotedThatClient,
    kb: &str,
    key: &str,
) -> Result<(), McpError> {
    let mut cursor: Option<String> = None;
    for _ in 0..PROBE_PAGE_CAP {
        let page =
            crate::resources_list::list_objects(client, kb, Some(key), cursor.as_deref()).await?;
        if page.objects.iter().any(|object| object.key == key) {
            return Ok(());
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Err(McpToolError::NotFound("object".to_owned()).into())
}

/// Move what is stranded under any credential but `client`'s onto `client`'s
/// watches.
///
/// Every key is probed again as `client` before it moves: the stranded keys
/// were admitted for the old credential, and a refresh can change the groups
/// a subject is in, so carrying them over unchecked would feed a caller
/// events for keys it may no longer list. A key that fails the probe is
/// dropped; the client was already told to re-read it when it was stranded.
/// A `list_changed` interest moves without a probe — the events route
/// filters each event as the new credential, as it does for
/// [`Subscriptions::watch_list_changes`].
///
/// Holds the session only while it touches state, never across a probe, so a
/// session that ends meanwhile is not kept alive by its own re-home.
async fn rehome(session: Weak<Subscriptions>, client: NotedThatClient) {
    let credential = client.credential();
    let foreign = |watch_key: &WatchKey| watch_key.1 != credential;
    let pending: Vec<(String, String)> = {
        let Some(live) = session.upgrade() else {
            return;
        };
        let inner = live.inner.lock().expect("subscriptions");
        inner
            .stranded
            .iter()
            .filter(|(watch_key, _)| foreign(watch_key))
            .flat_map(|(watch_key, stranded)| {
                stranded
                    .keys
                    .keys()
                    .map(|key| (watch_key.0.clone(), key.clone()))
            })
            .collect()
    };
    let mut verdicts = Vec::with_capacity(pending.len());
    for (kb, key) in pending {
        let listable = probe_listable(&client, &kb, &key).await.is_ok();
        verdicts.push((kb, key, listable));
    }

    let Some(live) = session.upgrade() else {
        return;
    };
    let mut inner = live.inner.lock().expect("subscriptions");
    inner.rehoming = false;
    let Some(peer) = inner.peer.clone() else {
        return;
    };
    for (kb, key, listable) in verdicts {
        // Taken only if it is still stranded: an `unsubscribe` while the probe
        // ran has removed it, and must win.
        let mut uri = None;
        for (watch_key, stranded) in &mut inner.stranded {
            if watch_key.0 == kb && foreign(watch_key) {
                uri = stranded.keys.remove(&key).or(uri);
            }
        }
        match uri {
            Some(uri) if listable => {
                let watch = live.ensure_watch(&mut inner, &kb, &client, &peer);
                watch
                    .keys
                    .lock()
                    .expect("subscription keys")
                    .insert(key, uri);
            }
            Some(_) => tracing::debug!(
                target: "notedthat::mcp",
                kb = %kb,
                "a stranded subscription is not listable as the newer credential; dropped"
            ),
            None => {}
        }
    }
    let lists: Vec<String> = inner
        .stranded
        .iter_mut()
        .filter(|(watch_key, stranded)| foreign(watch_key) && stranded.list_changed)
        .map(|(watch_key, stranded)| {
            stranded.list_changed = false;
            watch_key.0.clone()
        })
        .collect();
    for kb in lists {
        let watch = live.ensure_watch(&mut inner, &kb, &client, &peer);
        watch.list_changed.store(true, Ordering::SeqCst);
    }
    inner.stranded.retain(|_, stranded| stranded.wanted());
    // Something may have been stranded, or a newer credential presented,
    // while the probes ran.
    live.rehome_if_due(&mut inner);
}

/// Everything a forwarder needs, owned by its task.
struct Forwarder {
    session: Weak<Subscriptions>,
    watch_key: WatchKey,
    client: NotedThatClient,
    peer: Peer<RoleServer>,
    watch: KbWatch,
    coalescer: Arc<Coalescer>,
}

/// What a forwarder does with one event frame.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// `notifications/resources/updated` for this URI.
    Updated(String),
    /// Feed the session's `list_changed` coalescer.
    ListChanged,
}

/// The decision for one frame, separated from the I/O so it can be pinned by
/// a test: a change event for a subscribed key updates; any change event on
/// a knowledge base watched for list changes touches the coalescer; every
/// other frame — heartbeats, `retry:`, the indexer's verdicts — does nothing.
fn decide(event: Option<&str>, data: &str, watch: &KbWatch) -> Vec<Action> {
    let Some(event) = event else {
        return Vec::new();
    };
    if !CHANGE_EVENTS.contains(&event) {
        return Vec::new();
    }
    let Some(key) = serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|value| value["object_key"].as_str().map(str::to_owned))
    else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    if let Some(uri) = watch.keys.lock().expect("subscription keys").get(&key) {
        actions.push(Action::Updated(uri.clone()));
    }
    if watch.list_changed.load(Ordering::SeqCst) {
        actions.push(Action::ListChanged);
    }
    actions
}

/// Consume the knowledge base's event stream as the caller until the session
/// ends, the client goes away, or the route refuses the caller.
///
/// However it ends, the watch it was feeding is marked dead on the way out —
/// see [`Subscriptions::drop_watch`] for why that matters more than where it
/// ends.
async fn forward(fw: Forwarder) {
    let watch_key = fw.watch_key.clone();
    let watch = fw.watch.clone();
    let session = fw.session.clone();
    let peer = fw.peer.clone();
    let coalescer = fw.coalescer.clone();
    run_forwarder(fw).await;
    let Some(live) = session.upgrade() else {
        watch.alive.store(false, Ordering::SeqCst);
        return;
    };
    let refused = match *watch.link.borrow() {
        Link::Refused(status) => Some(status),
        Link::Connecting | Link::Open => None,
    };
    let Some(status) = refused else {
        live.drop_watch(&watch_key, &watch);
        return;
    };
    let (uris, list_changed) = live.strand(&watch_key, &watch, status);
    live.drop_watch(&watch_key, &watch);
    if status == 401 {
        live.rehome_if_due(&mut live.inner.lock().expect("subscriptions"));
    }
    // Not held across the notifications below; see `keep_alive`.
    drop(live);
    // Whatever happens to these keys next, events for them may have gone
    // unseen since this stream last had them, and a dropped key would
    // otherwise go quiet with no sign. The client re-reads what it is told
    // changed — which also presents its current credential, and so re-homes
    // what was stranded.
    for uri in uris {
        if peer
            .notify_resource_updated(ResourceUpdatedNotificationParam::new(uri))
            .await
            .is_err()
        {
            return;
        }
    }
    if list_changed {
        coalescer.touch();
    }
}

/// [`forward`]'s body: every `return` here is an exit the caller cleans up
/// after.
async fn run_forwarder(fw: Forwarder) {
    let kb = &fw.watch_key.0;
    let mut last_event_id: Option<String> = None;
    let mut backoff = BACKOFF_MIN;
    loop {
        let request = fw.client.events_stream(kb, last_event_id.as_deref());
        let response = tokio::select! {
            () = fw.watch.cancel.cancelled() => return,
            sent = request.send() => sent,
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(target: "notedthat::mcp", kb = %kb, %error, "events stream request failed; retrying");
                if !pause(&fw.watch.cancel, &mut backoff).await {
                    return;
                }
                continue;
            }
        };
        match response.status().as_u16() {
            200 => {
                fw.watch.link.send_replace(Link::Open);
            }
            // The position is gone; a subscription has no replay to owe, so
            // the id is dropped and the next attempt asks for the stream from
            // now. Guarded on there having *been* an id to drop: the events
            // route answers `410` only to a `Last-Event-ID`, so an
            // unconditional `410` — a proxy in front of the loopback call, a
            // later change to the route — would otherwise reconnect in a tight
            // loop for the life of the session, spinning a core and hammering
            // the API from inside the same process. Without an id it falls
            // through to the backoff arm below.
            410 if last_event_id.take().is_some() => continue,
            status @ (401 | 403 | 404) => {
                tracing::warn!(
                    target: "notedthat::mcp",
                    kb = %kb,
                    status,
                    "the events route refused the subscriber; its subscriptions for this knowledge base are stranded (401) or dropped"
                );
                fw.watch.link.send_replace(Link::Refused(status));
                return;
            }
            status => {
                tracing::debug!(target: "notedthat::mcp", kb = %kb, status, "events stream unavailable; retrying");
                if !pause(&fw.watch.cancel, &mut backoff).await {
                    return;
                }
                continue;
            }
        }

        let mut response = response;
        let mut parser = SseParser::new();
        loop {
            let chunk = tokio::select! {
                () = fw.watch.cancel.cancelled() => return,
                chunk = response.chunk() => chunk,
            };
            let Ok(Some(bytes)) = chunk else {
                break; // stream ended; reconnect with the last id
            };
            for frame in parser.feed(&bytes) {
                if let Some(id) = frame.id {
                    last_event_id = Some(id);
                }
                backoff = BACKOFF_MIN;
                for action in decide(frame.event.as_deref(), &frame.data, &fw.watch) {
                    match action {
                        Action::Updated(uri) => {
                            if fw
                                .peer
                                .notify_resource_updated(ResourceUpdatedNotificationParam::new(uri))
                                .await
                                .is_err()
                            {
                                // The session's transport is gone; nothing to notify.
                                return;
                            }
                        }
                        Action::ListChanged => fw.coalescer.touch(),
                    }
                }
            }
        }
        if !pause(&fw.watch.cancel, &mut backoff).await {
            return;
        }
    }
}

/// Wait out `backoff` (then double it, capped); `false` when cancelled.
async fn pause(cancel: &CancellationToken, backoff: &mut Duration) -> bool {
    let wait = *backoff;
    *backoff = (*backoff * 2).min(BACKOFF_MAX);
    tokio::select! {
        () = cancel.cancelled() => false,
        () = tokio::time::sleep(wait) => true,
    }
}

/// Whether any of a session's watches still holds a subscribed key, counting
/// stranded ones: those come back on the client's next request, so the
/// session is still worth holding open for it.
///
/// Takes `&Inner` rather than locking, so the keeper can test this and clear
/// `keeper_started` without letting go in between.
fn any_subscribed(inner: &Inner) -> bool {
    inner
        .kbs
        .values()
        .any(|watch| !watch.keys.lock().expect("subscription keys").is_empty())
        || inner
            .stranded
            .values()
            .any(|stranded| !stranded.keys.is_empty())
}

/// Ping the client once a minute, for as long as it has a subscription to
/// deliver, so rmcp's idle timer sees a message. A ping the client does not
/// answer means it has no working notification leg, so the subscriptions are
/// worthless: cancel them, mark the session undeliverable and let it expire.
///
/// It stops of its own accord once nothing is subscribed. The keeper is what
/// defeats rmcp's idle timer, so one that outlived the last subscription would
/// pin a `MAX_SESSIONS` slot for a session with nothing to deliver — reachable
/// by unsubscribing the last key, and by a `subscribe` that `ensure_keeper`
/// started before the route refused it.
///
/// The mark matters because cancelling tells rmcp nothing — the session stays
/// open and usable for tools. Without it, a later `resources/subscribe` on the
/// same session would build a watch whose token is already cancelled, its
/// forwarder would return on the first `select!`, `wait_open` would time out
/// still `Connecting`, and `subscribe` would answer `Ok` for a subscription
/// that can never fire.
async fn keep_alive(
    session: Weak<Subscriptions>,
    peer: Peer<RoleServer>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(KEEPER_INTERVAL) => {}
        }
        // Nothing subscribed any more — the last key was unsubscribed, or the
        // route refused the only forwarder and `drop_watch` cleared it. Stop
        // pinging and let rmcp's idle timer reclaim the session: holding it
        // open past that point is the exact thing moving the keeper out of
        // `resources/list` was meant to stop, arrived at from the other end.
        // `keeper_started` is released rather than the session cancelled, so a
        // later `subscribe` starts a fresh keeper.
        //
        // The upgraded handle is dropped before the ping below: a strong
        // reference held across an await would keep `Subscriptions` alive past
        // the session that owns it, and `Drop` is what cancels every task.
        let idle = match session.upgrade() {
            None => return,
            Some(live) => live.stand_down_if_idle(),
        };
        if idle {
            tracing::debug!(
                target: "notedthat::mcp",
                "a session has no subscriptions left; the keeper stops and it may idle out"
            );
            return;
        }
        let ping = peer.send_request_with_option(
            ServerRequest::PingRequest(PingRequest::default()),
            PeerRequestOptions::with_timeout(KEEPER_TIMEOUT),
        );
        let answered = tokio::select! {
            () = cancel.cancelled() => return,
            answer = ping => answer.is_ok(),
        };
        if !answered {
            tracing::info!(
                target: "notedthat::mcp",
                "a subscribed session stopped answering pings; dropping its subscriptions"
            );
            if let Some(session) = session.upgrade() {
                session.gone.store(true, Ordering::SeqCst);
            }
            cancel.cancel();
            return;
        }
    }
}

/// `list_changed`, bounded: a change sends one notification at once; changes
/// during the following window are folded into at most one more. A
/// [`Notify`] holds a single permit, which is exactly that bound.
#[derive(Default)]
pub(crate) struct Coalescer {
    notify: Notify,
}

impl Coalescer {
    fn touch(&self) {
        self.notify.notify_one();
    }

    async fn run(self: Arc<Self>, peer: Peer<RoleServer>, cancel: CancellationToken) {
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = self.notify.notified() => {}
            }
            if peer.notify_resource_list_changed().await.is_err() {
                return;
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(LIST_CHANGED_WINDOW) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watch(keys: &[(&str, &str)], list_changed: bool) -> KbWatch {
        KbWatch {
            keys: Arc::new(Mutex::new(
                keys.iter()
                    .map(|(k, u)| ((*k).to_owned(), (*u).to_owned()))
                    .collect(),
            )),
            list_changed: Arc::new(AtomicBool::new(list_changed)),
            alive: Arc::new(AtomicBool::new(true)),
            link: tokio::sync::watch::Sender::new(Link::Open),
            cancel: CancellationToken::new(),
        }
    }

    const WRITTEN: &str = r#"{"event":"object.written","kb":"notes","object_key":"a.md","etag":"\"e\"","size":1,"mime":"text/markdown","mtime":0,"source":"http","occurred_at":"2026-01-01T00:00:00Z"}"#;
    const DELETED: &str = r#"{"event":"object.deleted","kb":"notes","object_key":"a.md","source":"http","occurred_at":"2026-01-01T00:00:00Z"}"#;
    const INDEXED: &str = r#"{"event":"object.indexed","kb":"notes","object_key":"a.md","etag":"\"e\"","mime":"text/markdown","chunks":1,"source":"indexer","occurred_at":"2026-01-01T00:00:00Z"}"#;

    #[test]
    fn a_change_to_a_subscribed_key_updates_under_the_subscribed_uri() {
        let w = watch(&[("a.md", "notedthat://notes/a.md")], false);
        assert_eq!(
            decide(Some("object.written"), WRITTEN, &w),
            vec![Action::Updated("notedthat://notes/a.md".into())]
        );
        assert_eq!(
            decide(Some("object.deleted"), DELETED, &w),
            vec![Action::Updated("notedthat://notes/a.md".into())]
        );
    }

    #[test]
    fn a_change_to_another_key_is_nothing_unless_the_list_is_watched() {
        let other = WRITTEN.replace("a.md", "b.md");
        let w = watch(&[("a.md", "notedthat://notes/a.md")], false);
        assert!(decide(Some("object.written"), &other, &w).is_empty());
        let w = watch(&[("a.md", "notedthat://notes/a.md")], true);
        assert_eq!(
            decide(Some("object.written"), &other, &w),
            vec![Action::ListChanged]
        );
        assert_eq!(
            decide(Some("object.written"), WRITTEN, &w),
            vec![
                Action::Updated("notedthat://notes/a.md".into()),
                Action::ListChanged
            ]
        );
    }

    #[test]
    fn the_indexers_verdicts_heartbeats_and_priming_frames_do_nothing() {
        let w = watch(&[("a.md", "notedthat://notes/a.md")], true);
        assert!(decide(Some("object.indexed"), INDEXED, &w).is_empty());
        assert!(decide(Some("object.index_failed"), INDEXED, &w).is_empty());
        assert!(decide(None, "", &w).is_empty());
        assert!(decide(Some("object.written"), "not json", &w).is_empty());
    }

    fn key(kb: &str, token: Option<&str>) -> WatchKey {
        (kb.to_owned(), token.map(str::to_owned))
    }

    #[tokio::test]
    async fn unsubscribing_the_last_key_stops_the_watch_but_a_list_watch_keeps_it() {
        let subs = Subscriptions::new(&CancellationToken::new());
        let w = watch(&[("a.md", "u")], false);
        subs.inner
            .lock()
            .unwrap()
            .kbs
            .insert(key("notes", Some("t1")), w.clone());
        subs.unsubscribe("notes", "missing.md");
        assert!(
            subs.inner
                .lock()
                .unwrap()
                .kbs
                .contains_key(&key("notes", Some("t1")))
        );
        subs.unsubscribe("notes", "a.md");
        assert!(
            !subs
                .inner
                .lock()
                .unwrap()
                .kbs
                .contains_key(&key("notes", Some("t1")))
        );
        assert!(w.cancel.is_cancelled());

        let w = watch(&[("a.md", "u")], true);
        subs.inner
            .lock()
            .unwrap()
            .kbs
            .insert(key("notes", Some("t1")), w.clone());
        subs.unsubscribe("notes", "a.md");
        assert!(
            subs.inner
                .lock()
                .unwrap()
                .kbs
                .contains_key(&key("notes", Some("t1")))
        );
        assert!(!w.cancel.is_cancelled());
    }

    /// One `unsubscribe` carries a URI and nothing else, so it has to reach
    /// every watch on that knowledge base — including one opened by a token the
    /// client has since rotated away from.
    #[tokio::test]
    async fn unsubscribing_reaches_watches_opened_by_every_credential() {
        let subs = Subscriptions::new(&CancellationToken::new());
        let old_token = watch(&[("a.md", "u")], false);
        let new_token = watch(&[("a.md", "u")], false);
        let anonymous = watch(&[("a.md", "u")], false);
        {
            let mut inner = subs.inner.lock().unwrap();
            inner
                .kbs
                .insert(key("notes", Some("t1")), old_token.clone());
            inner
                .kbs
                .insert(key("notes", Some("t2")), new_token.clone());
            inner.kbs.insert(key("notes", None), anonymous.clone());
        }

        subs.unsubscribe("notes", "a.md");

        assert!(subs.inner.lock().unwrap().kbs.is_empty());
        assert!(old_token.cancel.is_cancelled());
        assert!(new_token.cancel.is_cancelled());
        assert!(anonymous.cancel.is_cancelled());
    }

    /// The token-refresh case: two credentials on one knowledge base get two
    /// forwarders, so the older one expiring cannot take the newer one's
    /// subscriptions with it.
    #[test]
    fn a_watch_is_keyed_on_the_credential_as_well_as_the_knowledge_base() {
        assert_ne!(key("notes", Some("t1")), key("notes", Some("t2")));
        assert_ne!(key("notes", Some("t1")), key("notes", None));
        assert_ne!(key("notes", None), key("other", None));
    }

    /// A watch whose task has stopped must not be handed back as live: its
    /// `link` still reads whatever it last was, so `subscribe` would answer
    /// `Ok` for something that can never fire.
    #[test]
    fn a_cancelled_watch_is_not_reused() {
        let w = watch(&[("a.md", "u")], false);
        assert!(w.alive.load(Ordering::SeqCst));
        w.cancel.cancel();
        assert!(
            w.alive.load(Ordering::SeqCst) && w.cancel.is_cancelled(),
            "still `alive`, which is exactly why `ensure_watch` checks the token too"
        );
    }

    /// The keeper holds a session open against rmcp's idle timer, so it must
    /// stop when the last subscription does — otherwise unsubscribing pins a
    /// `MAX_SESSIONS` slot for a session with nothing left to deliver.
    #[tokio::test]
    async fn the_keeper_stands_down_once_nothing_is_subscribed() {
        let subs = Subscriptions::new(&CancellationToken::new());
        subs.inner
            .lock()
            .unwrap()
            .kbs
            .insert(key("notes", Some("t1")), watch(&[("a.md", "u")], false));
        assert!(subs.has_subscriptions());

        // What the keeper does on a tick that finds nothing left.
        subs.unsubscribe("notes", "a.md");
        assert!(!subs.has_subscriptions());
        subs.inner.lock().unwrap().keeper_started = true;
        assert!(subs.stand_down_if_idle());
        assert!(
            !subs.inner.lock().unwrap().keeper_started,
            "a later subscribe must be able to start a fresh keeper"
        );
        assert!(
            !subs.cancel.is_cancelled(),
            "standing down lets the session idle out; it does not end it"
        );
    }

    /// The stand-down is all-or-nothing: a session that still holds a
    /// subscription keeps its keeper *and* its `keeper_started` mark, so a
    /// `subscribe` racing the keeper's tick cannot end up with neither.
    #[tokio::test]
    async fn a_session_with_a_subscription_left_keeps_its_keeper() {
        let subs = Subscriptions::new(&CancellationToken::new());
        subs.inner
            .lock()
            .unwrap()
            .kbs
            .insert(key("notes", Some("t1")), watch(&[("a.md", "u")], false));
        subs.inner.lock().unwrap().keeper_started = true;

        assert!(!subs.stand_down_if_idle(), "the keeper must keep going");
        assert!(
            subs.inner.lock().unwrap().keeper_started,
            "the mark must survive, or the next subscribe spawns a second keeper"
        );
    }

    /// A `list_changed` watch is exactly the case the keeper must not hold a
    /// session open for.
    #[tokio::test]
    async fn a_list_watch_alone_is_not_a_subscription() {
        let subs = Subscriptions::new(&CancellationToken::new());
        subs.inner
            .lock()
            .unwrap()
            .kbs
            .insert(key("notes", None), watch(&[], true));
        assert!(!subs.has_subscriptions());
    }

    /// Once the keeper has given up, `subscribe` says so instead of answering
    /// `Ok` and going quiet.
    #[test]
    fn a_session_the_keeper_gave_up_on_refuses_new_subscriptions() {
        let subs = Subscriptions::new(&CancellationToken::new());
        assert!(subs.still_deliverable().is_ok());
        subs.gone.store(true, Ordering::SeqCst);
        assert!(subs.still_deliverable().is_err());

        let subs = Subscriptions::new(&CancellationToken::new());
        subs.cancel.cancel();
        assert!(subs.still_deliverable().is_err());
    }

    /// A `401` is an expired bearer — what a token refresh leaves behind — so
    /// what the watch fed is kept for a newer credential; a `403` or `404` is a
    /// credential that lost access, so it is not. The client hears about the
    /// keys either way.
    #[test]
    fn an_expired_bearer_strands_its_keys_and_a_lost_grant_drops_them() {
        let subs = Subscriptions::new(&CancellationToken::new());
        let w = watch(&[("a.md", "u")], true);

        let (uris, list_changed) = subs.strand(&key("notes", Some("t1")), &w, 403);
        assert_eq!((uris, list_changed), (vec!["u".to_owned()], true));
        assert!(subs.inner.lock().unwrap().stranded.is_empty());

        let (uris, _) = subs.strand(&key("notes", Some("t1")), &w, 401);
        assert_eq!(uris, vec!["u".to_owned()]);
        let inner = subs.inner.lock().unwrap();
        let stranded = &inner.stranded[&key("notes", Some("t1"))];
        assert_eq!(stranded.keys.get("a.md").map(String::as_str), Some("u"));
        assert!(stranded.list_changed);
    }

    /// A stranded key is still a subscription: the keeper keeps the session
    /// open for it, and `unsubscribe` reaches it so a re-home cannot bring it
    /// back.
    #[tokio::test]
    async fn a_stranded_key_counts_as_subscribed_until_unsubscribed() {
        let subs = Subscriptions::new(&CancellationToken::new());
        subs.strand(
            &key("notes", Some("t1")),
            &watch(&[("a.md", "u")], false),
            401,
        );
        assert!(subs.has_subscriptions());

        subs.unsubscribe("notes", "a.md");

        assert!(!subs.has_subscriptions());
        assert!(subs.inner.lock().unwrap().stranded.is_empty());
    }

    /// Presenting the credential the keys were stranded under starts nothing:
    /// there is nowhere newer to move them.
    #[tokio::test]
    async fn the_stranding_credential_itself_does_not_re_home() {
        let subs = Subscriptions::new(&CancellationToken::new());
        subs.strand(
            &key("notes", Some("t1")),
            &watch(&[("a.md", "u")], false),
            401,
        );

        subs.note_caller(&NotedThatClient::new("http://127.0.0.1:9", "t1").unwrap());

        assert!(!subs.inner.lock().unwrap().rehoming);
        assert!(subs.has_subscriptions());
    }

    /// Records the `resources/updated` notifications a session is sent.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl rmcp::ClientHandler for Recorder {
        async fn on_resource_updated(
            &self,
            params: ResourceUpdatedNotificationParam,
            _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
        ) {
            self.0.lock().unwrap().push(params.uri);
        }
    }

    struct Idle;
    impl rmcp::ServerHandler for Idle {}

    /// A server peer connected to a [`Recorder`] over an in-memory pipe. The
    /// two running services are returned so they outlive the test body.
    async fn connected() -> (Peer<RoleServer>, Recorder, impl Sized) {
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let recorder = Recorder::default();
        let (server, client) = tokio::join!(
            rmcp::serve_server(Idle, server_io),
            rmcp::serve_client(recorder.clone(), client_io)
        );
        let server = server.expect("server handshake");
        let client = client.expect("client handshake");
        (server.peer().clone(), recorder, (server, client))
    }

    fn updates(recorder: &Recorder, uri: &str) -> usize {
        recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|u| *u == uri)
            .count()
    }

    async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    const URI: &str = "notedthat://notes/a.md";

    /// The old bearer's stream: open once, then — the bearer having expired —
    /// refused on the reconnect.
    async fn mount_expiring_stream(api: &wiremock::MockServer, token: &str) {
        use wiremock::matchers::{header, method, path};
        wiremock::Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/notes/events"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream"),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(api)
            .await;
        wiremock::Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/notes/events"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(wiremock::ResponseTemplate::new(401))
            .with_priority(2)
            .mount(api)
            .await;
    }

    /// The prefix listing the subscribe-time probe reads, as `token`.
    async fn mount_listing(api: &wiremock::MockServer, token: &str, keys: &[&str]) {
        use wiremock::matchers::{header, method, path, query_param};
        let objects: Vec<_> = keys
            .iter()
            .map(|key| serde_json::json!({ "key": key, "size": 0 }))
            .collect();
        wiremock::Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/notes"))
            .and(query_param("prefix", "a.md"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "objects": objects, "next_cursor": null })),
            )
            .mount(api)
            .await;
    }

    /// The issue's case: one knowledge base, two credentials of one session.
    /// The first stream is refused with `401`; the key subscribed under it
    /// moves to the newer credential's stream and keeps receiving updates, and
    /// the client is told to re-read it for the gap.
    #[tokio::test]
    async fn keys_under_an_expired_bearer_move_to_the_refreshed_one() {
        use wiremock::matchers::{header, method, path};
        let api = wiremock::MockServer::start().await;
        mount_expiring_stream(&api, "t1").await;
        mount_listing(&api, "t2", &["a.md"]).await;
        wiremock::Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases/notes/events"))
            .and(header("authorization", "Bearer t2"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(format!("event: object.written\ndata: {WRITTEN}\n\n")),
            )
            .mount(&api)
            .await;
        let (peer, recorder, _services) = connected().await;
        let subs = Subscriptions::new(&CancellationToken::new());
        let old = NotedThatClient::new(&api.uri(), "t1").unwrap();
        let new = NotedThatClient::new(&api.uri(), "t2").unwrap();

        subs.subscribe("notes", "a.md", URI, &old, &peer)
            .await
            .expect("subscribed as the old bearer");
        // The client refreshes and makes a call with the new bearer.
        subs.note_caller(&new);

        eventually("the key re-homed onto the refreshed bearer", || {
            let inner = subs.inner.lock().unwrap();
            inner.stranded.is_empty()
                && !inner.kbs.contains_key(&key("notes", Some("t1")))
                && inner.kbs.get(&key("notes", Some("t2"))).is_some_and(|w| {
                    w.keys.lock().unwrap().get("a.md").map(String::as_str) == Some(URI)
                })
        })
        .await;
        // One for the gap, and then the write the new stream carries.
        eventually("the gap notice and the forwarded write", || {
            updates(&recorder, URI) >= 2
        })
        .await;
    }

    /// Re-homing is not a way around authorization: the newer credential is
    /// probed, and a key it cannot list is dropped — after the client was told
    /// to re-read it, so it does not simply go quiet.
    #[tokio::test]
    async fn a_key_the_refreshed_bearer_cannot_list_is_dropped_not_moved() {
        let api = wiremock::MockServer::start().await;
        mount_expiring_stream(&api, "t1").await;
        mount_listing(&api, "t2", &[]).await;
        let (peer, recorder, _services) = connected().await;
        let subs = Subscriptions::new(&CancellationToken::new());
        let old = NotedThatClient::new(&api.uri(), "t1").unwrap();

        subs.subscribe("notes", "a.md", URI, &old, &peer)
            .await
            .expect("subscribed as the old bearer");
        // Refused while the client is idle: stranded, and the client told.
        eventually("the key stranded", || {
            !subs.inner.lock().unwrap().stranded.is_empty()
        })
        .await;
        eventually("the gap notice", || updates(&recorder, URI) == 1).await;

        // Its next call carries a refreshed bearer that may not list the key.
        subs.note_caller(&NotedThatClient::new(&api.uri(), "t2").unwrap());

        eventually("the re-home finished", || {
            !subs.inner.lock().unwrap().rehoming
        })
        .await;
        let inner = subs.inner.lock().unwrap();
        assert!(inner.stranded.is_empty(), "given up on, not kept");
        assert!(
            !inner.kbs.contains_key(&key("notes", Some("t2"))),
            "no stream opened for a key the credential cannot list"
        );
        assert!(!any_subscribed(&inner));
    }

    #[test]
    fn dropping_the_session_cancels_every_task() {
        let shutdown = CancellationToken::new();
        let subs = Subscriptions::new(&shutdown);
        let token = subs.cancel.clone();
        drop(subs);
        assert!(token.is_cancelled());
        assert!(!shutdown.is_cancelled(), "a child, never the parent");
    }
}

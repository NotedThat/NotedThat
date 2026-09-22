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
//! Everything here dies with the session: dropping the [`Subscriptions`] —
//! which rmcp does when the session ends — cancels every task. Nothing is
//! replayed and nothing persists.
//!
//! Two more tasks per subscribed session: a **keeper** that pings the client
//! once a minute, because rmcp's idle timeout counts messages, not an open
//! `GET` leg, and a session that only listens would otherwise be closed after
//! five minutes; and the **coalescer** behind `list_changed`.

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
    inner: Mutex<Inner>,
    list_changed: Arc<Coalescer>,
}

#[derive(Default)]
struct Inner {
    kbs: HashMap<String, KbWatch>,
    keeper_started: bool,
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
        let link = {
            let mut inner = self.inner.lock().expect("subscriptions");
            let watch = self.ensure_watch(&mut inner, kb, client, peer);
            watch
                .keys
                .lock()
                .expect("subscription keys")
                .insert(key.to_owned(), uri.to_owned());
            self.ensure_keeper(&mut inner, peer);
            watch.link.subscribe()
        };
        match wait_open(link).await {
            Link::Refused(403) => Err(McpToolError::Forbidden.into()),
            Link::Refused(401) => Err(McpToolError::Unauthorized.into()),
            Link::Refused(_) => Err(McpToolError::NotFound("object".to_owned()).into()),
            Link::Open | Link::Connecting => Ok(()),
        }
    }

    /// Forget `key` in `kb`; idempotent. A forwarder nobody wants any more is
    /// stopped.
    pub(crate) fn unsubscribe(&self, kb: &str, key: &str) {
        let mut inner = self.inner.lock().expect("subscriptions");
        let Some(watch) = inner.kbs.get(kb) else {
            return;
        };
        watch.keys.lock().expect("subscription keys").remove(key);
        if !watch.wanted() {
            watch.cancel.cancel();
            inner.kbs.remove(kb);
        }
    }

    /// Watch every knowledge base in `kbs` for list changes, as the caller
    /// whose listing named them. Waits for the streams to open, briefly, so
    /// a write right after the listing counts; a knowledge base whose stream
    /// the route refuses is simply not watched.
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
                self.ensure_keeper(&mut inner, peer);
            }
            links
        };
        for link in links {
            wait_open(link).await;
        }
    }

    /// The live watch for `kb`, started if missing or dead.
    fn ensure_watch(
        self: &Arc<Self>,
        inner: &mut Inner,
        kb: &str,
        client: &NotedThatClient,
        peer: &Peer<RoleServer>,
    ) -> KbWatch {
        if let Some(watch) = inner.kbs.get(kb)
            && watch.alive.load(Ordering::SeqCst)
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
        inner.kbs.insert(kb.to_owned(), watch.clone());
        tokio::spawn(forward(Forwarder {
            session: Arc::downgrade(self),
            kb: kb.to_owned(),
            client: client.clone(),
            peer: peer.clone(),
            watch: watch.clone(),
            coalescer: self.list_changed.clone(),
        }));
        watch
    }

    /// Start the keeper and the coalescer once, on the session's first
    /// interest; they run until the session's token is cancelled.
    fn ensure_keeper(self: &Arc<Self>, inner: &mut Inner, peer: &Peer<RoleServer>) {
        if inner.keeper_started {
            return;
        }
        inner.keeper_started = true;
        tokio::spawn(keep_alive(peer.clone(), self.cancel.clone()));
        tokio::spawn(
            self.list_changed
                .clone()
                .run(peer.clone(), self.cancel.clone()),
        );
    }

    /// The forwarder for `kb` found the route refusing its caller: forget
    /// everything about that knowledge base.
    fn drop_watch(&self, kb: &str) {
        let mut inner = self.inner.lock().expect("subscriptions");
        if let Some(watch) = inner.kbs.remove(kb) {
            watch.alive.store(false, Ordering::SeqCst);
            watch.list_changed.store(false, Ordering::SeqCst);
            watch.keys.lock().expect("subscription keys").clear();
            watch.cancel.cancel();
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

/// Everything a forwarder needs, owned by its task.
struct Forwarder {
    session: Weak<Subscriptions>,
    kb: String,
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
async fn forward(fw: Forwarder) {
    let mut last_event_id: Option<String> = None;
    let mut backoff = BACKOFF_MIN;
    loop {
        let request = fw.client.events_stream(&fw.kb, last_event_id.as_deref());
        let response = tokio::select! {
            () = fw.watch.cancel.cancelled() => return,
            sent = request.send() => sent,
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(target: "notedthat::mcp", kb = %fw.kb, %error, "events stream request failed; retrying");
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
            410 => {
                // The position is gone; a subscription has no replay to owe.
                last_event_id = None;
                continue;
            }
            status @ (401 | 403 | 404) => {
                tracing::warn!(
                    target: "notedthat::mcp",
                    kb = %fw.kb,
                    status,
                    "the events route refused the subscriber; dropping its subscriptions for this knowledge base"
                );
                fw.watch.link.send_replace(Link::Refused(status));
                if let Some(session) = fw.session.upgrade() {
                    session.drop_watch(&fw.kb);
                }
                return;
            }
            status => {
                tracing::debug!(target: "notedthat::mcp", kb = %fw.kb, status, "events stream unavailable; retrying");
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

/// Ping the client once a minute so rmcp's idle timer sees a message. A ping
/// the client does not answer means it has no working notification leg, so
/// the subscriptions are worthless: cancel them and let the session expire.
async fn keep_alive(peer: Peer<RoleServer>, cancel: CancellationToken) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(KEEPER_INTERVAL) => {}
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

    #[tokio::test]
    async fn unsubscribing_the_last_key_stops_the_watch_but_a_list_watch_keeps_it() {
        let subs = Subscriptions::new(&CancellationToken::new());
        let w = watch(&[("a.md", "u")], false);
        subs.inner
            .lock()
            .unwrap()
            .kbs
            .insert("notes".into(), w.clone());
        subs.unsubscribe("notes", "missing.md");
        assert!(subs.inner.lock().unwrap().kbs.contains_key("notes"));
        subs.unsubscribe("notes", "a.md");
        assert!(!subs.inner.lock().unwrap().kbs.contains_key("notes"));
        assert!(w.cancel.is_cancelled());

        let w = watch(&[("a.md", "u")], true);
        subs.inner
            .lock()
            .unwrap()
            .kbs
            .insert("notes".into(), w.clone());
        subs.unsubscribe("notes", "a.md");
        assert!(subs.inner.lock().unwrap().kbs.contains_key("notes"));
        assert!(!w.cancel.is_cancelled());
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

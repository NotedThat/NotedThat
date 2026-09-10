//! Coalescing what the kernel reports into the smallest set of work that covers it.
//!
//! # Why this is not just a debounce
//!
//! A debounce collapses repeated reports of *one* path. That is necessary — an editor
//! saving a file produces several — but it is not sufficient, because the expensive
//! reports are the ones that name a *subtree*.
//!
//! A directory appearing has to be walked: the kernel installs a watch on it but says
//! nothing about the files already inside, and for a directory created and filled in one
//! burst it may not install that watch before the first files land. A `git checkout` that
//! creates a thousand directories therefore produces a thousand subtree walks, each of
//! which asks the index what it holds — where one walk of the top directory covers every
//! one of them.
//!
//! So this coalesces by containment as well as by time. A pending subtree swallows every
//! pending subtree and object beneath it, and a whole-knowledge-base pass swallows
//! everything. What comes out is the smallest set of work that still covers everything
//! reported.
//!
//! # The two kinds of pending work settle differently
//!
//! "Held until it settles" is not quite uniform, and the difference is deliberate.
//! Reporting an object again defers its deadline, so a file still being written is not
//! read halfway through. Reporting a subtree again does not: an already-pending prefix
//! covers the report and returns early, so the subtree is walked a fixed window after its
//! *first* report rather than after the burst ends.
//!
//! Prefixes are the ones that must not wait. A subtree walk that fires early costs one
//! extra walk of a tree that is mostly unchanged — and an unchanged object costs a stat —
//! whereas a subtree whose deadline kept being pushed out could starve indefinitely under
//! sustained churn, which is exactly the case a `git checkout` produces. The asymmetry is
//! invisible at the call site, so it is written down here.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use notedthat_core::{KbSlug, ObjectPath};

use super::FsSignal;

/// Work reported but not yet emitted, held until it settles.
#[derive(Debug, Default)]
pub(super) struct Pending {
    /// Knowledge bases needing a full pass, and when that was last asked for.
    whole: HashMap<KbSlug, Instant>,
    /// Subtrees needing a pass. Prefixes end in `/`.
    prefixes: HashMap<(KbSlug, String), Instant>,
    /// Individual objects needing a look.
    keys: HashMap<(KbSlug, ObjectPath), Instant>,
    /// Whether work has been traded down to a full pass since this was last reported.
    overflowed: bool,
    capacity: usize,
}

/// Every prefix of `path` that a pending prefix could be, broadest first.
///
/// Pending prefixes always end in `/`, so these are exactly the positions of `/` in
/// `path`: `a/b/c.md` yields `a/` and `a/b/`, and `a/b/` yields `a/` and itself — a
/// prefix covers itself, which is what makes a repeated report of one subtree cheap.
fn ancestor_prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(at, _)| &path[..=at])
}

impl Pending {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            ..Self::default()
        }
    }

    /// Entries held for one knowledge base, across all three kinds.
    ///
    /// Counted per knowledge base rather than globally because that is what escalation
    /// does: `at_capacity` is only ever answered by a full pass over the knowledge base
    /// whose event is being charged. Counting globally would let a busy knowledge base
    /// force full passes on quiet ones — expensive, correct, and very hard to diagnose
    /// from the `FS_WATCH_RESCAN` line, which names only the pass it caused.
    fn len_for(&self, kb: &KbSlug) -> usize {
        usize::from(self.whole.contains_key(kb))
            + self
                .prefixes
                .keys()
                .filter(|(pending, _)| pending == kb)
                .count()
            + self
                .keys
                .keys()
                .filter(|(pending, _)| pending == kb)
                .count()
    }

    /// Ask for a whole knowledge base to be re-examined.
    ///
    /// Swallows every subtree and object pending for it: a full pass covers them all, and
    /// it is also the right answer when the map has overflowed and we no longer know what
    /// was dropped.
    pub(super) fn whole_kb(&mut self, kb: &KbSlug, now: Instant) {
        self.prefixes.retain(|(pending, _), _| pending != kb);
        self.keys.retain(|(pending, _), _| pending != kb);
        self.whole.insert(kb.clone(), now);
    }

    /// Ask for everything under `prefix` to be re-examined.
    ///
    /// `prefix` must end in `/`, so `notes/` cannot swallow `notes-archive/`.
    pub(super) fn prefix(&mut self, kb: &KbSlug, prefix: String, now: Instant) {
        if self.covered(kb, &prefix) {
            return;
        }
        self.prefixes
            .retain(|(pending_kb, pending), _| pending_kb != kb || !pending.starts_with(&prefix));
        self.keys
            .retain(|(pending_kb, key), _| pending_kb != kb || !key.as_str().starts_with(&prefix));
        if self.at_capacity(kb) {
            self.whole_kb(kb, now);
            return;
        }
        self.prefixes.insert((kb.clone(), prefix), now);
    }

    /// Ask for one object to be re-examined.
    pub(super) fn key(&mut self, kb: &KbSlug, key: ObjectPath, now: Instant) {
        if self.covered(kb, key.as_str()) {
            return;
        }
        if self.at_capacity(kb) {
            // Dropping a change silently is the one outcome worth avoiding: nothing else
            // would ever re-enqueue it. A full pass is expensive and correct, so trade
            // down to that instead.
            self.whole_kb(kb, now);
            return;
        }
        self.keys.insert((kb.clone(), key), now);
    }

    /// Whether a broader pending pass already covers `path`.
    ///
    /// Asked once per file the kernel reports, on `notify`'s event thread and under the
    /// lock, so it looks up the prefixes that could contain `path` rather than scanning
    /// the ones that do not. Every pending prefix ends in `/`, so the only candidates are
    /// `path`'s own ancestors — a handful of lookups bounded by the key's depth, whatever
    /// the map holds.
    fn covered(&self, kb: &KbSlug, path: &str) -> bool {
        if self.whole.contains_key(kb) {
            return true;
        }
        ancestor_prefixes(path)
            .any(|prefix| self.prefixes.contains_key(&(kb.clone(), prefix.to_owned())))
    }

    /// Whether `kb` holds as much pending work as is worth tracking individually.
    fn at_capacity(&mut self, kb: &KbSlug) -> bool {
        if self.len_for(kb) >= self.capacity {
            self.overflowed = true;
            return true;
        }
        false
    }

    /// Take everything untouched for at least `settle`, broadest work first.
    ///
    /// Broadest first because a consumer acting on a full pass makes narrower work behind
    /// it redundant, and because it is the order that reads correctly in a log.
    pub(super) fn take_settled(&mut self, now: Instant, settle: Duration) -> Vec<FsSignal> {
        let ready = |at: &Instant| now.saturating_duration_since(*at) >= settle;
        let mut out = Vec::new();

        let settled: Vec<KbSlug> = self
            .whole
            .iter()
            .filter(|(_, at)| ready(at))
            .map(|(kb, _)| kb.clone())
            .collect();
        for kb in settled {
            self.whole.remove(&kb);
            out.push(FsSignal::Kb { kb });
        }

        let settled: Vec<(KbSlug, String)> = self
            .prefixes
            .iter()
            .filter(|(_, at)| ready(at))
            .map(|(entry, _)| entry.clone())
            .collect();
        for entry in settled {
            self.prefixes.remove(&entry);
            let (kb, prefix) = entry;
            out.push(FsSignal::Prefix { kb, prefix });
        }

        let settled: Vec<(KbSlug, ObjectPath)> = self
            .keys
            .iter()
            .filter(|(_, at)| ready(at))
            .map(|(entry, _)| entry.clone())
            .collect();
        for entry in settled {
            self.keys.remove(&entry);
            let (kb, key) = entry;
            out.push(FsSignal::Changed { kb, key });
        }

        out
    }

    /// Whether work has been traded down to a full pass since this was last asked.
    pub(super) fn take_overflowed(&mut self) -> bool {
        std::mem::take(&mut self.overflowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> KbSlug {
        KbSlug::try_new("notes").expect("slug")
    }

    fn key(value: &str) -> ObjectPath {
        ObjectPath::try_from(value).expect("valid key")
    }

    fn settle() -> Duration {
        Duration::from_millis(100)
    }

    /// Later than any deadline set at `base`.
    fn later(base: Instant) -> Instant {
        base + Duration::from_millis(500)
    }

    #[test]
    fn nothing_is_emitted_before_it_settles() {
        let now = Instant::now();
        let mut pending = Pending::new(64);
        pending.key(&kb(), key("a.md"), now);

        assert!(pending.take_settled(now, settle()).is_empty());
        assert_eq!(pending.take_settled(later(now), settle()).len(), 1);
    }

    /// An editor saving one file reports it several times; the consumer should see it once.
    #[test]
    fn repeated_reports_of_one_object_emit_once() {
        let now = Instant::now();
        let mut pending = Pending::new(64);
        for _ in 0..5 {
            pending.key(&kb(), key("a.md"), now);
        }

        assert_eq!(pending.take_settled(later(now), settle()).len(), 1);
    }

    /// Each further report pushes the deadline out, so a file still being written is not
    /// read halfway through.
    #[test]
    fn a_further_report_defers_the_deadline() {
        let start = Instant::now();
        let mut pending = Pending::new(64);
        pending.key(&kb(), key("a.md"), start);

        let midway = start + Duration::from_millis(80);
        pending.key(&kb(), key("a.md"), midway);

        assert!(
            pending
                .take_settled(start + Duration::from_millis(120), settle())
                .is_empty(),
            "settled against the first report rather than the latest"
        );
        assert_eq!(pending.take_settled(later(midway), settle()).len(), 1);
    }

    #[test]
    fn distinct_objects_are_kept_apart() {
        let now = Instant::now();
        let mut pending = Pending::new(64);
        pending.key(&kb(), key("a.md"), now);
        pending.key(&kb(), key("b.md"), now);

        assert_eq!(pending.take_settled(later(now), settle()).len(), 2);
    }

    /// The `git checkout` case: a thousand nested directories must not become a thousand
    /// subtree passes.
    #[test]
    fn a_prefix_swallows_the_prefixes_and_keys_beneath_it() {
        let now = Instant::now();
        let mut pending = Pending::new(64);
        pending.prefix(&kb(), "top/sub/".to_owned(), now);
        pending.key(&kb(), key("top/sub/deep.md"), now);
        pending.prefix(&kb(), "top/".to_owned(), now);
        // Arriving after the broader pass is pending, so both are already covered.
        pending.prefix(&kb(), "top/other/".to_owned(), now);
        pending.key(&kb(), key("top/a.md"), now);

        assert_eq!(
            pending.take_settled(later(now), settle()),
            vec![FsSignal::Prefix {
                kb: kb(),
                prefix: "top/".to_owned()
            }]
        );
    }

    /// Containment is on the separator, or `notes/` would swallow `notes-archive/`.
    #[test]
    fn a_prefix_does_not_swallow_a_sibling_with_the_same_stem() {
        let now = Instant::now();
        let mut pending = Pending::new(64);
        pending.prefix(&kb(), "notes/".to_owned(), now);
        pending.key(&kb(), key("notes-archive/a.md"), now);

        assert_eq!(pending.take_settled(later(now), settle()).len(), 2);
    }

    #[test]
    fn a_whole_pass_swallows_everything_for_that_knowledge_base() {
        let now = Instant::now();
        let mut pending = Pending::new(64);
        pending.key(&kb(), key("a.md"), now);
        pending.prefix(&kb(), "top/".to_owned(), now);
        pending.whole_kb(&kb(), now);
        pending.key(&kb(), key("b.md"), now);

        assert_eq!(
            pending.take_settled(later(now), settle()),
            vec![FsSignal::Kb { kb: kb() }]
        );
    }

    #[test]
    fn one_knowledge_bases_full_pass_leaves_another_alone() {
        let now = Instant::now();
        let other = KbSlug::try_new("other").expect("slug");
        let mut pending = Pending::new(64);
        pending.key(&other, key("a.md"), now);
        pending.whole_kb(&kb(), now);

        let settled = pending.take_settled(later(now), settle());
        assert_eq!(settled.len(), 2);
        assert!(settled.contains(&FsSignal::Kb { kb: kb() }));
        assert!(settled.contains(&FsSignal::Changed {
            kb: other,
            key: key("a.md")
        }));
    }

    /// The deadline is deferred by a further report of an *object*, and deliberately not by
    /// a further report of a *subtree*: a walk that fires early is cheap, a walk whose
    /// deadline keeps moving could starve under sustained churn.
    #[test]
    fn a_further_report_of_a_subtree_does_not_defer_its_deadline() {
        let start = Instant::now();
        let mut pending = Pending::new(64);
        pending.prefix(&kb(), "top/".to_owned(), start);

        let midway = start + Duration::from_millis(80);
        pending.prefix(&kb(), "top/".to_owned(), midway);

        assert_eq!(
            pending.take_settled(start + Duration::from_millis(120), settle()),
            vec![FsSignal::Prefix {
                kb: kb(),
                prefix: "top/".to_owned()
            }],
            "a subtree settles against its first report, not its latest"
        );
    }

    /// Capacity is charged to the knowledge base that would be escalated. Counting globally
    /// would make a quiet knowledge base pay a full pass because a different one is busy.
    #[test]
    fn one_knowledge_bases_overflow_does_not_escalate_another() {
        let now = Instant::now();
        let busy = kb();
        let quiet = KbSlug::try_new("other").expect("slug");
        let mut pending = Pending::new(2);

        pending.key(&busy, key("a.md"), now);
        pending.key(&busy, key("b.md"), now);
        pending.key(&busy, key("c.md"), now);
        assert!(
            pending.take_overflowed(),
            "the busy one must have traded down"
        );

        pending.key(&quiet, key("only.md"), now);

        let settled = pending.take_settled(later(now), settle());
        assert!(settled.contains(&FsSignal::Kb { kb: busy }));
        assert!(
            settled.contains(&FsSignal::Changed {
                kb: quiet,
                key: key("only.md")
            }),
            "the quiet knowledge base must keep its one object, not be escalated"
        );
    }

    /// Dropping work silently is the one outcome worth avoiding — nothing would re-enqueue
    /// it. Overflow trades down to a full pass instead, which is expensive and correct.
    #[test]
    fn overflow_trades_down_to_a_full_pass_rather_than_dropping_work() {
        let now = Instant::now();
        let mut pending = Pending::new(2);
        pending.key(&kb(), key("a.md"), now);
        pending.key(&kb(), key("b.md"), now);
        pending.key(&kb(), key("c.md"), now);

        assert!(pending.take_overflowed(), "overflow must be reportable");
        assert!(!pending.take_overflowed(), "and reported only once");
        assert_eq!(
            pending.take_settled(later(now), settle()),
            vec![FsSignal::Kb { kb: kb() }]
        );
    }
}

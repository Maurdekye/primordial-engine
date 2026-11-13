//! The change log: what became of each entry, from the change record beside S5 and agreement's final cuts (api.md
//! 5.3.2, 9.1, 9.2 steps 6-7).
//!
//! pe-app feeds it every publication's `FrameView.changes` (the entries stepped since the last one, with their
//! outcomes; a re-stepped entry appears again and the newest verdict wins), and every final cut with the entries
//! that cut voided (`confirm_cut`). For the changes this machine submitted (`track`: their chain, seq and stamp),
//! a final cut whose frame is above the stamp decides (S3 v1.1; engine-architect):
//! - voided by that cut (late, or back-stamped) is `dropped`: no replica applies it, so its local effect is undone;
//! - included and not voided is `final` (or a final refusal), once the view's `final_frame` has also passed the frame
//!   it was stepped at here: only then is this machine's verdict for it the quorum's (S5 v1.5; coordinator). While a repair is under way `final_frame` waits, so a verdict stepped on a diverged state is
//!   never called final;
//! - neither stays as it was: pending, until a later cut includes it or voids it.
//!
//! The replica's own records end an entry too: `dropped` (an entry it stepped that the final order leaves out) and
//! `unknown` (after a rung-2 import: in the final order, stepped on a wrong state; coordinator). Each resolves once: a later record or cut saying the same is not reported again.
//!
//! Finality never comes from the frame alone: a dropped change is never stepped again on its author's machine, so a
//! frame test would call it final (@chunks-reviewer's Medium). `apply` waits here, on its own thread. Kept
//! bounded: settled entries are evicted first, oldest first, without scanning.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use pe_core::{ChainId, EntryRef};
use pe_seams::world_api::{Frame, SessionEvent, SessionHealth, Status, TxId, Wait};
use pe_seams::{Cut, Health, Outcome as WorldOutcome, StepRecord};

use crate::stream::{Retention, Stream};

/// The most entries remembered [I].
pub const CAPACITY: usize = 10_000;

/// Who submitted one of this machine's changes through the API, for `watch`'s `mine` (api.md 5.4).
#[derive(Clone, Debug, PartialEq)]
pub struct Owner {
    /// The caller's principal: a caller's own changes are those submitted under it, from any connection.
    pub principal: Arc<str>,
    /// For each pe-world op, the API op it came from, to name it in a refusal.
    pub op_of: Arc<[u32]>,
    /// The caller's `$names` for the objects it creates, with their new indices.
    pub refs: Arc<[(String, u32)]>,
}

/// One item of the change stream behind `watch` (api.md 5.4, 9.1). Raw and cheap to push: `watch` describes it on
/// the caller's thread.
#[derive(Clone, Debug, PartialEq)]
pub enum FeedItem {
    /// One of this machine's changes reached agreement as `change`, stamped `frame`.
    Submitted { change: TxId, frame: Frame, owner: Option<Owner> },
    /// An entry was stepped here, or re-stepped with another result (a re-step that changes nothing is not
    /// reported again). `undid`: the earlier step that applied it, when this one doesn't (a retraction).
    /// `status`/`is_final`: its fate as of this step.
    Stepped { step: StepRecord, undid: Option<StepRecord>, status: Status, is_final: bool, owner: Option<Owner> },
    /// An entry known here was settled: `final`, `dropped` or a final refusal, by a final cut, or once the view's
    /// `final_frame` passed the step of an entry a cut had included. `step`: its newest step here.
    Settled { change: TxId, status: Status, is_final: bool, step: Option<StepRecord>, owner: Option<Owner> },
    /// The newest final frame moved.
    Final { frame: Frame },
    /// Session news: from pe-app, membership taking effect, catching up and caught up; from the view, its health
    /// changing (api.md 5.4 `session`).
    Session(pe_seams::world_api::SessionEvent),
}

/// An entry's newest verdict.
#[derive(Clone, Debug, PartialEq)]
pub struct Fate {
    pub change: TxId,
    /// The frame it was (re-)stepped at; `None` if it was settled without being stepped here.
    pub frame: Option<Frame>,
    pub status: Status,
    /// For `refused` (and `dropped`, `unknown`): whether that is final.
    pub is_final: bool,
    /// pe-world's outcome at its newest step, if it was stepped here.
    pub outcome: Option<WorldOutcome>,
    /// Its newest step here.
    pub step: Option<StepRecord>,
    /// Who submitted it, if it was submitted through this machine's API.
    pub owner: Option<Owner>,
}

#[derive(Clone)]
struct Coords {
    chain: ChainId,
    seq: u64,
    stamp: Frame,
    owner: Option<Owner>,
}

struct Inner {
    fates: HashMap<TxId, Fate>,
    order: VecDeque<TxId>,
    final_frame: Option<Frame>,
    /// Entries a final cut has settled: included (true) or voided (false). A later re-step can't unsettle them.
    settled: HashMap<TxId, bool>,
    /// Settled entries in the order they settled, for eviction without a scan.
    settled_order: VecDeque<TxId>,
    /// This machine's submissions, for `Cut::includes`.
    coords: HashMap<TxId, Coords>,
    /// Entries a cut included whose step `final_frame` hasn't passed yet, by step frame. Stale pairs are skipped.
    awaiting: BTreeSet<(Frame, TxId)>,
    /// The view's health, as of the newest publication.
    health: Health,
}

pub struct ChangeLog {
    inner: Mutex<Inner>,
    changed: Condvar,
    /// The change stream behind `watch`, appended under the log's lock, so its order is the log's (the push is an
    /// append and a notify; nothing takes the log's lock while holding the stream's).
    feed: Stream<Arc<FeedItem>>,
}

/// A stream epoch unlikely to repeat across runs, so an old cursor is refused, never misread.
fn epoch() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    (t ^ N.fetch_add(1, Ordering::Relaxed).rotate_left(40)) & 0xffff_ffff_ffff
}

/// An entry's status from its newest outcome here, a final cut's decision (included or voided), and whether the
/// view's `final_frame` has passed its step (`passed`).
fn status(outcome: Option<&WorldOutcome>, settled: Option<bool>, passed: bool) -> (Status, bool) {
    match (outcome, settled) {
        (_, Some(false)) | (Some(WorldOutcome::Dropped { .. }), _) => (Status::Dropped, true),
        (Some(WorldOutcome::Unknown { .. }), _) => (Status::Unknown, true),
        (Some(WorldOutcome::Refused { .. }), s) => (Status::Refused, s == Some(true) && passed),
        (Some(WorldOutcome::Applied { .. }), Some(true)) if passed => (Status::Final, false),
        (Some(WorldOutcome::Applied { .. }), _) => (Status::Accepted, false),
        // Included, but not stepped here (yet): its verdict is not known here.
        (None, _) => (Status::Queued, false),
    }
}

/// Whether `final_frame` is past `frame`: a cut at frame `f` covers the entries stamped below `f` (S3, S5 v1.5).
fn passed(final_frame: Option<Frame>, frame: Option<Frame>) -> bool {
    matches!((final_frame, frame), (Some(ff), Some(f)) if f < ff)
}

/// S5's health as WorldApi's session shows it.
pub fn session_health(h: Health) -> SessionHealth {
    match h {
        Health::Agreed => SessionHealth::Agreed,
        Health::Repairing { since } => SessionHealth::Repairing { since: since.0 },
    }
}

impl Default for ChangeLog {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeLog {
    pub fn new() -> Self {
        Self::with_retention(Retention::DEFAULT)
    }

    /// A log whose change stream keeps `r` (spec 9.1).
    pub fn with_retention(r: Retention) -> Self {
        ChangeLog {
            feed: Stream::new(epoch(), r),
            inner: Mutex::new(Inner {
                fates: HashMap::new(),
                order: VecDeque::new(),
                final_frame: None,
                settled: HashMap::new(),
                settled_order: VecDeque::new(),
                coords: HashMap::new(),
                awaiting: BTreeSet::new(),
                health: Health::Agreed,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// pe-api, on a receipt: where this machine's change sits in agreement, for deciding it against final cuts.
    pub fn track(&self, change: TxId, chain: ChainId, seq: u64, stamp: Frame, owner: Option<Owner>) {
        let mut g = self.lock();
        g.coords.insert(change, Coords { chain, seq, stamp, owner: owner.clone() });
        self.feed.push(Arc::new(FeedItem::Submitted { change, frame: stamp, owner }), Instant::now());
        drop(g);
    }

    /// pe-app, on the simulation thread: a session event for `watch` (`Joined`/`Left` when agreement's membership
    /// change takes effect at its frame; `CatchingUp`/`CaughtUp`). `Final` comes from `record`.
    pub fn session_event(&self, e: pe_seams::world_api::SessionEvent) {
        // Under the log's lock, like every other push, so the feed keeps one order (@chunks-reviewer's nit).
        let g = self.lock();
        self.feed.push(Arc::new(FeedItem::Session(e)), Instant::now());
        drop(g);
    }

    /// The change stream behind `watch`.
    pub fn feed(&self) -> &Stream<Arc<FeedItem>> {
        &self.feed
    }

    fn push_all(&self, items: Vec<FeedItem>) {
        let now = Instant::now();
        for it in items {
            self.feed.push(Arc::new(it), now);
        }
    }

    /// pe-app, per publication, in order and without a gap (S5's completeness): the view's `changes` (the entries
    /// stepped since the last one), its `final_frame` and its `health`. A health change is reported on `watch`'s
    /// session stream, and `session` reads it here.
    pub fn publish(&self, steps: &[StepRecord], final_frame: Option<Frame>, health: Health) {
        self.record_inner(steps, final_frame, Some(health));
    }

    /// [`ChangeLog::publish`] with the health unchanged.
    pub fn record(&self, steps: &[StepRecord], final_frame: Option<Frame>) {
        self.record_inner(steps, final_frame, None);
    }

    /// The view's health, as of the newest publication.
    pub fn health(&self) -> Health {
        self.lock().health
    }

    fn record_inner(&self, steps: &[StepRecord], final_frame: Option<Frame>, health: Option<Health>) {
        let mut g = self.lock();
        let mut items = Vec::with_capacity(steps.len() + 2);
        let applied = |o: &WorldOutcome| matches!(o, WorldOutcome::Applied { .. });
        // A view's records and its final frame belong together: its records with the views before hold the final
        // verdict of every entry stamped below its final frame (S5). The final frame never goes backwards.
        let ff = final_frame.max(g.final_frame);
        // Repairing is told before the steps it explains; agreed again after the repair's records.
        let health_news = health.filter(|h| *h != g.health);
        if let Some(h) = health_news {
            g.health = h;
        }
        if let Some(h @ Health::Repairing { .. }) = health_news {
            items.push(FeedItem::Session(SessionEvent::HealthChanged { health: session_health(h) }));
        }
        let ended = |st: Status| matches!(st, Status::Dropped | Status::Unknown);
        for s in steps {
            let change = TxId::from(s.entry);
            let settled = g.settled.get(&change).copied();
            let (status, is_final) = status(Some(&s.outcome), settled, passed(ff, Some(s.frame.0)));
            if settled == Some(true) && !passed(ff, Some(s.frame.0)) {
                g.awaiting.insert((s.frame.0, change));
            }
            let prev = g.fates.get(&change);
            let before = prev.and_then(|f| f.step.as_ref());
            // A re-step with the same result changed nothing a watcher saw (spec 5.4: reported only when a re-step
            // changes what a frame did). An entry already dropped or unknown has resolved: the replica's record
            // after a cut voided it (or the reverse) says nothing new.
            let same = before.is_some_and(|p| p.frame == s.frame && p.outcome == s.outcome)
                || prev.is_some_and(|f| ended(f.status) && f.status == status);
            let undid = before.filter(|p| applied(&p.outcome) && !applied(&s.outcome)).cloned();
            let owner =
                g.coords.get(&change).and_then(|c| c.owner.clone()).or_else(|| prev.and_then(|f| f.owner.clone()));
            let f = Fate {
                change,
                frame: Some(s.frame.0),
                status,
                is_final,
                outcome: Some(s.outcome.clone()),
                step: Some(s.clone()),
                owner: owner.clone(),
            };
            if g.fates.insert(change, f).is_none() {
                g.order.push_back(change);
            }
            if !same {
                items.push(FeedItem::Stepped { step: s.clone(), undid, status, is_final, owner });
            }
        }
        if let Some(h @ Health::Agreed) = health_news {
            items.push(FeedItem::Session(SessionEvent::HealthChanged { health: session_health(h) }));
        }
        if ff > g.final_frame {
            g.final_frame = ff;
            // The included entries whose step the final frame now passes are final.
            while let Some(&(frame, t)) = g.awaiting.first() {
                if !passed(ff, Some(frame)) {
                    break;
                }
                g.awaiting.pop_first();
                let settled = g.settled.get(&t).copied();
                if let Some(f) = g.fates.get_mut(&t)
                    && f.frame == Some(frame)
                {
                    let (s, fin) = status(f.outcome.as_ref(), settled, true);
                    if (s, fin) != (f.status, f.is_final) {
                        f.status = s;
                        f.is_final = fin;
                        let (step, owner) = (f.step.clone(), f.owner.clone());
                        items.push(FeedItem::Settled { change: t, status: s, is_final: fin, step, owner });
                    }
                }
            }
            if let Some(frame) = ff {
                items.push(FeedItem::Final { frame });
            }
        }
        Self::evict(&mut g);
        self.push_all(items);
        drop(g);
        self.changed.notify_all();
    }

    /// Records a final cut's decision; the feed item for it, if the entry is known here (stepped or tracked) and
    /// its fate changed.
    fn settle(g: &mut Inner, t: TxId, included: bool) -> Option<FeedItem> {
        if g.settled.insert(t, included).is_none() {
            g.settled_order.push_back(t);
        }
        let known = g.coords.contains_key(&t);
        let tracked = g.coords.get(&t).and_then(|c| c.owner.clone());
        let ff = g.final_frame;
        match g.fates.get_mut(&t) {
            Some(f) => {
                let (s, fin) = status(f.outcome.as_ref(), Some(included), passed(ff, f.frame));
                let wait_at = f.frame.filter(|_| included && !passed(ff, f.frame));
                let item = ((s, fin) != (f.status, f.is_final)).then(|| {
                    f.status = s;
                    f.is_final = fin;
                    let owner = f.owner.clone().or(tracked);
                    FeedItem::Settled { change: t, status: s, is_final: fin, step: f.step.clone(), owner }
                });
                if let Some(frame) = wait_at {
                    g.awaiting.insert((frame, t));
                }
                item
            }
            // Included but not stepped here: nothing to tell until its step comes.
            None if known && !included => {
                let (s, fin) = status(None, Some(included), false);
                Some(FeedItem::Settled { change: t, status: s, is_final: fin, step: None, owner: tracked })
            }
            None => None,
        }
    }

    /// pe-app, every time a cut becomes final: the cut, and the entries it voided (agreement's `Voided` events
    /// naming this cut). One call, so an entry the cut includes and voids is never shown final.
    pub fn confirm_cut(&self, cut: &Cut, voided: &[EntryRef]) {
        let mut g = self.lock();
        let mut items = vec![];
        for e in voided {
            items.extend(Self::settle(&mut g, TxId::from(*e), false));
        }
        let voided: HashSet<TxId> = voided.iter().map(|e| TxId::from(*e)).collect();
        let decided: Vec<TxId> = g
            .coords
            .iter()
            .filter(|(t, c)| c.stamp < cut.at.frame.0 && !voided.contains(t) && cut.includes(&c.chain, c.seq))
            .map(|(t, _)| *t)
            .collect();
        for t in decided {
            if !g.settled.contains_key(&t) {
                items.extend(Self::settle(&mut g, t, true));
            }
            g.coords.remove(&t);
        }
        for t in &voided {
            g.coords.remove(t);
        }
        Self::evict(&mut g);
        self.push_all(items);
        drop(g);
        self.changed.notify_all();
    }

    /// Over capacity: settled entries go first, in the order they settled; only then the oldest unsettled. No
    /// scan: `order` and `settled_order` are cleaned lazily.
    fn evict(g: &mut Inner) {
        while g.fates.len() > CAPACITY {
            let victim = loop {
                if let Some(t) = g.settled_order.pop_front() {
                    if g.fates.contains_key(&t) {
                        break Some(t);
                    }
                    g.settled.remove(&t);
                    continue;
                }
                match g.order.pop_front() {
                    Some(t) if g.fates.contains_key(&t) => break Some(t),
                    Some(_) => continue,
                    None => break None,
                }
            };
            let Some(t) = victim else { break };
            g.fates.remove(&t);
            g.settled.remove(&t);
            g.coords.remove(&t);
        }
        if g.order.len() > 2 * CAPACITY {
            let fates = &g.fates;
            g.order.retain(|t| fates.contains_key(t));
        }
        // Settlements and coordinates of entries never stepped here stay bounded too.
        if g.settled_order.len() > 2 * CAPACITY {
            let old = g.settled_order.len() - CAPACITY;
            for t in g.settled_order.drain(..old).collect::<Vec<_>>() {
                if !g.fates.contains_key(&t) {
                    g.settled.remove(&t);
                }
            }
        }
        if g.coords.len() > 2 * CAPACITY {
            let fates = &g.fates;
            g.coords.retain(|t, _| fates.contains_key(t));
        }
        if g.awaiting.len() > 2 * CAPACITY {
            let fates = &g.fates;
            g.awaiting.retain(|(fr, t)| fates.get(t).is_some_and(|f| f.frame == Some(*fr)));
        }
    }

    /// Every change stepped here that the log still holds, with its newest step (for `history`).
    pub fn stepped(&self) -> Vec<Fate> {
        self.lock().fates.values().filter(|f| f.step.is_some()).cloned().collect()
    }

    pub fn get(&self, change: TxId) -> Option<Fate> {
        let g = self.lock();
        Self::fate(&g, change)
    }

    /// Its fate, or, if a final cut settled it without its being stepped here, that settlement.
    fn fate(g: &Inner, change: TxId) -> Option<Fate> {
        g.fates.get(&change).cloned().or_else(|| {
            g.settled.get(&change).map(|inc| {
                let (status, is_final) = status(None, Some(*inc), false);
                Fate { change, frame: None, status, is_final, outcome: None, step: None, owner: None }
            })
        })
    }

    /// Waits until `change` reaches `until` (`Local`: stepped here, accepted or refused; `Final`: final, dropped,
    /// unknown, or a final refusal), or `deadline`. Returns its newest fate, if it has one by then.
    pub fn wait(&self, change: TxId, until: Wait, deadline: Instant) -> Option<Fate> {
        let mut g = self.lock();
        loop {
            let f = Self::fate(&g, change);
            let done = match (&f, until) {
                (None, _) => false,
                (Some(f), Wait::None) | (Some(f), Wait::Local) => {
                    f.frame.is_some() || f.is_final || f.status == Status::Final
                }
                (Some(f), Wait::Final) => {
                    matches!(f.status, Status::Final | Status::Dropped | Status::Unknown)
                        || (f.status == Status::Refused && f.is_final)
                }
            };
            let now = Instant::now();
            if done || now >= deadline {
                return f;
            }
            g = self.changed.wait_timeout(g, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_core::{Frame as CoreFrame, Hash, LineId, MemberKey};
    use pe_seams::{ChainHead, Class, CutRef, Reason};
    use std::sync::Arc;
    use std::time::Duration;

    const CHAIN: ChainId = ChainId(Hash([5; 32]));

    fn entry(e: u8) -> EntryRef {
        EntryRef(Hash([e; 32]))
    }
    fn step(e: u8, frame: u64, applied: bool) -> StepRecord {
        StepRecord {
            entry: entry(e),
            author: MemberKey([1; 32]),
            class: Class::Edit,
            frame: CoreFrame(frame),
            outcome: if applied {
                WorldOutcome::Applied { created: vec![], overwrote: vec![], notes: vec![], inverse: None }
            } else {
                WorldOutcome::Refused { op: Some(0), reason: Reason::NoPermission }
            },
        }
    }
    fn tx(e: u8) -> TxId {
        TxId::from(entry(e))
    }
    /// A final cut closing `frame` that includes this chain up to `seq`.
    fn cut(frame: u64, seq: Option<u64>) -> Cut {
        let heads = seq
            .map(|s| vec![ChainHead { chain: CHAIN, author: MemberKey([1; 32]), seq: s, entry: entry(0) }])
            .unwrap_or_default();
        Cut { at: CutRef { line: LineId(Hash([0; 32])), height: 1, frame: CoreFrame(frame) }, heads }
    }

    #[test]
    fn a_final_cut_decides_final_pending_and_dropped() {
        let log = ChangeLog::new();
        log.record(&[step(1, 10, true), step(2, 10, true), step(3, 10, false)], Some(5));
        for (e, seq) in [(1, 1), (2, 2), (3, 3)] {
            log.track(tx(e), CHAIN, seq, 10, None);
        }
        // The final frame passing them decides nothing.
        log.record(&[], Some(20));
        assert_eq!(log.get(tx(1)).unwrap().status, Status::Accepted);
        // A cut above their stamp that includes seq 1 and 3, and voids 2.
        log.confirm_cut(&cut(12, Some(3)), &[entry(2)]);
        // The final frame (20) is past their step (10), so the cut's inclusion is final.
        assert_eq!(log.get(tx(1)).unwrap().status, Status::Final);
        let d = log.get(tx(2)).unwrap();
        assert_eq!((d.status, d.is_final), (Status::Dropped, true));
        let r = log.get(tx(3)).unwrap();
        assert_eq!((r.status, r.is_final), (Status::Refused, true));
        // Settled stays settled across a later record.
        log.record(&[step(1, 10, true)], None);
        assert_eq!(log.get(tx(1)).unwrap().status, Status::Final);
    }

    #[test]
    fn not_included_is_pending_and_a_cut_below_the_stamp_decides_nothing() {
        let log = ChangeLog::new();
        log.record(&[step(4, 10, true)], Some(5));
        log.track(tx(4), CHAIN, 7, 10, None);
        // A cut below its stamp: not for this change.
        log.confirm_cut(&cut(10, Some(9)), &[]);
        assert_eq!(log.get(tx(4)).unwrap().status, Status::Accepted);
        // A cut above it that doesn't include it: pending, still accepted.
        log.confirm_cut(&cut(11, Some(6)), &[]);
        assert_eq!(log.get(tx(4)).unwrap().status, Status::Accepted);
        // A later cut voids it as late.
        log.confirm_cut(&cut(12, Some(7)), &[entry(4)]);
        assert_eq!(log.get(tx(4)).unwrap().status, Status::Dropped);
    }

    // chunks-reviewer's Low 1: a change settled without being stepped here answers its waiter at once.
    #[test]
    fn a_waiter_learns_a_settlement_of_a_change_never_stepped_here() {
        let log = ChangeLog::new();
        log.confirm_cut(&cut(12, None), &[entry(8)]);
        let f = log.wait(tx(8), Wait::Final, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!((f.status, f.frame, f.outcome), (Status::Dropped, None, None));
    }

    #[test]
    fn waiting_for_local_and_final() {
        let log = Arc::new(ChangeLog::new());
        log.track(tx(7), CHAIN, 1, 3, None);
        let l = log.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            l.record(&[step(7, 3, true)], Some(1));
            std::thread::sleep(Duration::from_millis(20));
            l.confirm_cut(&cut(4, Some(1)), &[]);
            l.record(&[], Some(4));
        });
        let local = log.wait(tx(7), Wait::Local, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(local.status, Status::Accepted);
        let fin = log.wait(tx(7), Wait::Final, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(fin.status, Status::Final);
        t.join().unwrap();
        assert_eq!(log.wait(tx(9), Wait::Local, Instant::now() + Duration::from_millis(10)), None);
    }

    fn repaired(e: u8, frame: u64, at: u64) -> StepRecord {
        let repaired_at = CutRef { line: LineId(Hash([0; 32])), height: 2, frame: CoreFrame(at) };
        StepRecord { outcome: WorldOutcome::Unknown { repaired_at }, ..step(e, frame, true) }
    }
    fn st(log: &ChangeLog, e: u8) -> (Status, bool) {
        let f = log.get(tx(e)).unwrap();
        (f.status, f.is_final)
    }

    // A cut's inclusion is final only once the view's final frame passes the step: before that, this machine's
    // verdict may be one stepped on a diverged state (S5 v1.5; coordinator).
    #[test]
    fn final_waits_for_the_final_frame_to_pass_the_step() {
        let log = ChangeLog::new();
        log.track(tx(1), CHAIN, 1, 10, None);
        log.track(tx(2), CHAIN, 2, 10, None);
        log.record(&[step(1, 10, true), step(2, 10, false)], Some(5));
        log.confirm_cut(&cut(11, Some(2)), &[]);
        assert_eq!((st(&log, 1), st(&log, 2)), ((Status::Accepted, false), (Status::Refused, false)));
        // A final frame at the step's own frame doesn't pass it: a cut at f covers the entries stamped below f.
        log.record(&[], Some(10));
        assert_eq!((st(&log, 1), st(&log, 2)), ((Status::Accepted, false), (Status::Refused, false)));
        log.record(&[], Some(11));
        assert_eq!((st(&log, 1), st(&log, 2)), ((Status::Final, false), (Status::Refused, true)));
        let g = log.lock();
        assert!(g.awaiting.is_empty());
    }

    #[test]
    fn a_repaired_entry_is_unknown_for_good_and_ends_a_wait() {
        let log = ChangeLog::new();
        log.track(tx(3), CHAIN, 1, 10, None);
        log.publish(&[step(3, 10, true)], Some(5), Health::Agreed);
        let since = Health::Repairing { since: CoreFrame(8) };
        log.publish(&[], Some(5), since);
        assert_eq!(log.health(), since);
        // The quorum includes it, but this machine's final frame waits at its last agreed cut.
        log.confirm_cut(&cut(11, Some(1)), &[]);
        assert_eq!(st(&log, 3), (Status::Accepted, false));
        log.publish(&[repaired(3, 10, 12)], Some(12), Health::Agreed);
        let f = log.wait(tx(3), Wait::Final, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!((f.status, f.is_final), (Status::Unknown, true));
        assert_eq!(log.health(), Health::Agreed);
        log.record(&[], Some(20));
        assert_eq!(st(&log, 3), (Status::Unknown, true));
    }

    // chunks-reviewer's Low 2: with nothing settled, eviction takes the oldest without scanning, and stays bounded.
    #[test]
    fn eviction_is_bounded_with_nothing_settled() {
        let log = ChangeLog::new();
        for i in 0..(CAPACITY + 50) as u32 {
            let mut h = [0u8; 32];
            h[..4].copy_from_slice(&i.to_le_bytes());
            let s = StepRecord { entry: EntryRef(Hash(h)), ..step(0, 1, true) };
            log.record(&[s], None);
        }
        let g = log.lock();
        assert_eq!(g.fates.len(), CAPACITY);
        assert!(g.order.len() <= 2 * CAPACITY);
    }
}

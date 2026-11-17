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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

use pe_core::{ChainId, EntryRef, LineId, MemberKey};
use pe_seams::world_api::{Frame, HeldBatch, SessionEvent, SessionHealth, Status, TxId, Wait};
use pe_seams::{Cut, Health, Outcome as WorldOutcome, StepRecord, VoidReason};
use pe_world::WorldSnapshot;
use pe_world::edit::Transaction;

use crate::queue::Submission;
use crate::resend::{self, BOUND, Carrier, Later, Map, Slots, backoff};
use crate::stream::{Retention, Stream};

/// The most entries remembered [I].
pub const CAPACITY: usize = 10_000;
/// The most bytes of this machine's own transactions kept for the re-send backstop, and separately of held
/// batches (api.md 9.4) [I].
pub const KEPT_BYTES: usize = 64 << 20;
/// The most held batches kept (api.md 9.4) [I].
pub const HELD_BATCHES: usize = 64;

/// What pe-api keeps of one of its own changes, to re-send it (api.md 9.4). Shared, not copied.
#[derive(Clone, Debug)]
pub struct Kept {
    pub tx: Arc<Transaction>,
    /// Its `ObjectRef`s and fields, from the schema at submission.
    pub slots: Arc<Slots>,
    /// The line and author its created ids derive from.
    pub line: LineId,
    pub author: MemberKey,
    /// When `apply` first submitted it: the one-minute bound counts from here.
    pub first: Instant,
}

/// Why a change of this machine's ended `dropped`, beyond a void (api.md 9.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropNote {
    /// The author's later changes had replaced all of it, so nothing was re-sent.
    Superseded,
    /// Re-sending stopped at the one-minute bound; it is in this held batch.
    Held(u64),
}

/// A held batch's handle, as the API shows it: opaque, `"h:"` and 16 hex digits.
pub fn handle(b: u64) -> String {
    format!("h:{b:016x}")
}

fn parse_handle(h: &str) -> Option<u64> {
    let x = h.strip_prefix("h:")?;
    let ok = x.len() == 16 && x.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c));
    if ok { u64::from_str_radix(x, 16).ok() } else { None }
}

/// What the simulation thread takes from the backstop ahead of a fresh change (api.md 9.4).
#[derive(Clone, Debug)]
pub enum Taken {
    /// A re-send of change `origin`, prepared; `op_back`: each op's index in the original.
    Resend { origin: TxId, sub: Submission, op_back: Arc<[u32]> },
    /// A re-point follow-up: tracked for its cut, reported nowhere in `mine`.
    FollowUp(Submission),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Its newest carrier is in agreement, or settled.
    Live,
    /// To be re-sent from this instant.
    Due(Instant),
    /// Taken by the simulation thread; its receipt is on the way.
    Taken,
    /// In a held batch.
    Held(u64),
    /// Discarded, or let go.
    Gone,
}

/// One of this machine's own changes, while kept.
struct Own {
    kept: Kept,
    owner: Owner,
    /// Submission order on this machine (the chain's seq order).
    n: u64,
    /// The entries that carried it, oldest first: the original, then each re-send.
    carriers: Vec<Carrier>,
    phase: Phase,
    /// The backstop has it: `mine` hears nothing until its one final notice.
    open: bool,
    /// Re-sends taken so far.
    tries: u32,
    /// Each carrier's ops back to the original's, beside `carriers` (`None` for the original).
    backs: Vec<Option<Arc<[u32]>>>,
    /// Its final notice, once the backstop gave it.
    closed: Option<Fate>,
    /// An accepted dependent already given its re-point follow-up.
    repointed: bool,
    bytes: usize,
}

impl Own {
    /// The owner, with the newest carrier's ops mapped back to the API's.
    fn owner_now(&self) -> Owner {
        match self.backs.last().and_then(|b| b.as_ref()) {
            None => self.owner.clone(),
            Some(back) => Owner {
                op_of: back.iter().map(|&i| self.owner.op_of.get(i as usize).copied().unwrap_or(i)).collect(),
                ..self.owner.clone()
            },
        }
    }

    fn last(&self) -> TxId {
        TxId::from(self.carriers.last().expect("a carrier").entry)
    }

    /// Re-sent at least once, or in the backstop's hands: its notices go under its original id.
    fn aliased(&self) -> bool {
        self.open || self.carriers.len() > 1 || matches!(self.phase, Phase::Held(_) | Phase::Gone)
    }
}

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
    /// `resent_as`: the entry now carrying a change the backstop re-sent (api.md 9.4); `note`: why it is dropped.
    Settled {
        change: TxId,
        status: Status,
        is_final: bool,
        step: Option<StepRecord>,
        owner: Option<Owner>,
        resent_as: Option<TxId>,
        note: Option<DropNote>,
    },
    /// The newest final frame moved.
    Final { frame: Frame },
    /// Session news: from pe-app, membership taking effect, catching up and caught up; from the view, its health
    /// changing (api.md 5.4 `session`).
    Session(pe_seams::world_api::SessionEvent),
    /// The size limit let a held batch go: to each principal with changes in it, its `held_discarded` with
    /// `count` being theirs (api.md 9.4).
    HeldDiscarded { handle: String, counts: Vec<(Arc<str>, u32)> },
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
    /// The entry now carrying it, when the backstop re-sent it (api.md 9.4).
    pub resent_as: Option<TxId>,
    /// Why it is dropped, beyond a void.
    pub note: Option<DropNote>,
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
    /// This machine's own changes, kept for the backstop (api.md 9.4), by their original id.
    own: HashMap<TxId, Own>,
    /// The same, in submission order.
    own_order: BTreeMap<u64, TxId>,
    /// Every carrier of a kept change, the original included, to its original id.
    carrier_of: HashMap<TxId, TxId>,
    next_n: u64,
    kept_bytes: usize,
    /// Held batches, by handle: their changes in order.
    batches: BTreeMap<u64, Vec<TxId>>,
    held_bytes: usize,
    /// The batch an outage's changes join while the backstop has re-sends waiting.
    open_batch: Option<u64>,
    next_batch: u64,
    /// Accepted dependents waiting for their re-point follow-up.
    follow_ups: VecDeque<TxId>,
    /// Kept changes the backstop has open.
    open_count: usize,
}

pub struct ChangeLog {
    /// The newest published snapshot, for preparing a re-send (set by `Api::new`).
    latest: OnceLock<Box<dyn Fn() -> Arc<WorldSnapshot> + Send + Sync>>,
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
                own: HashMap::new(),
                own_order: BTreeMap::new(),
                carrier_of: HashMap::new(),
                next_n: 0,
                kept_bytes: 0,
                batches: BTreeMap::new(),
                held_bytes: 0,
                open_batch: None,
                next_batch: epoch() << 16,
                follow_ups: VecDeque::new(),
                open_count: 0,
            }),
            latest: OnceLock::new(),
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
        let now = Instant::now();
        let mut g = self.lock();
        let mut items = Vec::with_capacity(steps.len() + 2);
        // The record's late voids first, so a dependent it refuses joins its re-send before `mine` could report it
        // (api.md 9.4; @design-reviewer's L3).
        for s in steps {
            if let WorldOutcome::Dropped { reason: VoidReason::Late } = s.outcome {
                g.void(TxId::from(s.entry), now, &mut items);
            }
        }
        let mut names: Option<Map> = None;
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
            // A change of this machine's refused because it names what the backstop re-sends joins it (9.4).
            let backstop = g.open_count > 0 || !g.batches.is_empty();
            if matches!(s.outcome, WorldOutcome::Refused { .. }) && backstop && g.may_join(change) {
                let m = names.get_or_insert_with(|| g.old_names(None, false));
                let w = &g.own[&change];
                if resend::names(&w.kept.tx, &w.kept.slots, m).is_some() {
                    g.join(change, now, &mut items);
                    let w = &g.own[&change];
                    for c in &w.carriers {
                        m.name(w.kept.line, w.kept.author, &w.kept.tx, c);
                    }
                }
            }
            let f = Fate {
                change,
                frame: Some(s.frame.0),
                status,
                is_final,
                outcome: Some(s.outcome.clone()),
                step: Some(s.clone()),
                owner: owner.clone(),
                resent_as: None,
                note: None,
            };
            if g.fates.insert(change, f).is_none() {
                g.order.push_back(change);
            }
            if !same {
                let owner = g.loud(change, owner);
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
                        let owner = g.loud(t, owner);
                        items.push(FeedItem::Settled {
                            change: t,
                            status: s,
                            is_final: fin,
                            step,
                            owner,
                            resent_as: None,
                            note: None,
                        });
                    }
                }
            }
            if let Some(frame) = ff {
                items.push(FeedItem::Final { frame });
            }
        }
        g.sweep(now, &mut items);
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
        let quiet = g.quiet(t);
        let tracked = g.coords.get(&t).and_then(|c| c.owner.clone()).filter(|_| !quiet);
        let ff = g.final_frame;
        match g.fates.get_mut(&t) {
            Some(f) => {
                let (s, fin) = status(f.outcome.as_ref(), Some(included), passed(ff, f.frame));
                let wait_at = f.frame.filter(|_| included && !passed(ff, f.frame));
                let item = ((s, fin) != (f.status, f.is_final)).then(|| {
                    f.status = s;
                    f.is_final = fin;
                    let owner = f.owner.clone().filter(|_| !quiet).or(tracked);
                    FeedItem::Settled {
                        change: t,
                        status: s,
                        is_final: fin,
                        step: f.step.clone(),
                        owner,
                        resent_as: None,
                        note: None,
                    }
                });
                if let Some(frame) = wait_at {
                    g.awaiting.insert((frame, t));
                }
                item
            }
            // Included but not stepped here: nothing to tell until its step comes.
            None if known && !included => {
                let (s, fin) = status(None, Some(included), false);
                Some(FeedItem::Settled {
                    change: t,
                    status: s,
                    is_final: fin,
                    step: None,
                    owner: tracked,
                    resent_as: None,
                    note: None,
                })
            }
            None => None,
        }
    }

    /// pe-app, every time a cut becomes final: the cut, and the entries it voided with their reasons (agreement's
    /// `Voided` events naming this cut). One call, so an entry the cut includes and voids is never shown final. A
    /// change of this machine's API void as late goes to the re-send backstop (api.md 9.4).
    pub fn confirm_cut(&self, cut: &Cut, voided: &[(EntryRef, VoidReason)]) {
        let now = Instant::now();
        let mut g = self.lock();
        let mut items = vec![];
        for (e, why) in voided {
            if *why == VoidReason::Late {
                g.void(TxId::from(*e), now, &mut items);
            }
        }
        for (e, _) in voided {
            items.extend(Self::settle(&mut g, TxId::from(*e), false));
        }
        let voided: HashSet<TxId> = voided.iter().map(|(e, _)| TxId::from(*e)).collect();
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
        g.sweep(now, &mut items);
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

    /// Every change stepped here that the log still holds, with its newest step (for `history`). A change the
    /// backstop re-sent is listed once, under its original id, with its newest carrier's status (api.md 9.4).
    pub fn stepped(&self) -> Vec<Fate> {
        let g = self.lock();
        g.fates
            .values()
            .filter(|f| f.step.is_some())
            .filter_map(|f| match g.carrier_of.get(&f.change) {
                Some(o) if *o != f.change => None,
                Some(o) if g.own.get(o).is_some_and(Own::aliased) => {
                    let mut a = Self::fate(&g, *o)?;
                    if a.step.is_none() {
                        a.step = f.step.clone();
                        a.frame = f.frame;
                    }
                    Some(a)
                }
                _ => Some(f.clone()),
            })
            .collect()
    }

    pub fn get(&self, change: TxId) -> Option<Fate> {
        let g = self.lock();
        Self::fate(&g, change)
    }

    /// Its fate, or, if a final cut settled it without its being stepped here, that settlement. A change the
    /// backstop has answers for its newest carrier, under its own id (api.md 9.4).
    fn fate(g: &Inner, change: TxId) -> Option<Fate> {
        if let Some(w) = g.own.get(&change)
            && w.aliased()
        {
            if !w.open
                && let Some(f) = &w.closed
            {
                return Some(f.clone());
            }
            let last = w.last();
            let resent_as = (last != change).then_some(last);
            let owner = Some(w.owner_now());
            if w.phase == Phase::Live
                && last != change
                && let Some(f) = g.fates.get(&last)
            {
                return Some(Fate { change, resent_as, owner, note: None, ..f.clone() });
            }
            return Some(Fate {
                change,
                frame: None,
                status: Status::Queued,
                is_final: false,
                outcome: None,
                step: None,
                owner,
                resent_as,
                note: None,
            });
        }
        g.fates.get(&change).cloned().or_else(|| {
            g.settled.get(&change).map(|inc| {
                let (status, is_final) = status(None, Some(*inc), false);
                Fate {
                    change,
                    frame: None,
                    status,
                    is_final,
                    outcome: None,
                    step: None,
                    owner: None,
                    resent_as: None,
                    note: None,
                }
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

// ---- the re-send backstop (api.md 9.4) ------------------------------------------------------------------------------

impl Inner {
    /// The original id of a kept change's carrier.
    fn origin(&self, t: TxId) -> Option<TxId> {
        self.carrier_of.get(&t).copied()
    }

    /// Whether `mine` hears nothing of entry `t` itself: a re-send's carrier, or a change the backstop has, whose
    /// one notice goes under its original id.
    fn quiet(&self, t: TxId) -> bool {
        self.origin(t).and_then(|o| self.own.get(&o).map(|w| t != o || w.aliased())).unwrap_or(false)
    }

    /// `owner`, unless `t` is quiet.
    fn loud(&self, t: TxId, owner: Option<Owner>) -> Option<Owner> {
        if self.quiet(t) { None } else { owner }
    }

    fn set_open(&mut self, t: TxId, open: bool) {
        if let Some(w) = self.own.get_mut(&t)
            && w.open != open
        {
            w.open = open;
            if open {
                self.open_count += 1;
            } else {
                self.open_count -= 1;
            }
        }
    }

    /// A kept change that could join a re-send: not the backstop's already, still live, not code.
    fn may_join(&self, t: TxId) -> bool {
        self.own.get(&t).is_some_and(|w| !w.aliased() && w.phase == Phase::Live && !resend::is_code(&w.kept.tx))
    }

    /// A late void of entry `t`. If it carries a change of this machine's API, the backstop takes it: re-sent after
    /// its back-off, or held past the bound. Returns whether it did.
    fn void(&mut self, t: TxId, now: Instant, items: &mut Vec<FeedItem>) -> bool {
        let Some(o) = self.origin(t) else { return false };
        let Some(w) = self.own.get(&o) else { return false };
        if w.last() != t || w.phase != Phase::Live || resend::is_code(&w.kept.tx) {
            return false;
        }
        let past = now >= w.kept.first + BOUND;
        let wait = backoff(w.tries + 1);
        self.set_open(o, true);
        let w = self.own.get_mut(&o).expect("kept");
        w.closed = None;
        w.phase = Phase::Due(now + wait);
        if past {
            self.hold(o, items);
        }
        true
    }

    /// A refused dependent joins the re-sends, after the ones before it; one of a held change goes into its
    /// batch (coordinator (c)).
    fn join(&mut self, t: TxId, now: Instant, items: &mut Vec<FeedItem>) {
        let Some(w) = self.own.get(&t) else { return };
        let past = now >= w.kept.first + BOUND;
        let batch = self.held_batch_named(t);
        self.set_open(t, true);
        let w = self.own.get_mut(&t).expect("kept");
        w.closed = None;
        w.phase = Phase::Due(now);
        if batch.is_some() || past {
            self.hold_in(t, batch, items);
        }
    }

    /// The batch of a held change before `t` that `t` names, if any.
    fn held_batch_named(&self, t: TxId) -> Option<u64> {
        let w = self.own.get(&t)?;
        self.own_order.range(..w.n).find_map(|(_, p)| {
            let p = &self.own[p];
            let Phase::Held(b) = p.phase else { return None };
            let mut m = Map::default();
            for c in &p.carriers {
                m.name(p.kept.line, p.kept.author, &p.kept.tx, c);
            }
            resend::names(&w.kept.tx, &w.kept.slots, &m).map(|_| b)
        })
    }

    /// Re-sending `o` stops: it, and the re-sends waiting behind it, go into the open held batch, each with its one
    /// notice (api.md 9.4; coordinator (c)).
    fn hold(&mut self, o: TxId, items: &mut Vec<FeedItem>) {
        self.hold_in(o, None, items);
    }

    /// [`Inner::hold`], into batch `into` if given (a dependent joining its held predecessor).
    fn hold_in(&mut self, o: TxId, into: Option<u64>, items: &mut Vec<FeedItem>) {
        let Some(n) = self.own.get(&o).map(|w| w.n) else { return };
        let b = match into.filter(|b| self.batches.contains_key(b)).or(self.open_batch) {
            Some(b) if self.batches.contains_key(&b) => b,
            _ => {
                let b = self.next_batch;
                self.next_batch = self.next_batch.wrapping_add(1);
                self.batches.insert(b, vec![]);
                self.open_batch = Some(b);
                b
            }
        };
        // A dependent joining its held predecessor goes alone; a change that stops at the bound takes with it the
        // re-sends waiting behind it.
        let held: Vec<TxId> = if into.is_some() {
            vec![o]
        } else {
            self.own_order
                .range(n..)
                .map(|(_, t)| *t)
                .filter(|t| *t == o || self.own.get(t).is_some_and(|w| matches!(w.phase, Phase::Due(_))))
                .collect()
        };
        for t in held {
            self.set_open(t, false);
            let w = self.own.get_mut(&t).expect("kept");
            w.phase = Phase::Held(b);
            self.kept_bytes -= w.bytes;
            self.held_bytes += w.bytes;
            let last = w.last();
            let fate = Fate {
                change: t,
                frame: None,
                status: Status::Dropped,
                is_final: true,
                outcome: None,
                step: None,
                owner: Some(w.owner_now()),
                resent_as: (last != t).then_some(last),
                note: Some(DropNote::Held(b)),
            };
            w.closed = Some(fate.clone());
            items.push(FeedItem::Settled {
                change: t,
                status: Status::Dropped,
                is_final: true,
                step: None,
                owner: fate.owner,
                resent_as: fate.resent_as,
                note: fate.note,
            });
            self.batches.get_mut(&b).expect("open batch").push(t);
        }
        // Past the size limit the oldest batch goes, with a session event.
        while self.held_bytes > KEPT_BYTES || self.batches.len() > HELD_BATCHES {
            let Some((&b, _)) = self.batches.iter().next() else { break };
            let members = self.batches.remove(&b).unwrap_or_default();
            if self.open_batch == Some(b) {
                self.open_batch = None;
            }
            let mut counts: Vec<(Arc<str>, u32)> = vec![];
            for t in members {
                if let Some(w) = self.own.get(&t) {
                    match counts.iter_mut().find(|(p, _)| *p == w.owner.principal) {
                        Some((_, n)) => *n += 1,
                        None => counts.push((w.owner.principal.clone(), 1)),
                    }
                }
                self.let_go(t);
            }
            items.push(FeedItem::HeldDiscarded { handle: handle(b), counts });
        }
    }

    /// A held change discarded: its `dropped` stands, with no batch to act on.
    fn let_go(&mut self, t: TxId) {
        if let Some(w) = self.own.get_mut(&t)
            && let Phase::Held(_) = w.phase
        {
            self.held_bytes -= w.bytes;
            w.phase = Phase::Gone;
            if let Some(f) = &mut w.closed {
                f.note = None;
            }
        }
    }

    /// The backstop's change `o` ends with its newest carrier's fate `f`: its one notice (api.md 9.4).
    fn close(&mut self, o: TxId, f: Fate, items: &mut Vec<FeedItem>) {
        self.set_open(o, false);
        let Some(w) = self.own.get_mut(&o) else { return };
        let last = w.last();
        // A dropped carrier's retraction was reported with it; this notice is the change's status only.
        let step = f.step.filter(|_| f.status != Status::Dropped);
        let fate = Fate {
            change: o,
            step,
            owner: Some(w.owner_now()),
            resent_as: (last != o).then_some(last),
            note: f.note,
            ..f
        };
        w.closed = Some(fate.clone());
        items.push(FeedItem::Settled {
            change: o,
            status: fate.status,
            is_final: fate.is_final,
            step: fate.step,
            owner: fate.owner,
            resent_as: fate.resent_as,
            note: fate.note,
        });
    }

    /// The names of kept changes the backstop re-sent or has: from each carrier no longer current to the current
    /// one (`carry`), or to itself while it waits (`name`; only with `!carry_only`). Only changes before `before`,
    /// in submission order, if given.
    fn old_names(&self, before: Option<u64>, carry_only: bool) -> Map {
        let mut m = Map::default();
        let end = before.unwrap_or(u64::MAX);
        for t in self.own_order.range(..end).map(|(_, t)| *t) {
            let w = &self.own[&t];
            if !w.aliased() {
                continue;
            }
            let (k, line, author) = (&w.kept, w.kept.line, w.kept.author);
            if w.phase == Phase::Live {
                let last = w.carriers.len() - 1;
                let (now, to_back) = (&w.carriers[last], w.backs[last].as_deref());
                for (c, back) in w.carriers[..last].iter().zip(&w.backs) {
                    m.carry((line, author, &k.tx), c, back.as_deref(), now, to_back);
                }
            } else if !carry_only {
                for c in &w.carriers {
                    m.name(line, author, &k.tx, c);
                }
            }
        }
        m
    }

    /// Whether kept change `o` names one before it that waits to be re-sent or is held; with `strong`, only in a
    /// target, a precondition, an `Undelete` or a script's base.
    fn names_waiting(&self, o: TxId, strong: bool) -> bool {
        let Some(w) = self.own.get(&o) else { return false };
        let mut m = Map::default();
        for t in self.own_order.range(..w.n).map(|(_, t)| *t) {
            let p = &self.own[&t];
            if matches!(p.phase, Phase::Due(_) | Phase::Taken | Phase::Held(_)) {
                for c in &p.carriers {
                    m.name(p.kept.line, p.kept.author, &p.kept.tx, c);
                }
            }
        }
        !m.is_empty() && resend::names(&w.kept.tx, &w.kept.slots, &m).is_some_and(|s| s || !strong)
    }

    /// After every record and cut: dependents found by content join, accepted ones that stored a reference get
    /// their follow-up, and a change whose newest carrier reached its end gets its one notice.
    fn sweep(&mut self, now: Instant, items: &mut Vec<FeedItem>) {
        if self.open_count == 0 {
            self.open_batch = None;
            if self.batches.is_empty() {
                return;
            }
        }
        let mut m = self.old_names(None, false);
        let first = self
            .own
            .values()
            .filter(|w| w.open || matches!(w.phase, Phase::Held(_)))
            .map(|w| w.n)
            .min()
            .unwrap_or(u64::MAX);
        let later: Vec<TxId> = self.own_order.range(first..).map(|(_, t)| *t).collect();
        for t in later {
            if !self.may_join(t) {
                continue;
            }
            let w = &self.own[&t];
            let Some(strong) = resend::names(&w.kept.tx, &w.kept.slots, &m) else { continue };
            match self.fates.get(&t).map(|f| (&f.outcome, f.status)) {
                Some((Some(WorldOutcome::Refused { .. }), _)) => {
                    self.join(t, now, items);
                    let w = &self.own[&t];
                    for c in &w.carriers {
                        m.name(w.kept.line, w.kept.author, &w.kept.tx, c);
                    }
                }
                Some((Some(WorldOutcome::Applied { .. }), Status::Accepted | Status::Final)) if !strong => {
                    let w = self.own.get_mut(&t).expect("kept");
                    if !w.repointed {
                        w.repointed = true;
                        self.follow_ups.push_back(t);
                    }
                }
                _ => {}
            }
        }
        let open: Vec<TxId> = self
            .own_order
            .values()
            .copied()
            .filter(|t| self.own.get(t).is_some_and(|w| w.open && w.phase == Phase::Live))
            .collect();
        for o in open {
            let last = self.own[&o].last();
            let Some(f) = ChangeLog::fate_raw(self, last) else { continue };
            // Refused because a change before it went back to waiting: it goes again after that one. Before the
            // refusal is final, only if it names that change where a refusal can't be undone (the void that sent
            // the change back is final, so what its entry made never exists): an `ObjectRef` alone doesn't refuse.
            if f.status == Status::Refused && self.names_waiting(o, !f.is_final) {
                self.own.get_mut(&o).expect("kept").phase = Phase::Due(now);
                continue;
            }
            let end = matches!(f.status, Status::Final | Status::Unknown | Status::Dropped)
                || f.status == Status::Refused && f.is_final;
            if !end {
                continue;
            }
            self.close(o, f, items);
        }
        if !self.own.values().any(|w| w.open) {
            self.open_batch = None;
        }
    }

    /// The oldest kept changes the backstop doesn't have go first, past the count or the memory cap.
    fn trim_own(&mut self) {
        while self.kept_bytes > KEPT_BYTES || self.own.len() > CAPACITY {
            let victim = self
                .own_order
                .values()
                .copied()
                .find(|t| self.own.get(t).is_some_and(|w| !w.open && matches!(w.phase, Phase::Live | Phase::Gone)));
            let Some(t) = victim else { break };
            let w = self.own.remove(&t).expect("kept");
            self.own_order.remove(&w.n);
            for c in &w.carriers {
                self.carrier_of.remove(&TxId::from(c.entry));
            }
            match w.phase {
                Phase::Held(b) => {
                    self.held_bytes -= w.bytes;
                    if let Some(v) = self.batches.get_mut(&b) {
                        v.retain(|x| *x != t);
                    }
                }
                Phase::Gone => {}
                _ => self.kept_bytes -= w.bytes,
            }
            if w.open {
                self.open_count -= 1;
            }
        }
    }

    /// The first kept change that waits to be re-sent, and whether it may go now; a re-point follow-up first when
    /// every change before it has its carrier.
    fn take(&mut self, now: Instant, snap: Option<&WorldSnapshot>, items: &mut Vec<FeedItem>) -> Option<Taken> {
        // The bound: a change still waiting past it is held, with what waits behind it.
        while let Some(t) =
            self.own_order.values().copied().find(|t| {
                self.own.get(t).is_some_and(|w| matches!(w.phase, Phase::Due(_)) && now >= w.kept.first + BOUND)
            })
        {
            self.hold(t, items);
        }
        let first = self
            .own_order
            .values()
            .copied()
            .find(|t| self.own.get(t).is_some_and(|w| matches!(w.phase, Phase::Due(_) | Phase::Taken)));
        let first_n = first.map_or(u64::MAX, |t| self.own[&t].n);
        while let Some(&d) = self.follow_ups.front() {
            match self.own.get(&d) {
                None => {
                    self.follow_ups.pop_front();
                }
                Some(w) if w.n < first_n => {
                    self.follow_ups.pop_front();
                    if let Some(sub) = self.follow_up(d, snap) {
                        return Some(Taken::FollowUp(sub));
                    }
                }
                Some(_) => break,
            }
        }
        let o = first?;
        match self.own[&o].phase {
            Phase::Due(at) if at <= now => {}
            _ => return None,
        }
        let w = &self.own[&o];
        let m = self.old_names(Some(w.n), true);
        let tx = resend::rewrite(&w.kept.tx, &w.kept.slots, &m);
        let (later, mine) = self.later_than(w.n, &m);
        match resend::supersede(&tx, &later) {
            None => {
                // The author's later changes replaced all of it: nothing to send.
                let f = Fate {
                    change: o,
                    frame: None,
                    status: Status::Dropped,
                    is_final: true,
                    outcome: None,
                    step: None,
                    owner: None,
                    resent_as: None,
                    note: Some(DropNote::Superseded),
                };
                self.own.get_mut(&o).expect("kept").phase = Phase::Gone;
                self.kept_bytes -= self.own[&o].bytes;
                self.close(o, f, items);
                None
            }
            Some((mut tx, back)) => {
                if let Some(snap) = snap {
                    resend::move_preconditions(&mut tx, |id| snap.info(id).map(|i| i.version), &mine);
                }
                let w = self.own.get_mut(&o).expect("kept");
                w.phase = Phase::Taken;
                w.tries += 1;
                let base = snap.map_or(0, |s| s.frame().0);
                Some(Taken::Resend { origin: o, sub: Submission { tx, base }, op_back: back.into() })
            }
        }
    }

    /// What this machine's accepted API changes after submission `n` wrote, through `m`, and their entries.
    fn later_than(&self, n: u64, m: &Map) -> (Later, HashSet<EntryRef>) {
        let mut later = Later::default();
        let mut mine = HashSet::new();
        for t in self.own_order.range(n + 1..).map(|(_, t)| *t) {
            let d = &self.own[&t];
            if d.phase != Phase::Live || d.open {
                continue;
            }
            let last = d.last();
            if !self.fates.get(&last).is_some_and(|f| matches!(f.status, Status::Accepted | Status::Final)) {
                continue;
            }
            later.add(&resend::rewrite(&d.kept.tx, &d.kept.slots, m), &d.kept.slots);
            mine.insert(EntryRef::from(last));
        }
        (later, mine)
    }

    /// The re-point follow-up for accepted dependent `d`, if it still has something to re-point.
    fn follow_up(&self, d: TxId, snap: Option<&WorldSnapshot>) -> Option<Submission> {
        let w = self.own.get(&d)?;
        let m = self.old_names(Some(w.n), true);
        let at = *w.carriers.last()?;
        let (line, author) = (w.kept.line, w.kept.author);
        let created = |k| pe_core::ObjectId::created(line, author, at.chain, at.seq, k);
        let (later, _) = self.later_than(w.n, &m);
        let tx = resend::follow_up(&w.kept.tx, &w.kept.slots, &m, created, &later)?;
        Some(Submission { tx, base: snap.map_or(0, |s| s.frame().0) })
    }
}

impl ChangeLog {
    /// `Api::new`: where the backstop reads the newest snapshot when it prepares a re-send (the first given wins).
    pub fn read_with(&self, latest: impl Fn() -> Arc<WorldSnapshot> + Send + Sync + 'static) {
        let _ = self.latest.set(Box::new(latest));
    }

    /// [`ChangeLog::fate`] without the backstop's alias: an entry's own fate.
    fn fate_raw(g: &Inner, change: TxId) -> Option<Fate> {
        g.fates.get(&change).cloned().or_else(|| {
            g.settled.get(&change).map(|inc| {
                let (status, is_final) = status(None, Some(*inc), false);
                Fate {
                    change,
                    frame: None,
                    status,
                    is_final,
                    outcome: None,
                    step: None,
                    owner: None,
                    resent_as: None,
                    note: None,
                }
            })
        })
    }

    /// pe-api, on the receipt of a change `apply` submitted: as [`ChangeLog::track`], and kept for the re-send
    /// backstop (api.md 9.4).
    pub fn track_own(&self, change: TxId, chain: ChainId, seq: u64, stamp: Frame, owner: Owner, kept: Kept) {
        {
            let mut g = self.lock();
            let bytes = resend::bytes(&kept.tx);
            let n = g.next_n;
            g.next_n += 1;
            g.own.insert(
                change,
                Own {
                    kept,
                    owner: owner.clone(),
                    n,
                    carriers: vec![Carrier { entry: change.into(), chain, seq }],
                    phase: Phase::Live,
                    open: false,
                    tries: 0,
                    backs: vec![None],
                    closed: None,
                    repointed: false,
                    bytes,
                },
            );
            g.own_order.insert(n, change);
            g.carrier_of.insert(change, change);
            g.kept_bytes += bytes;
            g.trim_own();
        }
        self.track(change, chain, seq, stamp, Some(owner));
    }

    /// pe-api, on the receipt of a re-send: `carrier`, stamped `stamp`, now carries change `origin`.
    pub fn carried(&self, origin: TxId, carrier: Carrier, stamp: Frame, op_back: Arc<[u32]>) {
        let mut g = self.lock();
        let t = TxId::from(carrier.entry);
        g.coords.insert(t, Coords { chain: carrier.chain, seq: carrier.seq, stamp, owner: None });
        let Some(w) = g.own.get_mut(&origin) else { return };
        w.carriers.push(carrier);
        w.backs.push(Some(op_back));
        w.phase = Phase::Live;
        g.carrier_of.insert(t, origin);
        drop(g);
        self.changed.notify_all();
    }

    /// pe-api, when agreement refused a re-send at submission: try again after the back-off.
    pub fn resend_refused(&self, origin: TxId) {
        let now = Instant::now();
        let mut g = self.lock();
        if let Some(w) = g.own.get_mut(&origin)
            && w.phase == Phase::Taken
        {
            w.phase = Phase::Due(now + backoff(w.tries + 1));
        }
    }

    /// The simulation thread, through the queue's `drain`: a prepared re-send or follow-up, ahead of fresh changes.
    pub fn take(&self, now: Instant) -> Option<Taken> {
        {
            let g = self.lock();
            if g.open_count == 0 && g.follow_ups.is_empty() {
                return None;
            }
        }
        // Read before taking the log's lock: pe-app's snapshot lock is never taken inside it.
        let snap = self.latest.get().map(|f| f());
        let mut g = self.lock();
        let mut items = vec![];
        let r = g.take(now, snap.as_deref(), &mut items);
        self.push_all(items);
        drop(g);
        self.changed.notify_all();
        r
    }

    /// `WorldApi::held`: the caller's own changes in held batch `h`, applied (queued again in order, with a fresh
    /// bound) or discarded. Their original ids, or `None` if the batch holds none of the caller's.
    pub fn held(&self, principal: &str, h: &str, apply: bool) -> Option<Vec<TxId>> {
        let now = Instant::now();
        let mut g = self.lock();
        let b = parse_handle(h)?;
        let members = g.batches.get(&b)?;
        let mine: Vec<TxId> = members
            .iter()
            .copied()
            .filter(|t| g.own.get(t).is_some_and(|w| *w.owner.principal == *principal))
            .collect();
        if mine.is_empty() {
            return None;
        }
        let left = g.batches.get_mut(&b).expect("batch");
        left.retain(|t| !mine.contains(t));
        if left.is_empty() {
            g.batches.remove(&b);
            if g.open_batch == Some(b) {
                g.open_batch = None;
            }
        }
        for t in &mine {
            if apply {
                let w = g.own.get_mut(t).expect("kept");
                w.phase = Phase::Due(now);
                w.tries = 0;
                w.kept.first = now;
                w.closed = None;
                let bytes = w.bytes;
                g.held_bytes -= bytes;
                g.kept_bytes += bytes;
                g.set_open(*t, true);
            } else {
                g.let_go(*t);
            }
        }
        drop(g);
        self.changed.notify_all();
        Some(mine)
    }

    /// The held batches with changes of `principal`'s, for `session`.
    pub fn held_for(&self, principal: &str) -> Vec<HeldBatch> {
        let g = self.lock();
        g.batches
            .iter()
            .filter_map(|(b, members)| {
                let labels: Vec<String> = members
                    .iter()
                    .filter_map(|t| g.own.get(t))
                    .filter(|w| *w.owner.principal == *principal)
                    .map(|w| w.kept.tx.label.clone().unwrap_or_default())
                    .collect();
                (!labels.is_empty()).then(|| HeldBatch { handle: handle(*b), count: labels.len() as u32, labels })
            })
            .collect()
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
        log.confirm_cut(&cut(12, Some(3)), &[(entry(2), VoidReason::Late)]);
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
        log.confirm_cut(&cut(12, Some(7)), &[(entry(4), VoidReason::Late)]);
        assert_eq!(log.get(tx(4)).unwrap().status, Status::Dropped);
    }

    // chunks-reviewer's Low 1: a change settled without being stepped here answers its waiter at once.
    #[test]
    fn a_waiter_learns_a_settlement_of_a_change_never_stepped_here() {
        let log = ChangeLog::new();
        log.confirm_cut(&cut(12, None), &[(entry(8), VoidReason::Late)]);
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

    // ---- the re-send backstop (api.md 9.4) ----

    use crate::resend::{RefSlot, VALUE};
    use pe_core::ObjectId as Oid;
    use pe_sim::ComponentId;
    use pe_world::edit::{Actor, Op, Precondition, Target};

    const LINE: LineId = LineId(Hash([7; 32]));
    const ME: MemberKey = MemberKey([1; 32]);
    const TABLE: Oid = Oid::from_u128(10);
    const LAMP: Oid = Oid::from_u128(11);
    const COLOR: ComponentId = ComponentId(77);

    fn owner() -> Owner {
        Owner { principal: "claude".into(), op_of: Arc::from(vec![0u32, 1, 2]), refs: Arc::from(vec![]) }
    }
    fn txn(ops: Vec<Op>, pre: Vec<Precondition>) -> Transaction {
        Transaction { actor: Actor::Person, label: Some("t".into()), preconditions: pre, ops, blobs: vec![] }
    }
    fn kept_at(t: Transaction, slots: Slots, first: Instant) -> Kept {
        Kept { tx: Arc::new(t), slots: Arc::new(slots), line: LINE, author: ME, first }
    }
    fn created(seq: u64, k: u32) -> Oid {
        Oid::created(LINE, ME, CHAIN, seq, k)
    }
    fn set(id: Oid, field: u16, v: u8) -> Op {
        Op::SetField { id: Target::Existing(id), component: COLOR, instance: 0, field, value: vec![v; 4] }
    }
    fn make_cup() -> Op {
        Op::Create { new: 0, parent: Target::Existing(TABLE), index: None, rows: vec![] }
    }
    /// Submits `t` as this machine's own change `e` at `seq`, stepped at frame `seq` with `outcome`.
    fn own(log: &ChangeLog, e: u8, seq: u64, k: Kept, outcome: WorldOutcome) {
        log.track_own(tx(e), CHAIN, seq, seq, owner(), k);
        log.record(&[StepRecord { outcome, ..step(e, seq, true) }], None);
    }
    fn ok(created: Vec<Oid>) -> WorldOutcome {
        WorldOutcome::Applied { created, overwrote: vec![], notes: vec![], inverse: None }
    }
    fn resend(log: &ChangeLog, now: Instant) -> (TxId, Transaction, Arc<[u32]>) {
        match log.take(now) {
            Some(Taken::Resend { origin, sub, op_back }) => (origin, sub.tx, op_back),
            other => panic!("expected a re-send, got {other:?}"),
        }
    }
    fn carried(log: &ChangeLog, origin: TxId, e: u8, seq: u64, back: Arc<[u32]>) {
        log.carried(origin, Carrier { entry: entry(e), chain: CHAIN, seq }, seq, back);
    }
    /// A cursor at the stream's start, for [`mine_items`].
    fn start(log: &ChangeLog) -> pe_seams::world_api::Cursor {
        log.feed().read(None, |_: &Arc<FeedItem>| None::<()>, 1, Duration::ZERO).unwrap().cursor
    }
    /// What `mine` hears, from `from`: (change, status, resent_as, note).
    fn mine_items(
        log: &ChangeLog,
        from: &pe_seams::world_api::Cursor,
    ) -> Vec<(TxId, Status, Option<TxId>, Option<DropNote>)> {
        let pick = |it: &Arc<FeedItem>| match it.as_ref() {
            FeedItem::Stepped { step, status, owner: Some(_), .. } => {
                Some((TxId::from(step.entry), *status, None, None))
            }
            FeedItem::Settled { change, status, owner: Some(_), resent_as, note, .. } => {
                Some((*change, *status, *resent_as, *note))
            }
            FeedItem::Submitted { change, owner: Some(_), .. } => Some((*change, Status::Queued, None, None)),
            _ => None,
        };
        log.feed().read(Some(from), pick, 1000, Duration::ZERO).unwrap().items
    }

    #[test]
    fn a_late_void_is_re_sent_and_told_once_under_its_own_id() {
        let log = ChangeLog::new();
        let c0 = start(&log);
        let now = Instant::now();
        own(&log, 1, 1, kept_at(txn(vec![make_cup()], vec![]), Slots::default(), now), ok(vec![created(1, 0)]));
        // Nothing waits: the simulation thread takes nothing.
        assert!(log.take(Instant::now()).is_none());
        log.confirm_cut(&cut(5, Some(0)), &[(entry(1), VoidReason::Late)]);
        // Not dropped: waiting, as queued.
        let f = log.get(tx(1)).unwrap();
        assert_eq!((f.status, f.is_final, f.resent_as), (Status::Queued, false, None));
        let (o, t, back) = resend(&log, Instant::now());
        assert_eq!((o, t.ops.len(), &back[..]), (tx(1), 1, &[0u32][..]));
        // Taken, its receipt on the way: nothing more.
        assert!(log.take(Instant::now()).is_none());
        carried(&log, tx(1), 2, 2, back);
        log.record(&[StepRecord { outcome: ok(vec![created(2, 0)]), ..step(2, 6, true) }], None);
        let f = log.get(tx(1)).unwrap();
        assert_eq!((f.status, f.resent_as), (Status::Accepted, Some(tx(2))));
        log.confirm_cut(&cut(7, Some(2)), &[]);
        log.record(&[], Some(7));
        let f = log.get(tx(1)).unwrap();
        assert_eq!((f.status, f.resent_as, f.frame), (Status::Final, Some(tx(2)), Some(6)));
        // `mine`: queued, accepted, then the one final notice; nothing for the void or the re-sent entry.
        let m = mine_items(&log, &c0);
        assert_eq!(
            m,
            vec![
                (tx(1), Status::Queued, None, None),
                (tx(1), Status::Accepted, None, None),
                (tx(1), Status::Final, Some(tx(2)), None)
            ]
        );
        // History lists it once, under its own id.
        let h: Vec<_> = log.stepped().into_iter().map(|f| (f.change, f.status)).collect();
        assert_eq!(h, vec![(tx(1), Status::Final)]);
    }

    #[test]
    fn back_stamped_voids_and_others_changes_are_not_re_sent() {
        let log = ChangeLog::new();
        let now = Instant::now();
        own(&log, 1, 1, kept_at(txn(vec![make_cup()], vec![]), Slots::default(), now), ok(vec![created(1, 0)]));
        log.track(tx(3), CHAIN, 2, 2, None);
        log.confirm_cut(&cut(5, Some(0)), &[(entry(1), VoidReason::BackStamped), (entry(3), VoidReason::Late)]);
        assert!(log.take(Instant::now()).is_none());
        assert_eq!(log.get(tx(1)).unwrap().status, Status::Dropped);
    }

    #[test]
    fn a_dependent_is_re_sent_after_it_rewritten_to_the_re_sent_ids() {
        let log = ChangeLog::new();
        let c0 = start(&log);
        let now = Instant::now();
        let x = created(1, 0);
        own(&log, 1, 1, kept_at(txn(vec![make_cup()], vec![]), Slots::default(), now), ok(vec![x]));
        let v = pe_seams::Version { entry: entry(1), op: 0 };
        let b = txn(vec![set(x, 0, 9)], vec![Precondition::ObjectVersion { id: x, is: v }]);
        own(&log, 2, 2, kept_at(b, Slots::default(), now), ok(vec![]));
        // The void, then the record that refuses B because the cup is gone: B joins before `mine` hears of it.
        log.confirm_cut(&cut(5, Some(0)), &[(entry(1), VoidReason::Late)]);
        let gone = WorldOutcome::Refused { op: Some(0), reason: pe_seams::Reason::Unknown(x) };
        log.record(&[StepRecord { outcome: gone, ..step(2, 2, false) }], None);
        let (o, _, back) = resend(&log, Instant::now());
        assert_eq!(o, tx(1));
        // B waits until A's re-send has its seq.
        assert!(log.take(Instant::now()).is_none());
        carried(&log, tx(1), 5, 3, back);
        let (o, t, _) = resend(&log, Instant::now());
        assert_eq!(o, tx(2));
        let x2 = created(3, 0);
        assert_eq!(t.ops, vec![set(x2, 0, 9)]);
        assert_eq!(
            t.preconditions,
            vec![Precondition::ObjectVersion { id: x2, is: pe_seams::Version { entry: entry(5), op: 0 } }]
        );
        // `mine` heard B accepted, but not its refusal.
        let m = mine_items(&log, &c0);
        assert!(m.iter().all(|(c, s, ..)| !(*c == tx(2) && *s == Status::Refused)), "{m:?}");
    }

    #[test]
    fn a_re_sent_dependent_refused_when_its_predecessor_is_void_again_goes_again() {
        let log = ChangeLog::new();
        let now = Instant::now();
        let x = created(1, 0);
        own(&log, 1, 1, kept_at(txn(vec![make_cup()], vec![]), Slots::default(), now), ok(vec![x]));
        own(&log, 2, 2, kept_at(txn(vec![set(x, 0, 9)], vec![]), Slots::default(), now), ok(vec![]));
        log.confirm_cut(&cut(5, Some(0)), &[(entry(1), VoidReason::Late)]);
        let refused = |o| WorldOutcome::Refused { op: Some(0), reason: pe_seams::Reason::Unknown(o) };
        log.record(&[StepRecord { outcome: refused(x), ..step(2, 2, false) }], None);
        // A goes as entry 5, B after it as entry 6; both apply.
        let (_, _, back) = resend(&log, Instant::now());
        carried(&log, tx(1), 5, 3, back);
        let (o, _, back) = resend(&log, Instant::now());
        assert_eq!(o, tx(2));
        carried(&log, tx(2), 6, 4, back);
        let x2 = created(3, 0);
        log.record(&[StepRecord { outcome: ok(vec![x2]), ..step(5, 3, true) }], None);
        log.record(&[StepRecord { outcome: ok(vec![]), ..step(6, 4, true) }], None);
        // A's re-send is void as late too, and B's re-send is refused for it, not yet finally.
        log.confirm_cut(&cut(9, Some(0)), &[(entry(5), VoidReason::Late)]);
        log.record(&[StepRecord { outcome: refused(x2), ..step(6, 4, false) }], None);
        let later = Instant::now() + Duration::from_millis(600);
        let (o, _, back) = resend(&log, later);
        assert_eq!(o, tx(1));
        carried(&log, tx(1), 7, 5, back);
        // B goes again, after A's second re-send and rewritten to it, instead of closing refused.
        let (o, t, _) = resend(&log, later);
        assert_eq!((o, t.ops), (tx(2), vec![set(created(5, 0), 0, 9)]));
        assert_ne!(log.get(tx(2)).map(|f| f.status), Some(Status::Refused));
    }

    #[test]
    fn the_authors_later_word_wins() {
        let log = ChangeLog::new();
        let now = Instant::now();
        // A sets the lamp's fields 0 and 1; B, later and accepted, sets field 0 again.
        own(
            &log,
            1,
            1,
            kept_at(txn(vec![set(LAMP, 0, 1), set(LAMP, 1, 1)], vec![]), Slots::default(), now),
            ok(vec![]),
        );
        own(&log, 2, 2, kept_at(txn(vec![set(LAMP, 0, 2)], vec![]), Slots::default(), now), ok(vec![]));
        log.confirm_cut(&cut(5, Some(0)), &[(entry(1), VoidReason::Late)]);
        let (_, t, back) = resend(&log, Instant::now());
        assert_eq!((t.ops, &back[..]), (vec![set(LAMP, 1, 1)], &[1u32][..]));
    }

    #[test]
    fn a_version_of_an_op_after_a_dropped_one_moves_to_its_new_place() {
        let log = ChangeLog::new();
        let now = Instant::now();
        // X sets the lamp's field 0 and the table's field 1; Z, later and accepted, sets the lamp's field 0 again;
        // Y names the version X's second op gave the table, and is refused when X is voided.
        own(
            &log,
            1,
            1,
            kept_at(txn(vec![set(LAMP, 0, 1), set(TABLE, 1, 1)], vec![]), Slots::default(), now),
            ok(vec![]),
        );
        own(&log, 2, 2, kept_at(txn(vec![set(LAMP, 0, 2)], vec![]), Slots::default(), now), ok(vec![]));
        let named = |e, op| Precondition::ObjectVersion { id: TABLE, is: pe_seams::Version { entry: entry(e), op } };
        let y = txn(vec![set(TABLE, 1, 3)], vec![named(1, 1)]);
        own(
            &log,
            3,
            3,
            kept_at(y, Slots::default(), now),
            WorldOutcome::Refused { op: None, reason: Reason::Unknown(TABLE) },
        );
        log.confirm_cut(&cut(5, Some(0)), &[(entry(1), VoidReason::Late)]);
        // X is re-sent without its first op, so its second is now op 0 of X's new carrier.
        let (_, t, back) = resend(&log, Instant::now());
        assert_eq!((t.ops, &back[..]), (vec![set(TABLE, 1, 1)], &[1u32][..]));
        carried(&log, tx(1), 5, 4, back);
        let (o, t, _) = resend(&log, Instant::now());
        assert_eq!((o, t.preconditions), (tx(3), vec![named(5, 0)]));
    }

    #[test]
    fn a_version_of_a_dropped_op_is_left_to_refuse_as_stale() {
        let mut m = crate::resend::Map::default();
        let c = |e, seq| Carrier { entry: entry(e), chain: CHAIN, seq };
        let x = txn(vec![set(LAMP, 0, 1), set(TABLE, 1, 1), set(TABLE, 2, 1)], vec![]);
        // The second hop dropped op 0 of the original; the first carried all three.
        m.carry((LINE, ME, &x), &c(1, 1), None, &c(7, 7), Some(&[1, 2]));
        m.carry((LINE, ME, &x), &c(5, 4), Some(&[0, 1, 2]), &c(7, 7), Some(&[1, 2]));
        let named = |e, op| Precondition::ObjectVersion { id: TABLE, is: pe_seams::Version { entry: entry(e), op } };
        let y = txn(vec![], vec![named(1, 0), named(1, 2), named(5, 1)]);
        let out = crate::resend::rewrite(&y, &Slots::default(), &m);
        assert_eq!(out.preconditions, vec![named(1, 0), named(7, 1), named(7, 0)]);
    }

    #[test]
    fn a_change_its_author_replaced_entirely_is_dropped_saying_so() {
        let log = ChangeLog::new();
        let c0 = start(&log);
        let now = Instant::now();
        own(&log, 1, 1, kept_at(txn(vec![set(LAMP, 0, 1)], vec![]), Slots::default(), now), ok(vec![]));
        own(
            &log,
            2,
            2,
            kept_at(txn(vec![Op::Delete { id: Target::Existing(LAMP) }], vec![]), Slots::default(), now),
            ok(vec![]),
        );
        log.confirm_cut(&cut(5, Some(0)), &[(entry(1), VoidReason::Late)]);
        assert!(log.take(Instant::now()).is_none());
        let f = log.get(tx(1)).unwrap();
        assert_eq!((f.status, f.is_final, f.note), (Status::Dropped, true, Some(DropNote::Superseded)));
        assert_eq!(mine_items(&log, &c0).last().map(|m| m.3), Some(Some(DropNote::Superseded)));
    }

    #[test]
    fn a_re_send_void_again_backs_off() {
        let log = ChangeLog::new();
        let now = Instant::now();
        own(&log, 1, 1, kept_at(txn(vec![make_cup()], vec![]), Slots::default(), now), ok(vec![created(1, 0)]));
        log.confirm_cut(&cut(5, Some(0)), &[(entry(1), VoidReason::Late)]);
        let (_, _, back) = resend(&log, Instant::now());
        carried(&log, tx(1), 2, 2, back);
        log.confirm_cut(&cut(8, Some(1)), &[(entry(2), VoidReason::Late)]);
        // The second re-send waits 0.5 s.
        assert!(log.take(now + Duration::from_millis(400)).is_none());
        let (o, ..) = resend(&log, now + Duration::from_millis(600));
        assert_eq!(o, tx(1));
        // Refused at submission: waits 1 s more.
        log.resend_refused(tx(1));
        assert!(log.take(Instant::now() + Duration::from_millis(900)).is_none());
        assert!(log.take(Instant::now() + Duration::from_millis(1_100)).is_some());
        assert_eq!(crate::resend::backoff(9), Duration::from_secs(16));
    }

    #[test]
    fn past_the_bound_a_change_and_what_waits_behind_it_are_held() {
        let log = ChangeLog::new();
        let long_ago = Instant::now() - Duration::from_secs(61);
        let now = Instant::now();
        let x = created(1, 0);
        own(&log, 1, 1, kept_at(txn(vec![make_cup()], vec![]), Slots::default(), long_ago), ok(vec![x]));
        own(&log, 2, 2, kept_at(txn(vec![set(x, 0, 9)], vec![]), Slots::default(), now), ok(vec![]));
        log.confirm_cut(&cut(5, Some(0)), &[(entry(1), VoidReason::Late)]);
        let gone = WorldOutcome::Refused { op: Some(0), reason: pe_seams::Reason::Unknown(x) };
        log.record(&[StepRecord { outcome: gone, ..step(2, 2, false) }], None);
        // A is held at once; B, waiting behind it, is held with it on the next take.
        assert!(log.take(Instant::now()).is_none());
        let (a, b) = (log.get(tx(1)).unwrap(), log.get(tx(2)).unwrap());
        assert_eq!((a.status, a.is_final), (Status::Dropped, true));
        let Some(DropNote::Held(h)) = a.note else { panic!("held: {a:?}") };
        assert_eq!(b.note, Some(DropNote::Held(h)));
        let listed = log.held_for("claude");
        assert_eq!((listed.len(), listed[0].count, listed[0].handle.clone()), (1, 2, handle(h)));
        assert!(log.held_for("someone-else").is_empty());
        assert!(log.held("someone-else", &handle(h), true).is_none(), "only the caller's own changes");
        // Apply: queued again in order, with a fresh bound.
        assert_eq!(log.held("claude", &handle(h), true), Some(vec![tx(1), tx(2)]));
        assert!(log.held_for("claude").is_empty());
        let (o, ..) = resend(&log, Instant::now());
        assert_eq!(o, tx(1));
        assert!(log.held("claude", &handle(h), false).is_none(), "the batch is gone");
    }

    #[test]
    fn past_the_batch_limit_each_principal_hears_of_its_own_changes_let_go() {
        let log = ChangeLog::new();
        let c0 = start(&log);
        let long_ago = Instant::now() - Duration::from_secs(61);
        let by = |p: &str| Owner { principal: p.into(), ..owner() };
        let held = |e: u8, p: &str| {
            let k = kept_at(txn(vec![make_cup()], vec![]), Slots::default(), long_ago);
            log.track_own(tx(e), CHAIN, e as u64, e as u64, by(p), k);
            log.record(&[StepRecord { outcome: ok(vec![created(e as u64, 0)]), ..step(e, e as u64, true) }], None);
        };
        // The oldest batch holds one change of a person's and one of an agent's.
        held(1, "ann");
        held(2, "claude");
        log.confirm_cut(&cut(100, Some(0)), &[(entry(1), VoidReason::Late), (entry(2), VoidReason::Late)]);
        let first = log.held_for("ann");
        assert_eq!(first.len(), 1);
        assert_eq!(log.held_for("claude")[0].handle, first[0].handle, "one batch");
        // 64 more batches of the agent's: the oldest goes.
        for e in 3..=66u8 {
            held(e, "claude");
            log.confirm_cut(&cut(100 + e as u64, Some(0)), &[(entry(e), VoidReason::Late)]);
            assert!(log.take(Instant::now()).is_none());
        }
        assert!(log.held_for("ann").is_empty());
        let pick = |it: &Arc<FeedItem>| match it.as_ref() {
            FeedItem::HeldDiscarded { handle, counts } => Some((handle.clone(), counts.clone())),
            _ => None,
        };
        let gone = log.feed().read(Some(&c0), pick, 1000, Duration::ZERO).unwrap().items;
        let want: Vec<(Arc<str>, u32)> = vec![("ann".into(), 1), ("claude".into(), 1)];
        assert_eq!(gone, vec![(first[0].handle.clone(), want)]);
    }

    #[test]
    fn a_discarded_batch_stays_dropped() {
        let log = ChangeLog::new();
        let long_ago = Instant::now() - Duration::from_secs(61);
        own(&log, 1, 1, kept_at(txn(vec![make_cup()], vec![]), Slots::default(), long_ago), ok(vec![created(1, 0)]));
        log.confirm_cut(&cut(5, Some(0)), &[(entry(1), VoidReason::Late)]);
        let Some(DropNote::Held(h)) = log.get(tx(1)).unwrap().note else { panic!("held") };
        assert_eq!(log.held("claude", &handle(h), false), Some(vec![tx(1)]));
        let f = log.get(tx(1)).unwrap();
        assert_eq!((f.status, f.note), (Status::Dropped, None));
        assert!(log.take(Instant::now()).is_none());
    }

    #[test]
    fn an_accepted_reference_to_a_re_sent_object_is_re_pointed() {
        let log = ChangeLog::new();
        let now = Instant::now();
        let x = created(1, 0);
        own(&log, 1, 1, kept_at(txn(vec![make_cup()], vec![]), Slots::default(), now), ok(vec![x]));
        // D stores a reference to the cup in the lamp's field (an ObjectRef value), and is accepted.
        let r = Op::SetField {
            id: Target::Existing(LAMP),
            component: COLOR,
            instance: 0,
            field: 3,
            value: {
                let v = x.to_u128();
                [((v >> 64) as u64).to_le_bytes(), (v as u64).to_le_bytes()].concat()
            },
        };
        let slots = Slots {
            refs: vec![RefSlot { op: 0, row: VALUE, at: 0, field: 3, span: (0, 16) }],
            field_at: vec![Some((32, 16))],
        };
        own(&log, 2, 2, kept_at(txn(vec![r], vec![]), slots, now), ok(vec![]));
        log.confirm_cut(&cut(5, Some(0)), &[(entry(1), VoidReason::Late)]);
        let (_, _, back) = resend(&log, Instant::now());
        carried(&log, tx(1), 5, 3, back);
        let Some(Taken::FollowUp(sub)) = log.take(Instant::now()) else { panic!("a follow-up") };
        let x2 = created(3, 0).to_u128();
        let want = [((x2 >> 64) as u64).to_le_bytes(), (x2 as u64).to_le_bytes()].concat();
        assert!(
            matches!(&sub.tx.ops[..], [Op::SetField { value, field: 3, .. }] if *value == want),
            "{:?}",
            sub.tx.ops
        );
        assert!(sub.tx.preconditions.is_empty());
        // D itself is not re-sent, and its own outcome stands.
        assert!(log.take(Instant::now()).is_none());
        assert_eq!(log.get(tx(2)).unwrap().status, Status::Accepted);
    }
}

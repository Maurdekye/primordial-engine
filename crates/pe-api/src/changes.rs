//! The change log: what became of each entry, from the change record beside S5 and agreement's final cuts (api.md
//! 5.3.2, 9.1, 9.2 steps 6-7).
//!
//! pe-app feeds it every publication's `FrameView.changes` (the entries stepped since the last one, with their
//! outcomes; a re-stepped entry appears again and the newest verdict wins), and every final cut with the entries
//! that cut voided (`confirm_cut`). For the changes this machine submitted (`track`: their chain, seq and stamp),
//! a final cut whose frame is above the stamp decides (S3 v1.1; engine-architect m5f6621c80ca3):
//! - voided by that cut (late, or back-stamped) is `dropped`: no replica applies it, so its local effect is undone;
//! - included and not voided is `final` (or a final refusal);
//! - neither stays as it was: pending, until a later cut includes it or voids it.
//!
//! Finality never comes from the frame alone: a dropped change is never stepped again on its author's machine, so a
//! frame test would call it final (@chunks-reviewer's Medium). `apply` waits here, on its own thread. Kept
//! bounded: settled entries are evicted first, oldest first, without scanning.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Condvar, Mutex};
use std::time::Instant;

use pe_core::{ChainId, EntryRef};
use pe_seams::world_api::{Frame, Status, TxId, Wait};
use pe_seams::{Cut, Outcome as WorldOutcome, StepRecord};

/// The most entries remembered [I].
pub const CAPACITY: usize = 10_000;

/// An entry's newest verdict.
#[derive(Clone, Debug, PartialEq)]
pub struct Fate {
    pub change: TxId,
    /// The frame it was (re-)stepped at; `None` if it was settled without being stepped here.
    pub frame: Option<Frame>,
    pub status: Status,
    /// For `refused` (and `dropped`): whether that is final.
    pub is_final: bool,
    /// pe-world's outcome at its newest step, if it was stepped here.
    pub outcome: Option<WorldOutcome>,
}

#[derive(Clone, Copy)]
struct Coords {
    chain: ChainId,
    seq: u64,
    stamp: Frame,
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
}

pub struct ChangeLog {
    inner: Mutex<Inner>,
    changed: Condvar,
}

fn status(outcome: Option<&WorldOutcome>, settled: Option<bool>) -> (Status, bool) {
    match (outcome, settled) {
        (_, Some(false)) => (Status::Dropped, true),
        (Some(WorldOutcome::Refused { .. }), s) => (Status::Refused, s == Some(true)),
        (_, Some(true)) => (Status::Final, false),
        (_, None) => (Status::Accepted, false),
    }
}

impl Default for ChangeLog {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeLog {
    pub fn new() -> Self {
        ChangeLog {
            inner: Mutex::new(Inner {
                fates: HashMap::new(),
                order: VecDeque::new(),
                final_frame: None,
                settled: HashMap::new(),
                settled_order: VecDeque::new(),
                coords: HashMap::new(),
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// pe-api, on a receipt: where this machine's change sits in agreement, for deciding it against final cuts.
    pub fn track(&self, change: TxId, chain: ChainId, seq: u64, stamp: Frame) {
        self.lock().coords.insert(change, Coords { chain, seq, stamp });
    }

    /// pe-app, per publication: the entries stepped since the last one, and the newest final frame (reported, not
    /// used for status).
    pub fn record(&self, steps: &[StepRecord], final_frame: Option<Frame>) {
        let mut g = self.lock();
        if final_frame > g.final_frame {
            g.final_frame = final_frame;
        }
        for s in steps {
            let change = TxId::from(s.entry);
            let (status, is_final) = status(Some(&s.outcome), g.settled.get(&change).copied());
            let f = Fate { change, frame: Some(s.frame.0), status, is_final, outcome: Some(s.outcome.clone()) };
            if g.fates.insert(change, f).is_none() {
                g.order.push_back(change);
            }
        }
        Self::evict(&mut g);
        drop(g);
        self.changed.notify_all();
    }

    fn settle(g: &mut Inner, t: TxId, included: bool) {
        if g.settled.insert(t, included).is_none() {
            g.settled_order.push_back(t);
        }
        if let Some(f) = g.fates.get_mut(&t) {
            let (s, fin) = status(f.outcome.as_ref(), Some(included));
            f.status = s;
            f.is_final = fin;
        }
    }

    /// pe-app, every time a cut becomes final: the cut, and the entries it voided (agreement's `Voided` events
    /// naming this cut). One call, so an entry the cut includes and voids is never shown final.
    pub fn confirm_cut(&self, cut: &Cut, voided: &[EntryRef]) {
        let mut g = self.lock();
        for e in voided {
            Self::settle(&mut g, TxId::from(*e), false);
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
                Self::settle(&mut g, t, true);
            }
            g.coords.remove(&t);
        }
        for t in &voided {
            g.coords.remove(t);
        }
        Self::evict(&mut g);
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
    }

    pub fn get(&self, change: TxId) -> Option<Fate> {
        let g = self.lock();
        Self::fate(&g, change)
    }

    /// Its fate, or, if a final cut settled it without its being stepped here, that settlement.
    fn fate(g: &Inner, change: TxId) -> Option<Fate> {
        g.fates.get(&change).cloned().or_else(|| {
            g.settled.get(&change).map(|inc| {
                let (status, is_final) = status(None, Some(*inc));
                Fate { change, frame: None, status, is_final, outcome: None }
            })
        })
    }

    /// Waits until `change` reaches `until` (`Local`: stepped here, accepted or refused; `Final`: final, dropped,
    /// or a final refusal), or `deadline`. Returns its newest fate, if it has one by then.
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
                    f.status == Status::Final
                        || f.status == Status::Dropped
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
            log.track(tx(e), CHAIN, seq, 10);
        }
        // The final frame passing them decides nothing.
        log.record(&[], Some(20));
        assert_eq!(log.get(tx(1)).unwrap().status, Status::Accepted);
        // A cut above their stamp that includes seq 1 and 3, and voids 2.
        log.confirm_cut(&cut(12, Some(3)), &[entry(2)]);
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
        log.track(tx(4), CHAIN, 7, 10);
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
        log.track(tx(7), CHAIN, 1, 3);
        let l = log.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            l.record(&[step(7, 3, true)], Some(1));
            std::thread::sleep(Duration::from_millis(20));
            l.confirm_cut(&cut(4, Some(1)), &[]);
        });
        let local = log.wait(tx(7), Wait::Local, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(local.status, Status::Accepted);
        let fin = log.wait(tx(7), Wait::Final, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(fin.status, Status::Final);
        t.join().unwrap();
        assert_eq!(log.wait(tx(9), Wait::Local, Instant::now() + Duration::from_millis(10)), None);
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

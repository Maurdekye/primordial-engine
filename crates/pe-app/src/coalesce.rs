//! Coalesced pose rollbacks (@item:primordial-engine/friend-test-re-simulation-load-under-constant-co; @coordinator-opus, with @astra-deliberator's and @fable-reviewer's conditions).
//!
//! In the friend test's 18:01Z VR-to-VR run, the other player's hand poses reached each machine about a frame behind
//! its present. Under B (app.md 3, [`crate::shown`]), each replica had already stepped four frames past the present,
//! so almost every tick rolled back 5-9 frames: 56 rollbacks a second, and re-simulation at 5.6 times real time.
//!
//! A change agreement reports ([`EntrySource::take_changed_from`]) is held when every entry in the re-stepped span is
//! a pose that changes no grip and no jump. A change at or past the replica's frame is no rollback and is dropped.
//! This member's own entries count like anyone's, so its own edits (a spawn) still roll back at once. Held changes are let go together, from the earliest
//! frame any of them changed, at the [`WINDOW_TICKS`]th tick counted from the FIRST held one, so a steady stream of
//! poses can't put the correction off. Anything else rolls back at once, from the earliest of its frame and every held
//! one, the held poses included:
//! - an entry of another class (an edit, governance, protocol);
//! - an `Input` that is no pose (grab, release, throw, push, hand mode);
//! - a hand pose whose engaged flag differs from that hand's pose before it, or one with no pose before it in reach;
//! - a `Move` whose jump count differs from the one before it;
//! - this member's own grasp edge, a close or an open, in the tick ([`Coalesce::flush`]), so its grab, release or throw
//!   is stepped on the corrected world.
//!
//! Only re-simulation waits. Agreement keeps every entry, so frames the replica steps for the first time already
//! have them. Roots don't depend on it: a vote or finalize compares the live history's entries with the cut's frame by
//! frame and re-steps from a keyframe where they differ (pe-replica's `walk` and `first_difference`). So machines can
//! batch differently and still agree. A final cut that lands inside the window moves the replica's floor. Finalize
//! rebases the live history on it when that history was stepped without a held pose. The held frame is therefore
//! raised to the final cut's, never let below the floor, where the replica would count a protocol fault.

use pe_seams::{Class, Cut, EntryRef, EntrySource, FinalCut, Frame, HostTime, MemberKey, ObjectId, SignedEntry};
use pe_world::input::Input;

/// Held changes are let go at the tick this many ticks after the first, that tick included: at most one coalesced
/// rollback every three ticks.
pub const WINDOW_TICKS: u64 = 3;

/// How far before a changed span a pose's predecessor is looked for, in frames. Poses come every frame while a hand
/// is tracked; none in reach counts as a grip edge, and rolls back at once.
const LOOK_BACK: u64 = 8;

/// What the coalescer did, over a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoalesceStats {
    /// Changes held for a later tick.
    pub held: u64,
    /// Coalesced rollbacks let go at their deadline.
    pub released: u64,
    /// Changes that rolled back at once, with whatever was held.
    pub urgent: u64,
    /// Held frames raised to the final cut's.
    pub clamped: u64,
    /// Held changes let go early by this member's own grasp edge ([`Coalesce::flush`]).
    pub flushed: u64,
}

/// The coalescer's state across ticks.
#[derive(Clone, Debug)]
pub struct Coalesce {
    /// Ticks in the window; 1 or less turns coalescing off.
    window: u64,
    ticks: u64,
    /// The earliest held frame and the tick of the first held change.
    held: Option<(Frame, u64)>,
    /// Let the held go at the next filter ([`Coalesce::flush`]).
    flush: bool,
    pub stats: CoalesceStats,
}

impl Default for Coalesce {
    fn default() -> Coalesce {
        Coalesce::new(WINDOW_TICKS)
    }
}

impl Coalesce {
    /// A coalescer letting held changes go every `window` ticks; `window` <= 1 passes every change through.
    pub fn new(window: u64) -> Coalesce {
        Coalesce { window, ticks: 0, held: None, flush: false, stats: CoalesceStats::default() }
    }

    /// Ticks in the window (1 or less: off). The budget's keep-up floor covers this many calls' needs
    /// ([`crate::sim::Budget::set_need_window`]).
    pub fn window(&self) -> u64 {
        self.window
    }

    /// The frame held, if any.
    pub fn held(&self) -> Option<Frame> {
        self.held.map(|(f, _)| f)
    }

    /// Lets what is held go at the next [`Coalesce::filter`], as an urgent change would: this member's own grasp edge.
    pub fn flush(&mut self) {
        self.flush = true;
    }

    /// One advance's change: `changed`, what the source reported; `stepped`, the replica's frame before the advance
    /// (frames below it were stepped). Returns the change for the replica to act on now.
    pub fn filter(&mut self, changed: Option<Frame>, stepped: Frame, source: &dyn EntrySource) -> Option<Frame> {
        self.ticks += 1;
        let tick = self.ticks;
        if self.window <= 1 {
            return changed;
        }
        // At or past the stepped frame, a change is no rollback: the replica will step those frames with it anyway.
        let mut now = None;
        if let Some(f) = changed.filter(|f| *f < stepped) {
            if poses_only(source, f, stepped) {
                self.held = Some(self.held.map_or((f, tick), |(h, t)| (h.min(f), t)));
                self.stats.held += 1;
            } else {
                self.stats.urgent += 1;
                now = Some(f);
            }
        }
        let flush = std::mem::take(&mut self.flush);
        let due = self.held.is_some_and(|(_, t)| tick + 1 >= t + self.window);
        let held = if now.is_some() || due || flush { self.held.take().map(|(f, _)| f) } else { None };
        if held.is_some() && now.is_none() {
            if due {
                self.stats.released += 1;
            } else {
                self.stats.flushed += 1;
            }
        }
        let held = held.map(|f| match source.final_cut() {
            Some(c) if f < c.cut.at.frame => {
                self.stats.clamped += 1;
                c.cut.at.frame
            }
            _ => f,
        });
        match (now, held) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// Whether every entry stepped in `from..to` is a pose that changes no grip and no jump.
fn poses_only(source: &dyn EntrySource, from: Frame, to: Frame) -> bool {
    // (author, hand or body) -> the engaged flag or jump count of its newest pose so far, in frame order.
    let mut last: Vec<((MemberKey, Key), u32)> = Vec::new();
    for f in (from.0..to.0).map(Frame) {
        let Some(entries) = source.entries_at(f) else { return false };
        for e in entries {
            let Some(marks) = marks_of(e) else { return false };
            for (key, mark) in marks {
                let k = (e.entry.author, key);
                let before = match last.iter().position(|(x, _)| *x == k) {
                    Some(i) => Some(last[i].1),
                    None => before_span(source, from, k),
                };
                if before != Some(mark) {
                    return false;
                }
                match last.iter_mut().find(|(x, _)| *x == k) {
                    Some(slot) => slot.1 = mark,
                    None => last.push((k, mark)),
                }
            }
        }
    }
    true
}

/// What a pose is keyed by: a hand, or a player body's moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Hand(ObjectId),
    Body(ObjectId),
}

/// A pose entry's marks in order, one per pose: the engaged flag (0 or 1) for a hand, the jump count for a move.
/// `None` for anything that isn't a pose.
fn marks_of(e: &SignedEntry) -> Option<Vec<(Key, u32)>> {
    if e.entry.class != Class::Input {
        return None;
    }
    match Input::decode(&e.entry.payload)? {
        Input::Hand { hand, poses, .. } => {
            Some(poses.iter().map(|p| (Key::Hand(hand), u32::from(p.engaged))).collect())
        }
        Input::BodyHand { hand, poses, .. } => {
            Some(poses.iter().map(|p| (Key::Hand(hand), u32::from(p.engaged))).collect())
        }
        Input::Move { body, samples, .. } => Some(samples.iter().map(|s| (Key::Body(body), s.jumps)).collect()),
        _ => None,
    }
}

/// The mark of `k`'s newest pose below `from`, within [`LOOK_BACK`] frames.
fn before_span(source: &dyn EntrySource, from: Frame, k: (MemberKey, Key)) -> Option<u32> {
    let low = from.0.saturating_sub(LOOK_BACK);
    for f in (low..from.0).rev().map(Frame) {
        let entries = source.entries_at(f)?;
        let found = entries
            .iter()
            .filter(|e| e.entry.author == k.0)
            .filter_map(marks_of)
            .filter_map(|m| m.into_iter().rev().find(|(key, _)| *key == k.1))
            .last();
        if let Some((_, mark)) = found {
            return Some(mark);
        }
    }
    None
}

/// The source the replica reads through while a [`Coalesce`] filters its changes.
pub struct Coalesced<'a> {
    pub inner: &'a mut dyn EntrySource,
    pub coalesce: &'a mut Coalesce,
    pub stepped: Frame,
}

impl EntrySource for Coalesced<'_> {
    fn final_cut(&self) -> Option<&FinalCut> {
        self.inner.final_cut()
    }
    fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
        self.inner.entries_at(frame)
    }
    fn entries_for_cut(&self, cut: &Cut, frame: Frame) -> Option<Vec<SignedEntry>> {
        self.inner.entries_for_cut(cut, frame)
    }
    fn kept_from(&self) -> Frame {
        self.inner.kept_from()
    }
    fn keep_final_from(&mut self, cv: Frame, floor: Frame) {
        self.inner.keep_final_from(cv, floor)
    }
    fn take_changed_from(&mut self) -> Option<Frame> {
        let changed = self.inner.take_changed_from();
        self.coalesce.filter(changed, self.stepped, &*self.inner)
    }
    fn void_reason(&self, entry: &EntryRef) -> Option<pe_seams::VoidReason> {
        self.inner.void_reason(entry)
    }
    fn arrived_at(&self, entry: &EntryRef) -> Option<HostTime> {
        self.inner.arrived_at(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use pe_seams::agree::CutRef;
    use pe_seams::test_util::{EntryBuilder, TestKeys};
    use pe_seams::{Hash, LineId, Pos};
    use pe_world::input::HandPose;
    use std::collections::BTreeMap;

    /// Entries by frame and a final cut, with no change of its own (the filter is handed one).
    #[derive(Default)]
    struct Src {
        frames: BTreeMap<u64, Vec<SignedEntry>>,
        fin: Option<FinalCut>,
    }

    impl EntrySource for Src {
        fn final_cut(&self) -> Option<&FinalCut> {
            self.fin.as_ref()
        }
        fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
            Some(self.frames.get(&frame.0).map_or(&[][..], |v| v.as_slice()))
        }
        fn kept_from(&self) -> Frame {
            Frame(0)
        }
        fn keep_final_from(&mut self, _: Frame, _: Frame) {}
        fn take_changed_from(&mut self) -> Option<Frame> {
            None
        }
    }

    const HAND: u128 = 7;

    fn hand(engaged: bool) -> Bytes {
        let pose = HandPose { pos: Pos::ORIGIN, rot: [0.0, 0.0, 0.0, 1.0], grip: 0.0, engaged };
        Bytes::from(Input::Hand { hand: ObjectId::from_u128(HAND), poses: vec![pose], hold: 0 }.encode())
    }

    /// Member 1's hand at every frame of `frames`, engaged from `engaged_from` on.
    fn poses(keys: &TestKeys, frames: std::ops::Range<u64>, engaged_from: u64) -> Src {
        let mut b = EntryBuilder::new(keys, 1);
        let mut src = Src::default();
        for f in frames {
            src.frames.entry(f).or_default().push(b.next(Class::Input, Frame(f), hand(f >= engaged_from)));
        }
        src
    }

    fn final_at(frame: u64) -> FinalCut {
        let at = CutRef { line: LineId::origin(), height: 1, frame: Frame(frame) };
        FinalCut { cut: Cut { at, heads: Vec::new() }, root: Hash::of("pe-app.test", b"final"), voters: Vec::new() }
    }

    #[test]
    fn poses_are_held_and_let_go_together_at_the_third_tick_from_the_first() {
        let keys = TestKeys::new(5, 2);
        let src = poses(&keys, 0..100, u64::MAX);
        let mut c = Coalesce::default();
        // Tick 1 holds frame 50, tick 2 holds 48 (the earliest), tick 3 lets 48 go though nothing changed in it.
        assert_eq!(c.filter(Some(Frame(50)), Frame(55), &src), None);
        assert_eq!(c.filter(Some(Frame(48)), Frame(56), &src), None);
        assert_eq!(c.held(), Some(Frame(48)));
        assert_eq!(c.filter(None, Frame(57), &src), Some(Frame(48)));
        assert_eq!(c.held(), None);
        // A steady stream can't put it off: held at tick 4, let go at tick 6 whatever comes in between.
        assert_eq!(c.filter(Some(Frame(53)), Frame(58), &src), None);
        assert_eq!(c.filter(Some(Frame(54)), Frame(59), &src), None);
        assert_eq!(c.filter(Some(Frame(55)), Frame(60), &src), Some(Frame(53)));
        assert_eq!(c.stats, CoalesceStats { held: 5, released: 2, urgent: 0, clamped: 0, flushed: 0 });
        // A change at or past the stepped frame is no rollback: dropped, and it lets nothing held go.
        assert_eq!(c.filter(Some(Frame(58)), Frame(61), &src), None);
        assert_eq!(c.filter(Some(Frame(61)), Frame(61), &src), None);
        assert_eq!(c.held(), Some(Frame(58)));
    }

    #[test]
    fn a_grip_edge_a_grab_or_a_pose_with_nothing_before_it_rolls_back_at_once_with_what_is_held() {
        let keys = TestKeys::new(5, 2);
        // The hand engages at frame 52: a change from 50 spans the edge.
        let src = poses(&keys, 0..100, 52);
        let mut c = Coalesce::default();
        assert_eq!(c.filter(Some(Frame(45)), Frame(50), &src), None, "45..50 is poses only");
        assert_eq!(c.filter(Some(Frame(50)), Frame(55), &src), Some(Frame(45)), "the edge, with the held 45");
        // A grab in the span.
        let mut src = poses(&keys, 0..100, u64::MAX);
        let grab = Input::Grab { object: ObjectId::from_u128(9), hand: Some(ObjectId::from_u128(HAND)) }.encode();
        let mut b = EntryBuilder::new(&keys, 1);
        src.frames.entry(70).or_default().push(b.next(Class::Input, Frame(70), Bytes::from(grab)));
        assert_eq!(c.filter(Some(Frame(70)), Frame(74), &src), Some(Frame(70)));
        // An edit.
        let mut src = poses(&keys, 0..100, u64::MAX);
        src.frames.entry(80).or_default().push(b.next(Class::Edit, Frame(80), Bytes::from_static(b"edit")));
        assert_eq!(c.filter(Some(Frame(80)), Frame(84), &src), Some(Frame(80)));
        // A hand whose first pose is in the span: no pose before it within reach, so it could be an edge.
        let src = poses(&keys, 60..100, u64::MAX);
        assert_eq!(c.filter(Some(Frame(60)), Frame(64), &src), Some(Frame(60)));
        assert_eq!(c.stats.urgent, 4);
        // This member's own edit counts too: a spawn behind the shown present rolls back at once.
        let mut src = poses(&keys, 0..100, u64::MAX);
        let mut mine = EntryBuilder::new(&keys, 0);
        src.frames.entry(90).or_default().push(mine.next(Class::Edit, Frame(90), Bytes::from_static(b"spawn")));
        assert_eq!(c.filter(Some(Frame(90)), Frame(93), &src), Some(Frame(90)));
    }

    #[test]
    fn a_final_cut_inside_the_window_raises_the_held_frame_to_its_own() {
        // @fable-reviewer point 3: the replica's floor follows the final cut, so a held frame below it
        // would be a below-base report (a protocol fault there). It is let go at the cut's frame instead, where the
        // finalize's rebase has re-stepped what lies below.
        let keys = TestKeys::new(5, 2);
        let mut src = poses(&keys, 0..100, u64::MAX);
        let mut c = Coalesce::default();
        assert_eq!(c.filter(Some(Frame(40)), Frame(46), &src), None);
        src.fin = Some(final_at(43));
        assert_eq!(c.filter(None, Frame(47), &src), None);
        assert_eq!(c.filter(None, Frame(48), &src), Some(Frame(43)));
        assert_eq!(c.stats.clamped, 1);
        // A source's own urgent change below the final cut passes as it is: the replica judges that one.
        let edge = Src { fin: Some(final_at(43)), ..poses(&keys, 0..100, 42) };
        assert_eq!(c.filter(Some(Frame(41)), Frame(48), &edge), Some(Frame(41)));
    }

    #[test]
    fn an_own_grasp_edge_lets_the_held_go_and_a_window_of_one_holds_nothing() {
        let keys = TestKeys::new(5, 2);
        let src = poses(&keys, 0..100, u64::MAX);
        let mut c = Coalesce::default();
        assert_eq!(c.filter(Some(Frame(50)), Frame(55), &src), None);
        c.flush();
        assert_eq!(c.filter(None, Frame(56), &src), Some(Frame(50)));
        assert_eq!(c.filter(None, Frame(57), &src), None, "a flush is for one filter");
        assert_eq!(c.stats.flushed, 1);
        let mut off = Coalesce::new(1);
        assert_eq!(off.filter(Some(Frame(50)), Frame(55), &src), Some(Frame(50)));
        assert_eq!(off.stats, CoalesceStats::default());
    }
}

//! ReplicaImpl over the mock world and the real pe-physics: the properties of replica.md 4.3 that need no hands or
//! logic yet, the budget, roots at cuts, finalize and rung 1.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_replica::mock::{MockClock, MockOp, MockWorld, encode};
use pe_replica::world::{BodyKind, ShapeRow};
use pe_replica::{Phase, ReplicaConfig, ReplicaImpl, WorldModel, apply_order};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    ChainHead, Class, Cut, CutImport, CutRef, DomainId, EntryRef, EntrySource, FinalCut, Finalized, Frame, FrameView,
    GroupSel, Hash, Health, ImportError, LeafKey, LineId, ObjectId, Offset, Outcome, Pos, Replica, RootError,
    SignedEntry, SimRate, StepRecord, VoidReason,
};

/// Agreement's stand-in: entries by frame, in a fixed order (chain, then seq), whatever order they arrive in.
#[derive(Default)]
struct Source {
    entries: BTreeMap<Frame, Vec<SignedEntry>>,
    changed: Option<Frame>,
    /// Frames below this are trimmed (S3 v1.2's caps).
    kept: Frame,
    /// A frame at or above `kept` that answers `None` anyway: an S3 fault.
    withheld: Option<Frame>,
    /// What the replica last said it needs, `(cv, floor)`, each forward only.
    asked: Option<(Frame, Frame)>,
    /// S3's verdicts on the entries final cuts voided (S2 v1.5's `void_reason`).
    voided: BTreeMap<EntryRef, VoidReason>,
}

impl Source {
    fn add(&mut self, e: SignedEntry) {
        let f = e.entry.stamp;
        let at = self.entries.entry(f).or_default();
        at.push(e);
        at.sort_by_key(|e| (e.entry.chain, e.entry.seq));
        self.changed = Some(self.changed.map_or(f, |c| c.min(f)));
    }

    /// S3's cap trims everything below `frame`.
    fn trim_below(&mut self, frame: Frame) {
        let kept = self.kept.max(frame);
        self.kept = kept;
        self.entries.retain(|f, _| *f >= kept);
    }
}

impl EntrySource for Source {
    fn final_cut(&self) -> Option<&FinalCut> {
        None
    }
    fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
        (frame >= self.kept && self.withheld != Some(frame))
            .then(|| self.entries.get(&frame).map_or(&[][..], Vec::as_slice))
    }
    fn kept_from(&self) -> Frame {
        self.kept
    }
    fn keep_final_from(&mut self, cv: Frame, floor: Frame) {
        self.asked = Some(self.asked.map_or((cv, floor), |(c, f)| (c.max(cv), f.max(floor))));
    }
    fn take_changed_from(&mut self) -> Option<Frame> {
        self.changed.take()
    }
    fn void_reason(&self, entry: &EntryRef) -> Option<VoidReason> {
        self.voided.get(entry).copied()
    }
}

const K: u64 = 8;

fn id(n: u128) -> ObjectId {
    ObjectId::from_u128(n)
}

fn ball(n: u128, x: f64, y: f64) -> MockOp {
    MockOp::Spawn {
        id: id(n),
        kind: BodyKind::Dynamic,
        shape: ShapeRow::Sphere { center: [0.0; 3], radius: 0.25 },
        pos: [x, y, 0.0],
    }
}

/// The scene's entries: a ground, three balls that fall and touch, a push and a teleport, from two authors.
fn scene() -> Vec<SignedEntry> {
    let keys = TestKeys::new(7, 2);
    let (mut a, mut b) = (EntryBuilder::new(&keys, 0), EntryBuilder::new(&keys, 1));
    let edit = |ops: &[MockOp]| Bytes::from(encode(ops));
    let ground = MockOp::Spawn {
        id: id(1),
        kind: BodyKind::Static,
        shape: ShapeRow::Box { center: [0.0; 3], rotation: [0.0, 0.0, 0.0, 1.0], half_extents: [10.0, 0.5, 10.0] },
        pos: [0.0, -0.5, 0.0],
    };
    vec![
        a.next(Class::Edit, Frame(0), edit(&[ground])),
        a.next(Class::Edit, Frame(2), edit(&[ball(10, 0.0, 1.0)])),
        b.next(Class::Edit, Frame(3), edit(&[ball(11, 0.3, 2.0)])),
        a.next(Class::Edit, Frame(9), edit(&[MockOp::Push { id: id(10), impulse: [0.4, 0.0, 0.0] }])),
        b.next(Class::Edit, Frame(14), edit(&[ball(12, -0.2, 3.0)])),
        b.next(Class::Edit, Frame(21), edit(&[MockOp::Teleport { id: id(11), pos: [1.0, 1.5, 0.5] }])),
        a.next(Class::Edit, Frame(27), edit(&[MockOp::Push { id: id(12), impulse: [0.0, 0.0, -0.3] }])),
    ]
}

fn replica() -> ReplicaImpl<MockWorld> {
    let world = MockWorld::new(DomainId(0), SimRate::DEFAULT);
    let mut config =
        ReplicaConfig::new(SimRate::DEFAULT, Duration::from_millis(133), Arc::new(MockClock::new(SimRate::DEFAULT)));
    config.cut_every = K;
    ReplicaImpl::new(world, config).expect("a replica")
}

/// The cut at `frame`: per author, the last entry stamped below it.
fn cut(entries: &[SignedEntry], height: u64, frame: u64) -> Cut {
    let mut heads: BTreeMap<_, ChainHead> = BTreeMap::new();
    for e in entries.iter().filter(|e| e.entry.stamp.0 < frame) {
        let h = ChainHead { chain: e.entry.chain, author: e.entry.author, seq: e.entry.seq, entry: e.id() };
        if heads.get(&h.chain).is_none_or(|old| old.seq < h.seq) {
            heads.insert(h.chain, h);
        }
    }
    Cut { at: CutRef { line: LineId::origin(), height, frame: Frame(frame) }, heads: heads.into_values().collect() }
}

/// The live roots at every cut frame up to `to`.
fn roots(r: &ReplicaImpl<MockWorld>, to: u64) -> Vec<(u64, Hash)> {
    (0..=to).step_by(K as usize).filter_map(|f| r.live_root(Frame(f)).map(|h| (f, h))).collect()
}

/// A replica that had every entry from the start, run to `to`.
fn reference(to: u64) -> (ReplicaImpl<MockWorld>, Source) {
    let mut s = Source::default();
    scene().into_iter().for_each(|e| s.add(e));
    let mut r = replica();
    r.advance(Frame(to), &mut s);
    (r, s)
}

/// P1: equal entries give equal roots, and the scene moves (the roots change between cuts).
#[test]
fn equal_entries_give_equal_roots() {
    let (a, _) = reference(64);
    let (b, _) = reference(64);
    let ra = roots(&a, 64);
    assert_eq!(ra.len(), 9);
    assert_eq!(ra, roots(&b, 64));
    assert!(ra.windows(2).all(|w| w[0].1 != w[1].1), "every cut's root differs: {ra:?}");
    assert_eq!(a.faults(), 0);
    let ball = a.world().position(id(10)).expect("ball 10");
    assert!(ball[1] < 1.0 && ball[0] > 0.0, "ball 10 fell and was pushed: {ball:?}");
}

/// P2: entries that arrive late roll the replica back, and it reaches the same roots.
#[test]
fn late_entries_roll_back_to_the_same_roots() {
    let (a, _) = reference(64);
    let mut s = Source::default();
    let mut b = replica();
    let mut late = scene();
    let held_back = late.split_off(3); // the push at 9 and everything after it
    late.into_iter().for_each(|e| s.add(e));
    b.advance(Frame(40), &mut s);
    held_back.into_iter().rev().for_each(|e| s.add(e));
    let adv = b.advance(Frame(64), &mut s);
    // The newest keyframe the ring kept at or before frame 9, then everything from it to the old present again.
    let k = adv.rolled_back_to.expect("rolled back").0;
    assert!(k <= 9, "rolled back to {k}");
    assert_eq!((adv.resimulated, adv.stepped), (40 - k, 24));
    assert!(adv.caught_up);
    assert_eq!(roots(&b, 64), roots(&a, 64));
}

/// S2 v1.1: the work split by the budget never changes what is computed.
#[test]
fn the_budget_never_changes_the_result() {
    let (a, _) = reference(64);
    let mut s = Source::default();
    let mut c = replica();
    c.set_budget(Duration::from_millis(1)); // below two frames: raised to two
    let mut late = scene();
    let held_back = late.split_off(3);
    late.into_iter().for_each(|e| s.add(e));
    let mut calls = 0;
    for present in 1..=40 {
        c.advance(Frame(present), &mut s);
        calls += 1;
    }
    held_back.into_iter().for_each(|e| s.add(e));
    let mut adv = c.advance(Frame(41), &mut s);
    assert!(adv.rolled_back_to.is_some_and(|k| k <= Frame(9)));
    assert!(!adv.caught_up, "two frames of a re-simulation of more than 30");
    let mut present = 41;
    while !adv.caught_up {
        present = (present + 1).min(64);
        adv = c.advance(Frame(present), &mut s);
        calls += 1;
        assert!(adv.resimulated + adv.stepped <= 2);
        assert!(calls < 200);
    }
    while c.frame() < Frame(64) {
        c.advance(Frame(64), &mut s);
    }
    assert_eq!(roots(&c, 64), roots(&a, 64));
}

/// A cut's root has exactly the cut's entries, even when the prediction holds more; the live state is untouched.
#[test]
fn a_cuts_root_takes_exactly_its_entries() {
    let (mut a, mut s) = reference(48);
    let all = scene();
    // The cut at 32 excludes author b's teleport at 21: its root is that of a replica that never had it.
    let mut c32 = cut(&all, 4, 32);
    let b_chain = all[2].entry.chain;
    c32.heads.iter_mut().filter(|h| h.chain == b_chain).for_each(|h| {
        h.seq = all[4].entry.seq;
        h.entry = all[4].id();
    });
    let live_48 = a.live_root(Frame(48));
    let root = a.root_at_cut(&c32, &mut s).expect("a root");
    assert_ne!(Some(root), a.live_root(Frame(32)), "the live history has the teleport");
    let mut without = Source::default();
    all.iter().filter(|e| e.id() != all[5].id()).cloned().for_each(|e| without.add(e));
    let mut w = replica();
    w.advance(Frame(48), &mut without);
    assert_eq!(Some(root), w.live_root(Frame(32)));
    assert_eq!(a.live_root(Frame(48)), live_48, "the live state is unchanged");
    assert_eq!(a.frame(), Frame(48));
    // A cut whose entries the live history stepped exactly: the live root. Not yet simulated: no root.
    let c40 = cut(&all, 5, 40);
    assert_eq!(a.root_at_cut(&c40, &mut s), Ok(a.live_root(Frame(40)).expect("live")));
    assert_eq!(a.root_at_cut(&cut(&all, 9, 56), &mut s), Err(RootError::NotYetSimulated));
    // A cut that covers an entry the source leaves out (void, @harness-eng's) has a root, without it:
    // that of a replica that never had the entry. The d1 rule is S3's, so the replica never refuses for this.
    let mut voided = Source::default();
    all.iter().filter(|e| e.id() != all[3].id()).cloned().for_each(|e| voided.add(e));
    let mut v = replica();
    v.advance(Frame(48), &mut voided);
    assert_eq!(a.root_at_cut(&cut(&all, 6, 48), &mut voided), Ok(v.live_root(Frame(48)).expect("live")));
    // The roots given out are held: the tree and the physics image serve them.
    assert_eq!(a.tree(&root).map(|t| t.root()), Some(root));
    let image = a.physics_image(&root).expect("an image");
    assert_eq!(image.frame, Frame(32));
}

/// P4: finalize moves the floor, and nothing below it changes again; a change below it is a reported fault.
#[test]
fn finalize_moves_the_floor_and_nothing_below_it_changes() {
    let (mut a, mut s) = reference(48);
    let all = scene();
    let c16 = cut(&all, 2, 16);
    let quorum = a.live_root(Frame(16)).expect("live");
    assert_eq!(a.finalize(&FinalCut { cut: c16.clone(), root: quorum, voters: vec![] }, &mut s), Finalized::Agreed);
    assert_eq!(a.floor_frame(), Frame(16));
    assert_eq!(a.finalize(&FinalCut { cut: c16, root: quorum, voters: vec![] }, &mut s), Finalized::Agreed);
    let before = roots(&a, 48);
    s.changed = Some(Frame(5));
    let adv = a.advance(Frame(48), &mut s);
    assert_eq!(adv.rolled_back_to, Some(Frame(16)), "clamped to the floor");
    assert_eq!(a.faults(), 1);
    assert_eq!(roots(&a, 48), before);
    // Not yet simulated: not ready, and the floor stays.
    let c56 = cut(&all, 7, 56);
    assert_eq!(a.finalize(&FinalCut { cut: c56, root: quorum, voters: vec![] }, &mut s), Finalized::NotReady);
    assert_eq!(a.floor_frame(), Frame(16));
}

/// Rung 1: a transient fault after `Cv` makes my root differ at the next final cut; replaying from `Cv` with the
/// cut's entries repairs it, and finalize then agrees.
#[test]
fn rung_one_repairs_a_transient_fault() {
    let all = scene();
    let (good, _) = reference(48);
    let mut s = Source::default();
    all.iter().cloned().for_each(|e| s.add(e));
    let world = MockWorld::new(DomainId(0), SimRate::DEFAULT);
    world.glitch_at(Frame(20));
    let mut config =
        ReplicaConfig::new(SimRate::DEFAULT, Duration::from_millis(133), Arc::new(MockClock::new(SimRate::DEFAULT)));
    config.cut_every = K;
    let mut bad = ReplicaImpl::new(world, config).expect("a replica");
    bad.advance(Frame(48), &mut s);
    let c16 = cut(&all, 2, 16);
    let r16 = good.live_root(Frame(16)).expect("live");
    assert_eq!(bad.finalize(&FinalCut { cut: c16, root: r16, voters: vec![] }, &mut s), Finalized::Agreed);
    let c32 = cut(&all, 4, 32);
    let r32 = good.live_root(Frame(32)).expect("live");
    let fc32 = FinalCut { cut: c32.clone(), root: r32, voters: vec![] };
    let Finalized::Diverged { mine } = bad.finalize(&fc32, &mut s) else { panic!("the glitch shows at 32") };
    assert_ne!(mine, r32);
    assert_eq!(bad.tree(&mine).map(|t| t.root()), Some(mine), "my diverged root stays held");
    assert_eq!(bad.replay_to(&c32, &mut s), Ok(r32));
    assert_eq!(bad.replay_to(&c32, &mut s), Ok(r32), "asked again, at once");
    assert_eq!(bad.frame(), Frame(48));
    assert_eq!(bad.finalize(&fc32, &mut s), Finalized::Agreed);
    let kept: Vec<_> = roots(&good, 48).into_iter().filter(|r| r.0 >= 32).collect();
    assert_eq!(roots(&bad, 48), kept, "from the floor on, the roots are the good replica's");
    assert!(bad.tree(&mine).is_none(), "released once agreed");
}

/// A cut that covers only void entries stamped below the floor (@coordinator-opus): the source never
/// shows them, and the cut still has its root, the one without them. A count of the entries it sees would refuse it.
#[test]
fn a_cut_covering_only_void_entries_below_the_floor_has_a_root() {
    let (mut a, mut s) = reference(48);
    let all = scene();
    let root16 = a.live_root(Frame(16)).expect("live");
    let c16 = cut(&all, 2, 16);
    assert_eq!(a.finalize(&FinalCut { cut: c16, root: root16, voters: vec![] }, &mut s), Finalized::Agreed);
    // A third author's entries stamped at 10 and 12, below the floor: void, so the source leaves them out.
    let keys = TestKeys::new(7, 3);
    let mut late = EntryBuilder::new(&keys, 2);
    let push = Bytes::from(encode(&[MockOp::Push { id: id(10), impulse: [1.0, 0.0, 0.0] }]));
    let (_, last) = (late.next(Class::Edit, Frame(10), push.clone()), late.next(Class::Edit, Frame(12), push));
    // The cut at 20 covers nothing new but them: no entry of the scene is stamped in 16..20.
    let mut c20 = cut(&all, 3, 20);
    c20.heads.push(ChainHead {
        chain: last.entry.chain,
        author: last.entry.author,
        seq: last.entry.seq,
        entry: last.id(),
    });
    c20.heads.sort();
    let (mut w, mut ws) = reference(48);
    let want = w.root_at_cut(&cut(&all, 3, 20), &mut ws).expect("the reference's root");
    assert_eq!(a.root_at_cut(&c20, &mut s), Ok(want));
}

/// The reference replica at 48, with a third author's entry stamped at 20 that the cut at 24 does not cover.
fn with_late_entry_at_20() -> (ReplicaImpl<MockWorld>, Source, SignedEntry) {
    let (mut a, mut s) = reference(48);
    let keys = TestKeys::new(7, 3);
    let late = EntryBuilder::new(&keys, 2).next(Class::Edit, Frame(20), Bytes::from(encode(&[ball(13, 2.0, 1.0)])));
    s.add(late.clone());
    a.advance(Frame(48), &mut s);
    (a, s, late)
}

/// S3 voids the late entry as the cut at 24 goes final, and reports a change from `reported`.
fn void(s: &mut Source, late: &SignedEntry, reported: Frame) {
    s.entries.get_mut(&late.entry.stamp).expect("stamped").retain(|e| e.id() != late.id());
    s.changed = Some(reported);
}

/// A late entry above the floor that the cut does not cover is voided as the cut goes final (often by the vote that
/// completes the quorum, inside `drive`), and S3 reports the change below the new floor (@chunks-reviewer's M1 on
/// 40f9cca4, from its probe). `finalize` re-bases the live state on the cut-exact floor at once (option (a),
/// @coordinator-opus), and the report inside [g, floor) is excused, once.
#[test]
fn a_late_entry_voided_as_the_cut_goes_final_is_no_fault_and_leaves_no_wrong_state() {
    let (clean, _) = reference(48);
    let quorum = clean.live_root(Frame(24)).expect("live");
    let (mut a, mut s, late) = with_late_entry_at_20();
    let f0 = a.faults();
    void(&mut s, &late, Frame(20));
    assert_eq!(
        a.finalize(&FinalCut { cut: cut(&scene(), 3, 24), root: quorum, voters: vec![] }, &mut s),
        Finalized::Agreed
    );
    // Straight after finalize the live state is the cut-exact floor: the roots and the outcome that held the void
    // entry are gone, and the cut's entries in [g, floor) have the cut-exact history's outcomes.
    assert_eq!((a.frame(), a.live_root(Frame(24)), a.live_root(Frame(32))), (Frame(24), Some(quorum), None));
    assert_eq!(a.outcome(&late.id()), None);
    let teleport = scene()[5].id();
    assert_eq!(a.outcome(&teleport).map(|o| o.0), Some(Frame(21)));
    assert_eq!(a.outcome(&teleport), clean.outcome(&teleport));
    // The next call re-simulates to the present, and S3's report inside [g, floor) is no fault.
    let adv = a.advance(Frame(48), &mut s);
    assert_eq!(a.faults(), f0, "a report inside [g, floor) is expected");
    assert_eq!((adv.rolled_back_to, adv.resimulated), (None, 24));
    for f in [24, 32, 40, 48] {
        assert_eq!(a.live_root(Frame(f)), clean.live_root(Frame(f)), "frame {f}");
    }
    // Once: the same report again is a fault.
    s.changed = Some(Frame(20));
    a.advance(Frame(48), &mut s);
    assert_eq!(a.faults(), f0 + 1);
}

/// The excuse covers only the span the finalize re-based: a change report below the floor outside [g, floor) is
/// still a protocol fault, and is not acted on (@design-reviewer).
#[test]
fn a_change_below_the_floor_outside_the_rebased_span_is_still_a_fault() {
    let (clean, _) = reference(48);
    let quorum = clean.live_root(Frame(24)).expect("live");
    let (mut a, mut s, late) = with_late_entry_at_20();
    let f0 = a.faults();
    void(&mut s, &late, Frame(5));
    assert_eq!(
        a.finalize(&FinalCut { cut: cut(&scene(), 3, 24), root: quorum, voters: vec![] }, &mut s),
        Finalized::Agreed
    );
    a.advance(Frame(48), &mut s);
    assert_eq!(a.faults(), f0 + 1);
    assert_eq!(a.live_root(Frame(48)), clean.live_root(Frame(48)));
}

/// A fault in a frame that both the live history and a re-step off it step counts once against the live history; the
/// re-step's count is the scratch's (@chunks-reviewer's L1 on 40f9cca4).
#[test]
fn a_re_step_off_the_live_history_counts_only_against_the_scratch() {
    let mut world = MockWorld::new(DomainId(0), SimRate::DEFAULT);
    world.fault_transforms_at(Frame(12));
    let mut config =
        ReplicaConfig::new(SimRate::DEFAULT, Duration::from_millis(133), Arc::new(MockClock::new(SimRate::DEFAULT)));
    config.cut_every = K;
    let mut a = ReplicaImpl::new(world, config).expect("a replica");
    let mut s = Source::default();
    scene().into_iter().for_each(|e| s.add(e));
    // A third author's push at 10, which the cut at 24 does not cover: the cut's root is re-stepped from before 10.
    let keys = TestKeys::new(7, 3);
    let push = Bytes::from(encode(&[MockOp::Push { id: id(10), impulse: [0.2, 0.0, 0.0] }]));
    s.add(EntryBuilder::new(&keys, 2).next(Class::Edit, Frame(10), push));
    a.advance(Frame(32), &mut s);
    assert_eq!(a.faults(), 1, "frame 12 stepped once, live");
    a.root_at_cut(&cut(&scene(), 3, 24), &mut s).expect("a root");
    assert_eq!(a.faults(), 1, "the re-step's fault is the scratch's");
    assert_eq!(a.scratch().faults, 1);
}

/// The replica of `rung_one_repairs_a_transient_fault`: `Cv` at 16, a transient fault at 20, diverged at the final cut
/// at 32. Returns it, its source, the cut at 32 and the quorum's root there, and its own diverged root.
fn diverged_at_32() -> (ReplicaImpl<MockWorld>, Source, Cut, Hash, Hash) {
    let all = scene();
    let (good, _) = reference(48);
    let mut s = Source::default();
    all.iter().cloned().for_each(|e| s.add(e));
    let world = MockWorld::new(DomainId(0), SimRate::DEFAULT);
    world.glitch_at(Frame(20));
    let mut config =
        ReplicaConfig::new(SimRate::DEFAULT, Duration::from_millis(133), Arc::new(MockClock::new(SimRate::DEFAULT)));
    config.cut_every = K;
    let mut bad = ReplicaImpl::new(world, config).expect("a replica");
    bad.advance(Frame(48), &mut s);
    let r16 = good.live_root(Frame(16)).expect("live");
    assert_eq!(bad.finalize(&FinalCut { cut: cut(&all, 2, 16), root: r16, voters: vec![] }, &mut s), Finalized::Agreed);
    let (c32, r32) = (cut(&all, 4, 32), good.live_root(Frame(32)).expect("live"));
    let Finalized::Diverged { mine } = bad.finalize(&FinalCut { cut: c32.clone(), root: r32, voters: vec![] }, &mut s)
    else {
        panic!("the glitch shows at 32")
    };
    (bad, s, c32, r32, mine)
}

/// Rung 1's retention (S3 v1.2, @coordinator-opus): `finalize` tells S3 what it still needs, `Cv`'s
/// frame and the floor's. When a cap has trimmed past `Cv` anyway, `replay_to` answers `NotKept` before restoring
/// anything, never a root from empty frames: the live state is unchanged, and the diverged root rung 2 diffs against
/// (through `tree` and `physics_image`) stays held. The ladder goes on to rung 2 (pe-diverge, ladder.rs).
#[test]
fn a_replay_from_below_what_s3_keeps_is_not_kept_and_leaves_rung_2_its_root() {
    let (mut bad, mut s, c32, _, mine) = diverged_at_32();
    assert_eq!(s.asked, Some((Frame(16), Frame(32))), "Cv's frame, and the floor's, which the divergence moved");
    let live: Vec<_> = roots(&bad, 48);
    s.trim_below(Frame(24));
    assert_eq!(bad.replay_to(&c32, &mut s), Err(RootError::NotKept));
    assert_eq!(bad.replay_to(&c32, &mut s), Err(RootError::NotKept), "asked again, still no root");
    assert_eq!((bad.frame(), roots(&bad, 48)), (Frame(48), live), "the live state is unchanged");
    assert_eq!(bad.tree(&mine).map(|t| t.root()), Some(mine), "rung 2 diffs against my diverged root");
    assert!(bad.physics_image(&mine).is_some());
    // Control: with the entries since Cv kept, rung 1 cures it (rung_one_repairs_a_transient_fault).
    let (mut kept, mut ks, c32, r32, _) = diverged_at_32();
    assert_eq!(kept.replay_to(&c32, &mut ks), Ok(r32));
}

/// A frame at or above the floor that `advance` must re-simulate and S3 no longer keeps (its time cap, for a replica
/// far behind its final cut) is never stepped empty (S3 v1.2 round 2): the live history stops there, with one fault
/// however many calls follow, and the view keeps the last state it published. `root_at_cut` answers `Damaged` for any
/// cut past the stop, even one it gave a root for before, and so does `finalize`, so rung 2 is due at once.
#[test]
fn a_frame_at_or_above_the_floor_no_longer_kept_stops_advance() {
    let (mut a, mut s) = reference(48);
    let (c32, c40) = (cut(&scene(), 4, 32), cut(&scene(), 5, 40));
    let r32 = a.live_root(Frame(32)).expect("live");
    assert!(a.root_at_cut(&c40, &mut s).is_ok(), "a root at 40, before the trim");
    let root16 = a.live_root(Frame(16)).expect("live");
    let fc16 = FinalCut { cut: cut(&scene(), 2, 16), root: root16, voters: vec![] };
    assert_eq!(a.finalize(&fc16, &mut s), Finalized::Agreed);
    assert_eq!(s.asked, Some((Frame(16), Frame(16))));
    let (f0, shown) = (a.faults(), a.view().frame);
    s.trim_below(Frame(30));
    s.changed = Some(Frame(26));
    let adv = a.advance(Frame(48), &mut s);
    let from = adv.rolled_back_to.expect("rolled back");
    assert!((Frame(16)..Frame(30)).contains(&from));
    assert_eq!((a.frame(), adv.resimulated, adv.caught_up), (from, 0, false), "stopped at the first frame");
    for _ in 0..3 {
        a.advance(Frame(48), &mut s);
    }
    assert_eq!((a.frame(), a.faults() - f0), (from, 1), "one fault per stop, not one per call");
    assert_eq!(a.view().frame, shown, "the view keeps the last state published");
    assert_eq!(a.root_at_cut(&c40, &mut s), Err(RootError::Damaged), "no root past the stop, not even the one given");
    assert_eq!(a.finalize(&FinalCut { cut: c32, root: r32, voters: vec![] }, &mut s), Finalized::Damaged);
    assert_eq!((a.floor_frame(), a.faults() - f0), (Frame(16), 1));
}

/// An S3 fault: a frame at or above `kept_from` answers `None`. The live history stops there, as for a trim. When S3
/// answers for it again, the replica steps on with no further fault and gives roots past it again.
#[test]
fn a_stop_ends_when_s3_answers_for_the_frame_again() {
    let (clean, _) = reference(48);
    let (mut a, mut s) = reference(48);
    let f0 = a.faults();
    s.withheld = Some(Frame(28));
    s.changed = Some(Frame(26));
    a.advance(Frame(48), &mut s);
    assert_eq!((a.frame(), a.faults() - f0), (Frame(28), 1));
    let c32 = cut(&scene(), 4, 32);
    assert_eq!(a.root_at_cut(&c32, &mut s), Err(RootError::Damaged));
    s.withheld = None;
    assert!(a.advance(Frame(48), &mut s).caught_up);
    assert_eq!((roots(&a, 48), a.faults() - f0), (roots(&clean, 48), 1));
    assert_eq!(a.root_at_cut(&c32, &mut s).ok(), clean.live_root(Frame(32)));
}

/// A running replay meets a frame below the floor that S3's byte cap trimmed after the replay began (S3 may trim
/// there at any time). The replay is abandoned, with no fault, and asked again it answers `NotKept`, so the ladder
/// goes to rung 2. The member does not freeze below its floor: the live state goes back to the floor, which is still
/// kept, and simulates on from there (@design-reviewer's Medium on b3c2d18f and 3d8e9442).
#[test]
fn a_trim_below_the_floor_during_a_replay_abandons_it_and_the_member_simulates_on() {
    let (mut bad, mut s, c32, _, mine) = diverged_at_32();
    let before = roots(&bad, 48);
    bad.set_budget(Duration::from_millis(1));
    assert_eq!(bad.replay_to(&c32, &mut s), Err(RootError::Pending), "two frames from Cv at 16");
    s.trim_below(Frame(20));
    let f0 = bad.faults();
    let adv = bad.advance(Frame(48), &mut s);
    assert_eq!((bad.frame(), adv.caught_up, bad.faults() - f0), (Frame(32), false, 0), "back on the floor");
    assert_eq!(bad.replay_to(&c32, &mut s), Err(RootError::NotKept));
    bad.set_budget(Duration::from_secs(1));
    assert!(bad.advance(Frame(48), &mut s).caught_up);
    assert_eq!(roots(&bad, 48), before, "re-simulated from the floor: the history it had before the replay");
    assert_eq!(bad.faults() - f0, 0);
    assert_eq!(bad.tree(&mine).map(|t| t.root()), Some(mine), "rung 2 still diffs against my diverged root");
    assert_eq!(bad.root_at_cut(&cut(&scene(), 5, 40), &mut s).ok(), bad.live_root(Frame(40)), "it votes again");
}

/// A replay at a cut above the floor (a root split's, with `Cv` at the floor) meets a frame at or above the floor that
/// S3's time cap trimmed while it ran. The frame stops it as it stops `advance`, with one fault. Asked again,
/// `replay_to` answers `Damaged` when the cut's root needs that frame, and the root it took when the frame is past the
/// cut's (S2 v1.2 round 3, @engine-architect).
#[test]
fn a_trim_at_or_above_the_floor_during_a_replay_stops_it() {
    let agreed_at_16 = || {
        let (mut a, mut s) = reference(48);
        let fc16 = FinalCut { cut: cut(&scene(), 2, 16), root: a.live_root(Frame(16)).expect("live"), voters: vec![] };
        assert_eq!(a.finalize(&fc16, &mut s), Finalized::Agreed);
        (a, s)
    };
    let (c24, c32) = (cut(&scene(), 3, 24), cut(&scene(), 4, 32));
    // The cut's root needs the frame: the replay to 32 is at 18 when the cap trims below 20.
    let (mut a, mut s) = agreed_at_16();
    a.set_budget(Duration::from_millis(1));
    assert_eq!(a.replay_to(&c32, &mut s), Err(RootError::Pending), "two frames from Cv at 16");
    s.trim_below(Frame(20));
    let f0 = a.faults();
    a.advance(Frame(48), &mut s);
    assert_eq!((a.frame(), a.faults() - f0), (Frame(18), 1), "stopped, one fault");
    assert_eq!(a.replay_to(&c32, &mut s), Err(RootError::Damaged));
    assert_eq!(a.root_at_cut(&c24, &mut s), Err(RootError::Damaged));
    // The frame is past the cut's: the replay to 24 took its root there and is at 26 when the cap trims below 30.
    let (clean, _) = reference(48);
    let (mut b, mut s) = agreed_at_16();
    b.set_budget(Duration::from_millis(167));
    assert_eq!(b.replay_to(&c24, &mut s).ok(), clean.live_root(Frame(24)), "ten frames from 16, past 24");
    s.trim_below(Frame(30));
    b.advance(Frame(48), &mut s);
    assert_eq!(b.frame(), Frame(26));
    assert_eq!(b.replay_to(&c24, &mut s).ok(), clean.live_root(Frame(24)), "the root it took stands");
    assert_eq!(b.root_at_cut(&c32, &mut s), Err(RootError::Damaged));
}

/// S3's time cap trimmed frames above my floor before I finalized a cut over them (a member far behind). No root
/// there will ever come, so `finalize` answers `Damaged` instead of `NotReady` for ever, with one fault however often
/// it is asked. The floor stays, and `root_at_cut` answers `Damaged` too, so rung 2 is due at once (@chunks-reviewer's
/// probe on 3d8e9442, zz_probe_cap_trim.rs; option (a) by @coordinator-opus).
#[test]
fn a_cut_over_frames_trimmed_above_the_floor_is_damaged_not_a_silent_stall() {
    let (clean, _) = reference(48);
    let quorum = clean.live_root(Frame(24)).expect("live");
    let (mut a, mut s) = reference(48);
    s.trim_below(Frame(5));
    let f0 = a.faults();
    let fc = FinalCut { cut: cut(&scene(), 3, 24), root: quorum, voters: vec![] };
    for _ in 0..3 {
        assert_eq!(a.finalize(&fc, &mut s), Finalized::Damaged);
        a.advance(Frame(48), &mut s);
    }
    assert_eq!((a.floor_frame(), a.faults() - f0), (Frame(0), 1), "the floor stays; one fault, not one per call");
    assert_eq!(a.root_at_cut(&cut(&scene(), 3, 24), &mut s), Err(RootError::Damaged));
    assert_eq!((a.faults() - f0, s.asked), (1, None), "nothing finalized, nothing asked");
}

/// The donor's import at its cut: every leaf as a record (nothing absent), its physics image, its root as the target.
fn import_from(donor: &mut ReplicaImpl<MockWorld>, ds: &mut Source, cut: &Cut) -> CutImport {
    let root = donor.root_at_cut(cut, ds).expect("the donor's root");
    let records = donor.tree(&root).expect("held").export(&donor.world().leaves());
    CutImport { cut: cut.clone(), target_root: root, records, absent: Vec::new(), physics: donor.physics_image(&root) }
}

/// Ask until the import has re-simulated to the present, advancing between asks; how many times it was `Pending`.
fn import_until_done(r: &mut ReplicaImpl<MockWorld>, s: &mut Source, import: &CutImport, present: u64) -> (Hash, u32) {
    let mut pending = 0;
    for _ in 0..100 {
        match r.import_at_cut(&GroupSel::Domain, import.clone(), s) {
            Err(ImportError::Pending) => {
                pending += 1;
                r.advance(Frame(present), s);
            }
            done => return (done.expect("the import"), pending),
        }
    }
    panic!("the import never finished")
}

/// Rung 2 (S2 v1.2 round 3): a replica that diverged at 32 cannot replay from `Cv` (the entries since are trimmed),
/// so it imports the quorum's state at 32 from a donor: checked, then re-simulated to the present in budget-sized
/// steps (`Pending` meanwhile). Its root at 32 is then the imported one with no re-step below it, finalize agrees,
/// and it goes on with the good replica's roots.
#[test]
fn rung_2_imports_the_quorum_s_state_and_finalize_agrees() {
    let (mut bad, mut s, c32, r32, _) = diverged_at_32();
    s.trim_below(Frame(24));
    assert_eq!(bad.replay_to(&c32, &mut s), Err(RootError::NotKept), "rung 1 cannot: rung 2 is due");
    let (mut good, mut gs) = reference(48);
    let import = import_from(&mut good, &mut gs, &c32);
    assert_eq!(import.target_root, r32);
    bad.set_budget(Duration::from_millis(50));
    let (root, pending) = import_until_done(&mut bad, &mut s, &import, 48);
    assert_eq!(root, r32);
    assert!(pending > 0, "16 frames to re-simulate at 3 a call");
    assert_eq!(bad.import_at_cut(&GroupSel::Domain, import, &mut s), Ok(r32), "asked again, at once");
    assert_eq!(bad.frame(), Frame(48));
    assert_eq!(bad.root_at_cut(&c32, &mut s), Ok(r32));
    assert_eq!(bad.finalize(&FinalCut { cut: c32, root: r32, voters: vec![] }, &mut s), Finalized::Agreed);
    let from_32 = |r: &ReplicaImpl<MockWorld>, to| roots(r, to).into_iter().filter(|r| r.0 >= 32).collect::<Vec<_>>();
    assert_eq!(from_32(&bad, 48), from_32(&good, 48));
    bad.set_budget(Duration::from_secs(1));
    bad.advance(Frame(64), &mut s);
    good.advance(Frame(64), &mut gs);
    assert_eq!(from_32(&bad, 64), from_32(&good, 64), "and it goes on with the good replica");
}

/// An import that does not check changes nothing: a wrong target root, and an image whose bytes are not its check
/// hash's, are refused before the live state is touched.
#[test]
fn an_import_that_does_not_check_changes_nothing() {
    let (mut bad, mut s, c32, _, _) = diverged_at_32();
    let (mut good, mut gs) = reference(48);
    let import = import_from(&mut good, &mut gs, &c32);
    let live = (bad.frame(), roots(&bad, 48), bad.world().root());
    let mut wrong_root = import.clone();
    wrong_root.target_root = Hash([7; 32]);
    let got = bad.import_at_cut(&GroupSel::Domain, wrong_root, &mut s);
    assert!(matches!(got, Err(ImportError::RootMismatch { .. })), "a wrong target: {got:?}");
    let mut bad_bytes = import.clone();
    let image = bad_bytes.physics.as_mut().expect("an image");
    let mut bytes = image.bytes.to_vec();
    let at = bytes.len() / 2;
    bytes[at] ^= 0x5a;
    image.bytes = Bytes::from(bytes);
    assert_eq!(bad.import_at_cut(&GroupSel::Domain, bad_bytes, &mut s), Err(ImportError::PhysicsCheck));
    assert_eq!((bad.frame(), roots(&bad, 48), bad.world().root()), live, "the live state is unchanged");
}

/// An import below my floor is refused before anything changes: nothing there changes again (@chunks-reviewer's L1
/// on 05903341).
#[test]
fn an_import_below_the_floor_is_not_kept() {
    let (mut bad, mut s, _, _, _) = diverged_at_32();
    let (mut good, mut gs) = reference(48);
    let import = import_from(&mut good, &mut gs, &cut(&scene(), 2, 16));
    let live = (bad.frame(), roots(&bad, 48), bad.world().root());
    assert_eq!(bad.import_at_cut(&GroupSel::Domain, import, &mut s), Err(ImportError::NotKept));
    assert_eq!((bad.frame(), roots(&bad, 48), bad.world().root()), live, "the live state is unchanged");
}

/// A snapshot held from before an import has the old physics world's image, which never restores into the new one
/// (@chunks-reviewer's L2 on 05903341). Here the replica imports at 32 before that cut is final, and the cut then goes
/// final with another root (the import was a superseded root's): the import ends, and `Cv` is still the one at 16,
/// held from before it. Rung 1 from that `Cv` answers `NotKept` up front, not `Damaged` from a refused restore.
#[test]
fn a_cv_held_from_before_an_import_is_not_kept() {
    let all = scene();
    let (mut r, mut s) = reference(48);
    let r16 = r.live_root(Frame(16)).expect("live");
    assert_eq!(r.finalize(&FinalCut { cut: cut(&all, 2, 16), root: r16, voters: vec![] }, &mut s), Finalized::Agreed);
    let (mut donor, mut ds) = reference(48);
    let c32 = cut(&all, 4, 32);
    let import = import_from(&mut donor, &mut ds, &c32);
    let (mine, _) = import_until_done(&mut r, &mut s, &import, 48);
    let superseding = FinalCut { cut: c32, root: Hash([9; 32]), voters: vec![] };
    assert_eq!(r.finalize(&superseding, &mut s), Finalized::Diverged { mine });
    let before = (r.faults(), r.frame(), roots(&r, 48), r.world().root());
    assert_eq!(r.replay_to(&cut(&all, 6, 48), &mut s), Err(RootError::NotKept));
    assert_eq!((r.faults(), r.frame(), roots(&r, 48), r.world().root()), before, "nothing changed or counted");
}

/// A root of mine held from before an import, and taken again after it, is held again with the new physics world's
/// image (@chunks-reviewer's L2 on 05903341). Here the replica gave its root at 40, then imported its own state at 32
/// (nothing to repair, so the same root at 40 comes again). Finalizing at 40 makes that snapshot the floor's keyframe:
/// a late entry above it rolls back there with no fault, and rung 1 replays from it.
#[test]
fn a_root_taken_again_after_an_import_holds_the_new_world_s_image() {
    let all = scene();
    let (mut r, mut s) = reference(48);
    let c40 = cut(&all, 5, 40);
    let r40 = r.root_at_cut(&c40, &mut s).expect("my root at 40");
    let (mut donor, mut ds) = reference(48);
    let import = import_from(&mut donor, &mut ds, &cut(&all, 4, 32));
    import_until_done(&mut r, &mut s, &import, 48);
    assert_eq!(r.root_at_cut(&c40, &mut s), Ok(r40), "the same state again");
    assert_eq!(r.finalize(&FinalCut { cut: c40, root: r40, voters: vec![] }, &mut s), Finalized::Agreed);
    let keys = TestKeys::new(7, 3);
    let late = EntryBuilder::new(&keys, 2).next(Class::Edit, Frame(42), Bytes::from(encode(&[ball(13, 2.0, 1.0)])));
    s.add(late.clone());
    let f0 = r.faults();
    let adv = r.advance(Frame(48), &mut s);
    assert_eq!((adv.rolled_back_to, r.faults()), (Some(Frame(40)), f0), "the floor's keyframe restores");
    let mut ws = Source::default();
    all.iter().cloned().chain([late]).for_each(|e| ws.add(e));
    let mut w = replica();
    w.advance(Frame(48), &mut ws);
    assert_eq!(r.live_root(Frame(48)), w.live_root(Frame(48)), "with the late entry");
    let clean = donor.live_root(Frame(48)).expect("live");
    assert_eq!(r.replay_to(&cut(&all, 6, 48), &mut s), Ok(clean), "rung 1 from Cv at 40, without the late entry");
}

/// Joining: a replica with no root of its own at the cut imports the group's whole state from empty, at once (it is
/// behind the cut, so nothing to re-simulate), then simulates on with the good replica's roots.
#[test]
fn a_joiner_imports_the_whole_state_from_empty() {
    let (mut good, mut gs) = reference(48);
    let c32 = cut(&scene(), 4, 32);
    let import = import_from(&mut good, &mut gs, &c32);
    let mut s = Source::default();
    scene().into_iter().for_each(|e| s.add(e));
    // The joiner's S3 held these entries before it joined: no change to report below its floor.
    s.take_changed_from();
    let mut joiner = replica();
    assert_eq!(joiner.import_at_cut(&GroupSel::Domain, import.clone(), &mut s), Ok(import.target_root));
    assert_eq!(joiner.frame(), Frame(32));
    let fc32 = FinalCut { cut: c32, root: import.target_root, voters: vec![] };
    assert_eq!(joiner.finalize(&fc32, &mut s), Finalized::Agreed);
    joiner.advance(Frame(48), &mut s);
    let from_32 = |r: &ReplicaImpl<MockWorld>| roots(r, 48).into_iter().filter(|r| r.0 >= 32).collect::<Vec<_>>();
    assert_eq!(from_32(&joiner), from_32(&good));
    assert_eq!(joiner.faults(), 0);
}

// ---- the apply order within a frame (world-model.md 8.1; @coordinator-opus's ruling) ----------------

/// By phase, then in agreed order within a phase, and nothing dropped. There are enough entries that a sort that is
/// not stable would reorder those of one phase (@engine-architect).
#[test]
fn apply_order_is_by_phase_then_agreed_order() {
    let keys = TestKeys::new(9, 1);
    let mut author = EntryBuilder::new(&keys, 0);
    let entries: Vec<SignedEntry> = (0..256u32)
        .map(|i| {
            let class = if i % 3 == 0 || i % 7 == 0 { Class::Input } else { Class::Edit };
            author.next(class, Frame(1), Bytes::from(i.to_le_bytes().to_vec()))
        })
        .collect();
    let seqs = |order: Vec<&SignedEntry>| order.iter().map(|e| e.entry.seq).collect::<Vec<_>>();
    let (inputs, edits): (Vec<&SignedEntry>, Vec<&SignedEntry>) =
        entries.iter().partition(|e| e.entry.class == Class::Input);
    let want = seqs(inputs.into_iter().chain(edits).collect());
    assert_eq!(seqs(apply_order(&entries, MockWorld::phase)), want);
    assert_eq!(seqs(apply_order(&entries, |_| Phase::First)), seqs(entries.iter().collect()), "one phase: agreed");
}

/// Within a frame the mock's inputs apply before its edits, whatever their agreed order. An edit and an input stamped
/// in the same frame teleport one ball to two places, and in either agreed order the edit's place stands. A cut's root
/// over the frame is the live one, and finalize finds that the frame stepped exactly the cut's entries, so it does
/// not re-base the live state on the floor.
#[test]
fn a_frame_applies_its_inputs_before_its_edits() {
    let (by_edit, by_input) = ([2.0, 3.0, 0.0], [-2.0, 3.0, 0.0]);
    let run = |input_author: usize| {
        let keys = TestKeys::new(9, 2);
        let mut authors = [EntryBuilder::new(&keys, 0), EntryBuilder::new(&keys, 1)];
        let ops = |ops: &[MockOp]| Bytes::from(encode(ops));
        let teleport = |pos| ops(&[MockOp::Teleport { id: id(10), pos }]);
        let entries = vec![
            authors[0].next(Class::Edit, Frame(0), ops(&[ball(10, 0.0, 3.0)])),
            authors[input_author].next(Class::Input, Frame(3), teleport(by_input)),
            authors[1 - input_author].next(Class::Edit, Frame(3), teleport(by_edit)),
        ];
        let mut s = Source::default();
        entries.iter().cloned().for_each(|e| s.add(e));
        let mut r = replica();
        r.advance(Frame(16), &mut s);
        let c8 = cut(&entries, 1, 8);
        let live = r.live_root(Frame(8)).expect("live");
        assert_eq!(r.root_at_cut(&c8, &mut s), Ok(live));
        assert_eq!(r.finalize(&FinalCut { cut: c8, root: live, voters: vec![] }, &mut s), Finalized::Agreed);
        assert_eq!((r.frame(), r.faults()), (Frame(16), 0), "no re-base on the floor");
        r.world().position(id(10)).expect("the ball")
    };
    let (a, b) = (run(0), run(1));
    assert_eq!(a, b, "either agreed order");
    assert!((a[0] - by_edit[0]).abs() < 0.01 && a[2].abs() < 0.01, "the edit's place stands: {a:?}");
}

/// `agreed_state` is the world model's state at `Cv` (@engine-architect, for pe-api's
/// `WorldSource::final_`). There is none before the first agreed finalize. A finalize that diverges leaves it at the
/// last agreed cut, though the floor moves on, and the finalize that agrees after rung 1's repair moves it.
#[test]
fn the_agreed_state_is_cv_s() {
    assert!(replica().agreed_state().is_none());
    let (good, _) = reference(48);
    let (mut bad, mut s, c32, r32, _) = diverged_at_32();
    let (frame, state) = bad.agreed_state().expect("Cv at 16");
    assert_eq!((frame, bad.floor_frame()), (Frame(16), Frame(32)), "not the floor's");
    assert_eq!(Some(state.root()), good.live_root(Frame(16)));
    assert_eq!(bad.replay_to(&c32, &mut s), Ok(r32));
    assert_eq!(bad.finalize(&FinalCut { cut: c32, root: r32, voters: vec![] }, &mut s), Finalized::Agreed);
    let (frame, state) = bad.agreed_state().expect("Cv at 32");
    assert_eq!((frame, state.root()), (Frame(32), r32));
}

// ---- the world's settings on import (world-model.md 6.6; @coordinator-opus) --------------------------

/// A replica of `world`, at its rate, that had every entry from the start, run to `to`.
fn reference_in(world: MockWorld, to: u64) -> (ReplicaImpl<MockWorld>, Source) {
    let rate = world.settings().rate;
    let mut config = ReplicaConfig::new(rate, Duration::from_millis(133), Arc::new(MockClock::new(rate)));
    config.cut_every = K;
    let mut r = ReplicaImpl::new(world, config).expect("a replica");
    let mut s = Source::default();
    scene().into_iter().for_each(|e| s.add(e));
    r.advance(Frame(to), &mut s);
    (r, s)
}

/// A world whose gravity and origin are not the defaults, imported by a default joiner (@coordinator-opus, item 6). Besides the rate, which a joiner never adopts (the next test), gravity and origin are every
/// simulation setting an M1 world may choose (world-model.md 6.6), so this is item 6's "every setting non-default"
/// test for the replica. The joiner's new physics world takes them from the imported world, not from its own
/// construction, so the import checks and the joiner steps on with the host's roots. When a transient fault then
/// diverges the joiner, rung 1's replay re-simulates with the imported settings and agrees with the host again. An
/// import without the config record keeps the joiner's own settings, and the root check refuses it.
#[test]
fn a_joiner_takes_gravity_and_origin_from_the_world_it_imports_and_rung_one_agrees_with_the_host() {
    let origin = Pos::ORIGIN.checked_add(Offset([40.0, 0.0, -25.0])).expect("an origin");
    let host_world = MockWorld::new(DomainId(0), SimRate::DEFAULT).with_settings([0.0, -3.0, 0.0], origin);
    let (mut host, mut hs) = reference_in(host_world, 64);
    let c32 = cut(&scene(), 4, 32);
    let import = import_from(&mut host, &mut hs, &c32);
    let mut s = Source::default();
    scene().into_iter().for_each(|e| s.add(e));
    // The joiner's S3 held these entries before it joined: no change to report below its floor.
    s.take_changed_from();
    let mut joiner = replica();
    let before = (joiner.frame(), joiner.world().root());
    let mut no_config = import.clone();
    no_config.records.retain(|r| r.key != LeafKey::Config);
    let got = joiner.import_at_cut(&GroupSel::Domain, no_config, &mut s);
    assert!(matches!(got, Err(ImportError::RootMismatch { .. })), "my own settings are not the world's: {got:?}");
    assert_eq!((joiner.frame(), joiner.world().root()), before, "the live state is unchanged");
    assert_eq!(joiner.import_at_cut(&GroupSel::Domain, import.clone(), &mut s), Ok(import.target_root));
    assert_eq!(joiner.world().settings(), host.world().settings(), "the host's gravity and origin");
    let fc32 = FinalCut { cut: c32, root: import.target_root, voters: vec![] };
    assert_eq!(joiner.finalize(&fc32, &mut s), Finalized::Agreed);
    joiner.world().glitch_at(Frame(36));
    joiner.advance(Frame(48), &mut s);
    let (c40, r40) = (cut(&scene(), 5, 40), host.live_root(Frame(40)).expect("live"));
    let fc40 = FinalCut { cut: c40.clone(), root: r40, voters: vec![] };
    let Finalized::Diverged { mine } = joiner.finalize(&fc40, &mut s) else { panic!("the glitch shows at 40") };
    assert_ne!(mine, r40);
    assert_eq!(joiner.replay_to(&c40, &mut s), Ok(r40), "rung 1 re-simulates with the imported settings");
    assert_eq!(joiner.finalize(&fc40, &mut s), Finalized::Agreed);
    joiner.advance(Frame(64), &mut s);
    let from_40 = |r: &ReplicaImpl<MockWorld>| roots(r, 64).into_iter().filter(|r| r.0 >= 40).collect::<Vec<_>>();
    assert_eq!(from_40(&joiner), from_40(&host), "the host's roots from the repaired cut on");
    assert_eq!(joiner.faults(), 0);
}

/// A world runs at one rate (@coordinator-opus, item 3): a 30 Hz world's state imported by a 60 Hz
/// joiner is `RateMismatch`, a fault to report and never a rate to adopt, and the joiner's live state is unchanged.
#[test]
fn an_import_of_a_world_at_another_rate_is_refused_and_changes_nothing() {
    let thirty = SimRate::new(30).expect("30 Hz");
    let (mut host, mut hs) = reference_in(MockWorld::new(DomainId(0), thirty), 48);
    let c32 = cut(&scene(), 4, 32);
    let import = import_from(&mut host, &mut hs, &c32);
    let mut s = Source::default();
    scene().into_iter().for_each(|e| s.add(e));
    let mut joiner = replica();
    joiner.advance(Frame(8), &mut s);
    let live = (joiner.frame(), roots(&joiner, 8), joiner.world().root());
    let got = joiner.import_at_cut(&GroupSel::Domain, import, &mut s);
    assert_eq!(got, Err(ImportError::RateMismatch { world: thirty, mine: SimRate::DEFAULT }));
    assert_eq!((joiner.frame(), roots(&joiner, 8), joiner.world().root()), live, "the live state is unchanged");
    assert_eq!(joiner.world().settings().rate, SimRate::DEFAULT);
}

// ---- the change records (S5 v1.5) ------------------------------------------------------------------------------

/// A reader of the frame views, as pe-render's notices and WorldApi's `watch` read them (S5 v1.5): each entry's
/// newest record, told once `final_frame` passes its frame. View by view, it checks that `final_frame` never goes
/// back and that no record of an entry arrives after its verdict was told.
#[derive(Default)]
struct Reader {
    last: Option<Arc<FrameView>>,
    final_frame: Option<Frame>,
    newest: BTreeMap<EntryRef, StepRecord>,
    told: BTreeMap<EntryRef, Outcome>,
    /// How many records of each entry arrived, and how many of them were `Unknown`.
    records: BTreeMap<EntryRef, u32>,
    unknowns: BTreeMap<EntryRef, u32>,
    /// Each health the views showed, in order, repeats dropped.
    health: Vec<Health>,
}

impl Reader {
    /// Read the replica's view, if it published one since the last read.
    fn read(&mut self, r: &ReplicaImpl<MockWorld>) {
        let v = r.view();
        if self.last.as_ref().is_some_and(|l| Arc::ptr_eq(l, &v)) {
            return;
        }
        assert!(
            v.final_frame >= self.final_frame,
            "final_frame went back: {:?} to {:?}",
            self.final_frame,
            v.final_frame
        );
        for c in &v.changes {
            assert!(!self.told.contains_key(&c.entry), "a record after its verdict was told: {c:?}");
            *self.records.entry(c.entry).or_default() += 1;
            if matches!(c.outcome, Outcome::Unknown { .. }) {
                *self.unknowns.entry(c.entry).or_default() += 1;
            }
            self.newest.insert(c.entry, c.clone());
        }
        self.final_frame = v.final_frame;
        if let Some(ff) = v.final_frame {
            for (id, c) in self.newest.iter().filter(|(_, c)| c.frame < ff) {
                self.told.entry(*id).or_insert_with(|| c.outcome.clone());
            }
        }
        if self.health.last() != Some(&v.health) {
            self.health.push(v.health);
        }
        self.last = Some(v);
    }
}

/// The verdict the good replica gave `e`.
fn verdict(good: &ReplicaImpl<MockWorld>, e: &SignedEntry) -> Option<Outcome> {
    good.outcome(&e.id()).map(|o| o.1)
}

/// A late entry stepped live and then left out by the final cut (S5 v1.5; @coordinator-opus): the
/// reader has its `Dropped` record before `final_frame` passes its frame, and never a verdict for it. Every entry of
/// the cut is told the clean replica's verdict, once `final_frame` passes it and not before.
#[test]
fn a_late_entry_the_final_cut_leaves_out_reads_dropped_before_final_frame_passes() {
    let (clean, _) = reference(48);
    let quorum = clean.live_root(Frame(24)).expect("live");
    let mut s = Source::default();
    scene().into_iter().for_each(|e| s.add(e));
    let mut a = replica();
    let mut reader = Reader::default();
    a.advance(Frame(48), &mut s);
    reader.read(&a);
    assert_eq!((reader.final_frame, &reader.health[..]), (None, &[Health::Agreed][..]), "nothing final yet");
    let keys = TestKeys::new(7, 3);
    let late = EntryBuilder::new(&keys, 2).next(Class::Edit, Frame(20), Bytes::from(encode(&[ball(13, 2.0, 1.0)])));
    s.add(late.clone());
    a.advance(Frame(48), &mut s);
    reader.read(&a);
    assert!(matches!(reader.newest[&late.id()].outcome, Outcome::Applied { .. }), "stepped live");
    void(&mut s, &late, Frame(20));
    let fc24 = FinalCut { cut: cut(&scene(), 3, 24), root: quorum, voters: vec![] };
    assert_eq!(a.finalize(&fc24, &mut s), Finalized::Agreed);
    a.advance(Frame(49), &mut s);
    reader.read(&a);
    assert_eq!(reader.final_frame, Some(Frame(24)));
    assert_eq!(reader.told.get(&late.id()), Some(&Outcome::Dropped { reason: VoidReason::Late }));
    for e in scene() {
        let want = (e.entry.stamp < Frame(24)).then(|| verdict(&clean, &e)).flatten();
        assert_eq!(reader.told.get(&e.id()), want.as_ref(), "the entry at {:?}", e.entry.stamp);
    }
    assert_eq!(reader.health, vec![Health::Agreed]);
}

/// A replica whose world glitches at 20, so its root differs from the quorum's at 32, read from its first view.
fn diverged_at_32_read() -> (ReplicaImpl<MockWorld>, Source, FinalCut, Reader) {
    let all = scene();
    let (good, _) = reference(48);
    let mut s = Source::default();
    all.iter().cloned().for_each(|e| s.add(e));
    let world = MockWorld::new(DomainId(0), SimRate::DEFAULT);
    world.glitch_at(Frame(20));
    let mut config =
        ReplicaConfig::new(SimRate::DEFAULT, Duration::from_millis(133), Arc::new(MockClock::new(SimRate::DEFAULT)));
    config.cut_every = K;
    let mut bad = ReplicaImpl::new(world, config).expect("a replica");
    bad.set_budget(Duration::from_secs(10));
    let mut reader = Reader::default();
    bad.advance(Frame(48), &mut s);
    reader.read(&bad);
    let fc16 = FinalCut { cut: cut(&all, 2, 16), root: good.live_root(Frame(16)).expect("live"), voters: vec![] };
    assert_eq!(bad.finalize(&fc16, &mut s), Finalized::Agreed);
    let fc32 = FinalCut { cut: cut(&all, 4, 32), root: good.live_root(Frame(32)).expect("live"), voters: vec![] };
    assert!(matches!(bad.finalize(&fc32, &mut s), Finalized::Diverged { .. }), "the glitch shows at 32");
    bad.advance(Frame(49), &mut s);
    reader.read(&bad);
    // While it repairs, final_frame waits at the last agreed cut, and the view says so.
    assert_eq!(reader.final_frame, Some(Frame(16)), "it waits at the last agreed cut");
    assert_eq!(reader.health, vec![Health::Agreed, Health::Repairing { since: Frame(32) }]);
    let repairing: Vec<_> = all.iter().filter(|e| (16..32).contains(&e.entry.stamp.0)).collect();
    assert!(!repairing.is_empty() && repairing.iter().all(|e| !reader.told.contains_key(&e.id())));
    (bad, s, fc32, reader)
}

/// Rung 1 on a deliberately diverged replica (S5 v1.5; @coordinator-opus): the replay from my last
/// agreed cut re-steps the entries above it, and their records come before `final_frame` passes them, so no reader
/// ever had a verdict below `final_frame` other than the good replica's.
#[test]
fn after_rung_one_no_reader_ever_had_a_wrong_verdict_below_final_frame() {
    let (good, _) = reference(48);
    let (mut bad, mut s, fc32, mut reader) = diverged_at_32_read();
    let above: Vec<_> = scene().into_iter().filter(|e| (16..32).contains(&e.entry.stamp.0)).collect();
    let before: Vec<u32> = above.iter().map(|e| reader.records[&e.id()]).collect();
    assert_eq!(bad.replay_to(&fc32.cut, &mut s), Ok(fc32.root));
    assert_eq!(bad.finalize(&fc32, &mut s), Finalized::Agreed);
    bad.advance(Frame(50), &mut s);
    reader.read(&bad);
    assert_eq!(reader.final_frame, Some(Frame(32)));
    assert_eq!(reader.health.last(), Some(&Health::Agreed));
    for (e, n) in above.iter().zip(before) {
        assert!(reader.records[&e.id()] > n, "the replay re-recorded the entry at {:?}", e.entry.stamp);
    }
    for e in scene().iter().filter(|e| e.entry.stamp < Frame(32)) {
        assert_eq!(reader.told.get(&e.id()), verdict(&good, e).as_ref(), "the entry at {:?}", e.entry.stamp);
    }
}

/// Rung 2 on a deliberately diverged replica (S5 v1.5; @coordinator-opus): the import replaces the
/// state the entries above my last agreed cut were stepped on, so each of them reads `Unknown`, repaired at the cut,
/// exactly once, before `final_frame` passes it. S3 had trimmed the frame of one of them (21), and the cut's heads
/// still name it. The entries below my last agreed cut keep their verdicts.
#[test]
fn after_rung_two_each_affected_entry_reads_unknown_exactly_once() {
    let (mut good, mut gs) = reference(48);
    let (mut bad, mut s, fc32, mut reader) = diverged_at_32_read();
    s.trim_below(Frame(24));
    assert_eq!(bad.replay_to(&fc32.cut, &mut s), Err(RootError::NotKept), "rung 1 cannot: rung 2 is due");
    let import = import_from(&mut good, &mut gs, &fc32.cut);
    assert_eq!(bad.import_at_cut(&GroupSel::Domain, import.clone(), &mut s), Ok(fc32.root));
    assert_eq!(bad.import_at_cut(&GroupSel::Domain, import, &mut s), Ok(fc32.root), "asked again, at once");
    assert_eq!(bad.finalize(&fc32, &mut s), Finalized::Agreed);
    bad.advance(Frame(50), &mut s);
    reader.read(&bad);
    assert_eq!(reader.final_frame, Some(Frame(32)));
    assert_eq!(reader.health.last(), Some(&Health::Agreed));
    let repaired = Outcome::Unknown { repaired_at: fc32.cut.at };
    for e in scene() {
        let (id, f) = (e.id(), e.entry.stamp.0);
        match f {
            0..16 => assert_eq!(reader.told.get(&id), verdict(&good, &e).as_ref(), "the entry at {f}"),
            16..32 => {
                assert_eq!(reader.told.get(&id), Some(&repaired), "the entry at {f}");
                assert_eq!(reader.unknowns.get(&id), Some(&1), "the entry at {f}: once");
            }
            _ => assert_eq!(reader.told.get(&id), None, "the entry at {f} is past final_frame"),
        }
    }
}

/// A replica that had the scene and `extra` from the start, run to `to`: the quorum's roots for a cut over them.
fn clean_with(extra: &[SignedEntry], to: u64) -> ReplicaImpl<MockWorld> {
    let mut s = Source::default();
    scene().into_iter().chain(extra.iter().cloned()).for_each(|e| s.add(e));
    let mut r = replica();
    r.advance(Frame(to), &mut s);
    r
}

/// A replica over the scene and `extra`, run to 48 and read from its first view.
fn stepped_with(extra: &[SignedEntry]) -> (ReplicaImpl<MockWorld>, Source, Reader) {
    let mut s = Source::default();
    scene().into_iter().chain(extra.iter().cloned()).for_each(|e| s.add(e));
    let mut a = replica();
    let mut reader = Reader::default();
    a.advance(Frame(48), &mut s);
    reader.read(&a);
    (a, s, reader)
}

/// S3 voids `e` at the cut going final: absent from `entries_at` from then on, with `reason` if the cut includes it.
fn s3_voids(s: &mut Source, e: &SignedEntry, reason: Option<VoidReason>) {
    s.entries.get_mut(&e.entry.stamp).expect("stamped").retain(|x| x.id() != e.id());
    if let Some(r) = reason {
        s.voided.insert(e.id(), r);
    }
    s.changed = Some(e.entry.stamp);
}

/// A dropped entry reads S3's reason (@design-reviewer's Medium on f13cd045): the cut at 24 includes a back-stamped
/// entry (seq 2 stamped 16, after seq 1 stamped 18), S3 voids it as `BackStamped`, and the record copies that.
#[test]
fn a_dropped_entry_reads_the_reason_s3_voided_it_for() {
    let keys = TestKeys::new(7, 3);
    let mut x = EntryBuilder::new(&keys, 2);
    let first = x.next(Class::Edit, Frame(18), Bytes::from(encode(&[ball(14, 2.0, 1.0)])));
    let back = x.next(Class::Edit, Frame(16), Bytes::from(encode(&[ball(15, -2.0, 1.0)])));
    let quorum = clean_with(std::slice::from_ref(&first), 48).live_root(Frame(24)).expect("live");
    let (mut a, mut s, mut reader) = stepped_with(&[first.clone(), back.clone()]);
    assert!(matches!(reader.newest[&back.id()].outcome, Outcome::Applied { .. }), "stepped live");
    let mut all = scene();
    all.extend([first.clone(), back.clone()]);
    let c24 = cut(&all, 3, 24);
    assert!(c24.includes(&back.entry.chain, back.entry.seq), "the cut includes the back-stamped entry");
    s3_voids(&mut s, &back, Some(VoidReason::BackStamped));
    assert_eq!(a.finalize(&FinalCut { cut: c24, root: quorum, voters: vec![] }, &mut s), Finalized::Agreed);
    a.advance(Frame(49), &mut s);
    reader.read(&a);
    assert_eq!(reader.final_frame, Some(Frame(24)));
    assert_eq!(reader.told.get(&back.id()), Some(&Outcome::Dropped { reason: VoidReason::BackStamped }));
    assert!(matches!(reader.told.get(&first.id()), Some(Outcome::Applied { .. })), "its chain's first entry applies");
}

/// A pending entry reads `Late` whatever this replica saw of its chain (@design-reviewer's e1/e2 case on f13cd045):
/// seq 2 is stamped 20 after seq 1 at 28, so the cut at 24 can't include seq 1, nor seq 2. S3 holds seq 2 pending,
/// and voids it as `Late` once a later cut includes both, since judge tests late first. Every honest replica says
/// `Late`, whether or not its present had reached 28 and stepped seq 1.
#[test]
fn a_pending_entry_reads_late_whatever_this_replica_saw_of_its_chain() {
    let keys = TestKeys::new(7, 3);
    let mut y = EntryBuilder::new(&keys, 2);
    let e1 = y.next(Class::Edit, Frame(28), Bytes::from(encode(&[ball(14, 2.0, 1.0)])));
    let e2 = y.next(Class::Edit, Frame(20), Bytes::from(encode(&[ball(15, -2.0, 1.0)])));
    let quorum = reference(48).0.live_root(Frame(24)).expect("live");
    let (mut a, mut s, mut reader) = stepped_with(&[e1.clone(), e2.clone()]);
    assert!(reader.newest.contains_key(&e1.id()) && reader.newest.contains_key(&e2.id()), "both stepped live");
    let c24 = cut(&scene(), 3, 24);
    assert!(!c24.includes(&e2.entry.chain, e2.entry.seq), "pending: the cut can't include it");
    s3_voids(&mut s, &e2, None);
    assert_eq!(a.finalize(&FinalCut { cut: c24, root: quorum, voters: vec![] }, &mut s), Finalized::Agreed);
    a.advance(Frame(49), &mut s);
    reader.read(&a);
    assert_eq!(reader.told.get(&e2.id()), Some(&Outcome::Dropped { reason: VoidReason::Late }));
    assert_eq!(reader.told.get(&e1.id()), None, "28 is past final_frame");
}

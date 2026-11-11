//! ReplicaImpl over the mock world and the real pe-physics: the properties of replica.md 4.3 that need no hands or
//! logic yet, the budget, roots at cuts, finalize and rung 1.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_replica::mock::{MockClock, MockOp, MockWorld, encode};
use pe_replica::world::{BodyKind, ShapeRow};
use pe_replica::{ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    ChainHead, Class, Cut, CutRef, DomainId, EntrySource, FinalCut, Finalized, Frame, Hash, LineId, ObjectId, Replica,
    RootError, SignedEntry, SimRate,
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
    // A cut that covers an entry the source leaves out (void, @harness-eng's mcc79fdb5df77) has a root, without it:
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

/// A cut that covers only void entries stamped below the floor (@coordinator-opus, m8126544ad530): the source never
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
/// @coordinator-opus mb9b8ce2ae8da), and the report inside [g, floor) is excused, once.
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
/// still a protocol fault, and is not acted on (@design-reviewer, m46c0d79ab4ad).
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

/// Rung 1's retention (S3 v1.2, @coordinator-opus m852013ee7d53): `finalize` tells S3 what it still needs, `Cv`'s
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
/// kept, and simulates on from there (@design-reviewer's Medium on b3c2d18f and 3d8e9442, m76a7a148f6b5).
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
/// cut's (S2 v1.2 round 3, @engine-architect m0e9ba271e806).
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
/// probe on 3d8e9442, zz_probe_cap_trim.rs; option (a) by @coordinator-opus, ma30cdfd63b94).
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

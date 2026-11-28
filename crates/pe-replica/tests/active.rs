//! E4 (re-solve step 2; diverge-eng/resolve-e4-design.md): the hand sync's passes over what is active
//! (`ReplicaConfig::active_passes`, pe_replica::hands::Passes) make the same physics calls as the full passes, so the
//! same roots, physics images and faults, live and through rollbacks.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_replica::mock::MockClock;
use pe_replica::world::WorldModel;
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    ChainHead, Class, Cut, CutRef, DomainId, EntrySource, FinalCut, Frame, Hash, HostTime, LineId, MemberKey, ObjectId,
    Offset, Outcome, Pos, Replica, SignedEntry,
};
use pe_sim::Transform;
use pe_world::components::{Body, Hand, Shape, body_kind, shape_kind};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::{BlobStore, GENESIS, HandPose, Input, World, perm};

#[derive(Default)]
struct Source {
    entries: BTreeMap<Frame, Vec<SignedEntry>>,
    changed: Option<Frame>,
}

impl Source {
    fn add(&mut self, e: SignedEntry) {
        let f = e.entry.stamp;
        let at = self.entries.entry(f).or_default();
        at.push(e);
        at.sort_by_key(|e| (e.entry.chain, e.entry.seq));
        self.changed = Some(self.changed.map_or(f, |c| c.min(f)));
    }
}

impl EntrySource for Source {
    fn final_cut(&self) -> Option<&FinalCut> {
        None
    }
    fn entries_at(&self, frame: Frame) -> Option<&[SignedEntry]> {
        Some(self.entries.get(&frame).map_or(&[][..], Vec::as_slice))
    }
    fn kept_from(&self) -> Frame {
        Frame(0)
    }
    fn keep_final_from(&mut self, _cv: Frame, _floor: Frame) {}
    fn take_changed_from(&mut self) -> Option<Frame> {
        self.changed.take()
    }
}

const K: u64 = 8;
const CLOCK: MockClock = MockClock { rate: pe_seams::SimRate::DEFAULT, start: HostTime(1_234_567_891) };
const HAND: u128 = 50;
const BALL: u128 = 60;

fn oid(n: u128) -> ObjectId {
    ObjectId::from_u128(n)
}

fn pos(x: f64, y: f64, z: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, y, z])).expect("in range")
}

fn row<T: pe_sim::Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

fn keys() -> TestKeys {
    TestKeys::new(5, 2)
}

fn dynamic() -> Body {
    Body { kind: body_kind::DYNAMIC, friction: 0.6, gravity_scale: 1.0, may_sleep: 1, grabbable: 1, ..Body::default() }
}

fn sphere(radius: f32) -> Shape {
    Shape { kind: shape_kind::SPHERE, density: 800.0, size: [radius, 0.0, 0.0], ..Shape::default() }
}

/// A floor, a hand of member 0's at (0, 1, 0), a grabbable ball at x = 2 and six more balls in a row behind it.
fn world() -> World {
    let mut w = World::new(1, BlobStore::default());
    let k = keys().key(0);
    w.set_member(k, perm::EDIT);
    w.create(oid(1), None, MemberKey([1; 32]), GENESIS).expect("floor");
    w.insert(oid(1), Transform::at(pos(0.0, -0.5, 0.0))).expect("floor transform");
    w.insert(oid(1), Body { friction: 0.6, ..Body::default() }).expect("floor body");
    let floor = Shape { kind: shape_kind::BOX, density: 500.0, size: [10.0, 0.5, 10.0], ..Shape::default() };
    w.insert(oid(1), floor).expect("floor shape");
    w.create(oid(HAND), None, k, GENESIS).expect("hand");
    w.insert(oid(HAND), Transform::at(pos(0.0, 1.0, 0.0))).expect("hand transform");
    w.insert(oid(HAND), Hand { owner: k, ..Hand::default() }).expect("hand row");
    for i in 0..7u32 {
        let id = oid(BALL + u128::from(i));
        w.create(id, None, k, GENESIS).expect("ball");
        w.insert(id, Transform::at(pos(2.0, 0.1, -0.3 * f64::from(i)))).expect("ball transform");
        w.insert(id, dynamic()).expect("ball body");
        w.insert(id, sphere(0.1)).expect("ball shape");
    }
    w
}

fn replica(active: bool) -> ReplicaImpl<PeWorld> {
    let mut config = ReplicaConfig::new(pe_seams::SimRate::DEFAULT, Duration::from_millis(133), Arc::new(CLOCK));
    config.cut_every = K;
    config.active_passes = active;
    config.check_active_passes = active;
    ReplicaImpl::new(PeWorld::new(world(), DomainId(0)), config).expect("a replica")
}

fn input(b: &mut EntryBuilder, at: u64, i: &Input) -> SignedEntry {
    b.next(Class::Input, Frame(at), Bytes::from(i.encode()))
}

/// The hand at (x, 0.1, 0), engaged or open.
fn hand_at(x: f64, engaged: bool) -> Input {
    let pose = HandPose { pos: pos(x, 0.1, 0.0), rot: [0.0, 0.0, 0.0, 1.0], grip: 0.5, engaged };
    Input::Hand { hand: oid(HAND), poses: vec![pose], hold: 0 }
}

fn edit(b: &mut EntryBuilder, at: u64, ops: Vec<Op>) -> SignedEntry {
    let tx =
        Transaction { inline: vec![], actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] };
    b.next(Class::Edit, Frame(at), Bytes::from(tx.encode()))
}

/// The cut at `frame` over `entries`: per author, the last entry stamped below it.
fn cut_at(entries: &[SignedEntry], height: u64, frame: u64) -> Cut {
    let mut heads: BTreeMap<_, ChainHead> = BTreeMap::new();
    for e in entries.iter().filter(|e| e.entry.stamp.0 < frame) {
        let h = ChainHead { chain: e.entry.chain, author: e.entry.author, seq: e.entry.seq, entry: e.id() };
        if heads.get(&h.chain).is_none_or(|old| old.seq < h.seq) {
            heads.insert(h.chain, h);
        }
    }
    Cut { at: CutRef { line: LineId::origin(), height, frame: Frame(frame) }, heads: heads.into_values().collect() }
}

/// The full passes and the active ones, side by side, each with its own copy of the entries.
struct Pair {
    full: ReplicaImpl<PeWorld>,
    active: ReplicaImpl<PeWorld>,
    s_full: Source,
    s_active: Source,
}

impl Pair {
    fn add(&mut self, e: &SignedEntry) {
        self.s_full.add(e.clone());
        self.s_active.add(e.clone());
    }

    fn step_to(&mut self, present: u64) {
        while self.full.frame().0 < present {
            let next = Frame(self.full.frame().0 + 1);
            self.full.advance(next, &mut self.s_full);
            self.active.advance(next, &mut self.s_active);
            assert_eq!(self.active.active_misses(), 0, "the active passes and the full ones disagree at {next:?}");
            assert_eq!(self.full.view().frame, self.active.view().frame);
            assert_eq!(self.full.faults(), self.active.faults(), "the faults at {next:?}");
            assert_eq!(self.full.world().claims(), self.active.world().claims(), "the claims at {next:?}");
        }
    }

    /// Both replicas' root at the cut over `entries` at `frame`, with the same physics image.
    fn root(&mut self, entries: &[SignedEntry], frame: u64) -> Hash {
        let cut = cut_at(entries, frame / K, frame);
        let full = self.full.root_at_cut(&cut, &mut self.s_full).expect("the full root");
        let active = self.active.root_at_cut(&cut, &mut self.s_active).expect("the active root");
        assert_eq!(full, active, "the root at {frame}");
        let (a, b) =
            (self.full.physics_image(&full).expect("full image"), self.active.physics_image(&full).expect("image"));
        assert_eq!((a.frame, a.global), (b.frame, b.global), "the physics image's check hash at {frame}");
        assert!(a.bytes == b.bytes, "the transfer's bytes at {frame}");
        full
    }
}

/// Member 0 grabs the ball at 3, carries it to x = 2.5, releases it at 20, grabs it again at 26 and throws it at 34,
/// then opens the hand at 40 (its body goes, through the stale-body pass) and closes it again at 52. An edit at 44
/// deletes a ball (its body goes the same way) and one at 50 makes a new one. Two pushes and a release arrive late
/// (stamped 12, 22 and 46, each delivered 9 or more frames after its stamp), so the live history rolls back over the
/// grabs, the release, the hand opening and the edits. At every step the active passes agree with the full ones beside
/// them (`check_active_passes`), and the two replicas have the same claims and faults; every grid root and every physics
/// image is the same, and the active stale-body pass skipped most steps.
#[test]
fn the_active_passes_give_what_the_full_passes_give_live_and_through_rollbacks() {
    let keys = keys();
    let mut a = EntryBuilder::new(&keys, 0);
    let ball = oid(BALL);
    let grab = Input::Grab { object: ball, hand: Some(oid(HAND)) };
    let mut known = vec![input(&mut a, 3, &hand_at(2.0, true)), input(&mut a, 3, &grab)];
    for f in 4..20 {
        known.push(input(&mut a, f, &hand_at(2.0 + 0.5 * (f - 3) as f64 / 16.0, true)));
    }
    known.push(input(&mut a, 20, &Input::Release { object: ball }));
    known.push(input(&mut a, 26, &grab));
    known.push(input(&mut a, 34, &Input::Throw { object: ball, linear: [0.5, 0.5, 0.0], angular: [0.0; 3] }));
    known.push(input(&mut a, 40, &hand_at(2.5, false)));
    known.push(edit(&mut a, 44, vec![Op::Delete { id: Target::Existing(oid(BALL + 3)) }]));
    let new_ball = vec![row(Transform::at(pos(1.0, 0.5, 1.0))), row(dynamic()), row(sphere(0.1))];
    known.push(edit(&mut a, 50, vec![Op::Create { new: 0, parent: Target::Root, index: None, rows: new_ball }]));
    known.push(input(&mut a, 52, &hand_at(1.0, true)));
    let late = [
        (30, input(&mut a, 12, &Input::Push { object: oid(BALL + 1), impulse: [0.0, 0.3, 0.0], at: None })),
        (31, input(&mut a, 22, &Input::Release { object: ball })),
        (60, input(&mut a, 46, &Input::Push { object: oid(BALL + 2), impulse: [0.2, 0.3, 0.0], at: None })),
    ];
    let mut pair =
        Pair { full: replica(false), active: replica(true), s_full: Source::default(), s_active: Source::default() };
    known.iter().for_each(|e| pair.add(e));
    for (at, e) in &late {
        pair.step_to(*at);
        pair.add(e);
        known.push(e.clone());
    }
    pair.step_to(96);
    let grabbed = pair.active.outcome(&known[1].id()).map(|o| o.1);
    assert!(matches!(grabbed, Some(Outcome::Applied { .. })), "the grab at 3 held: {grabbed:?}");
    for f in (K..=88).step_by(K as usize) {
        pair.root(&known, f);
    }
    for f in (0..=96).step_by(K as usize) {
        let (full, active) = (pair.full.grid_root(Frame(f)), pair.active.grid_root(Frame(f)));
        assert_eq!(full, active, "the grid root at {f}");
    }
    assert_eq!(pair.active.active_misses(), 0);
    let skips = pair.active.active_skips();
    assert!(skips > 60, "the stale-body pass skipped most steps: {skips}");
}

//! ReplicaImpl over pe-world's `World` (src/peworld.rs): body rows from the world's rows, the physics effects of
//! edits, and the replica stepping the real world with rollback.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_replica::hands::hand_mode;
use pe_replica::mock::MockClock;
use pe_replica::world::{BodyKind, Change, Phase, ShapeReason, ShapeRow, Unbuildable, WorldModel};
use pe_replica::{CorrectedInteraction, PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    ChainHead, Class, Cut, CutImport, CutRef, DomainId, EntrySource, FinalCut, Finalized, Frame, FrameClock, GroupSel,
    Hash, Hit, HostTime, ImportError, LeafKey, LineId, MemberKey, ObjectId, Offset, Outcome, Pos, Reason, Replica,
    SignedEntry,
};
use pe_sim::Transform;
use pe_world::components::{Appearance, Body, BodyState, Hand, Name, Shape, body_kind, shape_kind};
use pe_world::edit::{Actor, AssetTarget, Op, Row, Target, Transaction};
use pe_world::{BlobStore, GENESIS, HandPose, Input, World, perm};

fn oid(n: u128) -> ObjectId {
    ObjectId::from_u128(n)
}

fn pos(x: f64, y: f64, z: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, y, z])).expect("in range")
}

fn row<T: pe_sim::Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

fn tx(ops: Vec<Op>) -> Transaction {
    Transaction { inline: vec![], actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] }
}

fn sphere(radius: f32) -> Shape {
    Shape { kind: shape_kind::SPHERE, density: 800.0, size: [radius, 0.0, 0.0], ..Shape::default() }
}

fn cuboid(half: [f32; 3]) -> Shape {
    Shape { kind: shape_kind::BOX, density: 500.0, size: half, ..Shape::default() }
}

fn dynamic() -> Body {
    Body { kind: body_kind::DYNAMIC, friction: 0.6, gravity_scale: 1.0, may_sleep: 1, grabbable: 1, ..Body::default() }
}

fn keys() -> TestKeys {
    TestKeys::new(5, 2)
}

/// Genesis: a static floor (1), and an editor (key 0).
fn genesis() -> World {
    let mut w = World::new(1, BlobStore::default());
    w.set_member(keys().key(0), perm::EDIT);
    w.create(oid(1), None, MemberKey([1; 32]), GENESIS).expect("floor");
    w.insert(oid(1), Transform::at(pos(0.0, -0.5, 0.0))).expect("floor transform");
    w.insert(oid(1), Body { friction: 0.6, ..Body::default() }).expect("floor body");
    w.insert(oid(1), cuboid([10.0, 0.5, 10.0])).expect("floor shape");
    w
}

fn close(a: [f32; 3], b: [f32; 3]) -> bool {
    a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-5)
}

#[test]
fn body_rows_follow_the_world_s_rows() {
    let mut w = genesis();
    let k = MemberKey([1; 32]);
    let quarter_turn = [0.0, std::f32::consts::FRAC_1_SQRT_2, 0.0, std::f32::consts::FRAC_1_SQRT_2];
    // A compound table: its own box, turned a quarter about y, and a child sphere 1 m along its local x.
    w.create(oid(2), None, k, GENESIS).expect("table");
    w.insert(oid(2), Transform::at(pos(3.0, 1.0, 0.0)).with_rot(quarter_turn)).expect("t");
    w.insert(oid(2), dynamic()).expect("t");
    w.insert(oid(2), cuboid([0.5, 0.1, 0.5])).expect("t");
    w.create(oid(3), Some(oid(2)), k, GENESIS).expect("child");
    w.insert(oid(3), Transform::local([1.0, 0.0, 0.0])).expect("c");
    w.insert(oid(3), sphere(0.2)).expect("c");
    // A sphere under non-uniform scale (refused), and a box scaled per axis.
    w.create(oid(4), None, k, GENESIS).expect("4");
    w.insert(oid(4), Transform::at(pos(-3.0, 1.0, 0.0)).with_scale([1.0, 2.0, 1.0])).expect("4");
    w.insert(oid(4), dynamic()).expect("4");
    w.insert(oid(4), sphere(0.5)).expect("4");
    w.create(oid(5), None, k, GENESIS).expect("5");
    w.insert(oid(5), Transform::at(pos(0.0, 4.0, 0.0)).with_scale([2.0, 3.0, 4.0])).expect("5");
    w.insert(oid(5), dynamic()).expect("5");
    w.insert(oid(5), cuboid([1.0, 1.0, 1.0])).expect("5");
    let pw = PeWorld::new(w, DomainId(0));

    assert_eq!(pw.physical(), [oid(1), oid(2), oid(4), oid(5)]);
    let floor = pw.body(oid(1)).unwrap().expect("floor");
    assert_eq!((floor.kind, floor.pos, floor.shapes.len()), (BodyKind::Static, pos(0.0, -0.5, 0.0), 1));
    let table = pw.body(oid(2)).unwrap().expect("table");
    assert_eq!(table.kind, BodyKind::Dynamic);
    assert_eq!(
        table.shapes[0].shape,
        ShapeRow::Box { center: [0.0; 3], rotation: [0.0, 0.0, 0.0, 1.0], half_extents: [0.5, 0.1, 0.5] }
    );
    assert_eq!(table.shapes[0].density, 500.0);
    match table.shapes[1].shape.clone() {
        ShapeRow::Sphere { center, radius } => {
            assert!(close(center, [1.0, 0.0, 0.0]), "the child sits 1 m along the table's own x: {center:?}");
            assert_eq!(radius, 0.2);
        }
        s => panic!("a sphere, not {s:?}"),
    }
    assert_eq!(table.shapes[1].density, 800.0);
    let non_uniform = Err(Unbuildable::Shape(ShapeReason::NonUniformScale));
    assert_eq!(pw.body(oid(4)), non_uniform, "a sphere under non-uniform scale cannot be built");
    match pw.body(oid(5)).unwrap().expect("box").shapes[0].shape.clone() {
        ShapeRow::Box { half_extents, .. } => assert_eq!(half_extents, [2.0, 3.0, 4.0]),
        s => panic!("a box, not {s:?}"),
    }
}

/// A body turned a quarter about y with its own box, a compound child without a `Transform` (it sits at its frame,
/// the body) and one placed 1 m along the body's x: chunks-reviewer's probe and L1 on 48e4e521.
#[test]
fn a_compound_child_sits_at_its_frame_and_a_move_does_not_rebuild() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut w = genesis();
    let k = MemberKey([1; 32]);
    let quarter_turn = [0.0, std::f32::consts::FRAC_1_SQRT_2, 0.0, std::f32::consts::FRAC_1_SQRT_2];
    w.create(oid(2), None, k, GENESIS).expect("body");
    w.insert(oid(2), Transform::at(pos(0.0, 2.0, 0.0)).with_rot(quarter_turn)).expect("b");
    w.insert(oid(2), dynamic()).expect("b");
    w.insert(oid(2), cuboid([0.5, 0.5, 0.5])).expect("b");
    w.create(oid(3), Some(oid(2)), k, GENESIS).expect("bare child");
    w.insert(oid(3), sphere(0.2)).expect("c");
    w.create(oid(4), Some(oid(2)), k, GENESIS).expect("placed child");
    w.insert(oid(4), Transform::local([1.0, 0.0, 0.0])).expect("c");
    w.insert(oid(4), cuboid([0.1, 0.1, 0.1])).expect("c");
    let mut pw = PeWorld::new(w, DomainId(0));
    let built = pw.body(oid(2)).unwrap().expect("the body is built");
    let shapes: Vec<ShapeRow> = built.shapes.iter().map(|p| p.shape.clone()).collect();
    let unturned = [0.0, 0.0, 0.0, 1.0];
    assert_eq!(
        shapes,
        [
            ShapeRow::Box { center: [0.0; 3], rotation: unturned, half_extents: [0.5; 3] },
            ShapeRow::Sphere { center: [0.0; 3], radius: 0.2 },
            ShapeRow::Box { center: [1.0, 0.0, 0.0], rotation: unturned, half_extents: [0.1; 3] },
        ]
    );
    let moved = row(Transform::at(pos(4.0, 2.0, 0.0)).with_rot([0.0, 0.0, 0.0, 1.0]));
    let (_, changed) = edit(&mut pw, &mut b, vec![Op::SetRow { id: Target::Existing(oid(2)), row: moved }]);
    assert_eq!(changed, [(oid(2), Change::Teleported)], "moving and turning the body leaves its shapes as they were");
    assert_eq!(pw.body(oid(2)).unwrap().expect("moved").shapes, built.shapes);
}

/// Apply one edit at frame 1 and return its outcome and changes.
fn edit(pw: &mut PeWorld, b: &mut EntryBuilder, ops: Vec<Op>) -> (Outcome, Vec<(ObjectId, Change)>) {
    let e = b.next(Class::Edit, Frame(1), Bytes::from(tx(ops).encode()));
    let (outcome, effects) = pw.apply(Frame(1), &e);
    (outcome, effects.changed)
}

#[test]
fn edits_become_physics_effects() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut pw = PeWorld::new(genesis(), DomainId(0));
    let rows = vec![row(Transform::at(pos(0.0, 2.0, 0.0))), row(dynamic()), row(sphere(0.25))];
    let (outcome, changed) =
        edit(&mut pw, &mut b, vec![Op::Create { new: 0, parent: Target::Root, index: None, rows }]);
    let ball = match outcome {
        Outcome::Applied { created, .. } => created[0],
        r => panic!("refused: {r:?}"),
    };
    assert_eq!(changed, [(ball, Change::Created)]);
    let at = |x| row(Transform::at(pos(x, 2.0, 0.0)));
    let set = |row| Op::SetRow { id: Target::Existing(ball), row };
    assert_eq!(edit(&mut pw, &mut b, vec![set(at(1.0))]).1, [(ball, Change::Teleported)]);
    assert_eq!(edit(&mut pw, &mut b, vec![set(row(sphere(0.3)))]).1, [(ball, Change::Rebuilt)]);
    let both = vec![set(row(sphere(0.4))), set(at(2.0))];
    assert_eq!(edit(&mut pw, &mut b, both).1, [(ball, Change::Rebuilt), (ball, Change::Teleported)]);
    assert_eq!(edit(&mut pw, &mut b, vec![set(at(2.0))]).1, [], "the same pose again moves nothing");
    let name = Name::default();
    assert_eq!(edit(&mut pw, &mut b, vec![set(row(name))]).1, [], "a name is not physics");
    assert_eq!(edit(&mut pw, &mut b, vec![Op::Delete { id: Target::Existing(ball) }]).1, [(ball, Change::Removed)]);

    let input = b.next(Class::Input, Frame(1), Bytes::from_static(&[0]));
    assert!(matches!(pw.apply(Frame(1), &input), (Outcome::Refused { .. }, fx) if fx.is_empty()));
    let governance = b.next(Class::Governance, Frame(1), Bytes::from_static(&[0]));
    assert!(matches!(pw.apply(Frame(1), &governance), (Outcome::Applied { .. }, fx) if fx.is_empty()));
}

/// Agreement's stand-in: entries by frame, in a fixed order, whatever order they arrive in; nothing is trimmed.
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

/// Two balls dropped onto the floor by edits, from one editor.
fn scene() -> Vec<SignedEntry> {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let drop = |x: f64, y: f64| {
        let rows = vec![row(Transform::at(pos(x, y, 0.0))), row(dynamic()), row(sphere(0.25))];
        tx(vec![Op::Create { new: 0, parent: Target::Root, index: None, rows }])
    };
    let first = b.next(Class::Edit, Frame(2), Bytes::from(drop(0.0, 1.0).encode()));
    let second = b.next(Class::Edit, Frame(5), Bytes::from(drop(0.3, 2.0).encode()));
    vec![first, second]
}

/// The shared time base: frame 0 well after host time 0, so a view time off this base shows.
const CLOCK: MockClock = MockClock { rate: pe_seams::SimRate::DEFAULT, start: HostTime(1_234_567_891) };

fn replica() -> ReplicaImpl<PeWorld> {
    let mut config = ReplicaConfig::new(pe_seams::SimRate::DEFAULT, Duration::from_millis(133), Arc::new(CLOCK));
    config.cut_every = K;
    ReplicaImpl::new(PeWorld::new(genesis(), DomainId(0)), config).expect("a replica")
}

/// The live history's root at every grid frame to `to` (`grid_root`: a re-step's left to the keyframe, E3).
fn roots(r: &mut ReplicaImpl<PeWorld>, to: u64) -> Vec<(u64, Hash)> {
    (0..=to).step_by(K as usize).filter_map(|f| r.grid_root(Frame(f)).map(|h| (f, h))).collect()
}

#[test]
fn the_replica_steps_the_real_world_and_rolls_back_to_the_same_roots() {
    let mut s = Source::default();
    scene().into_iter().for_each(|e| s.add(e));
    let mut a = replica();
    a.advance(Frame(64), &mut s);
    let ra = roots(&mut a, 64);
    assert_eq!(ra.len(), 9);
    assert!(ra.windows(2).all(|w| w[0].1 != w[1].1), "the roots move: {ra:?}");
    assert_eq!(a.faults(), 0);
    let world = a.world().world();
    assert_eq!(world.frame(), Frame(64), "World::frame() is the next frame to step");
    let view = a.view();
    assert_eq!(view.sim_time, CLOCK.host_time_of(view.frame), "the view's time is on the clock's base (S5)");
    let balls: Vec<ObjectId> = world.physical().into_iter().filter(|&id| id != oid(1)).collect();
    assert_eq!(balls.len(), 2);
    let y = world.store().world_transform(balls[0]).expect("a ball").pos;
    assert!((y - Pos::ORIGIN).0[1] < 1.0, "the ball fell: {y:?}");

    // The same entries, the second arriving late: a rollback, and the same roots.
    let mut late = Source::default();
    let mut entries = scene();
    let second = entries.pop().expect("two entries");
    entries.into_iter().for_each(|e| late.add(e));
    let mut b = replica();
    b.advance(Frame(40), &mut late);
    late.add(second);
    let adv = b.advance(Frame(64), &mut late);
    assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= 5), "rolled back: {adv:?}");
    assert_eq!(roots(&mut b, 64), ra);
    assert_eq!(b.faults(), 0);
}

/// The frame view draws pe-world's objects (S5): the floor's box, a ball's sphere and colour, the ball held by the
/// local member's right hand (`HELD_LOCAL`), that hand as a `HAND_RADIUS` sphere marked `OWN_HAND`, and another
/// member's hand, which is neither.
/// pe-api's snapshot is the live world at the replica's frame (S2 v1.7, `Replica::world_snapshot`; @coordinator-opus): the balls the scene's edits drop at frames 2 and 5 appear in it as the replica passes them, and
/// before any cut is agreed the final frame it names is the genesis cut's.
#[test]
fn the_world_snapshot_is_the_live_world_at_the_replica_s_frame() {
    let mut s = Source::default();
    scene().into_iter().for_each(|e| s.add(e));
    let mut r = replica();
    let objects_at = |r: &mut ReplicaImpl<PeWorld>, s: &mut Source, to: u64| {
        r.advance(Frame(to), s);
        let snap = r.world_snapshot().expect("PeWorld has one");
        let snap = snap.downcast::<pe_world::WorldSnapshot>().expect("pe-world's snapshot");
        assert_eq!(snap.frame(), r.frame(), "at the replica's frame");
        assert_eq!(snap.final_frame(), Frame(0), "no cut agreed yet");
        snap.all().len()
    };
    let before = objects_at(&mut r, &mut s, 1);
    assert_eq!(objects_at(&mut r, &mut s, 3), before + 1, "the first ball, dropped at frame 2");
    assert_eq!(objects_at(&mut r, &mut s, 10), before + 2, "and the second, at frame 5");
}

#[test]
fn the_view_draws_shapes_and_appearances_and_marks_the_local_members_hands() {
    let (me, other) = (keys().key(0), keys().key(1));
    let mut w = genesis();
    w.create(oid(2), None, me, GENESIS).expect("ball");
    w.insert(oid(2), Transform::at(pos(0.0, 1.0, 0.0))).expect("ball transform");
    w.insert(oid(2), sphere(0.1)).expect("ball shape");
    w.insert(oid(2), Appearance { base: [1.0, 0.0, 0.0, 1.0], ..Appearance::default() }).expect("ball colour");
    for (id, owner, side) in [(oid(3), me, 1), (oid(4), other, 0)] {
        w.create(id, None, owner, GENESIS).expect("hand");
        w.insert(id, Transform::at(pos(0.3, 1.2, 0.0))).expect("hand transform");
        w.insert(id, Hand { owner, side, ..Hand::default() }).expect("hand row");
    }
    w.set_claim(oid(2), pe_world::Claim { hand: oid(3), member: me, since: Frame(0), until: Frame(1_000) })
        .expect("held");
    // A box I hold hand-less (an agent's grab): not HELD_LOCAL, since no tracked hand draws it.
    w.create(oid(5), None, me, GENESIS).expect("box");
    w.insert(oid(5), Transform::at(pos(1.0, 0.5, 0.0))).expect("box transform");
    w.insert(oid(5), cuboid([0.1, 0.1, 0.1])).expect("box shape");
    w.set_claim(oid(5), pe_world::Claim { hand: ObjectId::NONE, member: me, since: Frame(0), until: Frame(1_000) })
        .expect("held");
    let mut config = ReplicaConfig::new(pe_seams::SimRate::DEFAULT, Duration::from_millis(133), Arc::new(CLOCK));
    config.local = Some(me);
    let mut r = ReplicaImpl::new(PeWorld::new(w, DomainId(0)), config).expect("a replica");
    r.advance(Frame(1), &mut Source::default());
    let view = r.view();
    let drawn = |id: ObjectId| view.objects.iter().find(|o| o.id == id).unwrap_or_else(|| panic!("{id:?} drawn"));

    assert_eq!(drawn(oid(1)).shape, Some(pe_seams::Shape::Box { half_extents: [10.0, 0.5, 10.0] }));
    let ball = drawn(oid(2));
    assert_eq!(ball.shape, Some(pe_seams::Shape::Sphere { radius: 0.1 }));
    assert_eq!(ball.appearance.map(|a| a.base_color), Some([1.0, 0.0, 0.0, 1.0]));
    assert_eq!(ball.held_by, Some(pe_seams::Holder { member: me, hand: oid(3) }));
    assert!((ball.radius - 0.1).abs() < 1e-6, "{}", ball.radius);
    assert!(ball.flags.contains(pe_seams::ViewFlags::HELD_LOCAL));
    let mine = drawn(oid(3));
    assert_eq!(mine.hand_of, Some((me, pe_seams::Hand::Right)));
    assert!(mine.flags.contains(pe_seams::ViewFlags::OWN_HAND));
    assert_eq!(mine.shape, Some(pe_seams::Shape::Sphere { radius: pe_replica::world::HAND_RADIUS }));
    let theirs = drawn(oid(4));
    assert_eq!(theirs.hand_of, Some((other, pe_seams::Hand::Left)));
    assert_eq!(theirs.flags, pe_seams::ViewFlags::default());
    let boxed = drawn(oid(5));
    assert_eq!(boxed.held_by, Some(pe_seams::Holder { member: me, hand: ObjectId::NONE }));
    assert!(!boxed.flags.contains(pe_seams::ViewFlags::HELD_LOCAL), "a hand-less hold is not drawn from tracking");
    assert_eq!(ball.version, None, "made at genesis, so no edit has written it");
}

/// A late entry whose ball lands on an earlier ball marks the earlier ball `CORRECTED` in the next view (S5): the
/// replay rewrote its pose at the last published frame. The floor, which nothing moved, is not marked, the late
/// ball is new rather than corrected, and nothing is marked without a replay.
#[test]
fn a_replay_that_moves_a_ball_marks_it_corrected() {
    let mut entries = scene();
    let late = entries.pop().expect("two entries");
    let mut s = Source::default();
    entries.into_iter().for_each(|e| s.add(e));
    let mut r = replica();
    r.advance(Frame(64), &mut s);
    let corrected = |r: &ReplicaImpl<PeWorld>| -> Vec<ObjectId> {
        let view = r.view();
        view.objects.iter().filter(|o| o.flags.contains(pe_seams::ViewFlags::CORRECTED)).map(|o| o.id).collect()
    };
    assert_eq!(corrected(&r), Vec::<ObjectId>::new(), "no replay, no correction");
    let first = r.view().objects.iter().map(|o| o.id).find(|&id| id != oid(1)).expect("the first ball");

    s.add(late);
    let adv = r.advance(Frame(65), &mut s);
    assert!(adv.rolled_back_to.is_some(), "{adv:?}");
    assert_eq!(corrected(&r), [first], "only the first ball, which the late one landed on");

    r.advance(Frame(66), &mut s);
    assert_eq!(corrected(&r), Vec::<ObjectId>::new(), "a correction is marked once");
}

/// F3's S5 (S2 v1.10, "the fastest machine sets the pace"): a replica the budget keeps behind the present publishes the
/// newest frame it completed at every call, not only once it catches up. Each such view draws what a replica that kept
/// up drew at that frame.
#[test]
fn a_replica_behind_the_present_shows_each_frame_it_completed() {
    let entries = scene();
    let mut s = Source::default();
    entries.iter().cloned().for_each(|e| s.add(e));
    let mut slow = replica();
    slow.set_budget(Duration::from_millis(1)); // raised to two frames a call
    for call in 1..=12u64 {
        // Wall time gains three frames a call, so the replica falls a frame further behind at each.
        let adv = slow.advance(Frame(3 * call), &mut s);
        assert_eq!((adv.stepped, adv.caught_up), (2, false), "call {call}");
        let shown = slow.view();
        assert_eq!(shown.frame, Frame(2 * call), "the newest frame it completed, at call {call}");
        let mut kept_up = replica();
        let mut ks = Source::default();
        entries.iter().cloned().for_each(|e| ks.add(e));
        assert!(kept_up.advance(Frame(2 * call), &mut ks).caught_up);
        assert_eq!(shown.objects, kept_up.view().objects, "the world at frame {}", 2 * call);
        assert_eq!(shown.sim_time, kept_up.view().sim_time);
    }
}

/// A rollback publishes nothing until the re-simulation is back at the newest frame the replica had completed (S2
/// v1.10), so no view goes back in time. The first view there marks the ball the late one landed on `CORRECTED`, though
/// the replica is still behind the present, and the next call shows the next frames it completed, unmarked.
#[test]
fn a_rollback_publishes_nothing_until_it_is_back_at_its_newest_frame() {
    let mut entries = scene();
    let late = entries.pop().expect("two entries");
    let mut s = Source::default();
    entries.into_iter().for_each(|e| s.add(e));
    let mut r = replica();
    r.advance(Frame(64), &mut s);
    let shown = r.view();
    assert_eq!(shown.frame, Frame(64));
    let first = shown.objects.iter().map(|o| o.id).find(|&id| id != oid(1)).expect("the first ball");
    let corrected = |v: &pe_seams::FrameView| -> Vec<ObjectId> {
        v.objects.iter().filter(|o| o.flags.contains(pe_seams::ViewFlags::CORRECTED)).map(|o| o.id).collect()
    };
    r.set_budget(Duration::from_millis(1)); // raised to two frames a call
    s.add(late);
    let mut present = 65;
    let adv = r.advance(Frame(present), &mut s);
    assert!(adv.rolled_back_to.is_some_and(|k| k <= Frame(5)), "{adv:?}");
    while r.frame() < Frame(64) {
        assert!(Arc::ptr_eq(&r.view(), &shown), "a view below 64, at {:?}", r.frame());
        present += 1;
        r.advance(Frame(present), &mut s);
    }
    let back = r.view();
    assert!(!Arc::ptr_eq(&back, &shown), "no view back at {:?}, behind the present {present}", r.frame());
    assert_eq!(back.frame, r.frame());
    assert_eq!(corrected(&back), [first], "only the first ball, which the late one landed on");
    // The view back says how deep the rollback went (from 64 to frame 5 or below); a later one only its own, none
    // here (S5's `deepest_rollback`, since the last view; pe-render's cue reads it per view).
    assert!(back.counters.deepest_rollback >= 59, "{:?}", back.counters);
    present += 1;
    let adv = r.advance(Frame(present), &mut s);
    assert_eq!((adv.stepped, adv.caught_up), (2, false));
    let next = r.view();
    assert_eq!(next.frame, Frame(back.frame.0 + 2));
    assert_eq!(corrected(&next), Vec::<ObjectId>::new(), "a correction is marked once");
    assert_eq!(next.counters.deepest_rollback, 0, "the rollback is counted in the view back only: {:?}", next.counters);
}

/// A measurement, in release (replica.md 5.3; was O13): what each vote's root costs in a finality stall's shape over
/// the M1 ball pit (200 balls), against the cut's distance from the pinned first difference. Every 30 frames one
/// member pushes four balls, so the pit keeps moving; the other member's push at frame 10, which no cut covers,
/// stays in S3's live view and pins every cut's first difference there. It votes at every cut from 16 to
/// `PE_VOTE_FRAMES` (600) and prints, per cut: its frame, its distance from 10, the frames re-stepped and the
/// milliseconds taken. Run it on a red with no resume to compare.
#[test]
#[ignore = "a measurement, in release: PE_VOTE_FRAMES (600)"]
fn measure_vote_cost_in_a_stall() {
    let to: u64 = std::env::var("PE_VOTE_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(600);
    let keys = keys();
    let mut world = pe_world::preset::ball_pit(7, BlobStore::default(), Pos::ORIGIN);
    world.set_member(keys.key(0), perm::EDIT);
    world.set_member(keys.key(1), perm::EDIT);
    let ball = |i: usize| pe_world::preset::find(&world, &format!("Ball {i}")).expect("a ball");
    let balls: Vec<ObjectId> = (0..5).map(ball).collect();
    let (mut a, mut b) = (EntryBuilder::new(&keys, 0), EntryBuilder::new(&keys, 1));
    let mut s = Source::default();
    let mut covered = Vec::new();
    for f in (5..to).step_by(30) {
        for (i, &object) in balls[..4].iter().enumerate() {
            let impulse = [0.02 * (i as f32 - 1.5), 0.03, 0.01];
            let e = input(&mut a, f, &Input::Push { object, impulse, at: None });
            covered.push(e.clone());
            s.add(e);
        }
    }
    s.add(input(&mut b, 10, &Input::Push { object: balls[4], impulse: [0.0, 0.03, 0.0], at: None }));
    let mut r = replica_of(world);
    r.advance(Frame(to), &mut s);
    assert_eq!(r.frame(), Frame(to));
    println!("vote: cut frame | distance from 10 | frames re-stepped | ms | snapshots held");
    let (mut frames, mut ms) = (0, 0.0);
    for (h, f) in (16..=to).step_by(K as usize).enumerate() {
        let cut = cut_at(&covered, h as u64 + 2, f);
        let before = r.scratch().frames;
        let t = std::time::Instant::now();
        r.root_at_cut(&cut, &mut s).expect("a root");
        let took = t.elapsed().as_secs_f64() * 1e3;
        let stepped = r.scratch().frames - before;
        println!("vote {f:5} | {:5} | {stepped:5} | {took:8.3} | {:3}", f - 10, r.held_count());
        (frames, ms) = (frames + stepped, ms + took);
    }
    println!("total: {frames} frames re-stepped, {ms:.1} ms");
}

/// pe-world's shape limit is a rule of the history (world-model.md 7.1, 7.6 step 5; @coordinator-opus):
/// two replicas, one stepping frame by frame and one in a single call, refuse the same edit with the same reason and
/// agree on every root. Genesis (its floor's shape) and `limit - 2` shaped objects; an edit adding two is refused at
/// `limit + 1`, and a later one adding one applies.
#[test]
fn an_edit_past_the_shape_limit_is_refused_by_every_replica() {
    // The world's own limit, as a `Limits` row (7.1): the build's `MAX_SHAPES` (52,000 since the friend test 11-27)
    // would make this test step 52,000 bodies.
    let max = 520;
    let mut world = genesis();
    world.create(oid(9_999), None, MemberKey([1; 32]), GENESIS).expect("the settings object");
    world.set_limits(oid(9_999), pe_world::components::Limits { max_shapes: max as u32 }).expect("its limits");
    for n in 0..(max - 2) as u128 {
        world.create(oid(10_000 + n), None, MemberKey([1; 32]), GENESIS).expect("an object");
        world.insert(oid(10_000 + n), Transform::at(pos(0.0, 5.0, 0.0))).expect("its transform");
        world.insert(oid(10_000 + n), sphere(0.06)).expect("its shape");
    }
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let shapes = |n: u32, x: f64| {
        let rows = || vec![row(Transform::at(pos(x, 5.0, 0.0))), row(sphere(0.06))];
        tx((0..n).map(|k| Op::Create { new: k, parent: Target::Root, index: None, rows: rows() }).collect())
    };
    let over = b.next(Class::Edit, Frame(2), Bytes::from(shapes(2, 1.0).encode()));
    let fits = b.next(Class::Edit, Frame(4), Bytes::from(shapes(1, 2.0).encode()));
    let mut s = Source::default();
    s.add(over.clone());
    s.add(fits.clone());
    let (mut one, mut each) = (replica_of(world.clone()), replica_of(world));
    one.advance(Frame(17), &mut s);
    for f in 1..=17 {
        each.advance(Frame(f), &mut s);
    }
    let refusal =
        Outcome::Refused { op: None, reason: Reason::TooManyShapes { limit: max as u32, count: max as u32 + 1 } };
    for r in [&one, &each] {
        assert_eq!(r.outcome(&over.id()), Some((Frame(2), refusal.clone())));
        assert!(matches!(r.outcome(&fits.id()), Some((Frame(4), Outcome::Applied { .. }))));
        assert_eq!(r.world().world().store().count(pe_world::components::ids::SHAPE), max);
    }
    assert_eq!(roots(&mut one, 16), roots(&mut each, 16));
    assert_eq!(roots(&mut one, 16).len(), 3, "roots at 0, 8 and 16");
}

// ---- inputs (pe-world's slice B) ------------------------------------------------------------------------------------

const HAND: u128 = 50;
const BALL: u128 = 60;

/// Genesis, with a hand of the editor's at (0, 1, 0) and a grabbable ball resting on the floor at x = 2.
fn with_hand_and_ball() -> World {
    let mut w = genesis();
    let k = keys().key(0);
    w.create(oid(HAND), None, k, GENESIS).expect("hand");
    w.insert(oid(HAND), Transform::at(pos(0.0, 1.0, 0.0))).expect("hand transform");
    w.insert(oid(HAND), Hand { owner: k, ..Hand::default() }).expect("hand row");
    w.create(oid(BALL), None, k, GENESIS).expect("ball");
    w.insert(oid(BALL), Transform::at(pos(2.0, 0.1, 0.0))).expect("ball transform");
    w.insert(oid(BALL), dynamic()).expect("ball body");
    w.insert(oid(BALL), sphere(0.1)).expect("ball shape");
    w
}

fn replica_of(world: World) -> ReplicaImpl<PeWorld> {
    let mut config = ReplicaConfig::new(pe_seams::SimRate::DEFAULT, Duration::from_millis(133), Arc::new(CLOCK));
    config.cut_every = K;
    ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).expect("a replica")
}

/// S5 v1.11: a collision hit names each side's part, the object whose `Shape` touched: the body's own shape or a
/// compound child's, by the physics' place for the shape in the body's row (its own shape first, then its children's
/// in id order). Body 2 is a plank whose child sphere (3) hangs 0.5 m under it, so the child lands first; body 4 is a
/// sphere with a child box (5) on top, so its own shape lands first.
#[test]
fn a_hit_names_the_compound_part_that_touched() {
    let mut w = genesis();
    let k = MemberKey([1; 32]);
    w.create(oid(2), None, k, GENESIS).expect("plank");
    w.insert(oid(2), Transform::at(pos(0.0, 1.0, 0.0))).expect("t");
    w.insert(oid(2), dynamic()).expect("b");
    w.insert(oid(2), cuboid([0.3, 0.05, 0.3])).expect("s");
    w.create(oid(3), Some(oid(2)), k, GENESIS).expect("child");
    w.insert(oid(3), Transform::local([0.0, -0.5, 0.0])).expect("t");
    w.insert(oid(3), sphere(0.1)).expect("s");
    w.create(oid(4), None, k, GENESIS).expect("4");
    w.insert(oid(4), Transform::at(pos(3.0, 1.0, 0.0))).expect("t");
    w.insert(oid(4), dynamic()).expect("b");
    w.insert(oid(4), sphere(0.2)).expect("s");
    w.create(oid(5), Some(oid(4)), k, GENESIS).expect("child");
    w.insert(oid(5), Transform::local([0.0, 0.5, 0.0])).expect("t");
    w.insert(oid(5), cuboid([0.2, 0.1, 0.2])).expect("s");
    let mut r = replica_of(w);
    let mut s = Source::default();
    let mut hits: Vec<Hit> = Vec::new();
    for n in 1..=60 {
        r.advance(Frame(n), &mut s);
        hits.extend(r.view().hits.iter().copied());
    }
    // Each body's first hit, from its side: (its part, the other body, the other's part).
    let first = |body: ObjectId| {
        let h = hits.iter().find(|h| h.a == body || h.b == body).copied().expect("a hit");
        if h.a == body { (h.part_a, h.b, h.part_b) } else { (h.part_b, h.a, h.part_a) }
    };
    assert_eq!(first(oid(2)), (oid(3), oid(1), oid(1)), "the plank's child sphere on the floor: {hits:?}");
    assert_eq!(first(oid(4)), (oid(4), oid(1), oid(1)), "body 4's own sphere on the floor: {hits:?}");
}

fn input(b: &mut EntryBuilder, at: u64, i: &Input) -> SignedEntry {
    b.next(Class::Input, Frame(at), Bytes::from(i.encode()))
}

/// A hand input with one pose per `x`, each at (x, 0.1, 0), held `hold` frames more.
fn hand_to(xs: &[f64], hold: u8) -> Input {
    let pose = |x| HandPose { pos: pos(x, 0.1, 0.0), rot: [0.0, 0.0, 0.0, 1.0], grip: 0.5, engaged: true };
    Input::Hand { hand: oid(HAND), poses: xs.iter().map(|&x| pose(x)).collect(), hold }
}

/// A grab sent ahead of its author's hand input in the same frame applies after it (world-model.md 8.1): the hand
/// input is phase `First`, so the grab's reach is measured from where the hand moved, and the grab holds. With the
/// hand input a frame later, the same grab is out of reach.
#[test]
fn a_hand_input_applies_before_a_grab_sent_ahead_of_it() {
    let grab = Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) };
    let run = |hand_at: u64| {
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let g = input(&mut b, 3, &grab);
        let h = input(&mut b, hand_at, &hand_to(&[2.0], 0));
        assert_eq!((PeWorld::phase(&h), PeWorld::phase(&g)), (Phase::First, Phase::Second));
        let mut s = Source::default();
        s.add(g.clone());
        s.add(h);
        let mut r = replica_of(with_hand_and_ball());
        r.advance(Frame(8), &mut s);
        assert_eq!(r.faults(), 0);
        (r.outcome(&g.id()).map(|o| o.1), r.world().claims())
    };
    let (outcome, claims) = run(3);
    assert!(matches!(outcome, Some(Outcome::Applied { .. })), "{outcome:?}");
    assert_eq!(claims.iter().map(|c| (c.object, c.hand)).collect::<Vec<_>>(), [(oid(BALL), oid(HAND))]);
    let (outcome, claims) = run(4);
    assert!(matches!(outcome, Some(Outcome::Refused { reason: Reason::OutOfReach, .. })), "{outcome:?}");
    assert!(claims.is_empty());
}

/// An input's push becomes a physics push: the ball rolls along x, and without the push it stays where it was.
#[test]
fn an_input_s_push_moves_its_body() {
    let run = |push: bool| {
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let mut s = Source::default();
        if push {
            s.add(input(&mut b, 3, &Input::Push { object: oid(BALL), impulse: [1.5, 0.0, 0.0], at: None }));
        }
        let mut r = replica_of(with_hand_and_ball());
        r.advance(Frame(60), &mut s);
        assert_eq!(r.faults(), 0);
        (r.world().body(oid(BALL)).unwrap().expect("the ball").pos - Pos::ORIGIN).0[0]
    };
    let (still, pushed) = (run(false), run(true));
    assert!((still - 2.0).abs() < 1e-3, "{still}");
    assert!(pushed > still + 0.1, "{pushed} against {still}");
}

/// M1's acceptance (refound plan 3.1; replica.md 3.4; @item:primordial-engine/g2-carried-state-is-a-check-not-an-overwrite-the):
/// two members push one ball within one network delay, each before it has the other's push, and both pushes take
/// effect. A steps past B's push before B's arrives, and B past A's before A's arrives; each rolls back when the
/// other's arrives. Their roots then equal those of a replica that had both pushes from the start, and the ball
/// moves along both pushes' directions, where each push alone moves it along its own only. No push carries a state
/// that would overwrite the other's (3.4: carried state is a check, never written).
#[test]
fn two_members_pushing_one_ball_within_a_network_delay_both_take_effect() {
    let keys = keys();
    let (mut a, mut b) = (EntryBuilder::new(&keys, 0), EntryBuilder::new(&keys, 1));
    let along_x = input(&mut a, 10, &Input::Push { object: oid(BALL), impulse: [1.5, 0.0, 0.0], at: None });
    let along_z = input(&mut b, 12, &Input::Push { object: oid(BALL), impulse: [0.0, 0.0, 1.5], at: None });
    let world = || {
        let mut w = with_hand_and_ball();
        w.set_member(keys.key(1), perm::EDIT);
        w
    };
    const END: u64 = 64;
    // A replica that has `first` from the start, steps to `early`, then receives `late`.
    let run = |first: &[&SignedEntry], early: u64, late: &[&SignedEntry]| {
        let mut s = Source::default();
        first.iter().for_each(|e| s.add((*e).clone()));
        let mut r = replica_of(world());
        r.advance(Frame(early), &mut s);
        late.iter().for_each(|e| s.add((*e).clone()));
        let adv = r.advance(Frame(END), &mut s);
        assert_eq!(r.faults(), 0);
        let ball = (r.world().body(oid(BALL)).unwrap().expect("the ball").pos - Pos::ORIGIN).0;
        (r, adv.rolled_back_to, ball)
    };
    let (mut reference, _, both) = run(&[&along_x, &along_z], 0, &[]);
    // A sends its push at 10 and steps to 15 before B's, stamped 12, arrives (a delay of 3 frames, 50 ms).
    let (mut at_a, a_back, a_ball) = run(&[&along_x], 15, &[&along_z]);
    // B sends its push at 12 and steps to 14 before A's, stamped 10, arrives.
    let (mut at_b, b_back, b_ball) = run(&[&along_z], 14, &[&along_x]);
    assert!(a_back.is_some_and(|f| f.0 <= 12), "A rolled back for B's push: {a_back:?}");
    assert!(b_back.is_some_and(|f| f.0 <= 10), "B rolled back for A's push: {b_back:?}");
    for (r, who) in [(&reference, "the reference"), (&at_a, "A"), (&at_b, "B")] {
        for e in [&along_x, &along_z] {
            let o = r.outcome(&e.id()).map(|o| o.1);
            assert!(matches!(o, Some(Outcome::Applied { .. })), "{who}: {o:?}");
        }
    }
    let want = roots(&mut reference, END);
    assert_eq!(want.len(), 9);
    assert_eq!(roots(&mut at_a, END), want, "A's roots");
    assert_eq!(roots(&mut at_b, END), want, "B's roots");
    assert_eq!((a_ball, b_ball), (both, both));
    // Each push alone moves the ball along its own direction only; together, along both.
    let (_, _, only_x) = run(&[&along_x], 0, &[]);
    let (_, _, only_z) = run(&[&along_z], 0, &[]);
    let start = [2.0, 0.1, 0.0];
    assert!(only_x[0] > start[0] + 0.1 && (only_x[2] - start[2]).abs() < 1e-3, "{only_x:?}");
    assert!(only_z[2] > start[2] + 0.1 && (only_z[0] - start[0]).abs() < 1e-3, "{only_z:?}");
    assert!(both[0] > start[0] + 0.1 && both[2] > start[2] + 0.1, "both take effect: {both:?}");
}

/// A replica of `world` for the member of key `local` (its own interactions are checked, 3.4).
fn replica_for(world: World, local: usize) -> ReplicaImpl<PeWorld> {
    let mut config = config();
    config.local = Some(keys().key(local));
    ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).expect("a replica")
}

/// Finalizes the cut at `frame` over `entries` with this replica's own root there, which must agree.
fn finalize_at(r: &mut ReplicaImpl<PeWorld>, s: &mut Source, entries: &[SignedEntry], frame: u64) {
    let cut = cut_at(entries, frame / K, frame);
    let root = r.root_at_cut(&cut, s).expect("my root at the cut");
    assert_eq!(r.finalize(&FinalCut { cut, root, voters: vec![] }, s), Finalized::Agreed);
}

/// The author's check after its own interactions (replica.md 3.4; @coordinator-opus, option (B), after
/// @fable-reviewer). A pushes the ball at 12. B's push, stamped 10, reaches A in time in one run and
/// after A stepped 12 in the other. Once the cut at 16 is final, A's check of its push agrees in the first run, and in
/// the second finds the ball left other than A's first step of 12 left it: a correction that hit a body A had just
/// acted on, counted in the view. B's push is not A's interaction, so A checks only its own; nothing is judged before
/// the cut is final, and the check changes no state (the roots are the reference's).
#[test]
fn an_author_s_check_counts_a_late_earlier_push_as_a_correction() {
    let keys = keys();
    let (mut a, mut b) = (EntryBuilder::new(&keys, 0), EntryBuilder::new(&keys, 1));
    let mine = input(&mut a, 12, &Input::Push { object: oid(BALL), impulse: [1.5, 0.0, 0.0], at: None });
    let theirs = input(&mut b, 10, &Input::Push { object: oid(BALL), impulse: [0.0, 0.0, 1.5], at: None });
    let entries = [mine.clone(), theirs.clone()];
    let world = || {
        let mut w = with_hand_and_ball();
        w.set_member(keys.key(1), perm::EDIT);
        w
    };
    let (mut reference, _) = reference_of(world(), &entries, 32);
    let mut run = |late: bool| {
        let mut r = replica_for(world(), 0);
        let mut s = Source::default();
        s.add(mine.clone());
        if !late {
            s.add(theirs.clone());
        }
        r.advance(Frame(14), &mut s);
        if late {
            s.add(theirs.clone());
        }
        r.advance(Frame(24), &mut s);
        assert_eq!(r.interaction_checks().judged, 0, "nothing is judged before its frame is final");
        finalize_at(&mut r, &mut s, &entries, 16);
        r.advance(Frame(32), &mut s);
        let above =
            |r: &mut ReplicaImpl<PeWorld>| roots(r, 32).into_iter().filter(|(f, _)| *f >= 16).collect::<Vec<_>>();
        assert_eq!(above(&mut r), above(&mut reference), "the check changes no state");
        assert_eq!(r.faults(), 0);
        (r.interaction_checks().clone(), r.view().counters.interactions_corrected)
    };
    let (on_time, shown) = run(false);
    assert_eq!((on_time.judged, on_time.corrected, on_time.unjudged, shown), (1, 0, 0, 0), "{on_time:?}");
    let (late, shown) = run(true);
    assert_eq!((late.judged, late.corrected, late.unjudged, shown), (1, 1, 0, 1), "{late:?}");
    let want = CorrectedInteraction { frame: Frame(12), object: oid(BALL) };
    assert_eq!(late.recent.iter().copied().collect::<Vec<_>>(), [want]);
}

/// The author's check across a finalize that re-bases on the floor (3.4; @vdw-physics's Medium on d378a2b4). A steps
/// B's push at 10 live, before its own at 12; S3 then voids B's push as the cut at 16 goes final. The live history
/// below the cut is no longer the cut's, so finalize re-steps it and re-bases, and A's check takes its latest from that
/// cut-exact re-step: the ball, which A first stepped with B's push, ends 12 without it. One correction, and the roots
/// from 16 on are those of a run that never had B's push.
#[test]
fn an_author_s_check_takes_the_cut_exact_re_step_when_finalize_re_bases() {
    let keys = keys();
    let (mut a, mut b) = (EntryBuilder::new(&keys, 0), EntryBuilder::new(&keys, 1));
    let mine = input(&mut a, 12, &Input::Push { object: oid(BALL), impulse: [1.5, 0.0, 0.0], at: None });
    let voided = input(&mut b, 10, &Input::Push { object: oid(BALL), impulse: [0.0, 0.0, 1.5], at: None });
    let world = || {
        let mut w = with_hand_and_ball();
        w.set_member(keys.key(1), perm::EDIT);
        w
    };
    let (mut reference, _) = reference_of(world(), std::slice::from_ref(&mine), 32);
    let mut r = replica_for(world(), 0);
    let mut s = Source::default();
    s.add(mine.clone());
    s.add(voided.clone());
    r.advance(Frame(24), &mut s);
    assert!(matches!(r.outcome(&voided.id()).map(|o| o.1), Some(Outcome::Applied { .. })), "stepped live");
    // S3 voids B's push as the cut at 16 goes final, and reports the change.
    s.entries.get_mut(&Frame(10)).expect("its frame").retain(|e| e.id() != voided.id());
    s.changed = Some(Frame(10));
    let cut = cut_at(std::slice::from_ref(&mine), 2, 16);
    let root = reference.live_root(Frame(16)).expect("the reference's root at 16");
    assert_eq!(r.finalize(&FinalCut { cut, root, voters: vec![] }, &mut s), Finalized::Agreed);
    let checks = r.interaction_checks().clone();
    assert_eq!((checks.judged, checks.corrected, checks.unjudged), (1, 1, 0), "{checks:?}");
    let want = CorrectedInteraction { frame: Frame(12), object: oid(BALL) };
    assert_eq!(checks.recent.iter().copied().collect::<Vec<_>>(), [want]);
    r.advance(Frame(32), &mut s);
    let above = |r: &mut ReplicaImpl<PeWorld>| roots(r, 32).into_iter().filter(|(f, _)| *f >= 16).collect::<Vec<_>>();
    assert_eq!(above(&mut r), above(&mut reference));
    assert_eq!(r.faults(), 0, "S3's report inside [10, 16) is excused");
}

/// What the author checks (3.4): a grab begin and a throw, each once, and a push; neither a renewal of its own hold
/// nor a hand input, and nothing on another member's replica, where these are not the local member's.
#[test]
fn an_author_checks_its_grab_begins_pushes_and_throws_only() {
    let keys = keys();
    let mut a = EntryBuilder::new(&keys, 0);
    let grab = Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) };
    let entries = vec![
        input(&mut a, 3, &hand_to(&[2.0], 0)),
        input(&mut a, 3, &grab),
        input(&mut a, 4, &grab),
        input(&mut a, 6, &Input::Throw { object: oid(BALL), linear: [0.5, 0.5, 0.0], angular: [0.0; 3] }),
        input(&mut a, 12, &Input::Push { object: oid(BALL), impulse: [0.0, 0.5, 0.0], at: None }),
    ];
    let world = || {
        let mut w = with_hand_and_ball();
        w.set_member(keys.key(1), perm::EDIT);
        w
    };
    let judged = |local: usize| {
        let mut r = replica_for(world(), local);
        let mut s = Source::default();
        entries.iter().cloned().for_each(|e| s.add(e));
        r.advance(Frame(24), &mut s);
        for e in &entries[1..] {
            let o = r.outcome(&e.id()).map(|o| o.1);
            assert!(matches!(o, Some(Outcome::Applied { .. })), "{o:?}");
        }
        finalize_at(&mut r, &mut s, &entries, 16);
        assert_eq!(r.faults(), 0);
        r.interaction_checks().clone()
    };
    let mine = judged(0);
    assert_eq!((mine.judged, mine.corrected), (3, 0), "the grab at 3, the throw at 6 and the push at 12: {mine:?}");
    assert_eq!(judged(1).judged, 0, "another member's replica checks none of them");
}

/// `hands()` gives each hand at its target, the hand object's world transform, with its row's stamp, span (in
/// frames), grip and engagement, and the input before it (@world-model).
#[test]
fn hands_follow_their_inputs() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut pw = PeWorld::new(with_hand_and_ball(), DomainId(0));
    let h = pw.hands();
    assert_eq!((h.len(), h[0].hand, h[0].span, h[0].previous), (1, oid(HAND), 0, None));
    let first = input(&mut b, 3, &hand_to(&[1.0, 1.5], 2));
    assert!(matches!(pw.apply(Frame(3), &first).0, Outcome::Applied { .. }));
    let h = pw.hands()[0];
    assert_eq!((h.owner, h.stamp, h.span, h.previous), (keys.key(0), Frame(3), 4, None));
    assert_eq!((h.target, h.grip, h.engaged), (pos(1.0, 0.1, 0.0), 0.5, true));
    let second = input(&mut b, 5, &hand_to(&[2.0], 0));
    let (outcome, effects) = pw.apply(Frame(5), &second);
    assert!(matches!(outcome, Outcome::Applied { .. }) && effects.is_empty(), "no body under the hand");
    let h = pw.hands()[0];
    assert_eq!((h.target, h.stamp, h.span, h.previous), (pos(2.0, 0.1, 0.0), Frame(5), 1, Some((Frame(3), 4))));
}

/// An `Admit` admits its key at its frame (agreement.md 8; app.md 4, "Joining from a cut"): the joiner gets the role
/// the payload names, and two hands it owns, with ids from that frame, at the lowest slot of the genesis row no hand
/// takes (`World::join_spawn`). The fixture's editor has no hands, so the first joiner takes slot 0 and a second one
/// slot 1; counting members would have put the second at slot 2. A second `Admit` of the joiner and a malformed one
/// are refused, as S3 voids them, and change nothing.
#[test]
fn an_admit_admits_its_key_with_its_role_and_two_hands() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut pw = PeWorld::new(genesis(), DomainId(0));
    let joiner = keys.key(1);
    let role = perm::ADMIN | perm::EDIT | perm::AGENT;
    let first = b.next(Class::Protocol, Frame(4), pe_seams::Admit::new(joiner, role).encode());
    assert!(matches!(pw.apply(Frame(4), &first).0, Outcome::Applied { .. }));
    assert_eq!(pw.world().snapshot(Frame(4)).perms(joiner), Some(role));
    let line = first.entry.line;
    let hands = |pw: &PeWorld| {
        pw.hands().into_iter().filter(|h| h.owner == joiner).map(|h| (h.hand, h.target)).collect::<Vec<_>>()
    };
    let hands_of = |pw: &PeWorld, who: MemberKey| {
        let mut v: Vec<_> = pw.hands().into_iter().filter(|h| h.owner == who).map(|h| (h.hand, h.target)).collect();
        v.sort_by_key(|(id, _)| id.to_u128());
        v
    };
    let slot = |who: MemberKey, frame: Frame, x: f64| {
        let mut v = vec![
            (ObjectId::hand(line, who, 0, frame), pos(x - 0.25, 1.2, 1.8)),
            (ObjectId::hand(line, who, 1, frame), pos(x + 0.25, 1.2, 1.8)),
        ];
        v.sort_by_key(|(id, _)| id.to_u128());
        v
    };
    assert_eq!(hands_of(&pw, joiner), slot(joiner, Frame(4), 0.0), "two hands, owned by the joiner, from frame 4");
    let second = MemberKey([9; 32]);
    let next = b.next(Class::Protocol, Frame(5), pe_seams::Admit::new(second, role).encode());
    assert!(matches!(pw.apply(Frame(5), &next).0, Outcome::Applied { .. }));
    assert_eq!(hands_of(&pw, second), slot(second, Frame(5), 0.6), "the next free slot, not the members' count");

    // A member admitted again is a returning one (F3 L3): it applies and changes nothing. A malformed one is refused.
    let again = b.next(Class::Protocol, Frame(6), pe_seams::Admit::new(joiner, perm::EDIT).encode());
    let before = pw.world().root_hash(DomainId(0));
    assert!(matches!(pw.apply(Frame(6), &again).0, Outcome::Applied { .. }));
    assert_eq!(pw.world().root_hash(DomainId(0)), before, "a returning member changes nothing");
    let malformed = b.next(Class::Protocol, Frame(6), Bytes::from([pe_seams::ADMIT, &[1]].concat()));
    assert!(matches!(pw.apply(Frame(6), &malformed).0, Outcome::Refused { .. }));
    assert_eq!(pw.world().snapshot(Frame(6)).perms(joiner), Some(role), "the first role stands");
    assert_eq!(hands(&pw).len(), 2, "no more hands");
}

/// A replay that drops a voided `Admit` (S3 voids one it finds `NotAuthorized`, say) leaves the world as if the entry
/// had never been written: the same roots, and no hands for the key (@vdw-physics's Low 1 on ae54e76a). The replay
/// restores the world and the physics from before the entry's frame, so the member row, the hands and their bodies go
/// together.
#[test]
fn a_replay_that_drops_a_voided_admit_leaves_no_trace_of_it() {
    let keys = keys();
    let joiner = MemberKey([9; 32]);
    let admit = EntryBuilder::new(&keys, 1).next(
        Class::Protocol,
        Frame(10),
        pe_seams::Admit::new(joiner, perm::ADMIN | perm::EDIT | perm::AGENT).encode(),
    );
    let hands_of =
        |r: &ReplicaImpl<PeWorld>| r.view().objects.iter().filter(|o| o.hand_of.is_some_and(|h| h.0 == joiner)).count();
    let mut s0 = Source::default();
    scene().into_iter().for_each(|e| s0.add(e));
    let mut clean = replica();
    clean.advance(Frame(48), &mut s0);
    let mut s = Source::default();
    scene().into_iter().for_each(|e| s.add(e));
    s.add(admit.clone());
    let mut a = replica();
    a.advance(Frame(32), &mut s);
    assert_eq!(hands_of(&a), 2, "the Admit applied: the joiner's two hands");
    assert_ne!(a.live_root(Frame(16)), clean.live_root(Frame(16)), "and the world holds the joiner");
    // S3 voids it: gone from the log, with a change reported from its frame.
    s.entries.get_mut(&Frame(10)).expect("its frame").retain(|e| e.id() != admit.id());
    s.changed = Some(Frame(10));
    a.advance(Frame(48), &mut s);
    assert_eq!(roots(&mut a, 48), roots(&mut clean, 48), "the same roots as a run without it");
    assert_eq!(hands_of(&a), 0, "and no hands for the key");
    assert_eq!(a.faults(), 0);
}

// ---- import through pe-world's tree (slice C, world-model.md 6.5 and 6.6) -------------------------------------------

/// The scene's two drops, then a third ball dropped at frame 40, all from the editor, in one chain.
fn scene_and_a_drop_at_40() -> Vec<SignedEntry> {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let drop = |x: f64, y: f64| {
        let rows = vec![row(Transform::at(pos(x, y, 0.0))), row(dynamic()), row(sphere(0.25))];
        Bytes::from(tx(vec![Op::Create { new: 0, parent: Target::Root, index: None, rows }]).encode())
    };
    vec![
        b.next(Class::Edit, Frame(2), drop(0.0, 1.0)),
        b.next(Class::Edit, Frame(5), drop(0.3, 2.0)),
        b.next(Class::Edit, Frame(40), drop(-0.4, 1.5)),
    ]
}

fn config() -> ReplicaConfig {
    let mut config = ReplicaConfig::new(pe_seams::SimRate::DEFAULT, Duration::from_millis(133), Arc::new(CLOCK));
    config.cut_every = K;
    config
}

/// A replica of `world` that had all of `entries` from the start, run to `to`.
fn reference_of(world: World, entries: &[SignedEntry], to: u64) -> (ReplicaImpl<PeWorld>, Source) {
    let mut r = ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config()).expect("a replica");
    let mut s = Source::default();
    entries.iter().cloned().for_each(|e| s.add(e));
    r.advance(Frame(to), &mut s);
    (r, s)
}

/// A joiner's replica before import: a bare world with the engine registry and default settings (S2 v1.3).
fn joiner_of(host: &ReplicaImpl<PeWorld>) -> ReplicaImpl<PeWorld> {
    let bare = World::new(0, host.world().world().blobs().clone());
    ReplicaImpl::new(PeWorld::new(bare, DomainId(0)), config()).expect("a replica")
}

/// The cut at `frame`: per author, the last entry stamped below it.
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

/// The donor's whole state at `cut`, as a full-state fetch asks for it: every object, every table and the config
/// record, with their proofs, and the physics image. No object is created or removed after the cut in these scenes
/// before the donor's present, so the live world's objects are the cut's.
fn full_import(donor: &mut ReplicaImpl<PeWorld>, s: &mut Source, cut: &Cut) -> CutImport {
    let root = donor.root_at_cut(cut, s).expect("the donor's root");
    let mut keys: Vec<LeafKey> =
        donor.world().world().store().objects().iter().map(|&id| LeafKey::Object(id)).collect();
    keys.extend((0..pe_world::tree::table::COUNT as u8).map(LeafKey::Table));
    keys.push(LeafKey::Config);
    let records = donor.tree(&root).expect("held").export(&keys);
    CutImport {
        blobs: vec![],
        cut: cut.clone(),
        target_root: root,
        records,
        absent: Vec::new(),
        physics: donor.physics_image(&root),
    }
}

fn from(r: &mut ReplicaImpl<PeWorld>, at: u64, to: u64) -> Vec<(u64, Hash)> {
    roots(r, to).into_iter().filter(|r| r.0 >= at).collect()
}

/// A joiner imports the real world at a cut from empty, through pe-world's tree (world-model.md 6.5): every record is
/// checked against the agreed root, so a record changed on the way is refused and changes nothing. The checked
/// import's root is the host's, finalize agrees, and the joiner steps on with the host's roots.
/// pe-api's `final` is the world at Cv (S2 v1.7, `Replica::agreed_snapshot`; @coordinator-opus): none
/// before the first agreed finalize, then the state at the agreed cut's frame, while the live world goes on.
#[test]
fn the_agreed_snapshot_is_the_world_at_the_last_agreed_cut() {
    let entries = scene();
    let (mut host, mut hs) = reference_of(genesis(), &entries, 64);
    assert!(host.agreed_snapshot().is_none(), "no cut agreed yet");
    let c32 = cut_at(&entries, 4, 32);
    let root = host.root_at_cut(&c32, &mut hs).expect("my root at 32");
    let fc32 = FinalCut { cut: c32, root, voters: vec![] };
    assert_eq!(host.finalize(&fc32, &mut hs), Finalized::Agreed);
    let (frame, snap) = host.agreed_snapshot().expect("Cv's snapshot");
    let snap = snap.downcast::<pe_world::WorldSnapshot>().expect("pe-world's snapshot");
    assert_eq!((frame, snap.frame(), snap.final_frame()), (Frame(32), Frame(32), Frame(32)));
    let live = host.world_snapshot().expect("the live one").downcast::<pe_world::WorldSnapshot>().expect("pe-world's");
    assert_eq!(live.frame(), host.frame(), "the live world is at the replica's frame");
    assert!(live.frame() > frame, "and further on than the agreed cut");
    assert_eq!(snap.all(), live.all(), "both balls were dropped before 32");
}

#[test]
fn a_joiner_imports_the_real_world_through_pe_world_s_tree() {
    let entries = scene();
    let (mut host, mut hs) = reference_of(genesis(), &entries, 32);
    let c32 = cut_at(&entries, 4, 32);
    let import = full_import(&mut host, &mut hs, &c32);
    let mut s = Source::default();
    entries.iter().cloned().for_each(|e| s.add(e));
    // The joiner's S3 held these entries before it joined: no change to report below its floor.
    s.take_changed_from();
    let mut joiner = joiner_of(&host);
    let before = (joiner.frame(), joiner.world().root());
    let mut tampered = import.clone();
    let i = tampered.records.iter().position(|r| matches!(r.key, LeafKey::Object(_))).expect("an object record");
    let mut bytes = tampered.records[i].bytes.to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    tampered.records[i].bytes = Bytes::from(bytes);
    let got = joiner.import_at_cut(&GroupSel::Domain, tampered, &mut s);
    assert!(matches!(got, Err(ImportError::BadProof(_))), "a changed record: {got:?}");
    assert_eq!((joiner.frame(), joiner.world().root()), before, "the live state is unchanged");
    assert_eq!(joiner.import_at_cut(&GroupSel::Domain, import.clone(), &mut s), Ok(import.target_root));
    let fc32 = FinalCut { cut: c32, root: import.target_root, voters: vec![] };
    assert_eq!(joiner.finalize(&fc32, &mut s), Finalized::Agreed);
    host.advance(Frame(64), &mut hs);
    joiner.advance(Frame(64), &mut s);
    assert_eq!(from(&mut joiner, 32, 64), from(&mut host, 32, 64));
    assert_eq!(joiner.faults(), 0);
}

/// Non-default gravity and origin, imported through PeWorld by a default joiner (@coordinator-opus,
/// item 6): the config record sets the joiner's world's settings, and the replica builds the new physics world from
/// them. When an entry then arrives late, the joiner rolls back into that physics world and re-simulates with the
/// imported settings, and its roots stay the host's.
#[test]
fn a_joiner_takes_gravity_and_origin_from_the_real_world_it_imports() {
    let entries = scene_and_a_drop_at_40();
    let mut world = genesis();
    world.set_gravity([0.0, -3.0, 0.0]);
    world.set_origin(Pos::ORIGIN.checked_add(Offset([40.0, 0.0, -25.0])).expect("an origin"));
    let (mut host, mut hs) = reference_of(world, &entries, 32);
    let c32 = cut_at(&entries, 4, 32);
    let import = full_import(&mut host, &mut hs, &c32);
    let (late, early) = entries.split_last().expect("three entries");
    let mut s = Source::default();
    early.iter().cloned().for_each(|e| s.add(e));
    s.take_changed_from();
    let mut joiner = joiner_of(&host);
    assert_ne!(joiner.world().world().settings(), host.world().world().settings(), "a default joiner");
    assert_eq!(joiner.import_at_cut(&GroupSel::Domain, import.clone(), &mut s), Ok(import.target_root));
    assert_eq!(joiner.world().world().settings(), host.world().world().settings(), "the host's gravity and origin");
    let fc32 = FinalCut { cut: c32, root: import.target_root, voters: vec![] };
    assert_eq!(joiner.finalize(&fc32, &mut s), Finalized::Agreed);
    joiner.advance(Frame(48), &mut s);
    s.add(late.clone());
    let adv = joiner.advance(Frame(64), &mut s);
    assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= 40), "the late drop rolls back: {adv:?}");
    host.advance(Frame(64), &mut hs);
    assert_eq!(from(&mut joiner, 32, 64), from(&mut host, 32, 64), "the host's roots, the late drop included");
    assert_eq!(joiner.faults(), 0);
}

/// Each cut's root warms pe-world's leaf cache (world-model.md 6.4; @chunks-reviewer's Low on 5e7effd2): right after a
/// cut every object's leaf is cached, so the next root rehashes only what changed, and every cached leaf is still the
/// hash of its rows.
#[test]
fn a_cut_s_root_leaves_every_leaf_cached_and_true() {
    let mut s = Source::default();
    scene().into_iter().for_each(|e| s.add(e));
    let mut r = replica();
    r.advance(Frame(64), &mut s);
    let world = r.world().world();
    let objects = world.store().objects();
    assert_eq!(objects.len(), 3, "the floor and two balls");
    assert!(objects.iter().all(|&id| world.store().cached_hash(id).is_some()), "every leaf is cached at the cut");
    assert!(world.verify_tree_cache(DomainId(0)).is_ok(), "and every cached leaf is its rows' hash");
}

/// Every world setting (world-model.md 6.6), none at its default, imported through PeWorld by a joiner that has only
/// the welcome's rate (app.md 4, step 4; @coordinator-opus): the seed, the line and its M1 marker,
/// pe-agree's settings, the rate (90 Hz), the depth limit, gravity, the origin and the input settings. The joiner's
/// config record is then the host's byte for byte, and through the hand moves, a grab, a push and a release after the
/// cut, which read the input settings (reach, slack, claims, the push budget), the joiner's roots stay the host's.
#[test]
fn a_joiner_takes_every_world_setting_from_the_real_world_it_imports() {
    let rate = pe_seams::SimRate::new(90).expect("90 Hz");
    let line = pe_world::preset::m1_line();
    let keys = keys();
    let mut world = pe_world::preset::ball_pit_with(9, BlobStore::default(), Pos::ORIGIN, 6);
    let [left, _] =
        world.admit(line, keys.key(0), perm::ADMIN | perm::EDIT, Frame(0), pos(0.0, 0.5, 0.0)).expect("admitted");
    world.set_rate(rate);
    world.set_depth_limit(12);
    world.set_gravity([0.5, -4.0, 0.25]);
    world.set_origin(Pos::ORIGIN.checked_add(Offset([-30.0, 2.0, 12.0])).expect("an origin"));
    world.set_agree_settings(b"pe-agree's settings, opaque to pe-world");
    let input_settings =
        pe_world::InputSettings { claim_ms: 900, cooldown_ms: 300, push_budget: 5_000, reach_mm: 450, slack_mm: 60 };
    world.set_input_settings(input_settings).expect("valid input settings");
    assert_ne!(input_settings, pe_world::InputSettings::default());
    let ball = world
        .physical()
        .into_iter()
        .find(|&id| world.store().get::<Body>(id).is_some_and(|b| b.kind == body_kind::DYNAMIC))
        .expect("a ball");
    let at = world.store().world_transform(ball).expect("placed").pos;
    let near = |dy: f64| at.checked_add(Offset([0.0, dy, 0.0])).expect("in range");
    // The left hand goes to the ball and stays over it; after the cut it grabs, pushes and releases the ball.
    let mut b = EntryBuilder::on_line(&keys, 0, line);
    let mut entries = Vec::new();
    for f in (4..60).step_by(4) {
        let pose = HandPose { pos: near(0.15), rot: [0.0, 0.0, 0.0, 1.0], grip: 0.8, engaged: f >= 36 };
        entries.push(input(&mut b, f, &Input::Hand { hand: left, poses: vec![pose], hold: 3 }));
        match f {
            40 => entries.push(input(&mut b, f, &Input::Grab { object: ball, hand: Some(left) })),
            44 => entries.push(input(&mut b, f, &Input::Push { object: ball, impulse: [0.2, 0.3, 0.0], at: None })),
            52 => entries.push(input(&mut b, f, &Input::Release { object: ball })),
            _ => {}
        }
    }
    let config = || {
        let mut c = ReplicaConfig::new(rate, Duration::from_millis(133), Arc::new(MockClock { rate, ..CLOCK }));
        c.cut_every = K;
        c
    };
    let mut host = ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config()).expect("the host");
    let mut hs = Source::default();
    entries.iter().cloned().for_each(|e| hs.add(e));
    host.advance(Frame(32), &mut hs);
    let c32 = cut_at(&entries, 4, 32);
    let import = full_import(&mut host, &mut hs, &c32);
    let mut s = Source::default();
    entries.iter().cloned().for_each(|e| s.add(e));
    s.take_changed_from();
    // A joiner with nothing but the welcome's rate: a bare world (engine registry, default settings) at that rate.
    let mut bare = World::new(0, host.world().world().blobs().clone());
    bare.set_rate(rate);
    let mut joiner = ReplicaImpl::new(PeWorld::new(bare, DomainId(0)), config()).expect("the joiner");
    let record = |r: &ReplicaImpl<PeWorld>| r.world().world().config_bytes();
    assert_ne!(record(&joiner), record(&host), "a default joiner");
    assert_eq!(joiner.import_at_cut(&GroupSel::Domain, import.clone(), &mut s), Ok(import.target_root));
    assert_eq!(record(&joiner), record(&host), "every setting is the host's");
    assert_eq!(joiner.world().world().input_settings(), input_settings);
    assert_eq!(joiner.world().world().m1_marker(), Some(line));
    let fc32 = FinalCut { cut: c32, root: import.target_root, voters: vec![] };
    assert_eq!(joiner.finalize(&fc32, &mut s), Finalized::Agreed);
    host.advance(Frame(64), &mut hs);
    joiner.advance(Frame(64), &mut s);
    assert_eq!(
        from(&mut joiner, 32, 64),
        from(&mut host, 32, 64),
        "the host's roots through the grab, push and release"
    );
    assert_eq!((joiner.faults(), host.faults()), (0, 0));
}

// ---- hand bodies (replica.md 3.3; world-model.md 8.1; D18b) ---------------------------------------------------------

/// A hand pack along the floor: one pose per `x`, at (x, 0.1, 0), closed or open.
fn hand_path(xs: &[f64], closed: bool) -> Input {
    let pose = |x| HandPose { pos: pos(x, 0.1, 0.0), rot: [0.0, 0.0, 0.0, 1.0], grip: 0.5, engaged: closed };
    Input::Hand { hand: oid(HAND), poses: xs.iter().map(|&x| pose(x)).collect(), hold: 0 }
}

/// The hand along `xs`, one pose a frame from frame `from`, in packs of at most pe-world's `MAX_POSES`, each
/// following the one before, so they make a stream.
fn hand_packs(b: &mut EntryBuilder, from: u64, xs: &[f64], closed: bool) -> Vec<SignedEntry> {
    let packs = xs.chunks(pe_world::input::MAX_POSES);
    packs
        .enumerate()
        .map(|(k, c)| input(b, from + (k * pe_world::input::MAX_POSES) as u64, &hand_path(c, closed)))
        .collect()
}

fn mode(m: u8) -> Input {
    Input::HandMode { hand: oid(HAND), mode: m }
}

/// Where `id` is now, as plain numbers.
fn at(r: &ReplicaImpl<PeWorld>, id: u128) -> [f64; 3] {
    (r.world().world().store().world_transform(oid(id)).expect("placed").pos - Pos::ORIGIN).0
}

/// Run `r` one frame at a time to `to`, returning the fastest `ids` moved between two frames, in m/s.
fn fastest(r: &mut ReplicaImpl<PeWorld>, s: &mut Source, ids: &[u128], to: u64) -> f64 {
    let mut last: Vec<[f64; 3]> = ids.iter().map(|&id| at(r, id)).collect();
    let mut top = 0.0_f64;
    while r.frame().0 < to {
        let next = r.frame().0 + 1;
        r.advance(Frame(next), s);
        for (k, &id) in ids.iter().enumerate() {
            let p = at(r, id);
            let d = (0..3).map(|i| (p[i] - last[k][i]).powi(2)).sum::<f64>().sqrt();
            top = top.max(d * 60.0);
            last[k] = p;
        }
    }
    top
}

/// The hand sweeps along the floor from x = 0 to x = 3 at 6 m/s, through the ball at x = 2, closed or open, after
/// an optional mode input. Where the ball ends.
fn sweep(closed: bool, mode_input: Option<u8>) -> [f64; 3] {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut s = Source::default();
    if let Some(m) = mode_input {
        s.add(input(&mut b, 1, &mode(m)));
    }
    let xs: Vec<f64> = (0..=30).map(|k| 0.1 * f64::from(k)).collect();
    hand_packs(&mut b, 2, &xs, closed).into_iter().for_each(|e| s.add(e));
    let mut r = replica_of(with_hand_and_ball());
    r.advance(Frame(90), &mut s);
    assert_eq!(r.faults(), 0);
    at(&r, BALL)
}

/// D18b's modes (@coordinator-opus): in "closed hand", the default, a closed hand has a
/// body and pushes the ball it sweeps through, and an open hand has none and passes through it; in "always within
/// reach" an open hand pushes too.
#[test]
fn a_closed_hand_pushes_a_ball_and_an_open_one_passes_through_unless_always_within_reach() {
    let pushed = sweep(true, None);
    assert!(pushed[0] > 2.5, "a closed hand pushes the ball along: {pushed:?}");
    let left = sweep(false, None);
    assert!((left[0] - 2.0).abs() < 0.01, "an open hand passes through it: {left:?}");
    let always = sweep(false, Some(hand_mode::ALWAYS_WITHIN_REACH));
    assert!(always[0] > 2.5, "always within reach, an open hand pushes it too: {always:?}");
}

/// The cap on a hand's speed (world-model.md 8.1's `max_push_speed`; @coordinator-opus, option (b)): a
/// move past the teleport distance in one frame places the hand's body without sweeping it, and without carrying its
/// speed into what it touches; a shorter move faster than the cap is slowed to it. A closed hand resting against the
/// ball teleports through it, 1.19 m in one frame (71 m/s), and leaves it where it was; one that teleports 0.75 m into
/// the ball's side pushes it out gently; one that moves 0.45 m into it in one frame (27 m/s) is slowed to the cap and
/// hits it at that speed, plus at most the solver's push-out speed.
#[test]
fn a_teleport_hits_nothing_and_a_fast_move_hits_at_the_cap() {
    let run = |from: f64, to: f64| {
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let mut s = Source::default();
        let mut xs = vec![from; 10];
        xs.extend([to; 6]);
        s.add(input(&mut b, 2, &hand_path(&xs, true)));
        let mut r = replica_of(with_hand_and_ball());
        r.advance(Frame(11), &mut s);
        let before = at(&r, BALL);
        let top = fastest(&mut r, &mut s, &[BALL], 40);
        assert_eq!(r.faults(), 0);
        (before, top, at(&r, BALL))
    };
    let cap = pe_replica::hands::MAX_HAND_PUSH_SPEED;
    let rest = 2.0 - 0.1 - f64::from(pe_replica::world::HAND_RADIUS);
    let (before, top, after) = run(rest, 3.0);
    assert!(top < 1.0, "a teleport through the ball leaves it: {top} m/s");
    assert!((after[0] - before[0]).abs() < 0.05, "where it was: {before:?} then {after:?}");
    let (_, top, after) = run(1.2, 1.95);
    assert!(top < contact_speed() + 1.0, "a teleport into the ball pushes it out gently: {top} m/s");
    assert!(after[0] > 2.05, "and out of the hand: {after:?}");
    let (_, top, _) = run(1.5, 1.95);
    assert!(top > 0.5 * cap && top < cap + contact_speed(), "a fast move hits it at the cap: {top} m/s");
}

/// Box3D's speed cap for overlap resolution, m/s, as pe-physics's engine world has it: a body driven at exactly the
/// hand cap can leave what it hits up to this much faster while their contact pushes them apart.
fn contact_speed() -> f64 {
    f64::from(pe_physics::PhysicsConfig::engine().world_def().contact_speed)
}

/// A hand that becomes physical inside a pile (its mode flips to always within reach) appears where it is, and the
/// soft contacts move the balls it overlaps apart gently instead of throwing them (world-model.md 8.1): three balls
/// 12 cm from the hand's centre, each 7 cm inside its body and clear of the others, so only the hand moves them.
#[test]
fn a_hand_that_becomes_physical_inside_a_pile_moves_the_balls_apart_gently() {
    let k = keys().key(0);
    let mut w = genesis();
    w.create(oid(HAND), None, k, GENESIS).expect("hand");
    w.insert(oid(HAND), Transform::at(pos(0.0, 0.1, 0.0))).expect("hand transform");
    w.insert(oid(HAND), Hand { owner: k, ..Hand::default() }).expect("hand row");
    let ring: Vec<u128> = (0..3).map(|i| BALL + i).collect();
    for (i, &id) in ring.iter().enumerate() {
        let angle = std::f64::consts::TAU * i as f64 / 3.0;
        w.create(oid(id), None, k, GENESIS).expect("ball");
        w.insert(oid(id), Transform::at(pos(0.12 * angle.cos(), 0.1, 0.12 * angle.sin()))).expect("ball transform");
        w.insert(oid(id), dynamic()).expect("ball body");
        w.insert(oid(id), sphere(0.1)).expect("ball shape");
    }
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut s = Source::default();
    s.add(input(&mut b, 10, &mode(hand_mode::ALWAYS_WITHIN_REACH)));
    let mut r = replica_of(w);
    r.advance(Frame(10), &mut s);
    let before: Vec<[f64; 3]> = ring.iter().map(|&id| at(&r, id)).collect();
    let top = fastest(&mut r, &mut s, &ring, 60);
    assert!(top < 4.0, "the pile moves apart gently: {top} m/s");
    let moved = ring.iter().zip(&before).any(|(&id, b)| (at(&r, id)[0] - b[0]).abs() > 0.01);
    assert!(moved, "the hand's body pushed the balls it overlapped");
    assert_eq!(r.faults(), 0);
}

/// Hand bodies are rebuilt from the hand rows inside each frame's step, never kept across a rollback (world-model.md
/// 8.1): a mode flip that arrives late rolls back to before it, and the roots are those of a replica that had it all
/// along.
#[test]
fn a_mode_flip_that_arrives_late_rolls_back_to_the_same_roots() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let xs: Vec<f64> = (0..=30).map(|k| 0.1 * f64::from(k)).collect();
    let sweep_in = hand_packs(&mut b, 2, &xs, false);
    let flip = input(&mut b, 12, &mode(hand_mode::ALWAYS_WITHIN_REACH));
    let mut all = Source::default();
    sweep_in.iter().cloned().for_each(|e| all.add(e));
    all.add(flip.clone());
    let mut a = replica_of(with_hand_and_ball());
    a.advance(Frame(64), &mut all);
    let mut late = Source::default();
    sweep_in.into_iter().for_each(|e| late.add(e));
    let mut c = replica_of(with_hand_and_ball());
    c.advance(Frame(40), &mut late);
    late.add(flip);
    let adv = c.advance(Frame(64), &mut late);
    assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= 12), "the late flip rolls back: {adv:?}");
    assert_eq!(roots(&mut c, 64), roots(&mut a, 64));
    assert!(at(&a, BALL)[0] > 2.5, "the flip made the open hand push the ball");
    assert_eq!((a.faults(), c.faults()), (0, 0));
}

/// After its input's span, a hand that came as part of a stream coasts at its last velocity for 100 ms (replica.md
/// 3.3, D52), then goes back to where the world's hand is and stays (@design-reviewer's Low on 52575b48): with single
/// poses 5 cm a frame apart (3 m/s) ending at frame 4, its body moves on for six frames at 60 Hz, then goes back to
/// 0.10 m at the cap, so it never parks away from the hand everyone sees.
#[test]
fn a_streamed_hand_coasts_for_100_ms_after_its_span_then_goes_back_to_the_hand() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut s = Source::default();
    for (f, x) in [(2u64, 0.0), (3, 0.05), (4, 0.10)] {
        s.add(input(&mut b, f, &hand_path(&[x], true)));
    }
    let mut r = replica_of(with_hand_and_ball());
    let mut xs = Vec::new();
    for f in 5..=16u64 {
        r.advance(Frame(f), &mut s);
        xs.push(r.physics().pose(oid(HAND)).expect("the hand's body").position[0]);
    }
    // xs[k] is the body's x after frame 4 + k: it reached 0.10 at frame 4, coasts through frames 5 to 10 to 0.40,
    // goes back a sixth of a metre in frame 11 (10 m/s, the cap), and is at 0.10 from frame 12 on.
    let step = |k: usize| xs[k + 1] - xs[k];
    assert!((0..5).all(|k| (step(k) - 0.05).abs() < 1e-3), "it coasts at 3 m/s: {xs:?}");
    assert!((xs[6] - 0.40).abs() < 1e-3, "six frames of coast: {xs:?}");
    let back = 0.40 - pe_replica::hands::MAX_HAND_PUSH_SPEED / 60.0;
    assert!((xs[7] - back).abs() < 1e-3, "it goes back at the cap: {xs:?}");
    assert!((8..xs.len()).all(|k| (xs[k] - 0.10).abs() < 1e-5), "then it is back at the hand and stays: {xs:?}");
    // The body is the replica's own: it writes no pose and no `BodyState` row back to the world model, whose hand
    // stays where its last input put it, through the cuts at frames 8 and 16.
    assert!((at(&r, HAND)[0] - 0.10).abs() < 1e-6, "the world's hand stays at its input: {:?}", at(&r, HAND));
    assert!(r.world().world().store().get::<BodyState>(oid(HAND)).is_none(), "no BodyState row for a hand");
    assert_eq!(r.faults(), 0);
}

/// A hand input for `hand` with one pose per point, turned by `rot`, closed or open.
fn hand_of(hand: u128, points: &[[f64; 3]], rot: [f32; 4], closed: bool) -> Input {
    let pose = |p: &[f64; 3]| HandPose {
        pos: pos(p[0], p[1], p[2]),
        rot,
        grip: if closed { 1.0 } else { 0.0 },
        engaged: closed,
    };
    Input::Hand { hand: oid(hand), poses: points.iter().map(pose).collect(), hold: 0 }
}

const UNTURNED: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// A hand input for the hand with one pose per point, closed or open.
fn hand_at(points: &[[f64; 3]], closed: bool) -> Input {
    hand_of(HAND, points, UNTURNED, closed)
}

/// Grabs mirror the claims (replica.md 3.3; world-model.md 7.2): a closed hand that grabs the ball holds it by a
/// joint, so the ball rises with the hand, without the hand's own body pushing it away (a grab's two bodies don't
/// collide), and falls once released, when the joint goes. An agent's hand-less grab is a claim with no joint.
#[test]
fn a_hand_holds_what_it_grabs_until_it_lets_go() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut s = Source::default();
    let rest = 2.0 - 0.1 - f64::from(pe_replica::world::HAND_RADIUS);
    let lift: Vec<[f64; 3]> = (0..32).map(|k| [rest, 0.1 + 0.9 * f64::from(k + 1) / 32.0, 0.0]).collect();
    s.add(input(&mut b, 2, &hand_at(&[[rest, 0.1, 0.0]; 8], true)));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    s.add(input(&mut b, 10, &hand_at(&lift[..16], true)));
    s.add(input(&mut b, 26, &hand_at(&lift[16..], true)));
    s.add(input(&mut b, 46, &Input::Release { object: oid(BALL) }));
    let mut r = replica_of(with_hand_and_ball());
    r.advance(Frame(44), &mut s);
    assert_eq!(r.world().claims().iter().map(|c| (c.object, c.hand)).collect::<Vec<_>>(), [(oid(BALL), oid(HAND))]);
    assert_eq!(r.physics().joint_count(), 1, "the claim's grab joint");
    let held = at(&r, BALL);
    assert!(held[1] > 0.7, "the ball rose with the hand: {held:?}");
    assert!((held[0] - 2.0).abs() < 0.1, "held where the hand took it, not pushed off: {held:?}");
    r.advance(Frame(110), &mut s);
    assert!(r.world().claims().is_empty() && r.physics().joint_count() == 0, "released: no claim, no joint");
    let dropped = at(&r, BALL);
    assert!(dropped[1] < 0.2, "the ball fell once released: {dropped:?}");
    assert_eq!(r.faults(), 0);

    // An agent's grab has no hand: a claim, and no joint.
    let mut b = EntryBuilder::new(&keys, 0);
    let mut s = Source::default();
    s.add(input(&mut b, 2, &Input::Grab { object: oid(BALL), hand: None }));
    let mut r = replica_of(with_hand_and_ball());
    r.advance(Frame(8), &mut s);
    let claims: Vec<_> = r.world().claims().iter().map(|c| (c.object, c.hand)).collect();
    assert_eq!(claims, [(oid(BALL), ObjectId::NONE)], "an agent's claim");
    assert_eq!(r.physics().joint_count(), 0, "with no joint");
}

/// A grab joint is in the physics image like the bodies (replica.md 3.3): a grab that arrives late rolls back to
/// before it, the joint is made at the same frame as on a replica that had it all along, and the roots are equal.
#[test]
fn a_grab_that_arrives_late_rolls_back_to_the_same_roots() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let rest = 2.0 - 0.1 - f64::from(pe_replica::world::HAND_RADIUS);
    let lift: Vec<[f64; 3]> = (0..16).map(|k| [rest, 0.1 + 0.05 * f64::from(k + 1), 0.0]).collect();
    let hold = input(&mut b, 2, &hand_at(&[[rest, 0.1, 0.0]; 8], true));
    let grab = input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) });
    let up = input(&mut b, 10, &hand_at(&lift, true));
    let mut all = Source::default();
    [hold.clone(), grab.clone(), up.clone()].into_iter().for_each(|e| all.add(e));
    let mut a = replica_of(with_hand_and_ball());
    a.advance(Frame(48), &mut all);
    let mut late = Source::default();
    [hold, up].into_iter().for_each(|e| late.add(e));
    let mut c = replica_of(with_hand_and_ball());
    c.advance(Frame(30), &mut late);
    late.add(grab);
    let adv = c.advance(Frame(48), &mut late);
    assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= 6), "the late grab rolls back: {adv:?}");
    assert_eq!(roots(&mut c, 48), roots(&mut a, 48));
    assert!(at(&a, BALL)[1] > 0.5, "the grab lifted the ball: {:?}", at(&a, BALL));
    assert_eq!((a.faults(), c.faults()), (0, 0));
}

// ---- what a hand holds: handoffs, jumps and the grab's spring (@design-reviewer's Medium and Low on cc037c10) -------

const RIGHT: u128 = HAND + 1;
const NEXT: u128 = BALL + 1;
const BESIDE: u128 = BALL + 2;

/// Genesis, with the editor's hands at their points and dynamic balls (id, centre, shape).
fn with_hands_and_balls(hands: &[(u128, [f64; 3])], balls: &[(u128, [f64; 3], Shape)]) -> World {
    let k = keys().key(0);
    let mut w = genesis();
    for &(h, p) in hands {
        w.create(oid(h), None, k, GENESIS).expect("hand");
        w.insert(oid(h), Transform::at(pos(p[0], p[1], p[2]))).expect("hand transform");
        w.insert(oid(h), Hand { owner: k, ..Hand::default() }).expect("hand row");
    }
    for &(b, p, shape) in balls {
        w.create(oid(b), None, k, GENESIS).expect("ball");
        w.insert(oid(b), Transform::at(pos(p[0], p[1], p[2]))).expect("ball transform");
        w.insert(oid(b), dynamic()).expect("ball body");
        w.insert(oid(b), shape).expect("ball shape");
    }
    w
}

/// The ball pit's ball: 3 cm in radius, 18 g.
fn pit_ball() -> Shape {
    Shape { kind: shape_kind::SPHERE, density: 160.0, size: [0.03, 0.0, 0.0], ..Shape::default() }
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    (0..3).map(|i| (a[i] - b[i]).powi(2)).sum::<f64>().sqrt()
}

/// The hand whose grab joint holds `id`, if one does.
fn held_by(r: &ReplicaImpl<PeWorld>, id: u128) -> Option<ObjectId> {
    r.physics().joint_objects(pe_replica::hands::grab_key(oid(id))).and_then(|(hand, _)| hand)
}

/// The handoff: the left hand grabs the ball at frame 6, the right takes it over by a renewing grab at frame 10, and
/// only the right moves, lifting it 0.9 m from frame 12 to 43; the left opens at frame 46.
fn handoff() -> (World, Vec<SignedEntry>) {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let hr = f64::from(pe_replica::world::HAND_RADIUS);
    let (left, right) = ([2.0 - 0.1 - hr, 0.1, 0.0], [2.0 + 0.1 + hr, 0.1, 0.0]);
    let lift: Vec<[f64; 3]> = (0..32).map(|k| [right[0], 0.1 + 0.9 * f64::from(k + 1) / 32.0, 0.0]).collect();
    let top = [right[0], 1.0, 0.0];
    let entries = vec![
        input(&mut b, 2, &hand_of(HAND, &[left; 8], UNTURNED, true)),
        input(&mut b, 2, &hand_of(RIGHT, &[right; 8], UNTURNED, true)),
        input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }),
        input(&mut b, 10, &Input::Grab { object: oid(BALL), hand: Some(oid(RIGHT)) }),
        input(&mut b, 12, &hand_of(RIGHT, &lift[..16], UNTURNED, true)),
        input(&mut b, 28, &hand_of(RIGHT, &lift[16..], UNTURNED, true)),
        input(&mut b, 44, &hand_of(RIGHT, &[top; 16], UNTURNED, true)),
        input(&mut b, 46, &hand_of(HAND, &[left], UNTURNED, false)),
        input(&mut b, 60, &hand_of(RIGHT, &[top; 16], UNTURNED, true)),
    ];
    let world = with_hands_and_balls(&[(HAND, left), (RIGHT, right)], &[(BALL, [2.0, 0.1, 0.0], sphere(0.1))]);
    (world, entries)
}

/// A renewing grab that names another of its author's hands moves the claim to it (world-model.md 7.2), and the
/// grab joint moves with it (@design-reviewer's Medium on cc037c10): after the handoff only the right hand moves, and
/// the ball rises with it. Once the left hand opens, losing its body, the ball stays in the right.
#[test]
fn a_grab_handed_to_the_other_hand_follows_that_hand() {
    let (world, entries) = handoff();
    let mut s = Source::default();
    entries.into_iter().for_each(|e| s.add(e));
    let mut r = replica_of(world);
    r.advance(Frame(44), &mut s);
    assert_eq!(r.world().claims().iter().map(|c| c.hand).collect::<Vec<_>>(), [oid(RIGHT)]);
    assert_eq!(held_by(&r, BALL), Some(oid(RIGHT)), "the joint moved with the claim");
    assert!(at(&r, BALL)[1] > 0.9, "the ball rose with the right hand: {:?}", at(&r, BALL));
    r.advance(Frame(70), &mut s);
    assert!(!r.physics().has_body(oid(HAND)), "the open left hand has no body");
    assert_eq!(held_by(&r, BALL), Some(oid(RIGHT)));
    assert!(at(&r, BALL)[1] > 0.9, "the ball stays in the right hand: {:?}", at(&r, BALL));
    assert_eq!(r.faults(), 0);
}

/// A hand holding a pit ball, which rests against two others, jumps 1 m along x in one frame at frame 12, through one
/// of them.
fn jump_holding() -> (World, Vec<SignedEntry>) {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let start = [2.0 - 0.03 - f64::from(pe_replica::world::HAND_RADIUS), 0.03, 0.0];
    let to = [start[0] + 1.0, 0.03, 0.0];
    let entries = vec![
        input(&mut b, 2, &hand_at(&[start; 10], true)),
        input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }),
        input(&mut b, 12, &hand_at(&[to; 10], true)),
    ];
    let balls = [
        (BALL, [2.0, 0.03, 0.0], pit_ball()),
        (NEXT, [2.06, 0.03, 0.0], pit_ball()),
        (BESIDE, [2.0, 0.03, 0.06], pit_ball()),
    ];
    (with_hands_and_balls(&[(HAND, start)], &balls), entries)
}

/// What a hand holds goes with it on a teleport (@design-reviewer's Low on cc037c10; world-model.md 8.1's test "a
/// one-frame jump past the teleport distance places hand and held object"): after the 1 m jump the held ball is where
/// the hand's move took it, keeping its place in the hand, and neither of the balls it rested against moves. Pulled
/// across, it would hit the one in its path far faster than the cap; left behind (as on cc037c10), it would stay held
/// 1 m from the hand.
#[test]
fn a_hand_that_jumps_carries_what_it_holds_without_hitting_anything() {
    let (world, entries) = jump_holding();
    let mut s = Source::default();
    entries.into_iter().for_each(|e| s.add(e));
    let mut r = replica_of(world);
    r.advance(Frame(12), &mut s);
    assert_eq!(held_by(&r, BALL), Some(oid(HAND)));
    let top = fastest(&mut r, &mut s, &[NEXT, BESIDE], 40);
    assert!(top < 0.5, "neither of the others moves: {top} m/s");
    let ball = at(&r, BALL);
    assert!(distance(ball, [3.0, 0.03, 0.0]) < 0.01, "the ball jumped with the hand, keeping its place: {ball:?}");
    assert_eq!(r.faults(), 0);
}

/// A hand holding a pit ball `offset` out along x turns 90 degrees about y in one frame at frame 12, with a second
/// ball on the held one's arc. The top speed of either ball, where the held one ends, and its turned place.
fn turn_holding(offset: f64) -> (f64, [f64; 3], [f64; 3]) {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut s = Source::default();
    let hand = [2.0, 0.03, 0.0];
    let turned = [0.0, std::f32::consts::FRAC_1_SQRT_2, 0.0, std::f32::consts::FRAC_1_SQRT_2];
    s.add(input(&mut b, 2, &hand_at(&[hand; 10], true)));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    s.add(input(&mut b, 12, &hand_of(HAND, &[hand; 10], turned, true)));
    let mid = offset * std::f64::consts::FRAC_1_SQRT_2;
    let balls = [(BALL, [2.0 + offset, 0.03, 0.0], pit_ball()), (NEXT, [2.0 + mid, 0.03, -mid], pit_ball())];
    let mut r = replica_of(with_hands_and_balls(&[(HAND, hand)], &balls));
    r.advance(Frame(12), &mut s);
    assert_eq!(held_by(&r, BALL), Some(oid(HAND)));
    let top = fastest(&mut r, &mut s, &[BALL, NEXT], 40);
    assert_eq!(r.faults(), 0);
    (top, at(&r, BALL), [2.0, 0.03, -offset])
}

/// A fast turn brings what the hand holds to its turned place (world-model.md 8.1; @design-reviewer's Low on
/// cc037c10): with the ball 15 cm out, a 90-degree turn in one frame (21 cm, 13 m/s), and the spring brings it to within
/// 1 cm of its turned place. Until the 11-27 kit such a turn was also slowed to the cap; a holding hand has none now
/// (@design-reviewer). On cc037c10 the ball was left 4 cm out of place.
#[test]
fn a_fast_turn_brings_what_the_hand_holds_to_its_turned_place() {
    let (top, ball, place) = turn_holding(0.15);
    println!("RESULT a 90-degree turn in a frame, ball 15 cm out: fastest {top:.1} m/s");
    assert!(distance(ball, place) < 0.01, "the held ball is at its turned place: {ball:?}, {place:?}");
}

/// A held object springs back to its place in the hand ([`pe_replica::hands::GRAB`]): with the ball 10 cm out the
/// same turn is no jump (14 cm, 8.5 m/s), so the hand turns and the spring brings the ball round after it, to within
/// 1 cm of its turned place, both balls moving at less than the cap. On cc037c10 the joint's velocity drive
/// cancelled the spring, and the ball was left 3.8 cm out.
#[test]
fn a_held_object_springs_back_to_its_place_in_the_hand() {
    let (top, ball, place) = turn_holding(0.1);
    assert!(top < pe_replica::hands::MAX_HAND_PUSH_SPEED, "both balls move under the cap: {top} m/s");
    assert!(distance(ball, place) < 0.01, "the held ball is back at its place: {ball:?}, {place:?}");
}

/// Handoffs, teleports and slowed moves are functions of the claims and the physics image, so an entry that arrives
/// late rolls back to the same roots as a replica that had it all along: the renewing grab that hands the ball over,
/// the teleport that carries it, and the first pose of an empty hand's run faster than the cap.
#[test]
fn a_late_handoff_and_a_late_jump_roll_back_to_the_same_roots() {
    let run = |make: fn() -> (World, Vec<SignedEntry>), late: usize| {
        let (world, entries) = make();
        let mut all = Source::default();
        entries.iter().cloned().for_each(|e| all.add(e));
        let mut a = replica_of(world);
        a.advance(Frame(56), &mut all);
        let (world, _) = make();
        let mut some = Source::default();
        entries.iter().enumerate().filter(|&(k, _)| k != late).for_each(|(_, e)| some.add(e.clone()));
        let mut c = replica_of(world);
        c.advance(Frame(36), &mut some);
        some.add(entries[late].clone());
        let adv = c.advance(Frame(56), &mut some);
        assert!(adv.rolled_back_to.is_some_and(|f| f <= entries[late].entry.stamp), "rolled back: {adv:?}");
        assert_eq!(roots(&mut c, 56), roots(&mut a, 56));
        assert_eq!((a.faults(), c.faults()), (0, 0));
    };
    run(handoff, 3);
    run(jump_holding, 2);
    run(fast_run, 1);
}

/// A held object keeps the rotation it was grabbed at, relative to the hand ([`pe_replica::hands::grab_torque`]): a
/// box resting at 30 degrees about y is grabbed 15 cm from the hand's centre, lifted 30 cm, then turned with the
/// hand by 30 degrees, 3 a frame, and ends at 60. An angular spring would turn it to the hand's own rotation (30),
/// and with no angular drive it would stay at 30.
#[test]
fn a_held_object_keeps_the_rotation_it_was_grabbed_at() {
    const BOX: u128 = BALL + 3;
    let about_y = |degrees: f64| {
        let h = (degrees.to_radians() / 2.0) as f32;
        [0.0, h.sin(), 0.0, h.cos()]
    };
    let half = 0.04;
    let start = [2.0 - 0.15, half, 0.0];
    let top = [start[0], half + 0.3, 0.0];
    let lift: Vec<[f64; 3]> = (0..16).map(|k| [start[0], half + 0.3 * f64::from(k + 1) / 16.0, 0.0]).collect();
    let turn: Vec<HandPose> = (1..=10)
        .map(|k| HandPose {
            pos: pos(top[0], top[1], top[2]),
            rot: about_y(3.0 * f64::from(k)),
            grip: 1.0,
            engaged: true,
        })
        .collect();
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut s = Source::default();
    s.add(input(&mut b, 2, &hand_at(&[start; 8], true)));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(BOX), hand: Some(oid(HAND)) }));
    s.add(input(&mut b, 10, &hand_at(&lift, true)));
    s.add(input(&mut b, 26, &Input::Hand { hand: oid(HAND), poses: turn, hold: 16 }));
    let mut w = with_hands_and_balls(&[(HAND, start)], &[]);
    let k = keys.key(0);
    w.create(oid(BOX), None, k, GENESIS).expect("box");
    w.insert(oid(BOX), Transform { rot: about_y(30.0), ..Transform::at(pos(2.0, half, 0.0)) }).expect("box transform");
    w.insert(oid(BOX), dynamic()).expect("box body");
    w.insert(oid(BOX), cuboid([half as f32; 3])).expect("box shape");
    let mut r = replica_of(w);
    r.advance(Frame(52), &mut s);
    assert_eq!(held_by(&r, BOX), Some(oid(HAND)));
    let q = r.physics().pose(oid(BOX)).expect("the box's body").rotation;
    let degrees = 2.0 * f64::from(q[1]).atan2(f64::from(q[3])).to_degrees();
    assert!((degrees - 60.0).abs() < 2.0, "the box turned with the hand, keeping its 30 degrees: {degrees} ({q:?})");
    assert!(q[0].abs() < 0.02 && q[2].abs() < 0.02, "and did not tip: {q:?}");
    assert_eq!(r.faults(), 0);
}

/// A box body of half extents `half`, `density` kg/m^3 and `friction`, resting on the floor with its centre at `at`.
fn add_box(w: &mut World, id: u128, at: [f64; 3], half: [f32; 3], density: f32, friction: f32) {
    let k = keys().key(0);
    w.create(oid(id), None, k, GENESIS).expect("box");
    w.insert(oid(id), Transform::at(pos(at[0], at[1], at[2]))).expect("box transform");
    w.insert(oid(id), Body { friction, ..dynamic() }).expect("box body");
    w.insert(oid(id), Shape { density, ..cuboid(half) }).expect("box shape");
}

/// The hand closed at each point in turn, one a frame from `from`, in packs of at most `MAX_POSES`.
fn closed_path(b: &mut EntryBuilder, from: u64, points: &[[f64; 3]]) -> Vec<SignedEntry> {
    points
        .chunks(pe_world::input::MAX_POSES)
        .enumerate()
        .map(|(i, pack)| input(b, from + (i * pe_world::input::MAX_POSES) as u64, &hand_at(pack, true)))
        .collect()
}

/// How far `id`'s local up is tipped from the world's, degrees.
fn tilt_of(r: &ReplicaImpl<PeWorld>, id: u128) -> f64 {
    let q = r.physics().pose(oid(id)).expect("a body").rotation.map(f64::from);
    (1.0 - 2.0 * (q[0] * q[0] + q[2] * q[2])).clamp(-1.0, 1.0).acos().to_degrees()
}

/// Where `id`'s local point `p` is now.
fn point_of(r: &ReplicaImpl<PeWorld>, id: u128, p: [f64; 3]) -> [f64; 3] {
    let pose = r.physics().pose(oid(id)).expect("a body");
    let q = pose.rotation.map(f64::from);
    let (u, w) = ([q[0], q[1], q[2]], q[3]);
    let cross =
        |a: [f64; 3], b: [f64; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let t = cross(u, p).map(|c| 2.0 * c);
    let ut = cross(u, t);
    [0usize, 1, 2].map(|i| pose.position[i] + p[i] + w * t[i] + ut[i])
}

/// The claim frame (problem 3's jump in the 11-27 test; the coordinator's request of 2026-11-27): a hand that closes inside a
/// 300 kg crate (the heavy preset's) and grabs it in the same frame, as pe-app sends the two, doesn't move it: the
/// grab joint is made in the sync that makes the hand's body, and a grab's two bodies never collide. The grab holds the
/// crate by the hand's centre, which is inside it. The control, the same close with no grab, is printed: the closed
/// hand's kinematic body inside the crate shoves it (the unclaimed case the hand's contact cap is for).
#[test]
fn a_hand_closing_inside_a_heavy_crate_and_grabbing_it_does_not_move_it() {
    const CRATE: u128 = BALL + 4;
    let hand = [2.0, 0.7, 0.2];
    let run = |grab: bool| -> (f64, f64, Option<[f32; 3]>) {
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let mut s = Source::default();
        for e in closed_path(&mut b, 10, &[hand; 61]) {
            s.add(e);
        }
        if grab {
            s.add(input(&mut b, 10, &Input::Grab { object: oid(CRATE), hand: Some(oid(HAND)) }));
        }
        let mut w = with_hands_and_balls(&[(HAND, hand)], &[]);
        add_box(&mut w, CRATE, [2.0, 0.5, 0.0], [0.5; 3], 300.0, 0.9);
        let mut r = replica_of(w);
        r.advance(Frame(9), &mut s);
        let before = r.physics().pose(oid(CRATE)).expect("the crate").position;
        let mut moved: f64 = 0.0;
        for f in 10..=70 {
            r.advance(Frame(f), &mut s);
            moved = moved.max(distance(before, r.physics().pose(oid(CRATE)).expect("the crate").position));
        }
        let mass = f64::from(r.physics().mass(oid(CRATE)).expect("the crate"));
        assert_eq!(r.faults(), 0);
        (moved, mass, r.physics().grab_object_anchor(pe_replica::hands::grab_key(oid(CRATE))).ok())
    };
    let (moved, mass, anchor) = run(true);
    let (shoved, _, none) = run(false);
    println!(
        "RESULT claim frame: {mass:.1} kg crate moved {:.3} mm with the grab, {:.1} mm without (control); anchor {anchor:?}",
        moved * 1e3,
        shoved * 1e3
    );
    assert!((mass - 300.0).abs() < 1.0, "the crate is the heavy preset's: {mass} kg");
    assert!(moved < 1e-3, "the crate didn't move on the claim frame or after: {moved} m");
    let anchor = anchor.expect("held");
    assert!(distance(anchor.map(f64::from), [0.0, 0.2, 0.2]) < 1e-4, "held by the hand's centre: {anchor:?}");
    assert_eq!(none, None);
}

/// Grab at the point (the user's ruling of 2026-11-27): a hand 5 cm out from the sack's top edge
/// (@world-model's mid-weight, 18 kg, friction 0.3) holds it by the edge's nearest point. Lifting the hand 25 cm over
/// half a second pulls that edge up with the grip's 110 N, less than the sack's weight (177 N), so it can't come off
/// the floor, and the drive's 33 N m holds it level against the turn about its far bottom edge (tilting it needs
/// 88 N plus 33 N m over the 0.6 m lever, 143 N in all); dragging it 1 m along x at 1 m/s slides it (friction 53 N). Both printed: the tilt, the held point's gap
/// and the far bottom edge's height.
#[test]
fn a_grab_holds_a_mid_weight_sack_by_the_point_the_hand_took_hold_of() {
    const SACK: u128 = BALL + 5;
    let half = [0.3f32, 0.15, 0.2];
    let hand = [2.0 + 0.3 + 0.05, 0.3 + 0.05, 0.0];
    let run = |path: &dyn Fn(u32) -> [f64; 3], frames: u32| {
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let mut s = Source::default();
        s.add(input(&mut b, 2, &hand_at(&[hand; 8], true)));
        s.add(input(&mut b, 6, &Input::Grab { object: oid(SACK), hand: Some(oid(HAND)) }));
        let points: Vec<[f64; 3]> = (1..=frames + 60).map(|k| path(k.min(frames))).collect();
        for e in closed_path(&mut b, 12, &points) {
            s.add(e);
        }
        let mut w = with_hands_and_balls(&[(HAND, hand)], &[]);
        add_box(&mut w, SACK, [2.0, 0.15, 0.0], half, 250.0, 0.3);
        let mut r = replica_of(w);
        r.advance(Frame(11), &mut s);
        let anchor = r.physics().grab_object_anchor(pe_replica::hands::grab_key(oid(SACK))).expect("held");
        r.advance(Frame(12 + u64::from(frames) + 60), &mut s);
        assert_eq!(held_by(&r, SACK), Some(oid(HAND)));
        assert_eq!(r.faults(), 0);
        let gap = pe_replica::hands::hold_gap(r.physics(), oid(SACK), oid(HAND)).expect("held");
        let far = point_of(&r, SACK, [-0.3, -0.15, 0.0]);
        let mass = r.physics().mass(oid(SACK)).expect("the sack");
        let at = r.physics().pose(oid(SACK)).expect("the sack").position;
        (anchor, tilt_of(&r, SACK), gap, far, mass, at)
    };
    let lift = |k: u32| [hand[0], hand[1] + 0.25 * f64::from(k.min(30)) / 30.0, 0.0];
    let (anchor, tilt, gap, far, mass, _) = run(&lift, 30);
    println!(
        "RESULT sack lift: {mass:.1} kg, anchor {anchor:?}, tilt {tilt:.1} deg, held point {:.1} cm from the hand, far bottom edge at y {:.1} cm",
        gap * 100.0,
        far[1] * 100.0
    );
    assert!((f64::from(mass) - 18.0).abs() < 0.1, "the sack: {mass} kg");
    // The sack settles a fraction of a millimetre before the grab.
    assert!(distance(anchor.map(f64::from), [0.3, 0.15, 0.0]) < 1e-3, "held by its top edge: {anchor:?}");
    let drag = |k: u32| [hand[0] + f64::from(k.min(60)) / 60.0, hand[1], 0.0];
    let (_, tilt, gap, _, _, at) = run(&drag, 60);
    println!(
        "RESULT sack drag: moved {:.1} cm along x, tilt {tilt:.1} deg, held point {:.1} cm from the hand",
        (at[0] - 2.0) * 100.0,
        gap * 100.0
    );
    assert!(at[0] - 2.0 > 0.8, "the grip drags the sack: it moved {} m", at[0] - 2.0);
}

/// The grip's torque (the coordinator's ruling of 2026-11-27): a 2 kg, 1 m plank held 5 cm past its end is lifted 30 cm.
/// Its weight turns it 9.8 N m about the held end, under the grip's 33 N m, so it stays level.
#[test]
fn a_plank_held_by_its_end_stays_level() {
    const PLANK: u128 = BALL + 6;
    let half = [0.5f32, 0.02, 0.05];
    let hand = [2.0 - 0.5 - 0.05, 0.02, 0.0];
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut s = Source::default();
    s.add(input(&mut b, 2, &hand_at(&[hand; 8], true)));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(PLANK), hand: Some(oid(HAND)) }));
    let points: Vec<[f64; 3]> = (1..=90).map(|k| [hand[0], hand[1] + 0.3 * f64::from(k.min(30)) / 30.0, 0.0]).collect();
    for e in closed_path(&mut b, 12, &points) {
        s.add(e);
    }
    let mut w = with_hands_and_balls(&[(HAND, hand)], &[]);
    add_box(&mut w, PLANK, [2.0, 0.02, 0.0], half, 500.0, 0.6);
    let mut r = replica_of(w);
    r.advance(Frame(12 + 30 + 60), &mut s);
    assert_eq!(held_by(&r, PLANK), Some(oid(HAND)));
    let mass = r.physics().mass(oid(PLANK)).expect("the plank");
    let tilt = tilt_of(&r, PLANK);
    let far = point_of(&r, PLANK, [0.5, 0.0, 0.0]);
    println!("RESULT plank: {mass:.2} kg, tilt {tilt:.2} deg, far end at y {:.1} cm", far[1] * 100.0);
    assert!((f64::from(mass) - 2.0).abs() < 0.01, "the plank: {mass} kg");
    assert!(tilt < 2.0, "it stays level: {tilt} deg");
    assert!(far[1] > 0.25, "and came up whole: its far end is at {} m", far[1]);
    assert_eq!(r.faults(), 0);
}

/// A closed, empty hand runs along x at 15 m/s (1.5 times the cap) for one second from frame 10, through pit balls
/// every 0.5 m (ids from 100), past the ball at x = -7.
fn fast_run() -> (World, Vec<SignedEntry>) {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let start = [-7.0 - 0.03 - f64::from(pe_replica::world::HAND_RADIUS), 0.03, 0.0];
    let path: Vec<[f64; 3]> = (0..60).map(|k| [start[0] + 0.25 * f64::from(k + 1), 0.03, 0.0]).collect();
    let mut entries = vec![input(&mut b, 2, &hand_at(&[start; 8], true))];
    for (i, pack) in path.chunks(pe_world::input::MAX_POSES).enumerate() {
        entries.push(input(&mut b, 10 + (i * pe_world::input::MAX_POSES) as u64, &hand_at(pack, true)));
    }
    let mut balls = vec![(BALL, [-7.0, 0.03, 0.0], pit_ball())];
    balls.extend((0..28u32).map(|i| (100 + u128::from(i), [-6.0 + 0.5 * f64::from(i), 0.03, 0.0], pit_ball())));
    (with_hands_and_balls(&[(HAND, start)], &balls), entries)
}

/// An empty hand kept faster than the cap (world-model.md 8.1's test; restored for empty hands after the 11-27 kit,
/// @design-reviewer): through the second at 15 m/s its body trails its target, and is placed at it
/// again whenever it would trail by more than the teleport distance, so it never trails by more (its contacts come in
/// bursts). The balls it runs through move no faster than the cap plus the solver's push-out speed. A hand that holds
/// something has no cap (`a_fast_swing_s_throw_leaves_at_the_hand_s_speed`).
#[test]
fn an_empty_hand_kept_faster_than_the_cap_trails_by_at_most_the_teleport_distance() {
    let (world, entries) = fast_run();
    let mut s = Source::default();
    entries.into_iter().for_each(|e| s.add(e));
    let mut r = replica_of(world);
    r.advance(Frame(10), &mut s);
    let run_through: Vec<u128> = (100..128).collect();
    let mut last: Vec<[f64; 3]> = run_through.iter().map(|&id| at(&r, id)).collect();
    let (mut lag, mut top) = (0.0_f64, 0.0_f64);
    for f in 10..72u64 {
        let target = at(&r, HAND);
        r.advance(Frame(f + 1), &mut s);
        lag = lag.max(distance(r.physics().pose(oid(HAND)).expect("the hand's body").position, target));
        for (k, &id) in run_through.iter().enumerate() {
            let p = at(&r, id);
            top = top.max(distance(p, last[k]) * 60.0);
            last[k] = p;
        }
    }
    let teleport = pe_replica::hands::teleport_distance(pe_seams::SimRate::DEFAULT);
    assert!(lag > 0.2 && lag <= teleport, "it trails, by at most {teleport} m: {lag} m");
    assert!(top > 5.0 && top < pe_replica::hands::MAX_HAND_PUSH_SPEED + contact_speed(), "what it hits: {top} m/s");
    assert_eq!(held_by(&r, BALL), None, "the hand holds nothing");
    assert_eq!(r.faults(), 0);
}

/// A two-hand hold of a long object (@design-reviewer's L3 on adac6a4d): pe-world's pick measures to the surface of the
/// object's bounding sphere about its centre, so a hand at either end of a 1 m plank picks it, and pe-app's hand-over
/// (`submit_hands`) finds it for the hand still holding.
#[test]
fn a_hand_at_either_end_of_a_long_plank_picks_it() {
    const PLANK: u128 = 75;
    let mut world = with_hands_and_balls(&[(HAND, [2.0, 0.5, 0.0])], &[]);
    let k = keys().key(0);
    world.create(oid(PLANK), None, k, GENESIS).expect("plank");
    world.insert(oid(PLANK), Transform::at(pos(2.5, 0.5, 0.0))).expect("its transform");
    world.insert(oid(PLANK), dynamic()).expect("its body");
    world.insert(oid(PLANK), cuboid([0.5, 0.05, 0.05])).expect("its shape");
    for end in [2.04, 2.96] {
        let picked = world.pick_grab(Frame(1), k, oid(HAND), pos(end, 0.5, 0.0));
        assert_eq!(picked, Some(oid(PLANK)), "a hand at x {end} picks the plank");
    }
}

/// A throw leaves at the hand's speed, with no cap (the user's ruling of 2026-11-26): the ball is
/// carried along with the hand at `v` and thrown at it.
#[test]
fn a_throw_leaves_at_the_hand_s_speed_with_no_cap() {
    for v in [4.0, 8.0, 11.0, 14.0, 25.0, 40.0] {
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let mut s = Source::default();
        let start = [2.0 - 0.03 - f64::from(pe_replica::world::HAND_RADIUS), 0.03, 0.0];
        let mut y = start[1];
        let path: Vec<[f64; 3]> = [1.0 / 3.0, 2.0 / 3.0, 1.0, 1.0, 1.0]
            .iter()
            .map(|f| {
                y += v * f / 60.0;
                [start[0], y, 0.0]
            })
            .collect();
        s.add(input(&mut b, 2, &hand_at(&[start; 8], true)));
        s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
        s.add(input(&mut b, 10, &hand_at(&path[..4], true)));
        s.add(input(&mut b, 14, &hand_at(&path[4..], false)));
        let linear = [0.0, v as f32, 0.0];
        s.add(input(&mut b, 14, &Input::Throw { object: oid(BALL), linear, angular: [0.0; 3] }));
        let mut r = replica_of(with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.0, 0.03, 0.0], pit_ball())]));
        r.advance(Frame(15), &mut s);
        assert_eq!(r.physics().joint_count(), 0, "released");
        let left = f64::from(r.physics().velocity(oid(BALL)).expect("the ball's body").0[1]);
        println!("RESULT a {v} m/s throw leaves at {left:.2} m/s");
        // Upward, less the frame of gravity after the throw applies.
        let expected = v - 9.81 / 60.0;
        assert!((left - expected).abs() < 0.01 * v, "a {v} m/s throw leaves at {left} m/s");
        assert_eq!(r.faults(), 0);
    }
}

/// The weak-throw fix's scene (world-model.md 8.1; @item:primordial-engine/throws-leave-at-the-hand-s-speed-letting-go-
/// carr): a ball of `shape` and radius `r` on the floor at x = 2, the hand touching it, closed from frame 2. It is
/// grabbed at frame 6 (unless `grab` is false), lifted 0.48 m from frame 10 and held still, and at frame 40 the hand
/// opens with `throw` (a `Throw` or a `Release` of `object`). Its world and entries, the throw last.
fn held_then(shape: Shape, r: f64, grab: bool, throw: Input) -> (World, Vec<SignedEntry>) {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let rest = [2.0 - r - f64::from(pe_replica::world::HAND_RADIUS), r, 0.0];
    let lift: Vec<[f64; 3]> = (0..16).map(|k| [rest[0], r + 0.03 * f64::from(k + 1), 0.0]).collect();
    let top = lift[15];
    let mut entries = vec![input(&mut b, 2, &hand_at(&[rest; 8], true))];
    if grab {
        entries.push(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    }
    entries.push(input(&mut b, 10, &hand_at(&lift, true)));
    entries.push(input(&mut b, 26, &hand_at(&[top; 14], true)));
    entries.push(input(&mut b, 40, &hand_at(&[top; 4], false)));
    entries.push(input(&mut b, 40, &throw));
    let world = with_hands_and_balls(&[(HAND, rest)], &[(BALL, [2.0, r, 0.0], shape)]);
    (world, entries)
}

/// The ball's velocity after `entries`, run to frame `to`: linear then angular, as plain numbers.
fn velocity_after(world: World, entries: Vec<SignedEntry>, to: u64) -> ([f64; 3], [f64; 3], ReplicaImpl<PeWorld>) {
    let mut s = Source::default();
    entries.into_iter().for_each(|e| s.add(e));
    let mut r = replica_of(world);
    r.advance(Frame(to), &mut s);
    let (v, w) = r.physics().velocity(oid(BALL)).expect("the ball's body");
    (v.map(f64::from), w.map(f64::from), r)
}

/// A holder's throw (world-model.md 8.1): it ends the hold, and the ball leaves with the velocity it carries, set as
/// the entry applies (one frame's gravity after), not with whatever the hold's spring had given it. A `Release`
/// instead leaves the ball at rest in the air. A throw of a ball nobody holds does nothing.
#[test]
fn a_holder_s_throw_gives_the_ball_its_velocity_as_the_entry_applies() {
    let g = 9.81 / 60.0;
    let throw = Input::Throw { object: oid(BALL), linear: [3.0, 1.0, 0.0], angular: [0.0, 20.0, 0.0] };
    let (world, entries) = held_then(pit_ball(), 0.03, true, throw.clone());
    let (v, w, r) = velocity_after(world, entries, 41);
    assert_eq!(r.physics().joint_count(), 0, "let go");
    assert!((v[0] - 3.0).abs() < 0.03 && (v[1] - (1.0 - g)).abs() < 0.03 && v[2].abs() < 0.01, "left at {v:?}");
    assert!((w[1] - 20.0).abs() < 0.2 && w[0].abs() < 0.01 && w[2].abs() < 0.01, "spinning at {w:?}");
    let (world, entries) = held_then(pit_ball(), 0.03, true, Input::Release { object: oid(BALL) });
    let (v, _, _) = velocity_after(world, entries, 41);
    assert!(v[0].abs() < 0.05 && v[1] < 0.0 && v[1] > -0.3, "a release only lets it fall: {v:?}");
    // Not held (no grab): the throw changes nothing, and the ball rests on the floor.
    let (world, entries) = held_then(pit_ball(), 0.03, false, throw);
    let (v, w, r) = velocity_after(world, entries, 41);
    assert!(v.iter().chain(&w).all(|c| c.abs() < 0.05), "a non-holder's throw does nothing: {v:?} {w:?}");
    assert_eq!(r.faults(), 0);
}

/// What limits a throw is the grip (the user's ruling of 2026-11-26): the change it makes to the held
/// object's velocity is at most what the holder's strength gives it in a frame, `F dt / m`. A 36 cm ball of 195 g held
/// still and thrown at 15 m/s leaves at 110 N / 0.195 kg / 60 Hz, 9.4 m/s, in the throw's direction; an 8 kg ball at
/// 0.23 m/s. The spin is the throw's, under Box3D's own limit of an eighth of a turn a frame (47.1 rad/s at 60 Hz).
#[test]
fn a_throw_is_limited_by_the_grip_and_its_spin_by_box3d_alone() {
    let g = 9.81 / 60.0;
    let strength = f64::from(pe_world::components::DEFAULT_STRENGTH);
    for (density, label) in [(1.0_f32, "195 g"), (41.0, "8 kg")] {
        let big = Shape { kind: shape_kind::SPHERE, density, size: [0.36, 0.0, 0.0], ..Shape::default() };
        let throw = Input::Throw { object: oid(BALL), linear: [12.0, 0.0, 9.0], angular: [0.0, 30.0, 0.0] };
        let (world, entries) = held_then(big, 0.36, true, throw);
        let (v, w, r) = velocity_after(world, entries, 41);
        let mass = f64::from(r.physics().mass(oid(BALL)).expect("the ball's mass"));
        let most = (strength / mass / 60.0).min(15.0);
        let along = (v[0] * 0.8 + v[2] * 0.6, v[0] * 0.6 - v[2] * 0.8);
        println!(
            "RESULT a {label} ball held still, thrown at 15 m/s: leaves at {:.2} m/s (the grip's {most:.2})",
            along.0
        );
        assert!((along.0 - most).abs() < 0.03 * most + 0.02 && along.1.abs() < 0.02, "{label}: {v:?}");
        // The light ball sits still in the hand, so only gravity's frame moves it down; the heavy one still bobs on
        // its spring (110 N against its 78 N weight), and keeps that vertical speed.
        if density < 2.0 {
            assert!(v[1] < 0.0 && v[1] > -g - 0.05, "{label}: only gravity's frame down: {v:?}");
        }
        assert!((w[1] - 30.0).abs() < 0.3, "{label}: the spin is the throw's: {w:?}");
        assert_eq!(r.faults(), 0);
    }
}

/// The far end of a dumbbell: a compound child of `BALL`.
const FAR_END: u128 = 74;

/// A throw's spin has no cap of its own (the user's ruling of 2026-11-26): a dumbbell, a pit ball with a second at
/// 0.5 m, thrown at 100 rad/s turns at Box3D's own limit of an eighth of a turn a frame, 47.1 rad/s at 60 Hz, where it
/// was 10 m/s at its 28 cm rim (36 rad/s) before.
#[test]
fn a_throw_s_spin_is_limited_only_by_box3d() {
    let throw = Input::Throw { object: oid(BALL), linear: [0.0; 3], angular: [0.0, 100.0, 0.0] };
    let (mut world, entries) = held_then(pit_ball(), 0.03, true, throw);
    let k = keys().key(0);
    world.create(oid(FAR_END), Some(oid(BALL)), k, GENESIS).expect("the far end");
    world.insert(oid(FAR_END), Transform::local([0.0, 0.0, 0.5])).expect("its place");
    world.insert(oid(FAR_END), pit_ball()).expect("its shape");
    let (_, w, r) = velocity_after(world, entries, 41);
    let limit = 0.25 * std::f64::consts::PI * 60.0;
    assert!((w[1] - limit).abs() < 0.1 && w[0].abs() < 0.5 && w[2].abs() < 0.5, "Box3D's 47.1 rad/s: {w:?}");
    assert_eq!(r.faults(), 0);
}

/// A throw applies whenever the hand's joint holds the object (the user's ruling of 2026-11-26: "keep
/// it straightforward and predictable"): a ball held away from its place by a wall, its hand carried 1 m past it, is
/// thrown back at 5 m/s. Until then it kept its solver velocity.
#[test]
fn a_throw_of_a_ball_held_away_by_a_wall_still_applies() {
    // `pinned`'s scene (the hand carried 1 m past the wall the ball is held against), with the throw on its chain.
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let (start, past) = ([2.08, 0.03, 0.0], [3.08, 0.03, 0.0]);
    let mut entries = path_inputs(&mut b, HAND, 60, 2, 150, &move |t: f64| lerp(start, past, (t - 0.17) / 0.5));
    entries.push(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    entries.push(input(&mut b, 45, &Input::Throw { object: oid(BALL), linear: [-5.0, 0.0, 0.0], angular: [0.0; 3] }));
    let mut world = with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.2, 0.03, 0.0], pit_ball())]);
    add_wall(&mut world);
    let mut s = Source::default();
    entries.into_iter().for_each(|e| s.add(e));
    let mut r = replica_of(world);
    r.advance(Frame(45), &mut s);
    assert!(held_by(&r, BALL) == Some(oid(HAND)), "held");
    r.advance(Frame(46), &mut s);
    assert_eq!(held_by(&r, BALL), None, "the throw ended the hold");
    let v = r.physics().velocity(oid(BALL)).expect("the ball").0;
    assert!((v[0] + 5.0).abs() < 0.2, "thrown back at 5 m/s: {v:?}");
    assert_eq!(r.faults(), 0);
}

/// A throw that arrives late rolls back to the same roots as a run that had it on time (world-model.md 8.1: the velocity
/// is set inside the step, from the entry, so a replay sets it again exactly).
#[test]
fn a_throw_that_arrives_late_rolls_back_to_the_same_roots() {
    let throw = Input::Throw { object: oid(BALL), linear: [3.0, 2.0, -1.0], angular: [1.0, 2.0, 3.0] };
    let (world, entries) = held_then(pit_ball(), 0.03, true, throw);
    let mut all = Source::default();
    entries.iter().cloned().for_each(|e| all.add(e));
    let mut a = replica_of(world.clone());
    a.advance(Frame(70), &mut all);
    let (head, last) = entries.split_at(entries.len() - 1);
    let mut late = Source::default();
    head.iter().cloned().for_each(|e| late.add(e));
    let mut c = replica_of(world);
    c.advance(Frame(50), &mut late);
    late.add(last[0].clone());
    let adv = c.advance(Frame(70), &mut late);
    assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= 40), "the late throw rolls back: {adv:?}");
    assert_eq!(roots(&mut c, 70), roots(&mut a, 70));
    assert!(at(&a, BALL)[0] > 2.5, "the throw moved the ball: {:?}", at(&a, BALL));
    assert_eq!((a.faults(), c.faults()), (0, 0));
}

/// Step 0 of the release fix (@item:primordial-engine/letting-go-in-vr-the-ball-flies-at-once-with-no-g;
/// @coordinator-opus): a pit ball (18 g) is grabbed on the floor, lifted to 1 m and held still, then
/// swung along x. The hand speeds up evenly over 0.15 s to `v` (`v / 0.15` m/s²), keeps that speed for 0.1 s, and
/// opens at speed. Prints, per speed: the ball's sag below its place in the hand; its trail behind that place along
/// the swing, the most while the hand speeds up and at the end of the steady part; and its launch speed against
/// the hand body's. A measurement: it asserts nothing.
#[test]
#[ignore = "a measurement (step 0 of the release fix): prints, asserts nothing"]
fn measure_the_hold_s_trail_and_launch_in_a_swing() {
    use pe_world::input::MAX_POSES;
    for v in [3.5, 8.0, 12.0] {
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let mut s = Source::default();
        let hr = f64::from(pe_replica::world::HAND_RADIUS);
        let start = [2.0 - 0.03 - hr, 0.03, 0.0];
        let mut path: Vec<[f64; 3]> = (1..=30).map(|k| [start[0], 0.03 + 0.97 * f64::from(k) / 30.0, 0.0]).collect();
        path.extend(std::iter::repeat_n([start[0], 1.0, 0.0], 30));
        let swing = path.len();
        let (speeding, steady) = (9_usize, 6_usize);
        let mut x = start[0];
        for k in 1..=speeding + steady {
            x += v * (k.min(speeding) as f64 / speeding as f64) / 60.0;
            path.push([x, 1.0, 0.0]);
        }
        let open = path.len();
        for _ in 0..12 {
            x += v / 60.0;
            path.push([x, 1.0, 0.0]);
        }
        s.add(input(&mut b, 2, &hand_at(&[start; 8], true)));
        s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
        for (i, pack) in path[..open].chunks(MAX_POSES).enumerate() {
            s.add(input(&mut b, 10 + (i * MAX_POSES) as u64, &hand_at(pack, true)));
        }
        let release = 10 + open as u64;
        s.add(input(&mut b, release, &hand_at(&path[open..], false)));
        s.add(input(&mut b, release, &Input::Release { object: oid(BALL) }));
        let mut r = replica_of(with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.0, 0.03, 0.0], pit_ball())]));
        let body = |r: &ReplicaImpl<PeWorld>| r.physics().pose(oid(HAND)).map(|p| p.position).unwrap_or(at(r, HAND));
        let offset = |r: &ReplicaImpl<PeWorld>| {
            let (h, o) = (body(r), at(r, BALL));
            [o[0] - h[0], o[1] - h[1], o[2] - h[2]]
        };
        r.advance(Frame(10), &mut s);
        assert_eq!(held_by(&r, BALL), Some(oid(HAND)), "held from the grab");
        let grabbed = offset(&r);
        r.advance(Frame(10 + swing as u64), &mut s);
        let still = offset(&r);
        let mut rows = Vec::new();
        let (mut most, mut most_at) = (0.0_f64, 0_u64);
        for f in 10 + swing as u64..release + 4 {
            r.advance(Frame(f + 1), &mut s);
            let o = offset(&r);
            let trail = still[0] - o[0];
            let hv = f64::from(r.physics().velocity(oid(HAND)).map(|(l, _)| l[0]).unwrap_or(f32::NAN));
            let bv = f64::from(r.physics().velocity(oid(BALL)).expect("the ball's body").0[0]);
            let k = f - (10 + swing as u64);
            if (k as usize) < speeding && trail > most {
                (most, most_at) = (trail, k);
            }
            rows.push(format!(
                "  frame +{k:2}: target x {:.3}, body x {:.3} ({hv:6.2} m/s), ball trail {:+.1} mm, ball {bv:6.2} m/s{}",
                at(&r, HAND)[0],
                body(&r)[0],
                trail * 1000.0,
                if f + 1 == release { "  <- the frame before the open" } else { "" }
            ));
        }
        let steady_trail = {
            let line = &rows[speeding + steady - 1];
            line.clone()
        };
        println!(
            "v {v} m/s (a {:.0} m/s^2): sag {:.1} mm (offset at the grab {:?}, held still {:?}); most trail while \
             speeding up {:.1} mm at +{most_at}; end of steady: {}",
            v / 0.15,
            (grabbed[1] - still[1]) * 1000.0,
            grabbed.map(|c| (c * 1000.0).round() / 1000.0),
            still.map(|c| (c * 1000.0).round() / 1000.0),
            most * 1000.0,
            steady_trail.trim()
        );
        for row in rows {
            println!("{row}");
        }
        assert_eq!(r.faults(), 0);
    }
}

/// The teleport distance (world-model.md 8.1): half a metre, or two frames at the cap where that is longer, so a band
/// of moves faster than the cap but short of a teleport exists at every rate (30 Hz: 0.67 m).
#[test]
fn the_teleport_distance_is_half_a_metre_or_two_frames_at_the_cap() {
    let at = |hz: u32| pe_replica::hands::teleport_distance(pe_seams::SimRate::new(hz).expect("a rate"));
    assert_eq!(at(60), 0.5);
    assert_eq!(at(120), 0.5);
    assert!((at(30) - 2.0 * pe_replica::hands::MAX_HAND_PUSH_SPEED / 30.0).abs() < 1e-12, "{}", at(30));
}

// ---- the hold bound and the wall check (world-model.md 8.1; @coordinator-opus) -------

const WALL: u128 = 70;

/// A replica of `world` at `hz` (the world's rate decides the replica's).
fn replica_at(mut world: World, hz: u32) -> ReplicaImpl<PeWorld> {
    let rate = pe_seams::SimRate::new(hz).expect("a rate");
    world.set_rate(rate);
    let mut config = ReplicaConfig::new(rate, Duration::from_millis(133), Arc::new(MockClock { rate, ..CLOCK }));
    config.cut_every = K;
    ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).expect("a replica")
}

/// Closed-hand inputs for `hand` over frames `from..to` at `hz`, the pose at frame `f` at `p(f / hz)`, in packs of an
/// eighth of a second (so its claims renew at every rate).
fn path_inputs(
    b: &mut EntryBuilder,
    hand: u128,
    hz: u32,
    from: u64,
    to: u64,
    p: &dyn Fn(f64) -> [f64; 3],
) -> Vec<SignedEntry> {
    let mut out = Vec::new();
    let mut f = from;
    while f < to {
        let n = (to - f).min(u64::from(hz / 8).max(1));
        let points: Vec<[f64; 3]> = (0..n).map(|j| p((f + j) as f64 / f64::from(hz))).collect();
        out.push(input(b, f, &hand_of(hand, &points, UNTURNED, true)));
        f += n;
    }
    out
}

fn lerp(a: [f64; 3], c: [f64; 3], u: f64) -> [f64; 3] {
    [0usize, 1, 2].map(|i| a[i] + (c[i] - a[i]) * u.clamp(0.0, 1.0))
}

/// A static wall 10 cm thick (x 2.25 to 2.35), 1 m high and 2 m wide (z -1 to 1).
fn add_wall(w: &mut World) {
    let k = keys().key(0);
    w.create(oid(WALL), None, k, GENESIS).expect("wall");
    w.insert(oid(WALL), Transform::at(pos(2.30, 0.5, 0.0))).expect("wall transform");
    w.insert(oid(WALL), Body { friction: 0.6, ..Body::default() }).expect("wall body");
    w.insert(oid(WALL), cuboid([0.05, 0.5, 1.0])).expect("wall shape");
}

/// Where `object`'s centre belongs in `hand` now: the hand's body and its grab's anchor, less the object's anchor (the
/// point the hand took hold of) in the object's rotation now.
fn place_in(r: &ReplicaImpl<PeWorld>, hand: u128, object: u128) -> [f64; 3] {
    let key = pe_replica::hands::grab_key(oid(object));
    let a = r.physics().grab_anchor(key).expect("held");
    let b = r.physics().grab_object_anchor(key).expect("held");
    let h = r.physics().pose(oid(hand)).expect("the hand's body");
    let o = r.physics().pose(oid(object)).expect("the object's body");
    let rotate = |q: [f32; 4], v: [f32; 3]| {
        let cross = |a: [f32; 3], b: [f32; 3]| {
            [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
        };
        let t = cross([q[0], q[1], q[2]], v).map(|c| 2.0 * c);
        let ut = cross([q[0], q[1], q[2]], t);
        [v[0] + q[3] * t[0] + ut[0], v[1] + q[3] * t[1] + ut[1], v[2] + q[3] * t[2] + ut[2]]
    };
    let (ra, rb) = (rotate(h.rotation, a), rotate(o.rotation, b));
    [0usize, 1, 2].map(|i| h.position[i] + f64::from(ra[i]) - f64::from(rb[i]))
}

fn ball_speed(r: &ReplicaImpl<PeWorld>, id: u128) -> f64 {
    let (v, _) = r.physics().velocity(oid(id)).expect("a body");
    f64::from(v.iter().map(|c| c * c).sum::<f32>().sqrt())
}

/// The pinned ball at 60 Hz: the hand holding a pit ball against the wall is carried `by` metres past it from 0.17 s
/// to 0.67 s, then at 1 s the wall is deleted (`lift` false) or the hand lifts to 2.5 m over 0.5 s (`lift` true).
/// Its entries, its world, and the frame the way clears.
fn pinned(by: f64, lift: bool) -> (Vec<SignedEntry>, World, u64) {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let start = [2.08, 0.03, 0.0];
    let past = [2.08 + by, 0.03, 0.0];
    let high = [2.08 + by, 2.5, 0.0];
    let p = move |t: f64| {
        if t < 1.0 {
            lerp(start, past, (t - 0.17) / 0.5)
        } else if lift {
            lerp(past, high, (t - 1.0) / 0.5)
        } else {
            past
        }
    };
    let mut entries = path_inputs(&mut b, HAND, 60, 2, 150, &p);
    entries.push(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    if !lift {
        let delete = tx(vec![Op::Delete { id: Target::Existing(oid(WALL)) }]);
        entries.push(b.next(Class::Edit, Frame(60), Bytes::from(delete.encode())));
    }
    let mut world = with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.2, 0.03, 0.0], pit_ball())]);
    add_wall(&mut world);
    (entries, world, 60)
}

/// The corner (@design-reviewer): a ball held against the wall's face 3 cm from its end, the hand
/// overrunning diagonally past the end at 25 m/s, so a trailing placement comes while the ball is still at the
/// corner. The ball's centre would pass 2 cm beside the end, but its volume overlaps it, so the shape cast keeps it;
/// it slides round the corner instead, and crosses the face's plane clear of the end (measured: at z 1.085; a ray from
/// its centre let the placement carry it across at z 1.020).
#[test]
fn a_placement_at_a_wall_s_end_does_not_carry_the_ball_through_the_corner() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let start = [2.08, 0.03, 0.97];
    let d = 3.0 / std::f64::consts::SQRT_2;
    let past = [2.08 + d, 0.03, 0.97 + d];
    let mut s = Source::default();
    path_inputs(&mut b, HAND, 60, 2, 72, &move |t| lerp(start, past, (t - 0.2) / 0.12))
        .into_iter()
        .for_each(|e| s.add(e));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    let mut world = with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.2, 0.03, 0.97], pit_ball())]);
    add_wall(&mut world);
    let mut r = replica_of(world);
    let mut last = at(&r, BALL);
    let mut crossed = None;
    while r.frame().0 < 72 {
        let next = r.frame().0 + 1;
        r.advance(Frame(next), &mut s);
        let a = at(&r, BALL);
        if last[0] < 2.25 && a[0] >= 2.25 {
            let u = (2.25 - last[0]) / (a[0] - last[0]);
            crossed = Some(last[2] + u * (a[2] - last[2]));
        }
        last = a;
    }
    let z = crossed.expect("it gets past the wall");
    assert!(z > 1.0 + 0.03, "round the corner, clear of the wall's end: crossed at z {z}");
    assert_eq!(r.faults(), 0);
}

/// D4's break-away (render.md 8, S5 v1.9): in the local member's view a held ball that the wall keeps more than
/// `BREAK_AWAY` from its place in the hand is `HELD_AWAY` as well as `HELD_LOCAL`, so the display draws it where it is.
/// Held in its place it is only `HELD_LOCAL`, and once the wall is deleted and it comes back, it is no longer away.
#[test]
fn a_held_ball_kept_from_its_place_by_a_wall_is_marked_away_in_the_view() {
    let (entries, world, clear) = pinned(1.0, false);
    let mut s = Source::default();
    entries.into_iter().for_each(|e| s.add(e));
    let mut config = ReplicaConfig::new(pe_seams::SimRate::DEFAULT, Duration::from_millis(133), Arc::new(CLOCK));
    config.cut_every = K;
    config.local = Some(keys().key(0));
    let mut r = ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config).expect("a replica");
    let flags = |r: &ReplicaImpl<PeWorld>| r.view().objects.iter().find(|o| o.id == oid(BALL)).expect("drawn").flags;
    let gap = |r: &ReplicaImpl<PeWorld>| pe_replica::hands::hold_gap(r.physics(), oid(BALL), oid(HAND)).expect("held");
    let (local, away) = (pe_seams::ViewFlags::HELD_LOCAL, pe_seams::ViewFlags::HELD_AWAY);
    r.advance(Frame(9), &mut s);
    assert!(gap(&r) < pe_seams::BREAK_AWAY, "in its place: {}", gap(&r));
    assert!(flags(&r).contains(local) && !flags(&r).contains(away), "held in its place: {:?}", flags(&r));
    r.advance(Frame(clear), &mut s);
    assert!(gap(&r) > pe_seams::BREAK_AWAY, "kept from its place: {}", gap(&r));
    assert!(flags(&r).contains(local) && flags(&r).contains(away), "kept away by the wall: {:?}", flags(&r));
    // Against the wall it is blocked, so the display draws it where it is.
    assert!(flags(&r).contains(pe_seams::ViewFlags::HELD_BLOCKED), "blocked by the wall: {:?}", flags(&r));
    r.advance(Frame(120), &mut s);
    assert!(flags(&r).contains(local) && !flags(&r).contains(away), "back in its place: {:?}", flags(&r));
    assert!(!flags(&r).contains(pe_seams::ViewFlags::HELD_BLOCKED), "in its place, not blocked: {:?}", flags(&r));
    assert_eq!(r.faults(), 0);
}

/// The break-away's hysteresis (@design-reviewer's L1 on ca1d3ac4): a held object goes away above `BREAK_AWAY` and
/// comes back only below `BREAK_BACK`, so a gap between the two keeps whichever it was and never flickers; with no grab
/// joint from its hand there is no gap, and no break-away.
#[test]
fn a_held_object_goes_away_above_five_cm_and_comes_back_below_three() {
    use pe_replica::hands::held_away;
    let (away, back) = (pe_seams::BREAK_AWAY, pe_seams::BREAK_BACK);
    assert!(back < away);
    let between = (away + back) / 2.0;
    assert!(!held_away(false, Some(between)), "in its place, a gap under 5 cm keeps it at the hand");
    assert!(held_away(false, Some(away + 0.001)), "over 5 cm it goes away");
    assert!(held_away(true, Some(between)), "away, a gap over 3 cm keeps it away");
    assert!(!held_away(true, Some(back - 0.001)), "under 3 cm it comes back");
    assert!(!held_away(true, None) && !held_away(false, None), "no hold, no break-away");
}

/// The bound is a function of the state at each step's start, never kept outside the physics image (an anchor moved
/// for a step is put back after it), so a rollback across frames where it acts replays to the same roots: the wall's
/// deletion arriving late, and a hand input of the pinned stretch arriving late.
#[test]
fn a_rollback_across_frames_where_a_hold_is_bounded_replays_the_same_roots() {
    let (entries, _, _) = pinned(3.0, false);
    let late_delete = entries.iter().position(|e| e.entry.class == Class::Edit).expect("the deletion");
    let late_hand = entries
        .iter()
        .position(|e| e.entry.class == Class::Input && e.entry.stamp >= Frame(24))
        .expect("an input of the pinned stretch");
    for late in [late_delete, late_hand] {
        let mut all = Source::default();
        entries.iter().cloned().for_each(|e| all.add(e));
        let mut a = replica_of(pinned(3.0, false).1);
        a.advance(Frame(96), &mut all);
        let mut some = Source::default();
        entries.iter().enumerate().filter(|&(k, _)| k != late).for_each(|(_, e)| some.add(e.clone()));
        let mut c = replica_of(pinned(3.0, false).1);
        c.advance(Frame(80), &mut some);
        some.add(entries[late].clone());
        let adv = c.advance(Frame(96), &mut some);
        assert!(adv.rolled_back_to.is_some_and(|f| f <= entries[late].entry.stamp), "rolled back: {adv:?}");
        assert_eq!(roots(&mut c, 96), roots(&mut a, 96));
        assert_eq!((a.faults(), c.faults()), (0, 0));
    }
}

/// A placement carries a heavy hold at its offset (@design-reviewer's L1 on 980e6ba7, @coordinator-opus):
/// a 12 kg ball, which a hand of 110 N can't lift, stays on the floor while the hand rises 0.5 m above its place, and
/// when the hand's target jumps 1 m sideways (a teleport) the ball goes 1 m sideways along the floor with it, not up to
/// its place in the hand, where only a light hold is carried ([`pe_replica::hands::placed`]'s rule).
#[test]
fn a_placement_carries_a_heavy_hold_at_its_offset_never_up_to_the_hand() {
    let hr = f64::from(pe_replica::world::HAND_RADIUS);
    let radius = 0.06_f32;
    let heavy = Shape {
        kind: shape_kind::SPHERE,
        density: 12.0 / (4.0 / 3.0 * std::f32::consts::PI * radius.powi(3)),
        size: [radius, 0.0, 0.0],
        ..Shape::default()
    };
    let start = [-f64::from(radius) - hr, f64::from(radius), 0.0];
    let up = [start[0], start[1] + 0.5, 0.0];
    let p = move |t: f64| {
        let at = lerp(start, up, (t - 0.2) / 0.5);
        if t < 1.0 { at } else { [at[0] + 1.0, at[1], 0.0] }
    };
    let mut b = EntryBuilder::new(&keys(), 0);
    let mut s = Source::default();
    path_inputs(&mut b, HAND, 60, 2, 80, &p).into_iter().for_each(|e| s.add(e));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    let mut r = replica_of(with_hands_and_balls(&[(HAND, start)], &[(BALL, [0.0, f64::from(radius), 0.0], heavy)]));
    r.advance(Frame(59), &mut s);
    let before = at(&r, BALL);
    assert!(before[1] < 0.1, "too heavy to lift: on the floor at {before:?}");
    r.advance(Frame(62), &mut s);
    let after = at(&r, BALL);
    assert_eq!(held_by(&r, BALL), Some(oid(HAND)), "still held");
    assert!((after[0] - before[0] - 1.0).abs() < 0.05, "carried 1 m with the hand: {before:?} -> {after:?}");
    assert!(after[1] < 0.1, "at its offset, still on the floor, not lifted to the hand: {after:?}");
    assert_eq!(r.faults(), 0);
}

fn add_static(w: &mut World, id: u128, center: [f64; 3], half: [f32; 3]) {
    let k = keys().key(0);
    w.create(oid(id), None, k, GENESIS).expect("static box");
    w.insert(oid(id), Transform::at(pos(center[0], center[1], center[2]))).expect("its transform");
    w.insert(oid(id), Body { friction: 0.6, ..Body::default() }).expect("its body");
    w.insert(oid(id), cuboid(half)).expect("its shape");
}

// ---- the tight hold (world-model.md 8.1; @item:primordial-engine/tight-hold-for-light-objects-capped-by-a-per-pla) --

/// A held pit ball, lifted to 1 m, then the hand speeding up along x at `a` m/s^2 to 8 m/s, which it keeps for 0.1 s,
/// at `hz`: the most the ball trails its place while the hand speeds up, m, and the ball's and the hand's speeds at
/// the end.
fn flick(hz: u32, a: f64) -> (f64, f64, f64) {
    let rate = f64::from(hz);
    let hr = f64::from(pe_replica::world::HAND_RADIUS);
    let start = [-0.03 - hr, 0.03, 0.0];
    let (t0, ramp) = (1.0, 8.0 / a);
    let p = move |t: f64| {
        let up = lerp(start, [start[0], 1.0, 0.0], (t - 0.2) / 0.5);
        let s = (t - t0).max(0.0);
        let x = if s < ramp { 0.5 * a * s * s } else { 0.5 * a * ramp * ramp + 8.0 * (s - ramp) };
        [up[0] + x, up[1], 0.0]
    };
    let mut b = EntryBuilder::new(&keys(), 0);
    let end = ((t0 + ramp + 0.1) * rate) as u64;
    let mut s = Source::default();
    path_inputs(&mut b, HAND, hz, 2, end, &p).into_iter().for_each(|e| s.add(e));
    s.add(input(&mut b, u64::from(hz / 10), &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    let mut r = replica_at(with_hands_and_balls(&[(HAND, start)], &[(BALL, [0.0, 0.03, 0.0], pit_ball())]), hz);
    let mut trail = 0.0_f64;
    while r.frame().0 < end {
        let next = r.frame().0 + 1;
        r.advance(Frame(next), &mut s);
        if next as f64 >= t0 * rate {
            trail = trail.max(distance(at(&r, BALL), place_in(&r, HAND, BALL)));
        }
    }
    assert_eq!(r.faults(), 0);
    (trail, ball_speed(&r, BALL), ball_speed(&r, HAND))
}

/// A light held object follows the hand within millimetres (the user's ruling; [`pe_replica::hands::
/// HOLD_HZ`]): a pit ball (18 g) on a hand speeding up at 100 m/s^2 to 8 m/s trails its place by a few millimetres at
/// most, at 30, 60 and 120 Hz, and moves at the hand's speed (measured: 2.6, 1.0 and 0.6 mm, where M1's 5 Hz spring
/// trailed by about 9 cm).
#[test]
fn a_light_held_ball_follows_the_hand_within_millimetres() {
    for hz in [30, 60, 120] {
        let (trail, ball, hand) = flick(hz, 100.0);
        println!("RESULT {hz} Hz: a 100 m/s^2 flick trails {:.1} mm; ball {ball:.3}, hand {hand:.3} m/s", trail * 1e3);
        assert!(trail < 0.004, "{hz} Hz: trails {trail} m");
        assert!((ball - hand).abs() < 0.02, "{hz} Hz: the ball at {ball} m/s, the hand at {hand}");
    }
}

/// A spawned box (0.2 m, density 300: 2.4 kg) held at the default strength, lifted to 1 m, then swept along x: speeding
/// up at 60 m/s^2 to `v` m/s, carried 0.2 s, stopped at `stop` m/s^2, then held still 0.5 s. The most it is from its
/// place in the hand (m), the frames it is break-away (`hands::held_away`, D4), and the most it is from its place in
/// the 0.5 s after the stop (its overshoot).
fn sweep_box(v: f64, stop: f64) -> (f64, u32, f64) {
    let hr = f64::from(pe_replica::world::HAND_RADIUS);
    let start = [-0.1 - hr, 0.1, 0.0];
    let (t0, up, cruise, down) = (1.0, v / 60.0, 0.2, v / stop);
    let p = move |t: f64| {
        let lift = lerp(start, [start[0], 1.0, 0.0], (t - 0.2) / 0.5);
        let s = (t - t0).max(0.0);
        let x = if s < up {
            30.0 * s * s
        } else if s < up + cruise {
            30.0 * up * up + v * (s - up)
        } else {
            let d = (s - up - cruise).min(down);
            30.0 * up * up + v * cruise + v * d - 0.5 * stop * d * d
        };
        [lift[0] + x, lift[1], 0.0]
    };
    let hz = 60;
    let stopped = t0 + up + cruise + down;
    let end = ((stopped + 0.5) * f64::from(hz)) as u64;
    let mut b = EntryBuilder::new(&keys(), 0);
    let mut s = Source::default();
    path_inputs(&mut b, HAND, hz, 2, end, &p).into_iter().for_each(|e| s.add(e));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    let crate_box = Shape { density: 300.0, ..cuboid([0.1, 0.1, 0.1]) };
    let mut r = replica_at(with_hands_and_balls(&[(HAND, start)], &[(BALL, [0.0, 0.1, 0.0], crate_box)]), hz);
    let (mut most, mut away_frames, mut after, mut away) = (0.0_f64, 0_u32, 0.0_f64, false);
    while r.frame().0 < end {
        let next = r.frame().0 + 1;
        r.advance(Frame(next), &mut s);
        if (next as f64) < t0 * f64::from(hz) {
            continue;
        }
        let gap = pe_replica::hands::hold_gap(r.physics(), oid(BALL), oid(HAND));
        away = pe_replica::hands::held_away(away, gap);
        away_frames += u32::from(away);
        let g = gap.expect("still held");
        most = most.max(g);
        // In the air nothing touches it: away only by lagging, never blocked (the display keeps it at the hand).
        if away {
            assert!(!r.physics().touches_other_than(oid(BALL), &[oid(HAND)]).unwrap(), "frame {next}: touches nothing");
        }
        if next as f64 >= stopped * f64::from(hz) {
            after = after.max(g);
        }
    }
    assert_eq!(r.faults(), 0);
    (most, away_frames, after)
}

/// Measurement for the arc (@ticket:primordial-engine/friend-test-11-27-feedback-held-object-arc-still): how far a
/// 2.4 kg box lags its place at the default 110 N on quick sweeps and stops, and whether it breaks away (over 5 cm),
/// which switches its drawing to where it is simulated.
#[test]
fn a_spawned_box_on_a_quick_sweep_at_the_default_strength() {
    for (v, stop) in [(1.5, 60.0), (3.0, 60.0), (3.0, 150.0), (5.0, 150.0)] {
        let (most, away, after) = sweep_box(v, stop);
        println!(
            "RESULT 2.4 kg box, {v} m/s, stop at {stop} m/s^2: most {:.1} cm from its place, {away} frames break-away, {:.1} cm after the stop",
            most * 100.0,
            after * 100.0
        );
    }
}

/// Lifts an object of `mass` kg (a sphere 6 cm across, held beside the hand) from the floor to 1 m over 0.5 s with a
/// hand of `strength` N, then holds it there 1 s: how far below its place it ends.
fn lift_with(strength: f32, mass: f32) -> f64 {
    let hr = f64::from(pe_replica::world::HAND_RADIUS);
    let radius = 0.06_f32;
    let shape = Shape {
        kind: shape_kind::SPHERE,
        density: mass / (4.0 / 3.0 * std::f32::consts::PI * radius.powi(3)),
        size: [radius, 0.0, 0.0],
        ..Shape::default()
    };
    let start = [-f64::from(radius) - hr, f64::from(radius), 0.0];
    let mut b = EntryBuilder::new(&keys(), 0);
    let mut s = Source::default();
    path_inputs(&mut b, HAND, 60, 2, 120, &move |t| lerp(start, [start[0], 1.0, 0.0], (t - 0.2) / 0.5))
        .into_iter()
        .for_each(|e| s.add(e));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    let mut world = with_hands_and_balls(&[(HAND, start)], &[(BALL, [0.0, f64::from(radius), 0.0], shape)]);
    let row = *world.store().get::<Hand>(oid(HAND)).expect("the hand's row");
    world.insert(oid(HAND), Hand { strength, ..row }).expect("its strength");
    let mut r = replica_of(world);
    r.advance(Frame(120), &mut s);
    assert_eq!(held_by(&r, BALL), Some(oid(HAND)), "still held");
    assert_eq!(r.faults(), 0);
    place_in(&r, HAND, BALL)[1] - at(&r, BALL)[1]
}

/// A hand's strength caps the force its hold applies (the user's ruling; world-model.md 8.1): at the
/// default 110 N it lifts 8 kg (78 N of weight) and holds it within millimetres, where a hand of 60 N can't lift it,
/// and it stays on the floor, still held. Its strength is its `Hand` row's.
#[test]
fn a_hands_strength_decides_what_it_can_lift() {
    assert_eq!(Hand::default().strength, pe_world::components::DEFAULT_STRENGTH);
    let strong = lift_with(pe_world::components::DEFAULT_STRENGTH, 8.0);
    let weak = lift_with(60.0, 8.0);
    println!("RESULT 8 kg: {:.1} mm below its place at 110 N, {:.3} m at 60 N", strong * 1e3, weak);
    assert!(strong < 0.005, "lifted by 110 N: {strong} m below its place");
    assert!(weak > 0.9, "not lifted by 60 N: {weak} m below its place");
}

const PEDESTAL: u128 = 73;

/// A pile of pit balls in a box with a hand holding a pit ball over it: 25 balls on the floor in a 5 x 5 square, 16 in
/// its hollows above, walls round it (inside 0.30 m square), and a pedestal at x 0.6 the held ball starts on, the hand
/// above it. The hand picks the ball up from above at frame 6, carries it over the pile, comes down at 1 m/s until
/// the ball's place is 5 cm below where it would first touch the pile's top, stays 0.5 s and lifts it out. The world,
/// its entries, the pile's ids, and the frames the press starts and ends.
fn press_into_pile() -> (World, Vec<SignedEntry>, Vec<u128>, u64, u64) {
    let hr = f64::from(pe_replica::world::HAND_RADIUS);
    let mut world =
        with_hands_and_balls(&[(HAND, [0.6, 0.53 + 0.03 + hr, 0.0])], &[(BALL, [0.6, 0.53, 0.0], pit_ball())]);
    add_static(&mut world, PEDESTAL, [0.6, 0.25, 0.0], [0.05, 0.25, 0.05]);
    for (n, (c, h)) in [
        ([0.20, 0.25, 0.0], [0.05, 0.25, 0.25]),
        ([-0.20, 0.25, 0.0], [0.05, 0.25, 0.25]),
        ([0.0, 0.25, 0.20], [0.25, 0.25, 0.05]),
        ([0.0, 0.25, -0.20], [0.25, 0.25, 0.05]),
    ]
    .into_iter()
    .enumerate()
    {
        add_static(&mut world, 80 + n as u128, c, h);
    }
    let k = keys().key(0);
    let mut pile = Vec::new();
    let layer = |n: i32, y: f64| {
        (0..n).flat_map(move |i| (0..n).map(move |j| (i, j))).map(move |(i, j)| {
            let off = -0.03 * f64::from(n - 1);
            [off + 0.06 * f64::from(i), y, off + 0.06 * f64::from(j)]
        })
    };
    for (n, p) in layer(5, 0.03).chain(layer(4, 0.03 + 0.06 * std::f64::consts::FRAC_1_SQRT_2)).enumerate() {
        let id = 100 + n as u128;
        world.create(oid(id), None, k, GENESIS).expect("pile ball");
        world.insert(oid(id), Transform::at(pos(p[0], p[1], p[2]))).expect("its transform");
        world.insert(oid(id), dynamic()).expect("its body");
        world.insert(oid(id), pit_ball()).expect("its shape");
        pile.push(id);
    }
    // The held ball first touches the pile's top in the middle hollow of the upper layer, at y 0.115.
    let touch = 0.03 + 0.06 * std::f64::consts::FRAC_1_SQRT_2 + 0.06 * std::f64::consts::FRAC_1_SQRT_2;
    let (high, low) = (0.9, touch - 0.05 + 0.03 + hr);
    let lift_from = [0.6, 0.53 + 0.03 + hr, 0.0];
    let (t_down, t_low) = (1.2, 1.2 + (high - low));
    let p = move |t: f64| {
        if t < 0.6 {
            lerp(lift_from, [0.6, high, 0.0], (t - 0.2) / 0.4)
        } else if t < t_down {
            lerp([0.6, high, 0.0], [0.0, high, 0.0], (t - 0.6) / 0.6)
        } else if t < t_low + 0.5 {
            [0.0, (high - (t - t_down)).max(low), 0.0]
        } else {
            [0.0, (low + (t - t_low - 0.5)).min(high), 0.0]
        }
    };
    let mut b = EntryBuilder::new(&keys(), 0);
    let end = ((t_low + 1.5) * 60.0) as u64;
    let mut entries = path_inputs(&mut b, HAND, 60, 2, end, &p);
    entries.push(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    (world, entries, pile, (t_down * 60.0) as u64, ((t_low + 0.5) * 60.0) as u64)
}

/// A ball held into a stack (world-model.md 8.1; @coordinator-opus): pressed 5 cm into a pile of pit
/// balls in a box, the hold's force cap pushes the pile gently (measured: no pile ball faster than 0.5 m/s), the pile
/// stays in its box and on the floor, the held ball goes no lower than the pile's lower layer, and it comes out with
/// the hand. A replica that gets one of the press's hand inputs late rolls back and reaches the same roots.
#[test]
fn a_ball_held_into_a_stack_pushes_it_gently_and_a_late_input_replays_the_same_roots() {
    let (world, entries, pile, press, pressed) = press_into_pile();
    let end = entries.iter().map(|e| e.entry.stamp.0).max().expect("entries") + 30;
    let mut s = Source::default();
    entries.iter().cloned().for_each(|e| s.add(e));
    let mut r = replica_of(world.clone());
    let (mut fastest, mut lowest, mut out_of_box) = (0.0_f64, f64::MAX, 0.0_f64);
    while r.frame().0 < end {
        let next = r.frame().0 + 1;
        r.advance(Frame(next), &mut s);
        if next >= press {
            for &id in &pile {
                fastest = fastest.max(ball_speed(&r, id));
                let p = at(&r, id);
                out_of_box = out_of_box.max(p[0].abs()).max(p[2].abs()).max(0.03 - p[1]);
            }
        }
        if (press..pressed).contains(&next) {
            lowest = lowest.min(at(&r, BALL)[1]);
        }
    }
    println!(
        "RESULT the pile's fastest {fastest:.3} m/s, its farthest out {out_of_box:.4} m; the held ball down to y \
         {lowest:.4} m"
    );
    assert!(fastest < 1.0, "the pile pushed gently: {fastest} m/s");
    assert!(out_of_box < 0.15, "the pile stays in its box and on the floor: {out_of_box} m");
    assert!(lowest > 0.06, "the held ball goes no lower than the pile's lower layer: {lowest} m");
    assert!(distance(at(&r, BALL), place_in(&r, HAND, BALL)) < 0.01, "out with the hand");
    assert_eq!(r.faults(), 0);
    // The same with a hand input of the press arriving late.
    let late = entries
        .iter()
        .position(|e| e.entry.class == Class::Input && (press + 10..pressed).contains(&e.entry.stamp.0))
        .expect("an input of the press");
    let mut l = replica_of(world);
    let mut early = Source::default();
    entries.iter().enumerate().filter(|&(i, _)| i != late).for_each(|(_, e)| early.add(e.clone()));
    l.advance(Frame(pressed), &mut early);
    early.add(entries[late].clone());
    let adv = l.advance(Frame(end), &mut early);
    assert!(adv.rolled_back_to.is_some_and(|f| f <= entries[late].entry.stamp), "rolled back: {adv:?}");
    assert_eq!(roots(&mut l, end), roots(&mut r, end), "the same roots");
    assert_eq!(l.faults(), 0);
}

/// A script runner that holds no module: a world's scripts never run on it, each counted missing (crate::logic).
struct NoModules;

impl pe_replica::ScriptRunner for NoModules {
    fn identity(&self) -> Bytes {
        Bytes::from_static(b"the test runner without modules")
    }

    fn holds(&self, _: &Hash) -> bool {
        false
    }

    fn module(&self, _: &Hash) -> Option<Bytes> {
        None
    }

    fn run_stage(
        &mut self,
        _: pe_seams::Stage,
        scripts: &[pe_replica::logic::ScriptRow],
        _: &mut pe_sim::SimStore,
        _: &pe_replica::logic::StageWorld<'_>,
    ) -> pe_replica::logic::StageOutcome {
        let missing = scripts.iter().map(|s| s.asset).collect();
        pe_replica::logic::StageOutcome { missing, ..Default::default() }
    }
}

/// A script commit's re-key happens when it activates, at the end of a frame (logic.md 6.2a), so no step record
/// names re-keyed objects (S5 v1.8 moved the record to `FrameView.activations` and removed `StepRecord::rekeyed`;
/// @coordinator-opus). With one member the commit is its own quorum and activates at the end of its
/// frame: the replica's world then runs version 2, with both lamps' rows moved onto its ids.
#[test]
fn a_script_commit_activates_at_its_frame_s_end_and_no_record_names_the_re_key() {
    use pe_sim::{ComponentDesc, FieldDesc, FieldKind};
    use pe_world::BlobRef;
    use pe_world::script::{pack_source, script_component_id, tag_desc};
    let archive = |src: u8| -> bytes::Bytes {
        let lib = format!("// blink {src}\n");
        pack_source(&[("src/lib.rs", lib.as_bytes())]).expect("an archive").into()
    };
    let version = |src: u8| Hash(BlobRef::of(&archive(src)).hash);
    let fields = |src: u8| {
        let f32f = |n: &str, at| FieldDesc::new(n, FieldKind::F32, at, 1);
        if src == 1 { vec![f32f("period", 0)] } else { vec![f32f("period", 0), f32f("red", 4)] }
    };
    let blink = |src: u8| {
        let id = script_component_id(&version(src), "Blink", fields(src)).expect("an id");
        ComponentDesc::new(id, "Blink", fields(src)).expect("a description")
    };
    let commit = |target, base, src: u8, at: u64| {
        tx(vec![
            Op::Register { components: vec![tag_desc(&version(src)), blink(src)] },
            Op::commit_script(target, base, archive(src), Hash([1; 32]), Hash([2; 32]), Hash([3; 32]), at),
        ])
    };
    let lamps = [oid(21), oid(20)];
    let set = |id: ObjectId, component, bytes: Vec<u8>| Op::SetRow {
        id: Target::Existing(id),
        row: Row { component, instance: 0, bytes },
    };
    let mut world = genesis();
    for (i, &l) in lamps.iter().enumerate() {
        world.create(l, None, keys().key(0), GENESIS).expect("a lamp");
        world.insert(l, Transform::at(pos(i as f64, 0.5, 0.0))).expect("its transform");
    }
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    // Version 1 at frame 1 (active from 2), attached to both lamps at 2; version 2 at frame 3 re-keys their rows
    // when it activates, at the end of frame 3.
    let first = b.next(Class::Edit, Frame(1), Bytes::from(commit(AssetTarget::New(0), None, 1, 1).encode()));
    let tag = tag_desc(&version(1)).id();
    let attach =
        lamps.iter().flat_map(|&l| [set(l, tag, vec![]), set(l, blink(1).id(), 2.0f32.to_le_bytes().to_vec())]);
    let attach = b.next(Class::Edit, Frame(2), Bytes::from(tx(attach.collect()).encode()));
    let mut scratch = world.clone();
    assert!(matches!(scratch.apply_edit(Frame(1), &first).0, Outcome::Applied { .. }), "version 1 commits");
    assert_eq!(scratch.end_frame(Frame(1)).activated.len(), 1, "its author is its quorum");
    // The asset's id is derived from the entry that created it (world-model.md 7.9).
    let e = &first.entry;
    let asset = pe_world::script::asset_created(e.line, e.author, e.chain, e.seq, 0);
    let base = scratch.modules()[&asset].version;
    let second =
        b.next(Class::Edit, Frame(3), Bytes::from(commit(AssetTarget::Existing(asset), Some(base), 2, 3).encode()));
    // A replica without a runner refuses a frame with a code commit (crate::logic): this one holds no module, so the
    // script never runs here, but its commits step.
    let replica = ReplicaImpl::new(PeWorld::new(world, DomainId(0)), config()).expect("a replica");
    let mut r = replica.with_runner(Box::new(NoModules));
    let mut s = Source::default();
    [first.clone(), attach.clone(), second.clone()].into_iter().for_each(|e| s.add(e));
    r.advance(Frame(6), &mut s);
    let record = |e: &SignedEntry| {
        r.view().changes.iter().rev().find(|c| c.entry == e.id()).cloned().expect("a record of the entry")
    };
    for e in [&first, &attach, &second] {
        let c = record(e);
        assert!(matches!(c.outcome, Outcome::Applied { .. }), "{:?}", c.outcome);
    }
    let w = r.world().world();
    assert_eq!(w.modules()[&asset].version, pe_world::Version { entry: second.id(), op: 1 }, "version 2 runs");
    for l in lamps {
        assert!(w.store().get_raw(blink(2).id(), l).is_some() && w.store().get_raw(blink(1).id(), l).is_none());
    }
}

// ---- leave (agreement.md 8; world-model.md 2.4) --------------------------------------------------------------------

const B_HAND: u128 = 51;
const TOY: u128 = 62;

/// `with_hand_and_ball`, and a second editor B (key 1) whose hand rests on the ball, holding a toy as the hand's
/// child.
fn with_bs_hand() -> World {
    let mut w = with_hand_and_ball();
    let b = keys().key(1);
    w.set_member(b, perm::EDIT);
    w.create(oid(B_HAND), None, b, GENESIS).expect("B's hand");
    w.insert(oid(B_HAND), Transform::at(pos(2.0, 0.1, 0.0))).expect("its transform");
    w.insert(oid(B_HAND), Hand { owner: b, ..Hand::default() }).expect("its row");
    w.create(oid(TOY), Some(oid(B_HAND)), b, GENESIS).expect("a toy in B's hand");
    w.insert(oid(TOY), Transform::at(pos(0.0, 0.3, 0.0))).expect("its transform");
    w
}

/// B grabs the ball at frame 3, and leaves at frame 10 (`pe_seams::LEAVE`, the last entry of its chain).
fn grab_then_leave() -> (SignedEntry, SignedEntry) {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 1);
    let grab = input(&mut b, 3, &Input::Grab { object: oid(BALL), hand: Some(oid(B_HAND)) });
    let leave = b.next(Class::Protocol, Frame(10), Bytes::from_static(pe_seams::LEAVE));
    (grab, leave)
}

/// A member's leave at its stamp (agreement.md 8): its hand goes from the world and from the physics, and so does
/// the grab joint of what it held. The claim ends. The toy that was the hand's child stays, as a root at the world
/// pose it had. The other member's hand stays.
#[test]
fn a_leave_removes_the_leavers_hand_and_its_grab_and_keeps_what_it_held() {
    let (grab, leave) = grab_then_leave();
    let mut s = Source::default();
    s.add(grab);
    s.add(leave.clone());
    let mut r = replica_of(with_bs_hand());
    r.advance(Frame(10), &mut s);
    let claims: Vec<_> = r.world().claims().iter().map(|c| (c.object, c.hand)).collect();
    assert_eq!(claims, [(oid(BALL), oid(B_HAND))]);
    assert!(r.physics().has_body(oid(B_HAND)) && r.physics().joint_count() == 1, "holding before the leave");
    let toy = r.world().world().store().world_transform(oid(TOY)).expect("the toy");
    r.advance(Frame(20), &mut s);
    assert_eq!(r.faults(), 0);
    assert!(matches!(r.outcome(&leave.id()).map(|o| o.1), Some(Outcome::Applied { .. })));
    let store = r.world().world().store();
    assert!(!store.exists(oid(B_HAND)), "B's hand is gone");
    assert!(store.exists(oid(HAND)), "A's stays");
    assert_eq!(store.parent(oid(TOY)), None, "the toy is a root");
    assert_eq!(store.world_transform(oid(TOY)).expect("the toy"), toy, "at the world pose it had");
    assert!(r.world().claims().is_empty());
    assert!(!r.physics().has_body(oid(B_HAND)) && r.physics().joint_count() == 0, "no hand body, no grab");
}

/// An admin's `Remove` of B (S3 v1.13; agreement.md 8) at frame 10 does what B's own leave does: B's hand and its
/// grab go, the claim ends, the toy stays as a root, A's hand stays. A malformed `Remove` is refused and changes
/// nothing.
#[test]
fn a_remove_takes_the_removed_members_hand_and_grab_as_its_leave_would() {
    let (grab, _) = grab_then_leave();
    let keys = keys();
    let mut a = EntryBuilder::new(&keys, 0);
    let bad = a.next(Class::Protocol, Frame(8), Bytes::from_static(b"pe.agree.remove/short"));
    let payload = pe_seams::Remove { key: keys.key(1), reason: pe_seams::RemoveReason::Admin }.encode();
    let remove = a.next(Class::Protocol, Frame(10), payload);
    let mut s = Source::default();
    s.add(grab);
    s.add(bad.clone());
    s.add(remove.clone());
    let mut r = replica_of(with_bs_hand());
    r.advance(Frame(9), &mut s);
    assert!(matches!(r.outcome(&bad.id()).map(|o| o.1), Some(Outcome::Refused { .. })), "the malformed Remove");
    assert!(r.world().world().store().exists(oid(B_HAND)) && r.physics().joint_count() == 1, "holding before");
    let toy = r.world().world().store().world_transform(oid(TOY)).expect("the toy");
    r.advance(Frame(20), &mut s);
    assert_eq!(r.faults(), 0);
    assert!(matches!(r.outcome(&remove.id()).map(|o| o.1), Some(Outcome::Applied { .. })));
    let store = r.world().world().store();
    assert!(!store.exists(oid(B_HAND)), "B's hand is gone");
    assert!(store.exists(oid(HAND)), "A's stays");
    assert_eq!(store.parent(oid(TOY)), None, "the toy is a root");
    assert_eq!(store.world_transform(oid(TOY)).expect("the toy"), toy, "at the world pose it had");
    assert!(r.world().claims().is_empty());
    assert!(!r.physics().has_body(oid(B_HAND)) && r.physics().joint_count() == 0, "no hand body, no grab");
}

/// Silent members (S3 v1.14; agreement.md 8): A's silence vote against B (`Remove{B, Silent}`) at frame 5 is nothing
/// in the world; S3's removal reaches it by `Remove{B, Departed}`, here A's at 10, which does what B's leave does. A
/// duplicate at 12 (every remaining member signs one) is a true no-op: applied, nothing written, the same root as a
/// world without it (@world-model). B admitted again after it comes back fresh: two new hands from the
/// `Admit`'s frame, none of the old ones (coordinator: S3 admits B only once a `Departed` is final).
#[test]
fn a_silence_vote_is_nothing_in_the_world_and_the_first_departure_takes_the_hand() {
    let (grab, _) = grab_then_leave();
    let keys = keys();
    let b_key = keys.key(1);
    let remove = |reason| pe_seams::Remove { key: b_key, reason }.encode();
    let mut a = EntryBuilder::new(&keys, 0);
    let vote = a.next(Class::Protocol, Frame(5), remove(pe_seams::RemoveReason::Silent));
    let departed = a.next(Class::Protocol, Frame(10), remove(pe_seams::RemoveReason::Departed));
    let duplicate = a.next(Class::Protocol, Frame(12), remove(pe_seams::RemoveReason::Departed));
    let run = |with_duplicate: bool| {
        let mut s = Source::default();
        s.add(grab.clone());
        s.add(vote.clone());
        s.add(departed.clone());
        if with_duplicate {
            s.add(duplicate.clone());
        }
        let mut r = replica_of(with_bs_hand());
        r.advance(Frame(9), &mut s);
        assert!(matches!(r.outcome(&vote.id()).map(|o| o.1), Some(Outcome::Applied { .. })), "the vote applies");
        let store = r.world().world().store();
        assert!(store.exists(oid(B_HAND)) && r.physics().joint_count() == 1, "the vote changes nothing");
        r.advance(Frame(20), &mut s);
        assert_eq!(r.faults(), 0);
        assert!(matches!(r.outcome(&departed.id()).map(|o| o.1), Some(Outcome::Applied { .. })));
        let store = r.world().world().store();
        assert!(!store.exists(oid(B_HAND)), "B's hand is gone");
        assert!(store.exists(oid(HAND)), "A's stays");
        assert_eq!(store.parent(oid(TOY)), None, "the toy is a root");
        assert!(r.world().claims().is_empty() && r.physics().joint_count() == 0, "no grab");
        r
    };
    let mut once = run(false);
    let mut twice = run(true);
    assert!(
        matches!(twice.outcome(&duplicate.id()).map(|o| o.1), Some(Outcome::Applied { .. })),
        "the duplicate applies as nothing, not a refusal"
    );
    assert_eq!(roots(&mut once, 20), roots(&mut twice, 20), "a duplicate departure moved the roots");
    assert_eq!(twice.world().root(), once.world().root(), "a duplicate departure moved the live root");

    // The rejoin, once the departure applied: fresh hands from the `Admit`'s frame, none of the old.
    let mut pw = PeWorld::new(with_bs_hand(), DomainId(0));
    assert!(matches!(pw.apply(Frame(10), &departed).0, Outcome::Applied { .. }));
    let again = a.next(Class::Protocol, Frame(15), pe_seams::Admit::new(b_key, perm::EDIT).encode());
    assert!(matches!(pw.apply(Frame(15), &again).0, Outcome::Applied { .. }));
    let mut hands: Vec<ObjectId> = pw.hands().into_iter().filter(|h| h.owner == b_key).map(|h| h.hand).collect();
    hands.sort_by_key(|id| id.to_u128());
    let line = again.entry.line;
    let mut fresh = vec![ObjectId::hand(line, b_key, 0, Frame(15)), ObjectId::hand(line, b_key, 1, Frame(15))];
    fresh.sort_by_key(|id| id.to_u128());
    assert_eq!(hands, fresh, "B's hands after the rejoin");
    assert!(!pw.world().store().exists(oid(B_HAND)), "B's old hand came back");
}

/// A leave that arrives late rolls the replica back to its stamp and reaches the roots of a replica that had it in
/// order. Until then the hand and the grab stay.
#[test]
fn a_late_leave_reaches_the_in_order_roots() {
    let (grab, leave) = grab_then_leave();
    let mut s = Source::default();
    s.add(grab.clone());
    s.add(leave.clone());
    let mut in_order = replica_of(with_bs_hand());
    in_order.advance(Frame(32), &mut s);
    let mut s = Source::default();
    s.add(grab);
    let mut late = replica_of(with_bs_hand());
    late.advance(Frame(20), &mut s);
    assert!(late.world().world().store().exists(oid(B_HAND)) && late.physics().joint_count() == 1);
    s.add(leave);
    let adv = late.advance(Frame(32), &mut s);
    assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= 10), "{adv:?}");
    assert_eq!((in_order.faults(), late.faults()), (0, 0));
    assert_eq!(roots(&mut late, 32), roots(&mut in_order, 32));
    assert_eq!(roots(&mut late, 32).len(), 5);
    assert!(!late.world().world().store().exists(oid(B_HAND)) && late.physics().joint_count() == 0);
}

// ---- physical objects the replica cannot build (replica.md 2.1; was O12) --------------------------------------------

/// A mesh (2), an unknown body kind (3), a body with no `Transform` (4) and a body whose compound child is a capsule
/// under non-uniform scale (7) are physical but cannot be built, and `body` says why. A `Body` with no shape anywhere
/// (5), even of an unknown kind (6), is not physical, and neither is a compound child (8). `physical` lists exactly
/// the objects whose `body` is not `Ok(None)`.
#[test]
fn a_physical_object_that_cannot_be_built_says_why() {
    let mut w = genesis();
    let k = MemberKey([1; 32]);
    let placed = |w: &mut World, id: u128, parent: Option<u128>, t: Option<Transform>| {
        w.create(oid(id), parent.map(oid), k, GENESIS).expect("create");
        if let Some(t) = t {
            w.insert(oid(id), t).expect("transform");
        }
    };
    let at = |x: f64| Some(Transform::at(pos(x, 1.0, 0.0)));
    placed(&mut w, 2, None, at(0.0));
    w.insert(oid(2), dynamic()).expect("2");
    w.insert(oid(2), Shape { kind: shape_kind::MESH, ..cuboid([0.5; 3]) }).expect("2");
    placed(&mut w, 3, None, at(2.0));
    w.insert(oid(3), Body { kind: 7, ..dynamic() }).expect("3");
    w.insert(oid(3), sphere(0.5)).expect("3");
    placed(&mut w, 4, None, None);
    w.insert(oid(4), dynamic()).expect("4");
    w.insert(oid(4), sphere(0.5)).expect("4");
    placed(&mut w, 5, None, at(4.0));
    w.insert(oid(5), dynamic()).expect("5");
    placed(&mut w, 6, None, at(6.0));
    w.insert(oid(6), Body { kind: 7, ..dynamic() }).expect("6");
    placed(&mut w, 7, None, at(8.0));
    w.insert(oid(7), dynamic()).expect("7");
    w.insert(oid(7), cuboid([0.5; 3])).expect("7");
    placed(&mut w, 8, Some(7), Some(Transform::local([1.0, 0.0, 0.0]).with_scale([1.0, 2.0, 1.0])));
    w.insert(oid(8), Shape { kind: shape_kind::CAPSULE, ..sphere(0.2) }).expect("8");
    let pw = PeWorld::new(w, DomainId(0));

    assert_eq!(pw.body(oid(2)), Err(Unbuildable::Shape(ShapeReason::SwitchOff)), "a mesh, in a world without them");
    assert_eq!(pw.body(oid(3)), Err(Unbuildable::Kind));
    assert_eq!(pw.body(oid(4)), Err(Unbuildable::Pose), "no Transform");
    assert_eq!(pw.body(oid(5)), Ok(None), "no shape");
    assert_eq!(pw.body(oid(6)), Ok(None), "no shape, so its kind is never read");
    let non_uniform = Err(Unbuildable::Shape(ShapeReason::NonUniformScale));
    assert_eq!(pw.body(oid(7)), non_uniform, "its child's capsule under non-uniform scale");
    assert_eq!(pw.body(oid(8)), Ok(None), "a compound child has no body of its own");
    let listed: Vec<ObjectId> = (1..=8).map(oid).filter(|&id| pw.body(id) != Ok(None)).collect();
    assert_eq!(listed, [oid(1), oid(2), oid(3), oid(4), oid(7)]);
    assert_eq!(pw.physical(), listed);
}

/// Genesis with a ball (`BALL`, a 0.25 m sphere) dropped from (2, 1, 0), a mesh (2) and a body of an unknown kind (3).
fn with_unbuildable_bodies() -> World {
    let mut w = genesis();
    let k = MemberKey([1; 32]);
    for (id, x) in [(BALL, 2.0), (2, -2.0), (3, -4.0)] {
        w.create(oid(id), None, k, GENESIS).expect("create");
        w.insert(oid(id), Transform::at(pos(x, 1.0, 0.0))).expect("transform");
    }
    w.insert(oid(BALL), dynamic()).expect("ball");
    w.insert(oid(BALL), sphere(0.25)).expect("ball");
    w.insert(oid(2), dynamic()).expect("2");
    w.insert(oid(2), Shape { kind: shape_kind::MESH, ..cuboid([0.5; 3]) }).expect("2");
    w.insert(oid(3), Body { kind: 7, ..dynamic() }).expect("3");
    w.insert(oid(3), sphere(0.5)).expect("3");
    w
}

/// Where `id`'s body is in the physics.
fn body_at(r: &ReplicaImpl<PeWorld>, id: u128) -> [f64; 3] {
    r.physics().pose(oid(id)).expect("a body").position
}

/// A replica gives a physical object it cannot build no body and counts a fault for each when it starts. Stepping
/// counts no more, and the rest of the world steps as before.
#[test]
fn a_replica_starts_without_the_bodies_it_cannot_build_and_counts_each() {
    let mut r = replica_of(with_unbuildable_bodies());
    assert_eq!(r.faults(), 2);
    assert_eq!(r.physics().objects().collect::<Vec<_>>(), [oid(1), oid(BALL)]);
    assert_eq!(r.world().physical(), [oid(1), oid(2), oid(3), oid(BALL)]);
    r.advance(Frame(24), &mut Source::default());
    assert_eq!(r.faults(), 2, "stepping counts no more");
    assert!(body_at(&r, BALL)[1] < 0.5, "the ball fell: {:?}", body_at(&r, BALL));
    assert!(!r.physics().has_body(oid(2)) && !r.physics().has_body(oid(3)));
}

/// One edit from the editor at `frame`.
fn edit_at(b: &mut EntryBuilder, frame: u64, ops: Vec<Op>) -> SignedEntry {
    b.next(Class::Edit, Frame(frame), Bytes::from(tx(ops).encode()))
}

/// Edits to the ball: at 4 a non-uniform scale makes its sphere unbuildable, at 8 it moves to x = 3 still so, and at
/// 12 a uniform scale makes it buildable again; at 16 an edit creates a sphere under non-uniform scale.
fn unbuildable_edits(b: &mut EntryBuilder) -> Vec<SignedEntry> {
    let ball = |x: f64, scale: [f32; 3]| Op::SetRow {
        id: Target::Existing(oid(BALL)),
        row: row(Transform::at(pos(x, 1.0, 0.0)).with_scale(scale)),
    };
    let scaled = Transform::at(pos(5.0, 1.0, 0.0)).with_scale([2.0, 1.0, 1.0]);
    let rows = vec![row(scaled), row(dynamic()), row(sphere(0.25))];
    vec![
        edit_at(b, 4, vec![ball(2.0, [1.0, 2.0, 1.0])]),
        edit_at(b, 8, vec![ball(3.0, [1.0, 2.0, 1.0])]),
        edit_at(b, 12, vec![ball(3.0, [1.0; 3])]),
        edit_at(b, 16, vec![Op::Create { new: 0, parent: Target::Root, index: None, rows }]),
    ]
}

/// An edit that makes the ball unbuildable takes its body away and counts a fault; a move while it stays so changes
/// nothing; one that makes it buildable again gives it a body, at rest where its row is; and creating an unbuildable
/// body counts one more fault. A replica that gets the first edit late rolls back to the same roots. It counts the
/// fault of its first history too: there, the edit at 8 found a body to take away.
#[test]
fn an_edit_that_makes_a_body_unbuildable_takes_its_body_away_and_counts_a_fault() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let edits = unbuildable_edits(&mut b);
    let mut s = Source::default();
    edits.iter().for_each(|e| s.add(e.clone()));
    let mut r = replica_of(with_unbuildable_bodies());
    let start = r.faults();
    let mut to = |r: &mut ReplicaImpl<PeWorld>, f: u64| {
        r.advance(Frame(f), &mut s);
        r.faults() - start
    };
    assert_eq!(to(&mut r, 4), 0);
    assert!(r.physics().has_body(oid(BALL)));
    assert_eq!(to(&mut r, 8), 1, "a sphere under non-uniform scale: no body, and a fault");
    assert!(!r.physics().has_body(oid(BALL)) && r.world().physical().contains(&oid(BALL)));
    assert_eq!(to(&mut r, 12), 1, "a move while it stays unbuildable");
    assert_eq!(to(&mut r, 13), 1);
    assert!((body_at(&r, BALL)[0] - 3.0).abs() < 1e-6, "buildable again, at its row's pose: {:?}", body_at(&r, BALL));
    assert_eq!(to(&mut r, 40), 2, "a body created unbuildable");
    let created = match r.outcome(&edits[3].id()) {
        Some((_, Outcome::Applied { created, .. })) => created[0],
        o => panic!("{o:?}"),
    };
    assert!(!r.physics().has_body(created) && r.world().physical().contains(&created));

    let mut late = Source::default();
    edits[1..].iter().for_each(|e| late.add(e.clone()));
    let mut l = replica_of(with_unbuildable_bodies());
    l.advance(Frame(10), &mut late);
    assert_eq!(l.faults() - start, 1, "without the edit at 4, the one at 8 takes the body away");
    late.add(edits[0].clone());
    let adv = l.advance(Frame(40), &mut late);
    assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= 4), "{adv:?}");
    assert_eq!(roots(&mut l, 40), roots(&mut r, 40));
    assert_eq!(roots(&mut r, 40).len(), 6);
    assert_eq!(l.faults() - start, 3, "the first history's fault, then the edits at 4 and 16");
}

/// The tight hold's figures at the world rates 30, 60 and 120 Hz (world-model.md 8.1; @item:primordial-engine/
/// tight-hold-for-light-objects-capped-by-a-per-pla), with a held pit ball: the spring's rate `k` and reach at rest;
/// the sag held still; the most trail while the hand speeds up at 100 and 200 m/s^2 to 8 m/s, and the ball's speed
/// against the hand's at the end; the deepest it goes pressed 20 cm into the floor; pinned by a 10 cm wall with the
/// hand 1 m past it, how deep it goes and how fast it comes back once the wall is deleted (by the distance it moves a
/// frame, and at a frame's end); and the hand at 1.5 times the cap for 2 s sideways, up and down: its top speed, its
/// most trail, and how far below its place it sags. A measurement: it prints, it asserts only that nothing faulted.
#[test]
#[ignore = "a measurement: prints the tight hold's figures at 30, 60 and 120 Hz"]
fn measure_the_tight_hold_at_30_60_and_120_hz() {
    let hr = f64::from(pe_replica::world::HAND_RADIUS);
    for hz in [30_u32, 60, 120] {
        let rate = f64::from(hz);
        let grab_at = u64::from(hz / 10);
        let run = |world: World, entries: Vec<SignedEntry>, to: u64, each: &mut dyn FnMut(&ReplicaImpl<PeWorld>)| {
            let mut s = Source::default();
            entries.into_iter().for_each(|e| s.add(e));
            let mut r = replica_at(world, hz);
            while r.frame().0 < to {
                let next = r.frame().0 + 1;
                r.advance(Frame(next), &mut s);
                each(&r);
            }
            assert_eq!(r.faults(), 0, "{hz} Hz");
        };
        let ball_world =
            |hand: [f64; 3], ball: [f64; 3]| with_hands_and_balls(&[(HAND, hand)], &[(BALL, ball, pit_ball())]);
        // The rate and the reach at rest.
        let k = {
            let r = replica_at(ball_world([0.0, 1.0, 0.0], [0.12, 1.0, 0.0]), hz);
            r.physics().grab_spring_rate(pe_replica::hands::grab_spring(0.0), 1.0 / hz as f32)
        };
        // Held still: grabbed on the floor, lifted to 1 m over 0.5 s, then held 1 s.
        let start = [-0.03 - hr, 0.03, 0.0];
        let lifted = move |t: f64| lerp(start, [start[0], 1.0, 0.0], (t - 0.2) / 0.5);
        let mut b = EntryBuilder::new(&keys(), 0);
        let end = (1.8 * rate) as u64;
        let mut entries = path_inputs(&mut b, HAND, hz, 2, end, &lifted);
        entries.push(input(&mut b, grab_at, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
        let mut sag = 0.0;
        run(ball_world(start, [0.0, 0.03, 0.0]), entries, end, &mut |r| {
            if r.frame().0 > grab_at {
                sag = place_in(r, HAND, BALL)[1] - at(r, BALL)[1];
            }
        });
        // Flicks: lifted as above, then speeding up along x at `a` to 8 m/s, which it keeps for 0.1 s.
        let mut flicks = Vec::new();
        for a in [100.0, 200.0] {
            let t0 = 1.0;
            let ramp = 8.0 / a;
            let p = move |t: f64| {
                let up = lifted(t);
                let s = (t - t0).max(0.0);
                let x = if s < ramp { 0.5 * a * s * s } else { 0.5 * a * ramp * ramp + 8.0 * (s - ramp) };
                [up[0] + x, up[1], 0.0]
            };
            let mut b = EntryBuilder::new(&keys(), 0);
            let end = ((t0 + ramp + 0.1) * rate) as u64;
            let mut entries = path_inputs(&mut b, HAND, hz, 2, end, &p);
            entries.push(input(&mut b, grab_at, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
            let (mut trail, mut ball_v, mut hand_v) = (0.0_f64, 0.0, 0.0);
            run(ball_world(start, [0.0, 0.03, 0.0]), entries, end, &mut |r| {
                if r.frame().0 as f64 >= t0 * rate {
                    trail = trail.max(distance(at(r, BALL), place_in(r, HAND, BALL)));
                    ball_v = ball_speed(r, BALL);
                    hand_v = ball_speed(r, HAND);
                }
            });
            flicks
                .push(format!("{a:.0} m/s^2: trail {:.1} mm, end {ball_v:.2} vs hand {hand_v:.2} m/s", trail * 1000.0));
        }
        // Pressed into the floor: held from above at 0.5 m, the hand comes down at 1 m/s until the ball's place is
        // 20 cm under the floor's top, and stays 0.5 s.
        let over = [0.0, 0.5 + 0.03 + hr, 0.0];
        let low = 0.03 - 0.2 + 0.03 + hr;
        let p = move |t: f64| [0.0, (over[1] - (t - 0.3).max(0.0)).max(low), 0.0];
        let mut b = EntryBuilder::new(&keys(), 0);
        let end = ((0.3 + over[1] - low + 0.5) * rate) as u64;
        let mut entries = path_inputs(&mut b, HAND, hz, 2, end, &p);
        entries.push(input(&mut b, grab_at, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
        let mut floor_in = 0.0_f64;
        run(ball_world(over, [0.0, 0.5, 0.0]), entries, end, &mut |r| floor_in = floor_in.max(0.03 - at(r, BALL)[1]));
        // Pinned by the wall (2.25 to 2.35): carried 1 m past it from 0.17 s to 0.67 s, the wall deleted at 1 s.
        let (from, past) = ([2.08, 0.03, 0.0], [3.08, 0.03, 0.0]);
        let mut b = EntryBuilder::new(&keys(), 0);
        let end = (2.0 * rate) as u64;
        let mut entries = path_inputs(&mut b, HAND, hz, 2, end, &move |t| lerp(from, past, (t - 0.17) / 0.5));
        entries.push(input(&mut b, grab_at, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
        let delete = tx(vec![Op::Delete { id: Target::Existing(oid(WALL)) }]);
        entries.push(b.next(Class::Edit, Frame(u64::from(hz)), Bytes::from(delete.encode())));
        let mut world = ball_world(from, [2.2, 0.03, 0.0]);
        add_wall(&mut world);
        let (mut deepest, mut moved, mut top, mut last) = (f64::MIN, 0.0_f64, 0.0_f64, [2.2, 0.03, 0.0]);
        run(world, entries, end, &mut |r| {
            let p = at(r, BALL);
            if r.frame().0 <= u64::from(hz) {
                deepest = deepest.max(p[0] + 0.03 - 2.25);
            } else {
                moved = moved.max(distance(p, last) * rate);
                top = top.max(ball_speed(r, BALL));
            }
            last = p;
        });
        // The overrun at 1.5 times the cap for 2 s, as in
        // a_held_ball_kept_above_the_cap_for_two_seconds_trails_and_stays_up.
        let mut overruns = Vec::new();
        for dir in 0..3u8 {
            let x0 = -9.0 - 0.03 - hr;
            let floor = [x0, 0.03, 0.0];
            let (t_run, path): (f64, Box<dyn Fn(f64) -> [f64; 3]>) = match dir {
                0 => (
                    1.0,
                    Box::new(move |t: f64| {
                        let up = lerp(floor, [x0, 1.0, 0.0], (t - 0.2) / 0.5);
                        [up[0] + 15.0 * (t - 1.0).clamp(0.0, 2.0), up[1], 0.0]
                    }),
                ),
                1 => (1.0, Box::new(move |t: f64| [x0, 0.03 + 15.0 * (t - 1.0).clamp(0.0, 2.0), 0.0])),
                _ => (
                    4.0,
                    Box::new(move |t: f64| {
                        let lifted = [x0, 0.03 + 10.0 * (t - 0.2).clamp(0.0, 3.1), 0.0];
                        [x0, lifted[1] - 15.0 * (t - 4.0).clamp(0.0, 2.0), 0.0]
                    }),
                ),
            };
            let end = ((t_run + 2.5) * rate) as u64;
            let mut b = EntryBuilder::new(&keys(), 0);
            let mut entries = path_inputs(&mut b, HAND, hz, 2, end, &*path);
            entries.push(input(&mut b, grab_at, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
            let (mut top, mut trail, mut below) = (0.0_f64, 0.0_f64, 0.0_f64);
            run(ball_world(floor, [-9.0, 0.03, 0.0]), entries, end, &mut |r| {
                if r.frame().0 as f64 >= t_run * rate {
                    let (ball, place) = (at(r, BALL), place_in(r, HAND, BALL));
                    top = top.max(ball_speed(r, BALL));
                    below = below.max(place[1] - ball[1]);
                    trail = trail.max(distance(ball, place));
                }
            });
            overruns.push(format!(
                "{}: {top:.2} m/s, trail {trail:.3} m, below {:.1} mm",
                ["side", "up", "down"][dir as usize],
                below * 1000.0
            ));
        }
        println!(
            "RESULT {hz} Hz: k {k:.1}/s, reach at rest {:.1} mm; sag {:.3} mm; flick {}; floor 20 cm press {:.2} mm \
             deep; pinned {:.2} mm into the wall, back at {moved:.2} m/s ({top:.2} at a frame's end); overrun {}",
            10.0 / f64::from(k) * 1000.0,
            sag * 1000.0,
            flicks.join(", "),
            floor_in * 1000.0,
            deepest * 1000.0,
            overruns.join("; ")
        );
    }
}

// ---- meshes and hulls built from their blobs (world-model.md 3.2; CCD part 2 D) ------------------------------------

/// A mesh v1 blob (`PEM1`).
fn mesh_blob(vertices: &[[f32; 3]], triangles: &[[u32; 3]]) -> Vec<u8> {
    let mut b = b"PEM1".to_vec();
    b.extend_from_slice(&(vertices.len() as u32).to_le_bytes());
    vertices.iter().flatten().for_each(|c| b.extend_from_slice(&c.to_le_bytes()));
    b.extend_from_slice(&(triangles.len() as u32).to_le_bytes());
    triangles.iter().flatten().for_each(|i| b.extend_from_slice(&i.to_le_bytes()));
    b
}

/// A hull v1 blob (`PEH1`).
fn hull_blob(points: &[[f32; 3]]) -> Vec<u8> {
    let mut b = b"PEH1".to_vec();
    b.extend_from_slice(&(points.len() as u32).to_le_bytes());
    points.iter().flatten().for_each(|c| b.extend_from_slice(&c.to_le_bytes()));
    b
}

/// A wedge, closed and asymmetric: the corner at the origin and the points 2 m along x, 1 m along y and 1 m along z,
/// each face counter-clockwise from outside. Its slanted face is x/2 + y + z = 1.
const WEDGE: [[f32; 3]; 4] = [[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
const WEDGE_FACES: [[u32; 3]; 4] = [[0, 2, 1], [0, 1, 3], [0, 3, 2], [1, 2, 3]];

/// The wedge mirrored across x = 0 by its vertices, its faces turned to keep them facing out.
fn wedge_mirrored_by_hand() -> Vec<u8> {
    let v: Vec<[f32; 3]> = WEDGE.iter().map(|p| [-p[0], p[1], p[2]]).collect();
    let t: Vec<[u32; 3]> = WEDGE_FACES.iter().map(|f| [f[0], f[2], f[1]]).collect();
    mesh_blob(&v, &t)
}

/// A closed box mesh, 9 vertices and 14 triangles (288 bytes: a carried blob), whose +z face is a fan around a point
/// `inset` in from its edge (pe-physics's golden sliver blob): at 4e-6 every triangle is built, at 2e-7 one is too
/// thin and Box3D leaves it out, which opens the mesh.
fn sliver_box(inset: f32) -> Vec<u8> {
    let (c, h) = ([0.123_f32, -0.456, 0.789], [0.3_f32, 0.7, 0.11]);
    let mut v: Vec<[f32; 3]> = (0..8u32)
        .map(|k| {
            let s = |bit: u32| if k & bit != 0 { 1.0 } else { -1.0 };
            [c[0] + s(1) * h[0], c[1] + s(2) * h[1], c[2] + s(4) * h[2]]
        })
        .collect();
    let mut t = vec![[0, 2, 1], [1, 2, 3], [0, 1, 4], [1, 5, 4], [2, 6, 3], [3, 6, 7], [0, 4, 2], [2, 4, 6]];
    t.extend([[1, 3, 5], [3, 7, 5]]);
    v.push([c[0], c[1] - h[1] + inset, c[2] + h[2]]);
    t.extend([[4, 5, 8], [5, 7, 8], [7, 6, 8], [6, 4, 8]]);
    mesh_blob(&v, &t)
}

/// A mesh or a hull `Shape` naming `blob`.
fn blob_shape(kind: u32, blob: pe_world::components::BlobRef) -> Shape {
    Shape { kind, density: 500.0, size: [1.0; 3], mesh: blob, ..Shape::default() }
}

/// Genesis in a world that builds meshes and hulls, or doesn't.
fn genesis_with_blob_shapes(on: bool) -> World {
    let mut w = genesis();
    w.set_blob_shapes(on);
    w
}

/// `id` at the root, at `t`, with `body` and `shape`.
fn object(w: &mut World, id: u128, t: Transform, body: Body, shape: Shape) {
    w.create(oid(id), None, MemberKey([1; 32]), GENESIS).expect("create");
    w.insert(oid(id), t).expect("transform");
    w.insert(oid(id), body).expect("body");
    w.insert(oid(id), shape).expect("shape");
}

/// `id`, a compound child of `parent` at its local transform `t`, with only a shape.
fn child(w: &mut World, id: u128, parent: u128, t: Transform, shape: Shape) {
    w.create(oid(id), Some(oid(parent)), MemberKey([1; 32]), GENESIS).expect("create");
    w.insert(oid(id), t).expect("transform");
    w.insert(oid(id), shape).expect("shape");
}

fn static_body() -> Body {
    Body { friction: 0.6, ..Body::default() }
}

/// Off by default and in every preset (world-model.md 6.6, config item 8): a mesh or a hull is
/// `Unbuildable::Shape(SwitchOff)`, gets no body, and is still drawn. On, both are built, from their blobs.
#[test]
fn meshes_and_hulls_are_built_only_in_a_world_that_builds_them() {
    for on in [false, true] {
        let mut w = genesis_with_blob_shapes(on);
        let mesh = w.blobs().put(&mesh_blob(&WEDGE, &WEDGE_FACES));
        let hull = w.blobs().put(&hull_blob(&WEDGE));
        object(&mut w, 2, Transform::at(pos(-3.0, 0.0, 0.0)), static_body(), blob_shape(shape_kind::MESH, mesh));
        object(&mut w, 3, Transform::at(pos(3.0, 1.0, 0.0)), dynamic(), blob_shape(shape_kind::HULL, hull));
        let pw = PeWorld::new(w.clone(), DomainId(0));
        let drawn: Vec<ObjectId> = pw
            .view_objects()
            .iter()
            .filter(|o| matches!(o.shape, Some(pe_seams::Shape::Mesh(_))))
            .map(|o| o.id)
            .collect();
        assert_eq!(drawn, [oid(2), oid(3)], "drawn either way");
        let r = replica_of(w);
        assert_eq!(r.faults(), if on { 0 } else { 2 });
        if on {
            assert!(
                matches!(&pw.body(oid(2)).unwrap().unwrap().shapes[0].shape, ShapeRow::Mesh { scale, .. } if *scale == [1.0; 3])
            );
            assert!(
                matches!(&pw.body(oid(3)).unwrap().unwrap().shapes[0].shape, ShapeRow::Hull { points } if points[..] == WEDGE)
            );
            assert!(r.physics().has_body(oid(2)) && r.physics().has_body(oid(3)));
        } else {
            let off = Err(Unbuildable::Shape(ShapeReason::SwitchOff));
            assert_eq!((pw.body(oid(2)), pw.body(oid(3))), (off.clone(), off));
            assert!(!r.physics().has_body(oid(2)) && !r.physics().has_body(oid(3)), "nothing collides with them");
        }
    }
}

/// Where a 0.1 m ball dropped at (-0.5, 1.5, 0.2) is after 40 frames, over a static wedge at the origin made of
/// `shape` and scaled by `scale`.
fn ball_over(shape: impl Fn(&mut World) -> Shape, scale: [f32; 3]) -> ([f64; 3], ReplicaImpl<PeWorld>) {
    let mut w = genesis_with_blob_shapes(true);
    let shape = shape(&mut w);
    object(&mut w, 2, Transform::at(pos(0.0, 0.0, 0.0)).with_scale(scale), static_body(), shape);
    object(&mut w, BALL, Transform::at(pos(-0.5, 1.5, 0.2)), dynamic(), sphere(0.1));
    let mut r = replica_of(w);
    r.advance(Frame(40), &mut Source::default());
    (body_at(&r, BALL), r)
}

/// A negative scale axis mirrors a mesh and a hull (@coordinator-opus, point 1): built with the signed
/// scale, each is the shape mirrored by hand, where the unsigned scale would build it unmirrored with no fault. The
/// view draws it with the signed scale too.
#[test]
fn a_mirrored_mesh_and_hull_build_mirrored() {
    let mirror = [-1.0, 1.0, 1.0];
    let mesh = |w: &mut World| blob_shape(shape_kind::MESH, w.blobs().put(&mesh_blob(&WEDGE, &WEDGE_FACES)));
    let by_hand = |w: &mut World| blob_shape(shape_kind::MESH, w.blobs().put(&wedge_mirrored_by_hand()));
    let (mirrored, r) = ball_over(mesh, mirror);
    let (reference, _) = ball_over(by_hand, [1.0; 3]);
    let (unmirrored, _) = ball_over(mesh, [1.0; 3]);
    // The slope turns the ball's fall sideways; with nothing under it, it falls straight down to the floor.
    let aside = |p: [f64; 3]| ((p[0] + 0.5).powi(2) + (p[2] - 0.2).powi(2)).sqrt();
    assert!(aside(mirrored) > 0.1, "the ball met the mirrored wedge's slope: {mirrored:?}");
    assert!(mirrored.iter().zip(&reference).all(|(a, b)| (a - b).abs() < 1e-3), "{mirrored:?} {reference:?}");
    assert!(aside(unmirrored) < 1e-3 && unmirrored[1] < 0.11, "nothing under it: {unmirrored:?}");
    assert_eq!(r.faults(), 0);
    let row = r.world().body(oid(2)).unwrap().unwrap();
    assert!(matches!(&row.shapes[0].shape, ShapeRow::Mesh { scale, .. } if *scale == mirror));
    let drawn = r.world().view_objects().into_iter().find(|o| o.id == oid(2)).expect("drawn");
    assert_eq!(drawn.scale, mirror, "the view's scale is signed");

    // A hull: the blob's points scaled with their signs are exactly the points mirrored by hand.
    let hull = |w: &mut World| blob_shape(shape_kind::HULL, w.blobs().put(&hull_blob(&WEDGE)));
    let flipped: Vec<[f32; 3]> = WEDGE.iter().map(|p| [-p[0], p[1], p[2]]).collect();
    let hull_by_hand = |w: &mut World| blob_shape(shape_kind::HULL, w.blobs().put(&hull_blob(&flipped)));
    let (mirrored, r) = ball_over(hull, mirror);
    let (reference, _) = ball_over(hull_by_hand, [1.0; 3]);
    assert!(aside(mirrored) > 0.1, "the ball met the mirrored hull: {mirrored:?}");
    assert_eq!(mirrored, reference, "the same points, so the same steps");
    let row = r.world().body(oid(2)).unwrap().unwrap();
    assert!(matches!(&row.shapes[0].shape, ShapeRow::Hull { points } if *points == flipped));

    // A dynamic mirrored closed mesh keeps its mass: mirroring reads its winding back.
    let mut w = genesis_with_blob_shapes(true);
    let shape = mesh(&mut w);
    object(&mut w, 2, Transform::at(pos(0.0, 2.0, 0.0)).with_scale(mirror), dynamic(), shape);
    let r = replica_of(w);
    assert!(r.physics().has_body(oid(2)) && r.faults() == 0);
}

/// v1's placement limit (world-model.md 3.2): a mesh is built where its composed place is exactly its body's, the
/// position as the build uses it all ±0 and the rotation's x, y and z all ±0 with w ±1, so a child at the body's
/// origin with the rotation (0, 0, 0, -1) passes. A mesh child offset or turned makes the whole body
/// `MisplacedMesh`. A hull has no such limit: an offset, turned child hull is placed point by point.
#[test]
fn a_mesh_must_sit_at_its_body_s_origin_unturned() {
    let quarter = [0.0, std::f32::consts::FRAC_1_SQRT_2, 0.0, std::f32::consts::FRAC_1_SQRT_2];
    let at = |local: Transform| {
        let mut w = genesis_with_blob_shapes(true);
        let mesh = w.blobs().put(&mesh_blob(&WEDGE, &WEDGE_FACES));
        object(&mut w, 2, Transform::at(pos(0.0, 1.0, 0.0)).with_rot(quarter), static_body(), cuboid([0.1; 3]));
        child(&mut w, 3, 2, local, blob_shape(shape_kind::MESH, mesh));
        PeWorld::new(w, DomainId(0)).body(oid(2))
    };
    let misplaced = Err(Unbuildable::Shape(ShapeReason::MisplacedMesh));
    assert!(at(Transform::local([0.0; 3])).is_ok_and(|b| b.is_some()), "at the origin, in a turned body");
    assert!(at(Transform::local([0.0; 3]).with_rot([0.0, 0.0, 0.0, -1.0])).is_ok(), "w = -1 is no turn");
    assert!(at(Transform::local([-0.0, 0.0, -0.0]).with_rot([-0.0, 0.0, 0.0, 1.0])).is_ok(), "-0 is 0");
    assert_eq!(at(Transform::local([0.5, 0.0, 0.0])), misplaced, "offset");
    assert_eq!(at(Transform::local([0.0; 3]).with_rot(quarter)), misplaced, "turned");
    assert_eq!(
        Unbuildable::Shape(ShapeReason::MisplacedMesh).to_string(),
        "a mesh child must sit at its body's origin, unturned: snap it there, or make the mesh the body's own shape"
    );

    // A hull child, 1 m along the body's x and turned a quarter about y: (2, 0, 0) goes to (1, 0, -2).
    let mut w = genesis_with_blob_shapes(true);
    let hull = w.blobs().put(&hull_blob(&WEDGE));
    object(&mut w, 2, Transform::at(pos(0.0, 1.0, 0.0)), static_body(), cuboid([0.1; 3]));
    child(&mut w, 3, 2, Transform::local([1.0, 0.0, 0.0]).with_rot(quarter), blob_shape(shape_kind::HULL, hull));
    let row = PeWorld::new(w, DomainId(0)).body(oid(2)).unwrap().expect("built");
    let ShapeRow::Hull { points } = &row.shapes[1].shape else { panic!("a hull: {:?}", row.shapes[1]) };
    assert!(close(points[1], [1.0, 0.0, -2.0]), "{points:?}");
}

/// Box3D's floor on scale (world-model.md 3.2): any axis of a mesh's or a hull's world scale, parent scales
/// included, under 1e-6 in magnitude refuses the body, naming the axis; 1e-6 itself builds, and so does a miniature's
/// 0.005, which the floor of 0.01 refused until E2 (@coordinator-opus).
#[test]
fn a_scale_axis_under_the_floor_refuses_a_mesh_or_a_hull() {
    let body_at = |kind: u32, parent: [f32; 3], own: [f32; 3]| {
        let mut w = genesis_with_blob_shapes(true);
        let blob = if kind == shape_kind::MESH { mesh_blob(&WEDGE, &WEDGE_FACES) } else { hull_blob(&WEDGE) };
        let blob = w.blobs().put(&blob);
        w.create(oid(9), None, MemberKey([1; 32]), GENESIS).expect("parent");
        w.insert(oid(9), Transform::at(pos(0.0, 1.0, 0.0)).with_scale(parent)).expect("parent");
        w.create(oid(2), Some(oid(9)), MemberKey([1; 32]), GENESIS).expect("create");
        w.insert(oid(2), Transform::local([0.0; 3]).with_scale(own)).expect("transform");
        w.insert(oid(2), static_body()).expect("body");
        w.insert(oid(2), blob_shape(kind, blob)).expect("shape");
        PeWorld::new(w, DomainId(0)).body(oid(2))
    };
    for kind in [shape_kind::MESH, shape_kind::HULL] {
        let floor = |axis| Err(Unbuildable::Shape(ShapeReason::ScaleFloor { axis }));
        assert_eq!(body_at(kind, [1.0; 3], [1.0, 0.5e-6, 1.0]), floor(1));
        assert_eq!(body_at(kind, [1.0; 3], [-0.99e-6, 1.0, 1.0]), floor(0), "in magnitude");
        assert_eq!(
            body_at(kind, [1.0, 1.0, 0.5e-5], [1.0, 1.0, 0.1]),
            floor(2),
            "the world scale, the parent's included"
        );
        assert!(body_at(kind, [1.0; 3], [1.0e-6, 1.0, -0.01]).is_ok_and(|b| b.is_some()), "1e-6 builds");
        assert!(body_at(kind, [0.005; 3], [1.0; 3]).is_ok_and(|b| b.is_some()), "a 1/200 miniature builds");
    }
    let why = Unbuildable::Shape(ShapeReason::ScaleFloor { axis: 2 }).to_string();
    assert_eq!(
        why,
        "the shape's world scale z is under 1e-6 in magnitude, Box3D's floor on scale: rescale the mesh at import"
    );
}

/// A hull its scale leaves unbuildable says why (@coordinator-opus, R3; CCD part 2 E): the wedge scaled
/// past the size limit is `Unbuildable::Shape(ScaledHull)`, where before `add_body` refused it as a fault with no
/// reason. A hull scaled 1 or -1 on every axis isn't built again to check it.
#[test]
fn a_hull_its_scale_leaves_unbuildable_names_the_reason() {
    let body_at = |scale: [f32; 3]| {
        let mut w = genesis_with_blob_shapes(true);
        let blob = w.blobs().put(&hull_blob(&WEDGE));
        let t = Transform::at(pos(0.0, 1.0, 0.0)).with_scale(scale);
        object(&mut w, 2, t, static_body(), blob_shape(shape_kind::HULL, blob));
        PeWorld::new(w, DomainId(0)).body(oid(2))
    };
    assert_eq!(body_at([6000.0, 1.0, 1.0]), Err(Unbuildable::Shape(ShapeReason::ScaledHull)), "12 km long");
    assert!(body_at([4000.0, 1.0, 1.0]).is_ok_and(|b| b.is_some()), "8 km long, within the size limit");
    assert!(body_at([-1.0, 1.0, 1.0]).is_ok_and(|b| b.is_some()), "mirrored");
    assert_eq!(
        Unbuildable::Shape(ShapeReason::ScaledHull).to_string(),
        "the hull's scale flattens it too far to build, or takes it past the size limit: rescale the hull at import"
    );
}

/// A shape that names no blob, or a blob that can't be read or built, is the same fault on every machine
/// (`NoBlob`, `BadBlob`). A blob this machine's store lacks is a local condition (`BlobAbsent`): the replica counts it
/// apart from its faults, with the frame, the body and the blob, and a reason every machine finds for another shape
/// of the body comes first.
#[test]
fn bad_and_absent_blobs_say_why() {
    let mut w = genesis_with_blob_shapes(true);
    let blobs = w.blobs().clone();
    let reason = |w: &World, kind: u32, r: pe_world::components::BlobRef| {
        let mut w = w.clone();
        object(&mut w, 2, Transform::at(pos(0.0, 1.0, 0.0)), static_body(), blob_shape(kind, r));
        PeWorld::new(w, DomainId(0)).body(oid(2))
    };
    let bad = Err(Unbuildable::Shape(ShapeReason::BadBlob));
    let none = pe_world::components::BlobRef::NONE;
    assert_eq!(reason(&w, shape_kind::MESH, none), Err(Unbuildable::Shape(ShapeReason::NoBlob)));
    assert_eq!(reason(&w, shape_kind::HULL, none), Err(Unbuildable::Shape(ShapeReason::NoBlob)));
    let mut magic = mesh_blob(&WEDGE, &WEDGE_FACES);
    magic[3] = b'2';
    assert_eq!(reason(&w, shape_kind::MESH, blobs.put(&magic)), bad, "an unknown version");
    assert_eq!(reason(&w, shape_kind::HULL, blobs.put(&mesh_blob(&WEDGE, &WEDGE_FACES))), bad, "a mesh read as a hull");
    let flat = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 1.0], [0.5, 0.0, 0.5]];
    assert_eq!(reason(&w, shape_kind::HULL, blobs.put(&hull_blob(&flat))), bad, "points the hull builder can't use");
    assert_eq!(reason(&w, shape_kind::HULL, blobs.put(&hull_blob(&WEDGE[..3]))), bad, "three points");
    assert_eq!(reason(&w, shape_kind::MESH, blobs.put(&mesh_blob(&WEDGE, &[[0, 1, 4]]))), bad, "an index past the end");

    // Absent: the reference, never the bytes.
    let lacking = pe_world::components::BlobRef::of(&sliver_box(4.0e-6));
    let absent = Err(Unbuildable::BlobAbsent { blob: Hash(lacking.hash) });
    assert_eq!(reason(&w, shape_kind::MESH, lacking), absent);
    object(&mut w, 2, Transform::at(pos(0.0, 1.0, 0.0)), static_body(), blob_shape(shape_kind::MESH, lacking));
    let pw = PeWorld::new(w.clone(), DomainId(0));
    assert_eq!(pw.body(oid(2)), absent);
    let r = replica_of(w.clone());
    assert_eq!(r.faults(), 0, "not a fault");
    assert_eq!(r.absent_blobs().count, 1);
    assert_eq!(r.absent_blobs().recent, [(Frame(0), oid(2), Hash(lacking.hash))]);
    assert!(!r.physics().has_body(oid(2)));
    // A child mesh off the body's origin is every machine's reason, so it comes first.
    let mesh = blobs.put(&mesh_blob(&WEDGE, &WEDGE_FACES));
    child(&mut w, 3, 2, Transform::local([0.5, 0.0, 0.0]), blob_shape(shape_kind::MESH, mesh));
    assert_eq!(PeWorld::new(w, DomainId(0)).body(oid(2)), Err(Unbuildable::Shape(ShapeReason::MisplacedMesh)));
}

/// A dynamic body with a mesh whose shapes together have no mass can't be built (world-model.md 3.2,
/// `Unbuildable::Mass`): an open mesh, or a mesh whose too-thin triangles Box3D left out, with their count. An open
/// mesh with a sphere beside it builds, and a static open mesh builds. A replica counts such a body as a fault at its
/// start, and starts.
#[test]
fn a_dynamic_mesh_body_without_mass_says_why() {
    let with = |body: Body, blob: Vec<u8>, sphere_child: bool| {
        let mut w = genesis_with_blob_shapes(true);
        let r = w.blobs().put(&blob);
        object(&mut w, 2, Transform::at(pos(0.0, 2.0, 0.0)), body, blob_shape(shape_kind::MESH, r));
        if sphere_child {
            child(&mut w, 3, 2, Transform::local([0.5, 0.5, 0.5]), sphere(0.1));
        }
        w
    };
    let open = mesh_blob(&WEDGE, &WEDGE_FACES[..3]);
    let body = |w: World| PeWorld::new(w, DomainId(0)).body(oid(2));
    assert_eq!(body(with(dynamic(), open.clone(), false)), Err(Unbuildable::Mass { dropped: 0 }));
    assert_eq!(body(with(dynamic(), sliver_box(2.0e-7), false)), Err(Unbuildable::Mass { dropped: 1 }), "one sliver");
    assert!(body(with(dynamic(), sliver_box(4.0e-6), false)).is_ok_and(|b| b.is_some()), "kept, so closed");
    assert!(body(with(dynamic(), open.clone(), true)).is_ok_and(|b| b.is_some()), "the sphere has mass");
    assert!(body(with(static_body(), open.clone(), false)).is_ok_and(|b| b.is_some()), "a static body takes none");
    let why = Unbuildable::Mass { dropped: 1 }.to_string();
    assert_eq!(
        why,
        "1 of the mesh's triangles are under the minimum area and were left out; without them the mesh is open"
    );
    let r = replica_of(with(dynamic(), open, false));
    assert_eq!((r.faults(), r.physics().has_body(oid(2))), (1, false));
}

/// @coordinator-opus, point 5: a joiner starts from a bare world, so with the switch off and an empty
/// blob store, and imports at 32 a world that builds meshes, with the blobs its rows name (a 288-byte carried one,
/// and an inline one): its world builds them from then on. At 40 an edit sets the dynamic mesh body's scale, which
/// rebuilds it: the host from the mesh it already built, the joiner from the blob, which it never built before (its
/// physics came in the transfer). The edit reaches the joiner late, after it stepped 44, so it also rolls back across
/// the rebuild. Their roots agree from 32 on, and neither counts a fault or an absent blob.
#[test]
fn a_joiner_with_the_switch_off_imports_meshes_and_rebuilds_one_after_a_scale_edit() {
    let mut world = genesis_with_blob_shapes(true);
    let wedge = world.blobs().put(&mesh_blob(&WEDGE, &WEDGE_FACES));
    let carried = world.blobs().put(&sliver_box(4.0e-6));
    object(&mut world, 2, Transform::at(pos(-1.0, 0.0, -0.5)), static_body(), blob_shape(shape_kind::MESH, wedge));
    object(&mut world, 3, Transform::at(pos(1.5, 1.5, 0.0)), dynamic(), blob_shape(shape_kind::MESH, carried));
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    // Two balls dropped onto the wedge, then the scale edit.
    let drop = |x: f64, y: f64| {
        let rows = vec![row(Transform::at(pos(x, y, 0.0))), row(dynamic()), row(sphere(0.25))];
        vec![Op::Create { new: 0, parent: Target::Root, index: None, rows }]
    };
    let scale = Op::SetField {
        id: Target::Existing(oid(3)),
        component: <Transform as pe_sim::Component>::ID,
        instance: 0,
        field: 3,
        value: [1.5_f32; 3].iter().flat_map(|s| s.to_le_bytes()).collect(),
    };
    let entries =
        vec![edit_at(&mut b, 2, drop(0.0, 1.0)), edit_at(&mut b, 5, drop(0.3, 2.0)), edit_at(&mut b, 40, vec![scale])];
    let late = entries[2].clone();
    let (mut host, mut hs) = reference_of(world, &entries, 32);
    let c32 = cut_at(&entries, 4, 32);
    let mut import = full_import(&mut host, &mut hs, &c32);
    let keys_all: Vec<LeafKey> = import.records.iter().map(|r| r.key).collect();
    import.blobs = host.tree(&import.target_root).expect("held").export_blobs(&keys_all);
    assert_eq!(import.blobs.len(), 2, "the wedge's and the box's");

    let bare = World::new(0, BlobStore::default());
    let mut joiner = ReplicaImpl::new(PeWorld::new(bare, DomainId(0)), config()).expect("a replica");
    assert!(!joiner.world().world().settings().blob_shapes, "a bare world builds no mesh");
    let mut s = Source::default();
    entries[..2].iter().cloned().for_each(|e| s.add(e));
    s.take_changed_from();
    assert_eq!(joiner.import_at_cut(&GroupSel::Domain, import.clone(), &mut s), Ok(import.target_root));
    assert!(joiner.world().world().settings().blob_shapes, "the imported world's setting");
    let fc32 = FinalCut { cut: c32, root: import.target_root, voters: vec![] };
    assert_eq!(joiner.finalize(&fc32, &mut s), Finalized::Agreed);
    joiner.advance(Frame(44), &mut s);
    s.add(late);
    let adv = joiner.advance(Frame(64), &mut s);
    assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= 40), "the late edit rolls back: {adv:?}");
    host.advance(Frame(64), &mut hs);
    assert!(
        matches!(&host.world().body(oid(3)).unwrap().unwrap().shapes[0].shape, ShapeRow::Mesh { scale, .. } if *scale == [1.5; 3])
    );
    assert_eq!(from(&mut joiner, 32, 64), from(&mut host, 32, 64));
    assert_eq!((joiner.faults(), host.faults()), (0, 0));
    assert_eq!((joiner.absent_blobs().count, host.absent_blobs().count), (0, 0));
}

/// A body the world model builds but the physics refuses (here a mesh scaled past pe-physics's size limit, which only
/// `add_body` checks) is a fault at the replica's start, as it is in a step, and the replica still starts.
#[test]
fn a_body_the_physics_refuses_is_a_fault_at_the_start() {
    let mut w = genesis_with_blob_shapes(true);
    let mesh = w.blobs().put(&mesh_blob(&WEDGE, &WEDGE_FACES));
    let huge = Transform::at(pos(0.0, 0.0, 0.0)).with_scale([6000.0; 3]);
    object(&mut w, 2, huge, static_body(), blob_shape(shape_kind::MESH, mesh));
    assert!(PeWorld::new(w.clone(), DomainId(0)).body(oid(2)).is_ok_and(|b| b.is_some()), "the world model builds it");
    let r = replica_of(w);
    assert_eq!((r.faults(), r.physics().has_body(oid(2))), (1, false));
}

/// The friend test's fast throw (11-26; @item:primordial-engine/friend-test-11-27-hold-and-release-unfiltered-th):
/// a pit ball grabbed, lifted 0.97 m and held still, then swung at `v` m/s (speeding up over 9 frames, steady for 6)
/// along a straight line or an arc of radius 0.6 m (an arm's swing), and thrown with the tracked hand's velocity at
/// the opening. Prints, per case, the ball's gap from its place in the hand the frame before the opening and its
/// velocity a frame after the throw applies, against the throw's.
#[test]
fn a_fast_swing_s_throw_leaves_at_the_hand_s_speed() {
    use pe_world::input::MAX_POSES;
    for radius in [None, Some(0.6_f64)] {
        for v in [8.0, 12.0, 15.0, 20.0, 25.0, 40.0] {
            let keys = keys();
            let mut b = EntryBuilder::new(&keys, 0);
            let mut s = Source::default();
            let hr = f64::from(pe_replica::world::HAND_RADIUS);
            let start = [2.0 - 0.03 - hr, 0.03, 0.0];
            let mut path: Vec<[f64; 3]> =
                (1..=30).map(|k| [start[0], 0.03 + 0.97 * f64::from(k) / 30.0, 0.0]).collect();
            path.extend(std::iter::repeat_n([start[0], 1.0, 0.0], 30));
            let top = [start[0], 1.0, 0.0];
            // Arc length s along the swing, to a point and the swing's direction there.
            let place = |s: f64| -> ([f64; 3], [f64; 3]) {
                match radius {
                    None => ([top[0] + s, top[1], top[2]], [1.0, 0.0, 0.0]),
                    Some(r) => {
                        let th = s / r;
                        ([top[0] + r * th.sin(), top[1], top[2] + r * (1.0 - th.cos())], [th.cos(), 0.0, th.sin()])
                    }
                }
            };
            let (speeding, steady) = (9_usize, 6_usize);
            let mut arc = 0.0;
            for k in 1..=speeding + steady {
                arc += v * (k.min(speeding) as f64 / speeding as f64) / 60.0;
                path.push(place(arc).0);
            }
            let open = path.len();
            let dir = place(arc).1;
            for _ in 0..12 {
                arc += v / 60.0;
                path.push(place(arc).0);
            }
            s.add(input(&mut b, 2, &hand_at(&[start; 8], true)));
            s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
            for (i, pack) in path[..open].chunks(MAX_POSES).enumerate() {
                s.add(input(&mut b, 10 + (i * MAX_POSES) as u64, &hand_at(pack, true)));
            }
            let release = 10 + open as u64;
            let linear = dir.map(|c| (c * v) as f32);
            s.add(input(&mut b, release, &hand_at(&path[open..], false)));
            s.add(input(&mut b, release, &Input::Throw { object: oid(BALL), linear, angular: [0.0; 3] }));
            let mut r = replica_of(with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.0, 0.03, 0.0], pit_ball())]));
            r.advance(Frame(release), &mut s);
            let gap = pe_replica::hands::hold_gap(r.physics(), oid(BALL), oid(HAND));
            let trail = distance(r.physics().pose(oid(HAND)).expect("the hand's body").position, at(&r, HAND));
            r.advance(Frame(release + 2), &mut s);
            let left = r.physics().velocity(oid(BALL)).expect("the ball's body").0.map(f64::from);
            let speed = left.iter().map(|c| c * c).sum::<f64>().sqrt();
            let along = (0..3).map(|i| left[i] * dir[i]).sum::<f64>();
            let shape = radius.map_or("straight".to_string(), |r| format!("arc r {r} m"));
            println!(
                "RESULT swing {shape}, {v} m/s: hand body trails its target by {trail:.3} m, ball {} from its place \
                 before the open; leaves at {speed:.2} m/s ({along:.2} along the throw's {v} m/s), velocity {:?}",
                gap.map_or("unheld".to_string(), |g| format!("{:.1} mm", g * 1000.0)),
                left.map(|c| (c * 100.0).round() / 100.0)
            );
            // The hand is never placed (a 40 m/s swing included, @fable-reviewer), and the ball leaves
            // at the hand's speed from within millimetres of its place.
            assert!(trail < 1e-6, "{shape}, {v} m/s: the hand's body is on its target: {trail}");
            assert!(gap.is_some_and(|g| g < 0.005), "{shape}, {v} m/s: held within 5 mm: {gap:?}");
            assert!((along - v).abs() < 0.01 * v, "{shape}, {v} m/s: leaves at {along}");
            assert_eq!(r.faults(), 0);
        }
    }
}

/// The hold at the grip's strength alone (the user's ruling of 2026-11-26): a pit ball held in a hand
/// that presses it into a fixed surface. Run along `path` (one hand point a frame after the lift, the ball grabbed on
/// the floor at x = 2 and lifted to `path[0]`), with an optional static wall (`wall`: its centre and half extents).
/// Returns the ball's deepest overlap with the floor (its radius less its height), the farthest x its centre reached,
/// and where it ends.
fn press(path: &[[f64; 3]], wall: Option<([f64; 3], [f32; 3])>) -> (f64, f64, [f64; 3]) {
    use pe_world::input::MAX_POSES;
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut s = Source::default();
    let hr = f64::from(pe_replica::world::HAND_RADIUS);
    let start = [2.0 - 0.03 - hr, 0.03, 0.0];
    let first = path[0];
    let mut all: Vec<[f64; 3]> = (1..=30)
        .map(|k| {
            let f = f64::from(k) / 30.0;
            [0usize, 1, 2].map(|i| start[i] + (first[i] - start[i]) * f)
        })
        .collect();
    all.extend(std::iter::repeat_n(first, 20));
    all.extend_from_slice(path);
    all.extend(std::iter::repeat_n(*path.last().expect("a path"), 60));
    s.add(input(&mut b, 2, &hand_at(&[start; 8], true)));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    for (i, pack) in all.chunks(MAX_POSES).enumerate() {
        s.add(input(&mut b, 10 + (i * MAX_POSES) as u64, &hand_at(pack, true)));
    }
    let mut world = with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.0, 0.03, 0.0], pit_ball())]);
    if let Some((c, half)) = wall {
        let k = keys.key(0);
        world.create(oid(70), None, k, GENESIS).expect("wall");
        world.insert(oid(70), Transform::at(pos(c[0], c[1], c[2]))).expect("wall transform");
        world.insert(oid(70), Body { friction: 0.6, ..Body::default() }).expect("wall body");
        world.insert(oid(70), cuboid(half)).expect("wall shape");
    }
    let mut r = replica_of(world);
    r.advance(Frame(10 + 50), &mut s);
    assert_eq!(held_by(&r, BALL), Some(oid(HAND)), "held after the lift");
    let (mut deepest, mut farthest) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for f in 10 + 50..10 + all.len() as u64 {
        r.advance(Frame(f + 1), &mut s);
        let p = at(&r, BALL);
        deepest = deepest.max(0.03 - p[1]);
        farthest = farthest.max(p[0]);
    }
    assert_eq!(r.faults(), 0);
    (deepest, farthest, at(&r, BALL))
}

/// A path from `a` to `b` at `v` m/s, one point a frame at 60 Hz.
fn line(a: [f64; 3], b: [f64; 3], v: f64) -> Vec<[f64; 3]> {
    let n = ((distance(a, b) / (v / 60.0)).ceil() as usize).max(1);
    (1..=n)
        .map(|k| {
            let f = k as f64 / n as f64;
            [a[0] + f * (b[0] - a[0]), a[1] + f * (b[1] - a[1]), a[2] + f * (b[2] - a[2])]
        })
        .collect()
}

/// How far a held pit ball is pushed into the floor and through a 2 cm wall when the hand goes into or through them
/// (the user's rulings of 2026-11-26). Prints; asserts only that nothing faulted.
#[test]
fn measure_push_through_at_the_grip_s_strength() {
    let x = 2.0 - 0.03 - f64::from(pe_replica::world::HAND_RADIUS);
    for (depth, v) in [(0.1, 1.0), (0.3, 1.0), (0.3, 5.0)] {
        let path = line([x, 0.3, 0.0], [x, -depth, 0.0], v);
        let (deepest, _, end) = press(&path, None);
        println!(
            "RESULT floor: hand {depth} m under the floor's top at {v} m/s: ball sinks {:.1} mm at most, ends at y {:.3} \
             ({})",
            deepest * 1000.0,
            end[1],
            if end[1] < -0.03 { "UNDER the floor's top" } else { "above it" }
        );
    }
    // A wall 2 cm thick across x = 2.5, from the floor to 2 m.
    let wall = ([2.5, 1.0, 0.0], [0.01, 1.0, 1.0]);
    for v in [1.0, 5.0, 15.0] {
        let path = line([2.2, 1.0, 0.0], [3.0, 1.0, 0.0], v);
        let (_, farthest, end) = press(&path, Some(wall));
        println!(
            "RESULT wall: hand through a 2 cm wall at {v} m/s: ball reaches x {:.3} (the wall's near face 2.490), ends \
             at x {:.3} ({})",
            farthest + 0.03,
            end[0],
            if end[0] > 2.51 { "THROUGH the wall" } else { "this side" }
        );
    }
}

/// A hand input for `hand` with one pose per (point, rotation), closed or open.
fn hand_turned(hand: u128, poses: &[([f64; 3], [f32; 4])], closed: bool) -> Input {
    let pose = |(p, rot): &([f64; 3], [f32; 4])| HandPose {
        pos: pos(p[0], p[1], p[2]),
        rot: *rot,
        grip: if closed { 1.0 } else { 0.0 },
        engaged: closed,
    };
    Input::Hand { hand: oid(hand), poses: poses.iter().map(pose).collect(), hold: 0 }
}

/// The friend test's twist (11-26 point 4: "when i twist my hand quickly while holding an object, it
/// would often arc outward before returning to my hand's position"): a pit ball grabbed beside the hand and lifted to
/// 1 m, then the hand turns about the vertical through its centre at `w` rad/s (speeding up over 3 frames, steady for
/// 12, slowing over 3) and holds still. Prints the ball's largest distance from its place in the hand and how much
/// farther from the hand's centre it got than its grip (the arc outward).
#[test]
fn a_fast_twist_keeps_the_ball_in_the_hand() {
    use pe_world::input::MAX_POSES;
    let hr = f64::from(pe_replica::world::HAND_RADIUS);
    for w in [10.0_f64, 20.0, 30.0, 45.0, 60.0] {
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let mut s = Source::default();
        let start = [2.0 - 0.03 - hr, 0.03, 0.0];
        let top = [start[0], 1.0, 0.0];
        let mut poses: Vec<([f64; 3], [f32; 4])> =
            (1..=30).map(|k| ([start[0], 0.03 + 0.97 * f64::from(k) / 30.0, 0.0], UNTURNED)).collect();
        poses.extend(std::iter::repeat_n((top, UNTURNED), 20));
        let mut angle = 0.0_f64;
        let rate = |k: usize| match k {
            0..3 => w * (k + 1) as f64 / 3.0,
            3..15 => w,
            15..18 => w * (17 - k) as f64 / 3.0,
            _ => 0.0,
        };
        for k in 0..30 {
            angle += rate(k) / 60.0;
            let h = (angle / 2.0) as f32;
            poses.push((top, [0.0, h.sin(), 0.0, h.cos()]));
        }
        s.add(input(&mut b, 2, &hand_at(&[start; 8], true)));
        s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
        for (i, pack) in poses.chunks(MAX_POSES).enumerate() {
            s.add(input(&mut b, 10 + (i * MAX_POSES) as u64, &hand_turned(HAND, pack, true)));
        }
        let mut r = replica_of(with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.0, 0.03, 0.0], pit_ball())]));
        r.advance(Frame(10 + 50), &mut s);
        let hand = |r: &ReplicaImpl<PeWorld>| r.physics().pose(oid(HAND)).expect("the hand's body").position;
        let ball = |r: &ReplicaImpl<PeWorld>| r.physics().pose(oid(BALL)).expect("the ball's body").position;
        let grip = distance(hand(&r), ball(&r));
        let (mut gap, mut out) = (0.0_f64, 0.0_f64);
        for f in 10 + 50..10 + poses.len() as u64 + 20 {
            r.advance(Frame(f + 1), &mut s);
            gap = gap.max(pe_replica::hands::hold_gap(r.physics(), oid(BALL), oid(HAND)).unwrap_or(f64::NAN));
            out = out.max(distance(hand(&r), ball(&r)) - grip);
        }
        println!(
            "RESULT twist {w} rad/s (ball {:.0} mm from the hand's centre): largest gap from its place {:.1} mm, arc \
             outward {:.1} mm (turned {angle:.2} rad)",
            grip * 1000.0,
            gap * 1000.0,
            out * 1000.0
        );
        // Box3D turns the hand's body at most an eighth of a turn a frame (47.1 rad/s), so at 60 rad/s it lags its
        // target and the ball with it; the ball still stays in the hand. On main c7cbc175 it fell 414 mm from its
        // place at 45 rad/s (the 200 m/s^2 cap on the hold).
        assert!(gap < 0.02, "{w} rad/s: the ball stays within 2 cm of its place: {gap}");
        assert_eq!(r.faults(), 0);
    }
}

/// The friend test's two-hand hold (11-26: "when i grab an object with both hands it behaves strangely
/// and jumps when i let go with both at the same time"): a pit ball between the member's two hands, grabbed by the
/// right then the left (the left's renewing grab takes the claim, as pe-app sends it), both lifted 0.5 m together and
/// held still, then both open in one frame, the left with a still throw (pe-app's: the right has given the ball up).
/// Prints the ball's fastest speed while held still and its speed after the opening.
#[test]
fn measure_a_two_hand_hold_and_release() {
    use pe_world::input::MAX_POSES;
    const LEFT: u128 = 51;
    let hr = f64::from(pe_replica::world::HAND_RADIUS);
    // (the ball's radius, its shape, how far each hand's centre is from the ball's, the frames the right stays closed
    // after the left opens)
    let pit = (0.03, pit_ball());
    let big = (0.1, sphere(0.1));
    for ((r, shape), apart, later) in
        [(pit, 0.03 + hr, 0), (pit, 0.03 + hr, 3), (big, 0.05, 0), (big, 0.05, 3), (big, 0.05, 10)]
    {
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let mut s = Source::default();
        let right = [2.0 - apart, r, 0.0];
        let left = [2.0 + apart, r, 0.0];
        let up = |p: [f64; 3], k: u32| [p[0], p[1] + 0.5 * f64::from(k.min(30)) / 30.0, p[2]];
        let path = |p: [f64; 3]| (1..=90).map(|k| up(p, k)).collect::<Vec<_>>();
        s.add(input(&mut b, 2, &hand_of(HAND, &[right; 8], UNTURNED, true)));
        s.add(input(&mut b, 2, &hand_of(LEFT, &[left; 8], UNTURNED, true)));
        s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
        s.add(input(&mut b, 8, &Input::Grab { object: oid(BALL), hand: Some(oid(LEFT)) }));
        for (i, (pr, pl)) in path(right).chunks(MAX_POSES).zip(path(left).chunks(MAX_POSES)).enumerate() {
            s.add(input(&mut b, 10 + (i * MAX_POSES) as u64, &hand_of(HAND, pr, UNTURNED, true)));
            s.add(input(&mut b, 10 + (i * MAX_POSES) as u64, &hand_of(LEFT, pl, UNTURNED, true)));
        }
        let open = 100;
        s.add(input(&mut b, open, &hand_of(LEFT, &[up(left, 30); 16], UNTURNED, false)));
        s.add(input(&mut b, open, &Input::Throw { object: oid(BALL), linear: [0.0; 3], angular: [0.0; 3] }));
        s.add(input(&mut b, open + later, &hand_of(HAND, &[up(right, 30); 16], UNTURNED, false)));
        let world = with_hands_and_balls(&[(HAND, right), (LEFT, left)], &[(BALL, [2.0, r, 0.0], shape)]);
        let mut rep = replica_of(world);
        rep.advance(Frame(50), &mut s);
        let held = held_by(&rep, BALL) == Some(oid(LEFT));
        let speed = |rep: &ReplicaImpl<PeWorld>| {
            let v = rep.physics().velocity(oid(BALL)).expect("the ball's body").0;
            f64::from((v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt())
        };
        let mut still = 0.0_f64;
        let at_50 = at(&rep, BALL);
        for f in 50..open {
            rep.advance(Frame(f + 1), &mut s);
            still = still.max(speed(&rep));
        }
        let moved = distance(at_50, at(&rep, BALL));
        let mut after = 0.0_f64;
        for f in open..open + later + 3 {
            rep.advance(Frame(f + 1), &mut s);
            after = after.max(speed(&rep));
        }
        println!(
            "RESULT two hands, ball r {r} m, hands {apart:.3} m from its centre, right opens {later} frames after the left: held by the left {held}; fastest while held still {still:.2} m/s (moved {:.1} mm); fastest after the opening {after:.2} m/s (a fall alone: {:.2})",
            moved * 1000.0,
            9.81 * (later + 3) as f64 / 60.0
        );
        assert_eq!(rep.faults(), 0);
    }
}

/// A small held object pushes large movable ones at the whole grip (the user's correction of 2026-11-26: "push large objects while holding another small object"): a pit ball held at 0.3 m is carried at
/// 1 m/s into a 1 m crate on the floor, of 10 kg and of 300 kg. Only fixed surfaces lower the hold's force; a crate
/// never does, so the hold keeps the holder's 110 N against it and the ball stays in the hand. Both crates are pushed
/// along: the hand's own body is kinematic and pushes them too, whatever their mass.
#[test]
fn a_small_held_ball_pushes_a_large_crate_with_the_whole_grip() {
    use pe_world::input::MAX_POSES;
    const CRATE: u128 = 80;
    for kg in [10.0_f32, 300.0] {
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let mut s = Source::default();
        let hr = f64::from(pe_replica::world::HAND_RADIUS);
        let start = [2.0 - 0.03 - hr, 0.03, 0.0];
        let mut path: Vec<[f64; 3]> = (1..=20).map(|k| [start[0], 0.03 + 0.27 * f64::from(k) / 20.0, 0.0]).collect();
        path.extend((1..=90).map(|k| [start[0] + f64::from(k) / 60.0, 0.3, 0.0]));
        s.add(input(&mut b, 2, &hand_at(&[start; 8], true)));
        s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
        for (i, pack) in path.chunks(MAX_POSES).enumerate() {
            s.add(input(&mut b, 10 + (i * MAX_POSES) as u64, &hand_at(pack, true)));
        }
        let mut world = with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.0, 0.03, 0.0], pit_ball())]);
        let k = keys.key(0);
        world.create(oid(CRATE), None, k, GENESIS).expect("crate");
        world.insert(oid(CRATE), Transform::at(pos(2.8, 0.5, 0.0))).expect("crate transform");
        world.insert(oid(CRATE), dynamic()).expect("crate body");
        world
            .insert(oid(CRATE), Shape { kind: shape_kind::BOX, density: kg, size: [0.5; 3], ..Shape::default() })
            .expect("crate shape");
        let mut r = replica_of(world);
        let before = at(&r, CRATE);
        let mut least = f32::INFINITY;
        for f in 0..10 + path.len() as u64 {
            r.advance(Frame(f + 1), &mut s);
            if f > 40 {
                least = least.min(r.physics().grab_force(pe_replica::hands::grab_key(oid(BALL))).unwrap_or(f32::NAN));
            }
        }
        let moved = at(&r, CRATE)[0] - before[0];
        println!("RESULT a held ball pushes a {kg} kg crate: it moves {moved:.3} m; the hold's least force {least} N");
        assert_eq!(least, pe_world::components::DEFAULT_STRENGTH, "a crate never lowers the grip");
        assert!(moved > 0.3, "pushed along: {moved}");
        let gap = pe_replica::hands::hold_gap(r.physics(), oid(BALL), oid(HAND)).expect("still held");
        assert!(gap < pe_seams::BREAK_AWAY, "the ball stays in the hand: {gap}");
        assert_eq!(r.faults(), 0);
    }
}

/// A fast throw's scene (@coordinator-opus's replays for the 11-27 kit, after @astra-deliberator): a pit
/// ball grabbed, lifted to 1 m and swung along +x, speeding up to 20 m/s over 9 frames and keeping it 6, then thrown
/// at 20 m/s as the hand opens. `obstacle` adds what it flies into, given the x the hand opens at. Its world, its
/// entries (the throw last) and the opening's frame.
fn fast_throw(obstacle: impl Fn(&mut World, f64)) -> (World, Vec<SignedEntry>, u64) {
    use pe_world::input::MAX_POSES;
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let hr = f64::from(pe_replica::world::HAND_RADIUS);
    let start = [2.0 - 0.03 - hr, 0.03, 0.0];
    let mut path: Vec<[f64; 3]> = (1..=30).map(|k| [start[0], 0.03 + 0.97 * f64::from(k) / 30.0, 0.0]).collect();
    path.extend(std::iter::repeat_n([start[0], 1.0, 0.0], 20));
    let mut x = start[0];
    for k in 1..=15_usize {
        x += 20.0 * (k.min(9) as f64 / 9.0) / 60.0;
        path.push([x, 1.0, 0.0]);
    }
    let open = 10 + path.len() as u64;
    let mut entries = vec![
        input(&mut b, 2, &hand_at(&[start; 8], true)),
        input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }),
    ];
    for (i, pack) in path.chunks(MAX_POSES).enumerate() {
        entries.push(input(&mut b, 10 + (i * MAX_POSES) as u64, &hand_at(pack, true)));
    }
    entries.push(input(&mut b, open, &hand_at(&[[x + 0.33, 1.0, 0.0]; 4], false)));
    entries.push(input(&mut b, open, &Input::Throw { object: oid(BALL), linear: [20.0, 0.0, 0.0], angular: [0.0; 3] }));
    let mut world = with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.0, 0.03, 0.0], pit_ball())]);
    obstacle(&mut world, x);
    (world, entries, open)
}

/// A fast throw's scene run twice: straight, and on a machine that gets the throw late and rolls back to it. Returns
/// both replicas at `open + 60` after asserting their roots agree.
fn fast_throw_both_ways(
    world: World,
    entries: Vec<SignedEntry>,
    open: u64,
) -> (ReplicaImpl<PeWorld>, ReplicaImpl<PeWorld>) {
    let end = open + 60;
    let mut all = Source::default();
    entries.iter().cloned().for_each(|e| all.add(e));
    let mut a = replica_of(world.clone());
    a.advance(Frame(end), &mut all);
    let (head, last) = entries.split_at(entries.len() - 1);
    let mut late = Source::default();
    head.iter().cloned().for_each(|e| late.add(e));
    let mut c = replica_of(world);
    c.advance(Frame(open + 8), &mut late);
    late.add(last[0].clone());
    let adv = c.advance(Frame(end), &mut late);
    assert!(adv.rolled_back_to.is_some_and(|f| f.0 <= open), "the late throw rolls back: {adv:?}");
    assert_eq!(roots(&mut c, end), roots(&mut a, end), "both machines agree after the rollback");
    assert_eq!((a.faults(), c.faults()), (0, 0));
    (a, c)
}

/// The ball's speed the frame after the throw, run straight.
fn left_at(world: World, entries: &[SignedEntry], open: u64) -> f64 {
    let mut s = Source::default();
    entries.iter().cloned().for_each(|e| s.add(e));
    let mut r = replica_of(world);
    r.advance(Frame(open + 1), &mut s);
    let v = r.physics().velocity(oid(BALL)).expect("the ball's body").0;
    f64::from((v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt())
}

/// Replay 1: a 20 m/s throw released 0.5 m before a 2 cm static wall. The ball leaves at the hand's speed; whether it
/// passes the wall is printed (Box3D's continuous collision against static shapes; tunnelling is accepted for the kit); a late throw rolls back to the same roots.
#[test]
fn a_fast_throw_at_a_thin_wall_leaves_at_the_hand_s_speed_and_rolls_back_alike() {
    let wall_at = std::cell::Cell::new(0.0);
    let (world, entries, open) = fast_throw(|w, x| {
        let k = keys().key(0);
        wall_at.set(x + 0.5);
        w.create(oid(70), None, k, GENESIS).expect("wall");
        w.insert(oid(70), Transform::at(pos(x + 0.5, 1.0, 0.0))).expect("wall transform");
        w.insert(oid(70), Body { friction: 0.6, ..Body::default() }).expect("wall body");
        w.insert(oid(70), cuboid([0.01, 1.0, 1.0])).expect("wall shape");
    });
    let speed = left_at(world.clone(), &entries, open);
    let (a, _) = fast_throw_both_ways(world, entries, open);
    let end = at(&a, BALL);
    println!(
        "RESULT thin wall: leaves at {speed:.2} m/s; ends at x {:.3} against the wall at {:.3} ({})",
        end[0],
        wall_at.get(),
        if end[0] > wall_at.get() { "THROUGH" } else { "stopped by it" }
    );
    assert!((speed - 20.0).abs() < 0.2, "leaves at the hand's speed: {speed}");
}

/// Replay 2: a 20 m/s throw into a stack of six 20 cm crates (to 1.2 m, across the ball's path), beside another member's hand (always physical, a
/// kinematic sphere like their body's part). The ball leaves at the hand's speed and knocks the stack; a late throw
/// rolls back to the same roots.
#[test]
fn a_fast_throw_into_a_stack_beside_another_player_rolls_back_alike() {
    const OTHER: u128 = 90;
    let stack_x = std::cell::Cell::new(0.0);
    let (world, entries, open) = fast_throw(|w, x| {
        stack_x.set(x + 0.6);
        let k = keys().key(0);
        for (n, id) in [71_u128, 72, 73, 74, 75, 76].into_iter().enumerate() {
            w.create(oid(id), None, k, GENESIS).expect("crate");
            w.insert(oid(id), Transform::at(pos(x + 0.6, 0.1 + 0.2 * n as f64, 0.0))).expect("crate transform");
            w.insert(oid(id), dynamic()).expect("crate body");
            w.insert(oid(id), cuboid([0.1; 3])).expect("crate shape");
        }
        let other = keys().key(1);
        w.create(oid(OTHER), None, other, GENESIS).expect("the other's hand");
        w.insert(oid(OTHER), Transform::at(pos(x + 0.6, 0.6, 0.35))).expect("its transform");
        w.insert(oid(OTHER), Hand { owner: other, mode: hand_mode::ALWAYS_WITHIN_REACH, ..Hand::default() })
            .expect("its row");
    });
    let speed = left_at(world.clone(), &entries, open);
    let before = [71_u128, 72, 73, 74, 75, 76]
        .map(|id| (world.store().world_transform(oid(id)).expect("placed").pos - Pos::ORIGIN).0);
    let (a, _) = fast_throw_both_ways(world, entries, open);
    let moved =
        [71_u128, 72, 73, 74, 75, 76].iter().zip(before).map(|(&id, p)| distance(at(&a, id), p)).fold(0.0, f64::max);
    println!(
        "RESULT stack: leaves at {speed:.2} m/s; the stack's crates moved up to {moved:.3} m; the ball ends at x {:.3} (the stack's far face {:.3}); roots agree after the rollback",
        at(&a, BALL)[0],
        stack_x.get() + 0.1
    );
    assert!((speed - 20.0).abs() < 0.2, "leaves at the hand's speed: {speed}");
}

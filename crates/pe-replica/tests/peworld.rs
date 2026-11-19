//! ReplicaImpl over pe-world's `World` (src/peworld.rs): body rows from the world's rows, the physics effects of
//! edits, and the replica stepping the real world with rollback.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_replica::hands::hand_mode;
use pe_replica::mock::MockClock;
use pe_replica::world::{BodyKind, Change, Phase, ShapeRow, Unbuildable, WorldModel};
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    ChainHead, Class, Cut, CutImport, CutRef, DomainId, EntrySource, FinalCut, Finalized, Frame, FrameClock, GroupSel,
    Hash, HostTime, ImportError, LeafKey, LineId, MemberKey, ObjectId, Offset, Outcome, Pos, Reason, Replica,
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
    match table.shapes[1].shape {
        ShapeRow::Sphere { center, radius } => {
            assert!(close(center, [1.0, 0.0, 0.0]), "the child sits 1 m along the table's own x: {center:?}");
            assert_eq!(radius, 0.2);
        }
        s => panic!("a sphere, not {s:?}"),
    }
    assert_eq!(table.shapes[1].density, 800.0);
    assert_eq!(pw.body(oid(4)), Err(Unbuildable::Shape), "a sphere under non-uniform scale cannot be built");
    match pw.body(oid(5)).unwrap().expect("box").shapes[0].shape {
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
    let shapes: Vec<ShapeRow> = built.shapes.iter().map(|p| p.shape).collect();
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

fn roots(r: &ReplicaImpl<PeWorld>, to: u64) -> Vec<(u64, Hash)> {
    (0..=to).step_by(K as usize).filter_map(|f| r.live_root(Frame(f)).map(|h| (f, h))).collect()
}

#[test]
fn the_replica_steps_the_real_world_and_rolls_back_to_the_same_roots() {
    let mut s = Source::default();
    scene().into_iter().for_each(|e| s.add(e));
    let mut a = replica();
    a.advance(Frame(64), &mut s);
    let ra = roots(&a, 64);
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
    assert_eq!(roots(&b, 64), ra);
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

    let again = b.next(Class::Protocol, Frame(6), pe_seams::Admit::new(joiner, perm::EDIT).encode());
    let malformed = b.next(Class::Protocol, Frame(6), Bytes::from([pe_seams::ADMIT, &[1]].concat()));
    for e in [&again, &malformed] {
        assert!(matches!(pw.apply(Frame(6), e).0, Outcome::Refused { .. }));
    }
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
    assert_eq!(roots(&a, 48), roots(&clean, 48), "the same roots as a run without it");
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

fn from(r: &ReplicaImpl<PeWorld>, at: u64, to: u64) -> Vec<(u64, Hash)> {
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
    assert_eq!(from(&joiner, 32, 64), from(&host, 32, 64));
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
    assert_eq!(from(&joiner, 32, 64), from(&host, 32, 64), "the host's roots, the late drop included");
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
    assert_eq!(from(&joiner, 32, 64), from(&host, 32, 64), "the host's roots through the grab, push and release");
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
    assert_eq!(roots(&c, 64), roots(&a, 64));
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
    assert_eq!(roots(&c, 48), roots(&a, 48));
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

/// A turn that would take a held object faster than the cap is slowed too (world-model.md 8.1; @design-reviewer's Low
/// on cc037c10): with the ball 15 cm out, a 90-degree turn in one frame would move it 21 cm (13 m/s). Neither it nor
/// the ball on its arc moves faster than the cap, and the spring brings it to within 1 cm of its turned place. On
/// cc037c10 it was left 4 cm out of place.
#[test]
fn a_fast_turn_moves_what_the_hand_holds_no_faster_than_the_cap() {
    let (top, ball, place) = turn_holding(0.15);
    assert!(top < pe_replica::hands::MAX_HAND_PUSH_SPEED, "the held ball and the one on its arc: {top} m/s");
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
/// the teleport that carries it, and the grab before a run faster than the cap.
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
        assert_eq!(roots(&c, 56), roots(&a, 56));
        assert_eq!((a.faults(), c.faults()), (0, 0));
    };
    run(handoff, 3);
    run(jump_holding, 2);
    run(fast_run, 1);
}

/// A held object keeps the rotation it was grabbed at, relative to the hand ([`pe_replica::hands::GRAB_TORQUE`]): a
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

/// A closed hand holding a pit ball runs along x at 15 m/s (1.5 times the cap) for one second from frame 10, through
/// pit balls every 0.5 m (ids from 100).
fn fast_run() -> (World, Vec<SignedEntry>) {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let start = [-7.0 - 0.03 - f64::from(pe_replica::world::HAND_RADIUS), 0.03, 0.0];
    let path: Vec<[f64; 3]> = (0..60).map(|k| [start[0] + 0.25 * f64::from(k + 1), 0.03, 0.0]).collect();
    let mut entries = vec![
        input(&mut b, 2, &hand_at(&[start; 8], true)),
        input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }),
    ];
    for (i, pack) in path.chunks(pe_world::input::MAX_POSES).enumerate() {
        entries.push(input(&mut b, 10 + (i * pe_world::input::MAX_POSES) as u64, &hand_at(pack, true)));
    }
    let mut balls = vec![(BALL, [-7.0, 0.03, 0.0], pit_ball())];
    balls.extend((0..28u32).map(|i| (100 + u128::from(i), [-6.0 + 0.5 * f64::from(i), 0.03, 0.0], pit_ball())));
    (with_hands_and_balls(&[(HAND, start)], &balls), entries)
}

/// A hand kept faster than the cap (world-model.md 8.1's test): through the second at 15 m/s its body trails its
/// target, and is placed at it again, carrying the ball it holds, whenever it would trail by more than the teleport
/// distance, so it never trails by more (its contacts come in bursts). The balls it runs through move no faster than
/// the cap plus the solver's push-out speed (Box3D's soft contacts resolving the overlap of a body driven at exactly
/// the cap: measured 10.5 m/s, with or without the held ball), and the ball stays in the hand.
#[test]
fn a_hand_kept_faster_than_the_cap_trails_by_at_most_the_teleport_distance() {
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
    assert_eq!(held_by(&r, BALL), Some(oid(HAND)), "the ball stays in the hand");
    assert_eq!(r.faults(), 0);
}

/// A flick with an object held half a metre out (world-model.md 8.1's test): a 36 cm ball, light enough (1 kg/m3,
/// 0.2 kg) for the grab's spring to swing it at the hand's speed, held 0.5 m from the hand's centre, turns 45 degrees
/// with it in one frame, which would carry it 38 cm (23 m/s; Box3D alone would turn the hand that far in one step,
/// and the pit ball on its arc would leave at 16 m/s). The turn is slowed so that the held ball, and the pit ball,
/// move no faster than the cap, and the held ball ends at its turned place.
#[test]
fn a_flick_with_an_object_held_half_a_metre_out_stays_under_the_cap() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let mut s = Source::default();
    let hand = [2.0, 0.36, 0.0];
    let half = std::f32::consts::FRAC_PI_8;
    let turned = [0.0, half.sin(), 0.0, half.cos()];
    s.add(input(&mut b, 2, &hand_at(&[hand; 10], true)));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    s.add(input(&mut b, 12, &hand_of(HAND, &[hand; 16], turned, true)));
    let big = Shape { kind: shape_kind::SPHERE, density: 1.0, size: [0.36, 0.0, 0.0], ..Shape::default() };
    let (c, n) = (std::f64::consts::FRAC_PI_8.cos(), std::f64::consts::FRAC_PI_8.sin());
    let balls = [(BALL, [2.5, 0.36, 0.0], big), (NEXT, [2.0 + 0.5 * c, 0.03, -0.5 * n], pit_ball())];
    let mut r = replica_of(with_hands_and_balls(&[(HAND, hand)], &balls));
    r.advance(Frame(12), &mut s);
    assert_eq!(held_by(&r, BALL), Some(oid(HAND)));
    let top = fastest(&mut r, &mut s, &[BALL, NEXT], 40);
    assert!(top < pe_replica::hands::MAX_HAND_PUSH_SPEED, "the held ball and the one on its arc: {top} m/s");
    let quarter = std::f64::consts::FRAC_PI_4;
    let place = [2.0 + 0.5 * quarter.cos(), 0.36, -0.5 * quarter.sin()];
    assert!(distance(at(&r, BALL), place) < 0.01, "at its turned place: {:?}, {place:?}", at(&r, BALL));
    assert_eq!(r.faults(), 0);
}

/// Throws (world-model.md 8.1; @coordinator-opus): a pit ball thrown straight up, the hand speeding up
/// over three frames and keeping its speed for two (0.2 to 0.9 m of travel), then opening and releasing it, leaves at
/// the hand's speed up to the cap: at 4, 8, 11 and 14 m/s, within 10% of the smaller of the two (the grab's spring
/// trails a little while the hand speeds up, and overshoots a little catching up).
#[test]
fn a_throw_leaves_at_the_hand_s_speed_up_to_the_cap() {
    let cap = pe_replica::hands::MAX_HAND_PUSH_SPEED;
    for v in [4.0, 8.0, 11.0, 14.0] {
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
        s.add(input(&mut b, 14, &Input::Release { object: oid(BALL) }));
        let mut r = replica_of(with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.0, 0.03, 0.0], pit_ball())]));
        r.advance(Frame(15), &mut s);
        assert_eq!(r.physics().joint_count(), 0, "released");
        let left = f64::from(r.physics().velocity(oid(BALL)).expect("the ball's body").0[1]);
        let expected = v.min(cap);
        println!("RESULT a {v} m/s throw leaves at {left:.2} m/s");
        assert!((left - expected).abs() < 0.1 * expected, "a {v} m/s throw leaves at {left} m/s");
        assert_eq!(r.faults(), 0);
    }
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

/// Where `object`'s place in `hand` is now: the hand's body and its grab's anchor.
fn place_in(r: &ReplicaImpl<PeWorld>, hand: u128, object: u128) -> [f64; 3] {
    let a = r.physics().grab_anchor(pe_replica::hands::grab_key(oid(object))).expect("held");
    let h = r.physics().pose(oid(hand)).expect("the hand's body");
    let (q, v) = (h.rotation, a);
    let cross =
        |a: [f32; 3], b: [f32; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let t = cross([q[0], q[1], q[2]], v).map(|c| 2.0 * c);
    let ut = cross([q[0], q[1], q[2]], t);
    let r = [v[0] + q[3] * t[0] + ut[0], v[1] + q[3] * t[1] + ut[1], v[2] + q[3] * t[2] + ut[2]];
    [0usize, 1, 2].map(|i| h.position[i] + f64::from(r[i]))
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

/// A held ball pinned by a wall (world-model.md 8.1's test, @coordinator-opus): with the hand 1 m or
/// 3 m past the wall it stays on its side, pressed less than a centimetre in, and when the wall is deleted it comes
/// back to its place no faster than the cap, because its spring sees at most the cap's reach of the error. With the
/// tight hold's stiff spring it closes that reach within a frame, so it comes back at about its reach a frame,
/// below the cap (measured: 4.65 m/s by the distance it moves a frame, 3.40 m/s at a frame's end; 8.2 and 8.4 m/s with
/// M1's 5 Hz spring; unbounded it came back at 10.1 and 30.7 m/s).
#[test]
fn a_ball_pinned_by_a_wall_stays_on_its_side_and_comes_back_no_faster_than_the_cap() {
    for by in [1.0, 3.0] {
        let (entries, world, clear) = pinned(by, false);
        let mut s = Source::default();
        entries.into_iter().for_each(|e| s.add(e));
        let mut r = replica_of(world);
        let mut deepest = f64::MIN;
        while r.frame().0 < clear {
            let next = r.frame().0 + 1;
            r.advance(Frame(next), &mut s);
            deepest = deepest.max(at(&r, BALL)[0]);
        }
        let (mut top, mut moved, mut last) = (0.0_f64, 0.0_f64, at(&r, BALL));
        while r.frame().0 < 120 {
            let next = r.frame().0 + 1;
            r.advance(Frame(next), &mut s);
            top = top.max(ball_speed(&r, BALL));
            moved = moved.max(distance(at(&r, BALL), last) * 60.0);
            last = at(&r, BALL);
        }
        println!(
            "RESULT pinned {by} m past: x {deepest:.4} m at deepest, back at {moved:.2} m/s ({top:.2} at a frame's end)"
        );
        let cap = pe_replica::hands::MAX_HAND_PUSH_SPEED;
        assert!(deepest < 2.23, "{by} m past: on its side, less than 1 cm in: {deepest}");
        assert!(
            moved > 4.0 && moved < cap && top < cap,
            "{by} m past: comes back at {moved} m/s ({top} at a frame's end)"
        );
        assert!(distance(at(&r, BALL), place_in(&r, HAND, BALL)) < 0.01, "{by} m past: back in its place");
        assert_eq!(r.faults(), 0);
    }
}

/// The realistic way past a wall (@design-reviewer): the hand 1 m past it lifts to 2.5 m, the ball
/// slides up the wall's face (until the pull's angle from the face's normal is within its friction's, which a lift to
/// only 1.3 m never passes), crosses the face's plane only above the wall, and comes to its place under the cap.
#[test]
fn a_ball_held_against_a_wall_is_lifted_over_it() {
    let (entries, world, _) = pinned(1.0, true);
    let mut s = Source::default();
    entries.into_iter().for_each(|e| s.add(e));
    let mut r = replica_of(world);
    let (mut last, mut top) = (at(&r, BALL), 0.0_f64);
    while r.frame().0 < 150 {
        let next = r.frame().0 + 1;
        r.advance(Frame(next), &mut s);
        let a = at(&r, BALL);
        if last[0] < 2.25 && a[0] >= 2.25 {
            let u = (2.25 - last[0]) / (a[0] - last[0]);
            let y = last[1] + u * (a[1] - last[1]);
            assert!(y > 1.0, "crosses the face's plane above the wall: at y = {y}");
        }
        top = top.max(ball_speed(&r, BALL));
        last = a;
    }
    assert!(at(&r, BALL)[0] > 2.35, "over the wall: {:?}", at(&r, BALL));
    assert!(distance(at(&r, BALL), place_in(&r, HAND, BALL)) < 0.01, "in its place: {:?}", at(&r, BALL));
    assert!(top < pe_replica::hands::MAX_HAND_PUSH_SPEED, "{top} m/s");
    assert_eq!(r.faults(), 0);
}

/// The wall check on a trailing placement (@coordinator-opus): a hand at 12 m/s carried 3 m past a wall
/// trails its target and is placed again and again, but a placement carries the ball it holds only where no static
/// geometry is in the way, so the ball stays on its side at 30, 60 and 120 Hz (as built on 0eebdfca, a placement
/// carried it 0.72 m through the wall in one frame).
#[test]
fn an_overrun_past_a_wall_leaves_the_held_ball_on_its_side() {
    for hz in [30, 60, 120] {
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let fr = |t: f64| (t * f64::from(hz)).round() as u64;
        let start = [2.08, 0.03, 0.0];
        let past = [5.08, 0.03, 0.0];
        let mut s = Source::default();
        path_inputs(&mut b, HAND, hz, 2, fr(1.5), &move |t| lerp(start, past, (t - 0.2) / 0.25))
            .into_iter()
            .for_each(|e| s.add(e));
        s.add(input(&mut b, fr(0.1), &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
        let mut world = with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.2, 0.03, 0.0], pit_ball())]);
        add_wall(&mut world);
        let mut r = replica_at(world, hz);
        let mut deepest = f64::MIN;
        while r.frame().0 < fr(1.5) {
            let next = r.frame().0 + 1;
            r.advance(Frame(next), &mut s);
            deepest = deepest.max(at(&r, BALL)[0]);
        }
        assert!(deepest < 2.24, "{hz} Hz: on its side: {deepest}");
        assert_eq!(held_by(&r, BALL), Some(oid(HAND)), "{hz} Hz: still held");
        assert_eq!(r.faults(), 0);
    }
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

/// Two hands, two holds, each bounded by its own hand's speed (@design-reviewer): the left holds a ball
/// pinned against the wall with the hand still 1 m past it, while the right carries another along in the open at
/// 15 m/s, above the cap. The pinned ball stays on its side; the right's ball catches up no faster than the cap
/// allows (its excess is at most the support's, about 0.44 m/s), and stays up in its hand.
#[test]
fn two_holds_are_bounded_each_by_its_own_hand() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let (start, past) = ([2.08, 0.03, 0.0], [3.08, 0.03, 0.0]);
    let right = [-5.0 - 0.03 - f64::from(pe_replica::world::HAND_RADIUS), 0.03, -3.0];
    let mut s = Source::default();
    path_inputs(&mut b, HAND, 60, 2, 120, &move |t| lerp(start, past, (t - 0.17) / 0.5))
        .into_iter()
        .for_each(|e| s.add(e));
    let run = move |t: f64| {
        let up = lerp(right, [right[0], 1.0, -3.0], (t - 0.2) / 0.3);
        [up[0] + 15.0 * (t - 0.8).clamp(0.0, 0.5), up[1], up[2]]
    };
    path_inputs(&mut b, RIGHT, 60, 2, 120, &run).into_iter().for_each(|e| s.add(e));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(NEXT), hand: Some(oid(RIGHT)) }));
    let balls = [(BALL, [2.2, 0.03, 0.0], pit_ball()), (NEXT, [-5.0, 0.03, -3.0], pit_ball())];
    let mut world = with_hands_and_balls(&[(HAND, start), (RIGHT, right)], &balls);
    add_wall(&mut world);
    let mut r = replica_of(world);
    let (mut deepest, mut top, mut below) = (f64::MIN, 0.0_f64, 0.0_f64);
    while r.frame().0 < 120 {
        let next = r.frame().0 + 1;
        r.advance(Frame(next), &mut s);
        deepest = deepest.max(at(&r, BALL)[0]);
        if next > 48 {
            top = top.max(ball_speed(&r, NEXT));
            below = below.max(place_in(&r, RIGHT, NEXT)[1] - at(&r, NEXT)[1]);
        }
    }
    assert!(deepest < 2.23, "the pinned ball stays on its side: {deepest}");
    assert!(top > 8.0 && top < pe_replica::hands::MAX_HAND_PUSH_SPEED + 0.5, "the carried ball: {top} m/s");
    assert!(below < 0.02, "and stays up in its hand: {below} m below its place");
    assert_eq!((held_by(&r, BALL), held_by(&r, NEXT)), (Some(oid(HAND)), Some(oid(RIGHT))));
    assert_eq!(r.faults(), 0);
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
        assert_eq!(roots(&c, 96), roots(&a, 96));
        assert_eq!((a.faults(), c.faults()), (0, 0));
    }
}

/// A held ball with the hand at 1.5 times the cap for 2 s (world-model.md 8.1's test), sideways at 1 m up, straight up
/// and straight down: it trails its place and catches up once the hand stops, moving at most about the cap, and, the
/// support part of its hold keeping it up, it never sags more than a centimetre or so below its place sideways or
/// downwards. Measured with the tight hold: 10.47, 10.02 and 10.61 m/s, the last with gravity's fall (it falls freely
/// while the hand goes down at the cap, until it is below its place); trails of 1.54, 1.81 and 1.10 m; 4.1 mm below
/// its place sideways. The trail grows with the overrun: each placement of the hand carries the ball at its offset with
/// no velocity, and the hold's force cap ([`pe_replica::hands::HOLD_ACCELERATION`]) takes about three frames to bring
/// it back to the cap. With M1's 5 Hz spring and a cap of 1,000 N it was 9.95, 10.20 and 10.12 m/s, under 1 m of trail
/// and 1.0 cm below.
#[test]
fn a_held_ball_kept_above_the_cap_for_two_seconds_trails_and_stays_up() {
    for dir in 0..3u8 {
        let keys = keys();
        let mut b = EntryBuilder::new(&keys, 0);
        let x0 = -9.0 - 0.03 - f64::from(pe_replica::world::HAND_RADIUS);
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
        let end = ((t_run + 2.5) * 60.0) as u64;
        let mut s = Source::default();
        path_inputs(&mut b, HAND, 60, 2, end, &*path).into_iter().for_each(|e| s.add(e));
        s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
        let mut r = replica_of(with_hands_and_balls(&[(HAND, floor)], &[(BALL, [-9.0, 0.03, 0.0], pit_ball())]));
        let (mut top, mut below, mut trail) = (0.0_f64, 0.0_f64, 0.0_f64);
        while r.frame().0 < end {
            let next = r.frame().0 + 1;
            r.advance(Frame(next), &mut s);
            if next as f64 >= t_run * 60.0 {
                let (ball, place) = (at(&r, BALL), place_in(&r, HAND, BALL));
                top = top.max(ball_speed(&r, BALL));
                below = below.max(place[1] - ball[1]);
                trail = trail.max(distance(ball, place));
            }
        }
        let cap = pe_replica::hands::MAX_HAND_PUSH_SPEED;
        println!("RESULT overrun direction {dir}: top {top:.2} m/s, trail {trail:.3} m, below {below:.4} m");
        // Straight down, gravity's fall adds to the cap.
        let over = if dir == 2 { 0.7 } else { 0.5 };
        assert!(top > 9.0 && top < cap + over, "direction {dir}: {top} m/s");
        assert!(trail > 0.2 && trail < 2.0, "direction {dir}: it trails, at most {trail} m");
        if dir != 1 {
            assert!(below < 0.02, "direction {dir}: it stays up, at most {below} m below its place");
        }
        assert!(distance(at(&r, BALL), place_in(&r, HAND, BALL)) < 0.02, "direction {dir}: back in its place");
        assert_eq!(r.faults(), 0);
    }
}

const SHELF: u128 = 71;
const UPPER: u128 = 72;

fn add_static(w: &mut World, id: u128, center: [f64; 3], half: [f32; 3]) {
    let k = keys().key(0);
    w.create(oid(id), None, k, GENESIS).expect("static box");
    w.insert(oid(id), Transform::at(pos(center[0], center[1], center[2]))).expect("its transform");
    w.insert(oid(id), Body { friction: 0.6, ..Body::default() }).expect("its body");
    w.insert(oid(id), cuboid(half)).expect("its shape");
}

/// A ball pinned in a corner at 45 degrees from its place, then released with the hand still (@design-reviewer's
/// Medium on 9d3a0270). `below`: the ball under a ceiling against the wall, its place up and beyond (the hand carried 1
/// m up and 1 m past); otherwise the ball on a shelf against a wall above it, its place down and beyond. At 1 s both
/// obstacles are deleted. The ball's top speed after.
fn diagonal(below: bool) -> f64 {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let y0 = if below { 0.03 } else { 1.03 };
    let start = [2.08, y0, 0.0];
    let to = [3.08, if below { 1.03 } else { 0.03 }, 0.0];
    let mut s = Source::default();
    path_inputs(&mut b, HAND, 60, 2, 150, &move |t| lerp(start, to, (t - 0.17) / 0.5))
        .into_iter()
        .for_each(|e| s.add(e));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    let delete =
        tx(vec![Op::Delete { id: Target::Existing(oid(SHELF)) }, Op::Delete { id: Target::Existing(oid(UPPER)) }]);
    s.add(b.next(Class::Edit, Frame(60), Bytes::from(delete.encode())));
    let mut world = with_hands_and_balls(&[(HAND, start)], &[(BALL, [2.2, y0, 0.0], pit_ball())]);
    if below {
        add_static(&mut world, SHELF, [1.9, 0.35, 0.0], [0.35, 0.05, 1.0]);
        add_static(&mut world, UPPER, [2.30, 0.5, 0.0], [0.05, 0.5, 1.0]);
    } else {
        add_static(&mut world, SHELF, [1.9, 0.95, 0.0], [0.35, 0.05, 1.0]);
        add_static(&mut world, UPPER, [2.30, 1.5, 0.0], [0.05, 0.5, 1.0]);
    }
    let mut r = replica_of(world);
    r.advance(Frame(60), &mut s);
    let pinned = at(&r, BALL);
    assert!(pinned[0] < 2.23, "pinned against the wall: {pinned:?}");
    let (mut top, mut last) = (0.0_f64, pinned);
    while r.frame().0 < 150 {
        let next = r.frame().0 + 1;
        r.advance(Frame(next), &mut s);
        top = top.max(ball_speed(&r, BALL)).max(distance(at(&r, BALL), last) * 60.0);
        last = at(&r, BALL);
    }
    assert!(distance(at(&r, BALL), place_in(&r, HAND, BALL)) < 0.01, "back in its place: {:?}", at(&r, BALL));
    assert_eq!(r.faults(), 0);
    top
}

/// The 45 degree pins (world-model.md 8.1): a ball pinned below and beside its place, or above and beside it, comes
/// back with the hand still at no more than the cap plus the support's excess (k s_min, 0.16 m/s at 60 Hz; above,
/// gravity also pulls it down, by about g times its return), its speed the larger of its velocity at a frame's end and
/// the distance it moves a frame. Measured with the tight hold, which closes its reach within a frame: 4.65 and 4.85
/// m/s (8.39 and 8.57 m/s with M1's 5 Hz spring). Cut part by part, as on 9d3a0270, the spring would see sqrt(2) times
/// the reach: about 12 m/s by the linear model at 5 Hz [I].
#[test]
fn a_ball_pinned_at_45_degrees_from_its_place_comes_back_under_the_cap() {
    let cap = pe_replica::hands::MAX_HAND_PUSH_SPEED;
    let below = diagonal(true);
    let above = diagonal(false);
    println!("RESULT 45 degree pins: below and beside {below:.2} m/s, above and beside {above:.2} m/s");
    assert!(below > 4.0 && below < cap + 0.5, "below and beside: {below} m/s");
    assert!(above > 4.0 && above < cap + 0.5, "above and beside: {above} m/s");
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
/// default 110 N it lifts 8 kg (78 N of weight) and holds it within millimetres, where a hand of 60 N can't lift it, and
/// it stays on the floor, still held. Its strength is its `Hand` row's.
#[test]
fn a_hands_strength_decides_what_it_can_lift() {
    assert_eq!(Hand::default().strength, pe_world::components::DEFAULT_STRENGTH);
    let strong = lift_with(pe_world::components::DEFAULT_STRENGTH, 8.0);
    let weak = lift_with(60.0, 8.0);
    println!("RESULT 8 kg: {:.1} mm below its place at 110 N, {:.3} m at 60 N", strong * 1e3, weak);
    assert!(strong < 0.005, "lifted by 110 N: {strong} m below its place");
    assert!(weak > 0.9, "not lifted by 60 N: {weak} m below its place");
}

/// A ball held into the floor stays on it (world-model.md 8.1; @coordinator-opus): the hand comes down
/// at 1 m/s until the ball's place is 20 cm under the floor's top, stays 0.5 s, and lifts it out. The hold's force cap
/// keeps it from sinking in (measured: 1.55 mm deep at 30, 60 and 120 Hz; a stiff spring capped at 110 N alone sank
/// it 43-47 mm), it doesn't creep sideways, and it comes out with the hand.
#[test]
fn a_ball_held_into_the_floor_stays_on_it_and_comes_out_with_the_hand() {
    let hr = f64::from(pe_replica::world::HAND_RADIUS);
    for hz in [30_u32, 60, 120] {
        let rate = f64::from(hz);
        let over = [0.0, 0.5 + 0.03 + hr, 0.0];
        let low = 0.03 - 0.2 + 0.03 + hr;
        let down = 0.3 + over[1] - low;
        let p = move |t: f64| {
            let y = if t < down + 0.5 { (over[1] - (t - 0.3).max(0.0)).max(low) } else { low + (t - down - 0.5) };
            [0.0, y.min(over[1]), 0.0]
        };
        let mut b = EntryBuilder::new(&keys(), 0);
        let end = ((down + 1.5) * rate) as u64;
        let mut s = Source::default();
        path_inputs(&mut b, HAND, hz, 2, end, &p).into_iter().for_each(|e| s.add(e));
        s.add(input(&mut b, u64::from(hz / 10), &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
        let mut r = replica_at(with_hands_and_balls(&[(HAND, over)], &[(BALL, [0.0, 0.5, 0.0], pit_ball())]), hz);
        let (mut deepest, mut aside) = (0.0_f64, 0.0_f64);
        while r.frame().0 < end {
            let next = r.frame().0 + 1;
            r.advance(Frame(next), &mut s);
            let ball = at(&r, BALL);
            deepest = deepest.max(0.03 - ball[1]);
            aside = aside.max(ball[0].abs()).max(ball[2].abs());
        }
        println!("RESULT {hz} Hz: {:.2} mm into the floor, {:.3} mm aside", deepest * 1e3, aside * 1e3);
        assert!(deepest < 0.003, "{hz} Hz: {deepest} m into the floor");
        assert!(aside < 0.001, "{hz} Hz: crept {aside} m aside");
        assert!(distance(at(&r, BALL), place_in(&r, HAND, BALL)) < 0.01, "{hz} Hz: out with the hand");
        assert_eq!(r.faults(), 0);
    }
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
    assert_eq!(roots(&l, end), roots(&r, end), "the same roots");
    assert_eq!(l.faults(), 0);
}

/// A trailing placement still carries a held box sliding along the floor (@design-reviewer): the box's
/// enclosing capsule overlaps the floor more than the box does, and the mover cast passes it because the move goes
/// along the floor, not into it. Each time the hand's body is placed, the box moves with it.
#[test]
fn a_trailing_placement_carries_a_held_box_sliding_along_the_floor() {
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let start = [-6.0 - 0.05 - f64::from(pe_replica::world::HAND_RADIUS), 0.05, 0.0];
    let mut s = Source::default();
    path_inputs(&mut b, HAND, 60, 2, 90, &move |t| [start[0] + 15.0 * (t - 0.2).clamp(0.0, 1.0), 0.05, 0.0])
        .into_iter()
        .for_each(|e| s.add(e));
    s.add(input(&mut b, 6, &Input::Grab { object: oid(BALL), hand: Some(oid(HAND)) }));
    let box_shape = Shape { kind: shape_kind::BOX, density: 200.0, size: [0.05, 0.05, 0.15], ..Shape::default() };
    let mut r = replica_of(with_hands_and_balls(&[(HAND, start)], &[(BALL, [-6.0, 0.05, 0.0], box_shape)]));
    r.advance(Frame(12), &mut s);
    let (mut placements, mut carried) = (0, 0);
    while r.frame().0 < 72 {
        let hand = r.physics().pose(oid(HAND)).expect("the hand's body").position;
        let held = at(&r, BALL);
        let next = r.frame().0 + 1;
        r.advance(Frame(next), &mut s);
        if distance(r.physics().pose(oid(HAND)).expect("the hand's body").position, hand) > 0.3 {
            placements += 1;
            carried += usize::from(distance(at(&r, BALL), held) > 0.3);
        }
    }
    assert!(placements > 0, "the hand's body was placed");
    assert_eq!(carried, placements, "and the box went with it every time");
    assert_eq!(held_by(&r, BALL), Some(oid(HAND)));
    assert_eq!(r.faults(), 0);
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
    assert_eq!(roots(&late, 32), roots(&in_order, 32));
    assert_eq!(roots(&late, 32).len(), 5);
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

    assert_eq!(pw.body(oid(2)), Err(Unbuildable::Shape), "a mesh");
    assert_eq!(pw.body(oid(3)), Err(Unbuildable::Kind));
    assert_eq!(pw.body(oid(4)), Err(Unbuildable::Pose), "no Transform");
    assert_eq!(pw.body(oid(5)), Ok(None), "no shape");
    assert_eq!(pw.body(oid(6)), Ok(None), "no shape, so its kind is never read");
    assert_eq!(pw.body(oid(7)), Err(Unbuildable::Shape), "its child's capsule under non-uniform scale");
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
    assert_eq!(roots(&l, 40), roots(&r, 40));
    assert_eq!(roots(&r, 40).len(), 6);
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
            r.physics().grab_spring_rate(pe_replica::hands::grab_spring(0.0, 0.0, 0.0), 1.0 / hz as f32)
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

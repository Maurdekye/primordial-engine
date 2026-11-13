//! ReplicaImpl over pe-world's `World` (src/peworld.rs): body rows from the world's rows, the physics effects of
//! edits, and the replica stepping the real world with rollback.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use pe_replica::hands::hand_mode;
use pe_replica::mock::MockClock;
use pe_replica::world::{BodyKind, Change, Phase, ShapeRow, WorldModel};
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    ChainHead, Class, Cut, CutImport, CutRef, DomainId, EntrySource, FinalCut, Finalized, Frame, FrameClock, GroupSel,
    Hash, HostTime, ImportError, LeafKey, LineId, MemberKey, ObjectId, Offset, Outcome, Pos, Reason, Replica,
    SignedEntry,
};
use pe_sim::Transform;
use pe_world::components::{Appearance, Body, BodyState, Hand, Name, Shape, body_kind, shape_kind};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
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
    Transaction { actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] }
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
    let floor = pw.body(oid(1)).expect("floor");
    assert_eq!((floor.kind, floor.pos, floor.shapes.len()), (BodyKind::Static, pos(0.0, -0.5, 0.0), 1));
    let table = pw.body(oid(2)).expect("table");
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
    assert_eq!(pw.body(oid(4)), None, "a sphere under non-uniform scale cannot be built");
    match pw.body(oid(5)).expect("box").shapes[0].shape {
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
    let built = pw.body(oid(2)).expect("the body is built");
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
    assert_eq!(pw.body(oid(2)).expect("moved").shapes, built.shapes);
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
        (r.world().body(oid(BALL)).expect("the ball").pos - Pos::ORIGIN).0[0]
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
    CutImport { cut: cut.clone(), target_root: root, records, absent: Vec::new(), physics: donor.physics_image(&root) }
}

fn from(r: &ReplicaImpl<PeWorld>, at: u64, to: u64) -> Vec<(u64, Hash)> {
    roots(r, to).into_iter().filter(|r| r.0 >= at).collect()
}

/// A joiner imports the real world at a cut from empty, through pe-world's tree (world-model.md 6.5): every record is
/// checked against the agreed root, so a record changed on the way is refused and changes nothing. The checked
/// import's root is the host's, finalize agrees, and the joiner steps on with the host's roots.
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

/// A pose jump faster than a hand may push (world-model.md 8.1's `max_push_speed`) moves the hand's body without
/// sweeping it, and without carrying its speed into what it touches: a closed hand resting against the ball jumps
/// through it, 1.19 m in one frame (71 m/s). Driven there, the hand would hit the ball through their contact at that
/// speed; placed there, it leaves the ball where it was. A jump from 1.5 m into the ball's side (27 m/s) pushes the
/// ball out of the hand gently.
#[test]
fn a_pose_jump_moves_the_hand_without_hitting_what_it_jumps_from_or_onto() {
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
    let rest = 2.0 - 0.1 - f64::from(pe_replica::world::HAND_RADIUS);
    let (before, top, after) = run(rest, 3.0);
    assert!(top < 1.0, "a jump through the ball leaves it: {top} m/s");
    assert!((after[0] - before[0]).abs() < 0.05, "where it was: {before:?} then {after:?}");
    let (_, top, after) = run(1.5, 1.95);
    assert!(top < 4.0, "a jump into the ball pushes it out gently: {top} m/s");
    assert!(after[0] > 2.05, "and out of the hand: {after:?}");
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
/// poses 5 cm a frame apart (3 m/s) ending at frame 4, its body moves on for six frames at 60 Hz, then is back at
/// 0.10 m, so it never parks away from the hand everyone sees.
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
    // xs[k] is the body's x after frame 4 + k: it reached 0.10 at frame 4, coasts through frames 5 to 10 to 0.40, and is
    // back at 0.10 after frame 11 (a 30 cm return in one frame is faster than a hand pushes, so it is placed).
    let step = |k: usize| xs[k + 1] - xs[k];
    assert!((0..5).all(|k| (step(k) - 0.05).abs() < 1e-3), "it coasts at 3 m/s: {xs:?}");
    assert!((xs[6] - 0.40).abs() < 1e-3, "six frames of coast: {xs:?}");
    assert!((7..xs.len()).all(|k| (xs[k] - 0.10).abs() < 1e-6), "then it is back at the hand and stays: {xs:?}");
    // The body is the replica's own: it writes no pose and no `BodyState` row back to the world model, whose hand
    // stays where its last input put it, through the cuts at frames 8 and 16.
    assert!((at(&r, HAND)[0] - 0.10).abs() < 1e-6, "the world's hand stays at its input: {:?}", at(&r, HAND));
    assert!(r.world().world().store().get::<BodyState>(oid(HAND)).is_none(), "no BodyState row for a hand");
    assert_eq!(r.faults(), 0);
}

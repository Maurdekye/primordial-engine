//! ReplicaImpl over pe-world's `World` (src/peworld.rs): body rows from the world's rows, the physics effects of
//! edits, and the replica stepping the real world with rollback.

use std::collections::BTreeMap;
use std::time::Duration;

use bytes::Bytes;
use pe_replica::world::{BodyKind, Change, ShapeRow, WorldModel};
use pe_replica::{PeWorld, ReplicaConfig, ReplicaImpl};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    Class, DomainId, EntrySource, FinalCut, Frame, Hash, MemberKey, ObjectId, Offset, Outcome, Pos, Replica,
    SignedEntry,
};
use pe_sim::Transform;
use pe_world::components::{Body, Name, Shape, body_kind, shape_kind};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::{BlobStore, GENESIS, World, perm};

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

fn replica() -> ReplicaImpl<PeWorld> {
    let mut config = ReplicaConfig::new(pe_seams::SimRate::DEFAULT, Duration::from_millis(133));
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

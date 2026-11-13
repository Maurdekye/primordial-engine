//! M1's ball pit and admission (world-model.md 2.4): @coordinator-opus m949a335461b2's conditions, with objects
//! found by name, never by id.

use bytes::Bytes;
use pe_core::{Frame, MemberKey, Offset, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Outcome};
use pe_world::components::{Body, Hand, Shape, SimSize, body_kind, shape_kind};
use pe_world::preset::{BALLS, MAX_BALLS, ball_pit, ball_pit_with, find, m1_line, names};
use pe_world::{BlobStore, HandPose, Input, World, WorldError, perm};

fn origin() -> Pos {
    Pos::ORIGIN.checked_add(Offset([3.0, 0.0, -2.0])).unwrap()
}

fn pit(seed: u64) -> World {
    ball_pit(seed, BlobStore::default(), origin())
}

#[test]
fn the_same_arguments_give_the_same_root() {
    let (a, b) = (pit(7), pit(7));
    assert_eq!(a.root(), b.root());
    assert_eq!(a.store().state_hash(), b.store().state_hash());
    // The seed moves the balls' jitter and the ids' chain, so another seed is another world.
    assert_ne!(a.root(), pit(8).root());
    assert_ne!(a.root(), ball_pit(7, BlobStore::default(), Pos::ORIGIN).root());
}

#[test]
fn the_pit_has_its_named_objects_and_no_hands() {
    let w = pit(1);
    let root = find(&w, names::ROOT).expect("the pit's root");
    assert_eq!(w.store().parent(root), None);
    for n in [names::FLOOR, names::LAMP].into_iter().chain(names::WALLS) {
        let id = find(&w, n).unwrap_or_else(|| panic!("{n}"));
        assert_eq!(w.store().parent(id), Some(root), "{n}");
    }
    let snap = w.snapshot(Frame(0));
    assert_eq!(snap.children(root).len(), 6 + BALLS);
    for i in [0, BALLS - 1] {
        let ball = find(&w, &format!("{}{i}", names::BALL_PREFIX)).unwrap();
        let body = w.store().get::<Body>(ball).unwrap();
        assert_eq!(body.kind, body_kind::DYNAMIC);
        assert_eq!(body.gravity_scale, 1.0);
        let shape = w.store().get::<Shape>(ball).unwrap();
        assert_eq!(shape.kind, shape_kind::SPHERE);
        // About 18 g, a real ball-pit ball.
        let kg = f64::from(shape.density) * 4.0 / 3.0 * std::f64::consts::PI * f64::from(shape.size[0]).powi(3);
        assert!((0.015..0.02).contains(&kg), "{kg} kg");
    }
    assert!(find(&w, &format!("{}{BALLS}", names::BALL_PREFIX)).is_none());
    assert_eq!(w.store().iter::<Hand>().count(), 0, "content only: hands come from admission");
    let floor = find(&w, names::FLOOR).unwrap();
    assert_eq!(w.store().get::<Body>(floor).unwrap().kind, body_kind::STATIC);
    assert_eq!(ball_pit_with(1, BlobStore::default(), origin(), 3).snapshot(Frame(0)).children(root).len(), 9);
}

#[test]
fn the_pit_is_made_by_an_ordinary_edit_from_world_logic() {
    let w = pit(1);
    let ball = find(&w, "Ball 0").unwrap();
    let info = w.snapshot(Frame(0)).info(ball).unwrap();
    // An edit made it, so it has an edit version, and world logic is its creator.
    assert_ne!(info.version, pe_world::GENESIS);
    assert_eq!(w.store().node(ball).creator, MemberKey::LOGIC);
    // Logic held `EDIT` only while the preset applied.
    assert!(!w.permits(MemberKey::LOGIC, perm::EDIT));
    assert_eq!(w.frame(), Frame(0));
    // Every object of the pit has a simulation size (3.5): root, floor, four walls, lamp and the balls.
    assert_eq!(w.store().iter::<SimSize>().count(), 7 + BALLS);
    // The world's physics origin is at the pit.
    assert_eq!(w.settings().origin, origin());
}

#[test]
fn an_m1_world_carries_the_marker_in_its_root() {
    let w = pit(1);
    assert_eq!(w.m1_marker(), Some(m1_line()));
    assert_ne!(m1_line(), pe_core::LineId::origin(), "the M1 line is not the origin");
    assert_eq!(World::new(1, BlobStore::default()).m1_marker(), None);
}

#[test]
fn admission_gives_a_member_two_hands_it_alone_moves() {
    let keys = TestKeys::new(1, 2);
    let (admin, other) = (keys.key(0), keys.key(1));
    let mut w = pit(1);
    let line = m1_line();
    let hands = w.admit(line, admin, perm::ADMIN | perm::EDIT, Frame(0), origin()).unwrap();
    assert_eq!(hands, [0, 1].map(|s| pe_core::ObjectId::hand(line, admin, s, Frame(0))));
    for (side, h) in hands.iter().enumerate() {
        let row = w.store().get::<Hand>(*h).unwrap();
        assert_eq!((row.owner, row.side), (admin, side as u32));
        assert!(w.store().world_transform(*h).is_some());
    }
    assert!(w.permits(admin, perm::EDIT));
    let snap = w.snapshot(Frame(0));
    assert_eq!(snap.perms(admin), Some(perm::ADMIN | perm::EDIT));
    assert_eq!(snap.perms(other), None);
    assert_eq!(snap.members().collect::<Vec<_>>(), vec![(admin, perm::ADMIN | perm::EDIT)]);
    assert_eq!(w.admit(line, admin, perm::EDIT, Frame(0), origin()), Err(WorldError::Duplicate(hands[0])));
    let before = w.root();
    let mut w2 = pit(1);
    w2.admit(line, admin, perm::ADMIN | perm::EDIT, Frame(0), origin()).unwrap();
    assert_eq!(w2.root(), before, "admission is deterministic");

    // The admin's hand reaches a ball and grabs it; the other key cannot move the admin's hand.
    let ball = find(&w, "Ball 0").unwrap();
    let to = w.store().world_transform(ball).unwrap().pos;
    let pose = HandPose { pos: to, rot: [0.0, 0.0, 0.0, 1.0], grip: 1.0, engaged: true };
    let mut a = EntryBuilder::new(&keys, 0);
    let mut b = EntryBuilder::new(&keys, 1);
    let mv = Input::Hand { hand: hands[1], poses: vec![pose], hold: 0 };
    let e = b.next(Class::Input, Frame(0), Bytes::from(mv.encode()));
    assert!(matches!(w.apply_input(Frame(0), &e).0, Outcome::Refused { .. }));
    let e = a.next(Class::Input, Frame(0), Bytes::from(mv.encode()));
    assert!(matches!(w.apply_input(Frame(0), &e).0, Outcome::Applied { .. }));
    w.end_frame(Frame(0));
    w.set_frame(Frame(1));
    let g = Input::Grab { object: ball, hand: Some(hands[1]) };
    let e = a.next(Class::Input, Frame(1), Bytes::from(g.encode()));
    let out = w.apply_input(Frame(1), &e).0;
    assert!(matches!(out, Outcome::Applied { .. }), "{out:?}");
    assert_eq!(w.claims().find(|(o, _)| *o == ball).map(|(_, c)| c.member), Some(admin));
}

#[test]
fn the_largest_pit_builds_and_the_count_is_clamped() {
    let w = ball_pit_with(3, BlobStore::default(), origin(), MAX_BALLS);
    let root = find(&w, names::ROOT).unwrap();
    assert_eq!(w.snapshot(Frame(0)).children(root).len(), 6 + MAX_BALLS);
    assert_eq!(ball_pit_with(3, BlobStore::default(), origin(), MAX_BALLS + 100).root(), w.root());
    let one = ball_pit_with(3, BlobStore::default(), origin(), 0);
    assert_eq!(one.snapshot(Frame(0)).children(find(&one, names::ROOT).unwrap()).len(), 7);
}

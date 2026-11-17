//! A member's graceful leave (world-model.md 2.4, 8; @coordinator-opus m003987d0d045, mbdc0966ab9ab).

use bytes::Bytes;
use pe_core::{EntryRef, Frame, Hash, MemberKey, ObjectId, Offset, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, FieldPath, Outcome, Reason, SignedEntry};
use pe_sim::Transform;
use pe_world::components::{Appearance, Body, BodyState, Hand, Protection, Shape, ids, shape_kind};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::preset::m1_line;
use pe_world::{BlobStore, Footprint, Input, World, WorldError, perm};

const D: pe_seams::DomainId = pe_seams::DomainId(7);
const BALL: u128 = 10;
const BOX: u128 = 11;
const HELD: u128 = 12;

fn oid(n: u128) -> ObjectId {
    ObjectId::from_u128(n)
}

fn at(x: f64, y: f64, z: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, y, z])).unwrap()
}

fn leave_entry() -> EntryRef {
    EntryRef(Hash([0x1e; 32]))
}

struct Fx {
    a: EntryBuilder,
    b: EntryBuilder,
    kb: MemberKey,
    hands_a: [ObjectId; 2],
    hands_b: [ObjectId; 2],
    w: World,
}

/// A (admin) and B joined at frame 0, both spawned at the origin, and a ball moving at 1 m/s within reach of both
/// right hands, created by B and protected by B.
fn fx() -> Fx {
    let keys = TestKeys::new(1, 3);
    let (ka, kb) = (keys.key(0), keys.key(1));
    let mut w = World::new(1, BlobStore::default());
    let hands_a = w.admit(m1_line(), ka, perm::ADMIN | perm::EDIT, Frame(0), Pos::ORIGIN).unwrap();
    let hands_b = w.admit(m1_line(), kb, perm::EDIT, Frame(0), Pos::ORIGIN).unwrap();
    w.create(oid(BALL), None, kb, pe_world::GENESIS).unwrap();
    w.insert(oid(BALL), Transform::at(at(0.25, 1.2, 0.35))).unwrap();
    w.insert(oid(BALL), Body::default()).unwrap();
    w.insert(oid(BALL), Shape { kind: shape_kind::SPHERE, size: [0.05, 0.0, 0.0], ..Shape::default() }).unwrap();
    w.insert(oid(BALL), BodyState { linear: [1.0, 0.0, 0.0], awake: 1, ..BodyState::default() }).unwrap();
    w.insert(oid(BALL), Protection { owner: kb }).unwrap();
    Fx { a: EntryBuilder::new(&keys, 0), b: EntryBuilder::new(&keys, 1), kb, hands_a, hands_b, w }
}

fn input(w: &mut World, f: u64, b: &mut EntryBuilder, i: Input) -> (Outcome, Footprint) {
    let e: SignedEntry = b.next(Class::Input, Frame(f), Bytes::from(i.encode()));
    w.apply_input(Frame(f), &e)
}

fn edit(w: &mut World, f: u64, b: &mut EntryBuilder, ops: Vec<Op>) -> Outcome {
    let t =
        Transaction { inline: vec![], actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] };
    let e: SignedEntry = b.next(Class::Edit, Frame(f), Bytes::from(t.encode()));
    w.apply_edit(Frame(f), &e).0
}

fn row<T: pe_sim::Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

fn applied(o: &Outcome) -> bool {
    matches!(o, Outcome::Applied { .. })
}

fn reason(o: &Outcome) -> Reason {
    match o {
        Outcome::Refused { reason, .. } => reason.clone(),
        a => panic!("applied: {a:?}"),
    }
}

fn grab(object: u128, hand: ObjectId) -> Input {
    Input::Grab { object: oid(object), hand: Some(hand) }
}

#[test]
fn a_leave_while_holding_a_moving_object_ends_the_claim_keeps_its_velocity_and_frees_it() {
    let mut f = fx();
    assert!(applied(&input(&mut f.w, 0, &mut f.b, grab(BALL, f.hands_b[1])).0));
    assert_eq!(f.w.claims().count(), 1);
    let r = f.w.leave(f.kb, leave_entry()).unwrap();
    let mut hands = f.hands_b.to_vec();
    hands.sort();
    assert_eq!(r.hands, hands);
    assert_eq!(r.released, vec![oid(BALL)]);
    assert!(r.moved.is_empty());
    for h in f.hands_b {
        assert!(!f.w.store().exists(h), "hand removed outright");
    }
    assert_eq!(f.w.claims().count(), 0);
    assert_eq!(f.w.store().get::<BodyState>(oid(BALL)).unwrap().linear, [1.0, 0.0, 0.0], "velocity kept");
    assert!(!f.w.permits(f.kb, perm::EDIT));
    // The next frame another member grabs it: no claim and no cooldown stand in the way.
    f.w.set_frame(Frame(2));
    assert!(applied(&input(&mut f.w, 2, &mut f.a, grab(BALL, f.hands_a[1])).0));
}

#[test]
fn a_leave_drops_the_leavers_cooldowns_and_push_window() {
    let mut f = fx();
    // B's claim expires, which starts B's cooldown on the ball; then B pushes it.
    assert!(applied(&input(&mut f.w, 0, &mut f.b, grab(BALL, f.hands_b[1])).0));
    for g in 0..31 {
        f.w.end_frame(Frame(g));
        f.w.set_frame(Frame(g + 1));
    }
    assert!(f.w.cooldown(f.kb, oid(BALL)).is_some(), "the fixture starts a cooldown");
    let push = Input::Push { object: oid(BALL), impulse: [0.01, 0.0, 0.0], at: None };
    assert!(applied(&input(&mut f.w, 31, &mut f.b, push).0));
    f.w.leave(f.kb, leave_entry()).unwrap();
    assert_eq!(f.w.cooldown(f.kb, oid(BALL)), None);
    // Nothing keyed by B stays in the cooldowns, hand packs or push windows.
    for t in [5u8, 7, 8] {
        for (k, _) in f.w.table_entries(t) {
            assert!(!k.windows(32).any(|x| x == f.kb.0), "table {t} still names B");
        }
    }
}

#[test]
fn admit_then_leave_restores_the_root_before_the_admit() {
    let keys = TestKeys::new(1, 3);
    let (ka, kb) = (keys.key(0), keys.key(1));
    let mut w = World::new(1, BlobStore::default());
    w.admit(m1_line(), ka, perm::ADMIN | perm::EDIT, Frame(0), Pos::ORIGIN).unwrap();
    let r0 = w.root_hash(D);
    w.admit(m1_line(), kb, perm::EDIT, Frame(3), Pos::ORIGIN).unwrap();
    assert_ne!(w.root_hash(D), r0);
    w.leave(kb, leave_entry()).unwrap();
    assert_eq!(w.root_hash(D), r0, "nothing of B's stays: no hands, no tombstones, no member row");
}

#[test]
fn a_hands_children_move_to_its_parent_keeping_their_world_pose() {
    let mut f = fx();
    let hand = f.hands_b[0];
    f.w.create(oid(BOX), Some(hand), f.kb, pe_world::GENESIS).unwrap();
    f.w.insert(oid(BOX), Transform::local([0.0, 0.0, 0.1])).unwrap();
    let pose = f.w.store().world_transform(oid(BOX)).unwrap();
    let r = f.w.leave(f.kb, leave_entry()).unwrap();
    assert_eq!(r.moved, vec![oid(BOX)]);
    assert_eq!(f.w.store().parent(oid(BOX)), None, "M1's hands are roots, so it becomes one");
    assert_eq!(f.w.store().world_transform(oid(BOX)).unwrap(), pose);
    assert!(f.w.verify_tree_cache(D).is_ok());
}

#[test]
fn what_the_leaver_made_stays_and_its_protections_stay_frozen_until_agent_powers() {
    let mut f = fx();
    f.w.leave(f.kb, leave_entry()).unwrap();
    assert!(f.w.store().exists(oid(BALL)), "a leave is not a delete");
    assert_eq!(f.w.store().get::<Protection>(oid(BALL)), Some(&Protection { owner: f.kb }));
    // A, the admin left, can't lift it yet: an admin's lock-flag clear is agent powers' (8.1), a later slice.
    let tag = f.w.snapshot(Frame(1)).row_tag(oid(BALL), "Protection").unwrap();
    let ops = vec![Op::RemoveRow { id: Target::Existing(oid(BALL)), component: ids::PROTECTION, instance: 0, tag }];
    assert_eq!(reason(&edit(&mut f.w, 1, &mut f.a, ops)), Reason::Protected { owner: f.kb });
}

#[test]
fn the_leavers_later_entries_are_refused() {
    let mut f = fx();
    f.w.leave(f.kb, leave_entry()).unwrap();
    assert!(!applied(&input(&mut f.w, 1, &mut f.b, grab(BALL, f.hands_b[1])).0), "its hands are gone");
    let paint = vec![Op::SetRow { id: Target::Existing(oid(BALL)), row: row(Appearance::default()) }];
    assert_eq!(reason(&edit(&mut f.w, 1, &mut f.b, paint)), Reason::NoPermission);
}

#[test]
fn a_non_member_cannot_leave_and_nothing_changes() {
    let mut f = fx();
    let before = f.w.root_hash(D);
    let stranger = MemberKey([9; 32]);
    assert_eq!(f.w.leave(stranger, leave_entry()), Err(WorldError::NotAMember(stranger)));
    assert_eq!(f.w.root_hash(D), before);
    f.w.leave(f.kb, leave_entry()).unwrap();
    assert_eq!(f.w.leave(f.kb, leave_entry()), Err(WorldError::NotAMember(f.kb)), "twice");
}

#[test]
fn leave_then_readmit_replays_to_the_same_root_and_gives_new_hands() {
    let f = fx();
    let step = |w: &mut World| {
        w.leave(f.kb, leave_entry()).unwrap();
        w.admit(m1_line(), f.kb, perm::EDIT, Frame(5), Pos::ORIGIN).unwrap()
    };
    let mut w1 = f.w.clone();
    let new = step(&mut w1);
    for h in new {
        assert!(!f.hands_b.contains(&h), "the ids include the frame");
    }
    // A rollback across the leave: re-stepping from the world before it reaches the same root.
    let mut w2 = f.w.clone();
    assert_eq!(step(&mut w2), new);
    assert_eq!(w1.root_hash(D), w2.root_hash(D));
    assert_ne!(w1.root_hash(D), f.w.root_hash(D));
}

#[test]
fn no_edit_deletes_a_hand_or_a_subtree_holding_one() {
    let mut f = fx();
    let refused = Reason::Invalid { field: Some(FieldPath { component: ids::HAND.0, field: u32::MAX }) };
    // Not another member's hand, nor one's own: hands are made and removed only by admit and leave (7.5).
    for (b, hand) in [(&mut f.a, f.hands_b[0]), (&mut f.b, f.hands_b[0])] {
        let del = edit(&mut f.w, 0, b, vec![Op::Delete { id: Target::Existing(hand) }]);
        assert_eq!(reason(&del), refused);
    }
    // Nor a subtree that holds one (built by trusted calls, since no edit can move a hand).
    f.w.create(oid(HELD), Some(oid(BALL)), f.kb, pe_world::GENESIS).unwrap();
    f.w.insert(oid(HELD), Transform::default()).unwrap();
    f.w.insert(oid(HELD), Hand { owner: f.kb, ..Hand::default() }).unwrap();
    let del = edit(&mut f.w, 0, &mut f.a, vec![Op::Delete { id: Target::Existing(oid(BALL)) }]);
    assert_eq!(reason(&del), refused);
    assert!(f.w.tombstone(f.hands_b[0]).is_none());
}

/// Hands are logic's alone (7.5; @coordinator-opus m7821f3319dc2, mb4ecbbb53073): no edit moves a hand, its owner's
/// included, or writes or removes its transform, body or shape; an object may still be moved under a hand, and a
/// subtree holding a hand may move.
#[test]
fn no_edit_moves_a_hand_or_writes_its_transform() {
    let mut f = fx();
    let refused = Reason::Invalid { field: Some(FieldPath { component: ids::HAND.0, field: u32::MAX }) };
    let hand = f.hands_b[0];
    let tag = u128::from_le_bytes(f.w.store().row_version(pe_sim::ComponentId::TRANSFORM, hand).unwrap().tag);
    let up = |id, parent| Op::Reparent { id: Target::Existing(id), parent, index: None, keep_world: true };
    let refusals = [
        up(hand, Target::Existing(oid(BALL))),
        up(hand, Target::Root),
        Op::SetRow { id: Target::Existing(hand), row: row(Transform::at(at(0.0, 2.0, 0.0))) },
        Op::SetField {
            id: Target::Existing(hand),
            component: pe_sim::ComponentId::TRANSFORM,
            instance: 0,
            field: 1,
            value: bytemuck::bytes_of(&[0.0f64, 2.0, 0.0]).to_vec(),
        },
        Op::RemoveRow { id: Target::Existing(hand), component: pe_sim::ComponentId::TRANSFORM, instance: 0, tag },
        // Nor makes it a body, which physics would then move (@chunks-reviewer's Medium).
        Op::SetRow { id: Target::Existing(hand), row: row(Body::default()) },
        Op::SetRow {
            id: Target::Existing(hand),
            row: row(Shape { kind: shape_kind::SPHERE, size: [0.05, 0.0, 0.0], ..Shape::default() }),
        },
    ];
    for op in refusals {
        for b in [&mut f.a, &mut f.b] {
            let o = edit(&mut f.w, 0, b, vec![op.clone()]);
            assert_eq!(reason(&o), refused, "{op:?}");
        }
    }
    // Another of the hand's rows is the edit's to write, and an object may be moved under it.
    f.w.create(oid(BOX), None, f.kb, pe_world::GENESIS).unwrap();
    f.w.insert(oid(BOX), Transform::at(at(0.5, 1.0, 0.0))).unwrap();
    let name =
        edit(&mut f.w, 0, &mut f.b, vec![Op::SetRow { id: Target::Existing(hand), row: row(Appearance::default()) }]);
    assert!(applied(&name), "{name:?}");
    let under = edit(&mut f.w, 0, &mut f.a, vec![up(oid(BOX), Target::Existing(hand))]);
    assert!(applied(&under), "{under:?}");
    // A subtree that holds a hand (built by trusted calls) may move.
    f.w.create(oid(HELD), Some(oid(BOX)), f.kb, pe_world::GENESIS).unwrap();
    f.w.insert(oid(HELD), Transform::default()).unwrap();
    f.w.insert(oid(HELD), Hand { owner: f.kb, ..Hand::default() }).unwrap();
    let moved = edit(&mut f.w, 0, &mut f.a, vec![up(oid(BOX), Target::Root)]);
    assert!(applied(&moved), "{moved:?}");
}

//! The record ceiling (world-model.md 6.6, R_max; @coordinator-opus): no edit leaves an object's
//! record or the tombstones table's record past `tree::MAX_RECORD_BYTES` (256 MiB), since pe-diverge pulls a large
//! record in pieces and can check it only whole. The tests lower the ceiling on their own thread
//! (`test_util::set_record_limit`), so they reach it without 256 MiB of state.

use bytes::Bytes;
use pe_core::{Frame, ObjectId, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, DomainId, Outcome, Reason};
use pe_sim::{Component, Transform};
use pe_world::components::{Appearance, Name, Shape};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::tree::{MAX_RECORD_BYTES, record_len, table, table_record, tombstones_record_len};
use pe_world::{BlobRef, BlobStore, World, perm};

fn row<T: Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

fn tx(ops: Vec<Op>) -> Transaction {
    Transaction { actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![], inline: vec![] }
}

struct Fx {
    keys: EntryBuilder,
    w: World,
}

fn fx() -> Fx {
    let keys = TestKeys::new(1, 1);
    let mut w = World::new(1, BlobStore::default());
    w.set_member(keys.key(0), perm::EDIT);
    Fx { keys: EntryBuilder::new(&keys, 0), w }
}

fn apply(f: &mut Fx, t: Transaction) -> Outcome {
    let e = f.keys.next(Class::Edit, Frame(1), Bytes::from(t.encode()));
    f.w.apply_edit(Frame(1), &e).0
}

/// An object with a position far from the origin (its `Pos` encodes long), a shape, a look, and a child.
fn thing(new: u32) -> Op {
    let far = Pos::at_chunk([12_345, -7, 999_999]).unwrap();
    Op::Create {
        new,
        parent: Target::Root,
        index: None,
        rows: vec![row(Transform::at(far)), row(Shape::default()), row(Appearance::default())],
    }
}

fn created(o: &Outcome) -> Vec<ObjectId> {
    match o {
        Outcome::Applied { created, .. } => created.clone(),
        r => panic!("refused: {r:?}"),
    }
}

#[test]
fn the_lengths_are_the_encoders() {
    let mut f = fx();
    let ids = created(&apply(&mut f, tx((0..4).map(thing).collect())));
    let child = Op::Create {
        new: 0,
        parent: Target::Existing(ids[0]),
        index: None,
        rows: vec![row(Transform::at(Pos::ORIGIN)), row(Name { text: BlobRef { hash: [3; 32], len: 900 } })],
    };
    let kid = created(&apply(&mut f, tx(vec![child])))[0];
    for &id in ids.iter().chain([&kid]) {
        assert_eq!(record_len(f.w.store(), id), f.w.object_record(id).map(|r| r.len()), "{id:?}");
    }
    assert_eq!(record_len(f.w.store(), ObjectId::from_u128(77)), None);
    let empty = table_record(&f.w.table_entries(table::TOMBSTONES)).len();
    assert_eq!(tombstones_record_len(&f.w), empty);
    // A deleted subtree (a root and its child, with a parent link) and a deleted root.
    apply(&mut f, tx(vec![Op::Delete { id: Target::Existing(ids[0]) }, Op::Delete { id: Target::Existing(ids[1]) }]));
    let full = table_record(&f.w.table_entries(table::TOMBSTONES)).len();
    assert!(full > empty);
    assert_eq!(tombstones_record_len(&f.w), full);
}

#[test]
fn a_delete_that_would_take_the_tombstones_past_the_ceiling_is_refused() {
    let mut f = fx();
    let ids = created(&apply(&mut f, tx((0..3).map(thing).collect())));
    apply(&mut f, tx(vec![Op::Delete { id: Target::Existing(ids[0]) }]));
    let one = tombstones_record_len(&f.w);
    // Room for the second tombstone but not the third.
    pe_world::test_util::set_record_limit(one + (one - 8) + 4);
    let before = f.w.root_hash(DomainId(0));
    let o = apply(
        &mut f,
        tx(vec![Op::Delete { id: Target::Existing(ids[1]) }, Op::Delete { id: Target::Existing(ids[2]) }]),
    );
    let limit = (one + (one - 8) + 4) as u32;
    assert!(matches!(o, Outcome::Refused { reason: Reason::TooLarge { limit: l }, .. } if l == limit), "{o:?}");
    assert_eq!(f.w.root_hash(DomainId(0)), before, "a refused edit changes nothing");
    // One at a time: the first fits, the second is refused.
    assert!(matches!(apply(&mut f, tx(vec![Op::Delete { id: Target::Existing(ids[1]) }])), Outcome::Applied { .. }));
    let o = apply(&mut f, tx(vec![Op::Delete { id: Target::Existing(ids[2]) }]));
    assert!(matches!(o, Outcome::Refused { reason: Reason::TooLarge { .. }, .. }), "{o:?}");
    // An undelete shrinks the table, so the ceiling never refuses it: only an edit that grows the table is checked.
    assert!(matches!(apply(&mut f, tx(vec![Op::Undelete { id: ids[0] }])), Outcome::Applied { .. }));
}

#[test]
fn an_edit_that_would_take_an_object_past_the_ceiling_is_refused() {
    let mut f = fx();
    let id = created(&apply(&mut f, tx(vec![thing(0)])))[0];
    let now = record_len(f.w.store(), id).unwrap();
    pe_world::test_util::set_record_limit(now);
    let before = f.w.root_hash(DomainId(0));
    // A new row would add to the record; rewriting an existing one keeps its length.
    let named = Op::SetRow { id: Target::Existing(id), row: row(Name { text: BlobRef { hash: [4; 32], len: 900 } }) };
    let o = apply(&mut f, tx(vec![named]));
    assert!(matches!(o, Outcome::Refused { reason: Reason::TooLarge { .. }, .. }), "{o:?}");
    assert_eq!(f.w.root_hash(DomainId(0)), before);
    let look =
        Op::SetRow { id: Target::Existing(id), row: row(Appearance { roughness: 0.25, ..Appearance::default() }) };
    assert!(matches!(apply(&mut f, tx(vec![look])), Outcome::Applied { .. }));
    // A created object past the ceiling is refused too.
    let o = apply(&mut f, tx(vec![thing(0)]));
    assert!(matches!(o, Outcome::Applied { .. }), "a new object of the same length fits: {o:?}");
    pe_world::test_util::set_record_limit(now - 1);
    let o = apply(&mut f, tx(vec![thing(0)]));
    assert!(matches!(o, Outcome::Refused { reason: Reason::TooLarge { .. }, .. }), "{o:?}");
}

#[test]
fn the_ceiling_is_256_mebibytes() {
    assert_eq!(MAX_RECORD_BYTES, 256 * 1024 * 1024);
}

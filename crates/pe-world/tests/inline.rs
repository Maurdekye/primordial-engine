//! Inline blobs (world-model.md 3.4; @coordinator-opus's ruling m84f3b5262a91): a blob of at most 256 bytes rides
//! inside the signed entry that introduces it, so a machine that applies the entry has the bytes, and apply checks
//! each one against what the ops introduce.

use bytes::Bytes;
use pe_core::{Frame, ObjectId, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Outcome, Reason, SignedEntry};
use pe_sim::{Component, Transform};
use pe_world::components::{Name, Shape};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::inline::{MAX_INLINE_BLOB, MAX_INLINE_BYTES, is_inline};
use pe_world::{BlobRef, BlobStore, World, perm};

fn row<T: Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

fn tx(ops: Vec<Op>, inline: Vec<Vec<u8>>) -> Transaction {
    Transaction { actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![], inline }
}

/// Two machines, each with its own blob store, and member 0's entries.
struct Fx {
    a: EntryBuilder,
    here: World,
    there: World,
}

fn fx() -> Fx {
    let keys = TestKeys::new(1, 2);
    let world = || {
        let mut w = World::new(1, BlobStore::default());
        w.set_member(keys.key(0), perm::EDIT);
        w
    };
    Fx { a: EntryBuilder::new(&keys, 0), here: world(), there: world() }
}

fn entry(f: &mut Fx, t: &Transaction) -> SignedEntry {
    f.a.next(Class::Edit, Frame(1), Bytes::from(t.encode()))
}

/// The entry applied on both machines; both give the same outcome.
fn both(f: &mut Fx, t: &Transaction) -> Outcome {
    let e = entry(f, t);
    let (a, _) = f.here.apply_edit(Frame(1), &e);
    let (b, _) = f.there.apply_edit(Frame(1), &e);
    assert_eq!(a, b, "the same verdict on every machine");
    a
}

fn named(text: &[u8]) -> Op {
    Op::Create {
        new: 0,
        parent: Target::Root,
        index: None,
        rows: vec![row(Transform::at(Pos::ORIGIN)), row(Name { text: BlobRef::of(text) })],
    }
}

fn sorted(mut v: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    v.sort_by_key(|b| BlobRef::of(b).hash);
    v
}

fn created(o: &Outcome) -> ObjectId {
    match o {
        Outcome::Applied { created, .. } => created[0],
        r => panic!("refused: {r:?}"),
    }
}

fn refused(o: &Outcome) -> Reason {
    match o {
        Outcome::Refused { reason, .. } => reason.clone(),
        a => panic!("applied: {a:?}"),
    }
}

#[test]
fn a_name_reaches_a_machine_that_never_had_it() {
    let mut f = fx();
    let o = both(&mut f, &tx(vec![named(b"Cup")], vec![b"Cup".to_vec()]));
    let id = created(&o);
    assert_eq!(f.there.snapshot(Frame(1)).name(id).as_deref(), Some("Cup"));
    assert_eq!(f.here.root_hash(pe_seams::DomainId(0)), f.there.root_hash(pe_seams::DomainId(0)));
}

#[test]
fn the_codec_keeps_one_encoding() {
    let t = tx(vec![named(b"a"), named(b"b")], sorted(vec![b"a".to_vec(), b"b".to_vec()]));
    assert_eq!(Transaction::decode(&t.encode()), Some(t.clone()));
    let mut unsorted = t.clone();
    unsorted.inline.reverse();
    assert_eq!(Transaction::decode(&unsorted.encode()), None, "out of hash order");
    let mut twice = t.clone();
    twice.inline = vec![b"a".to_vec(), b"a".to_vec()];
    assert_eq!(Transaction::decode(&twice.encode()), None, "a blob twice");
    let mut empty = t.clone();
    empty.inline = vec![Vec::new()];
    assert_eq!(Transaction::decode(&empty.encode()), None, "the empty blob is never inline");
    let mut big = t;
    big.inline = vec![vec![7; MAX_INLINE_BLOB + 1]];
    assert_eq!(Transaction::decode(&big.encode()), None, "over 256 bytes");
}

#[test]
fn a_blob_no_op_introduces_is_refused_and_nothing_is_stored() {
    let mut f = fx();
    let before = f.there.root_hash(pe_seams::DomainId(0));
    let stray = b"stray".to_vec();
    let o = both(&mut f, &tx(vec![named(b"Cup")], sorted(vec![b"Cup".to_vec(), stray.clone()])));
    assert_eq!(refused(&o), Reason::Invalid { field: None });
    assert_eq!(f.there.root_hash(pe_seams::DomainId(0)), before);
    assert!(f.there.blobs().get(&BlobRef::of(&stray)).is_none());
    assert!(f.there.blobs().get(&BlobRef::of(b"Cup")).is_none());
}

#[test]
fn over_a_kibibyte_is_too_large() {
    let mut f = fx();
    let names: Vec<Vec<u8>> = (0..5u8).map(|i| vec![b'a' + i; 220]).collect();
    let ops = names.iter().enumerate().map(|(i, n)| match named(n) {
        Op::Create { rows, parent, index, .. } => Op::Create { new: i as u32, parent, index, rows },
        _ => unreachable!(),
    });
    let o = both(&mut f, &tx(ops.collect(), sorted(names.clone())));
    assert_eq!(refused(&o), Reason::TooLarge { limit: MAX_INLINE_BYTES as u32 });
    // Four of them fit.
    let mut f = fx();
    let ops = names[..4].iter().enumerate().map(|(i, n)| match named(n) {
        Op::Create { rows, parent, index, .. } => Op::Create { new: i as u32, parent, index, rows },
        _ => unreachable!(),
    });
    assert!(matches!(both(&mut f, &tx(ops.collect(), sorted(names[..4].to_vec()))), Outcome::Applied { .. }));
}

#[test]
fn set_row_and_set_field_introduce_and_the_inverse_carries_the_old_bytes() {
    let mut f = fx();
    let id = created(&both(&mut f, &tx(vec![named(b"Cup")], vec![b"Cup".to_vec()])));
    // A rename by SetRow.
    let set = Op::SetRow { id: Target::Existing(id), row: row(Name { text: BlobRef::of(b"Mug") }) };
    let o = both(&mut f, &tx(vec![set], vec![b"Mug".to_vec()]));
    assert_eq!(f.there.snapshot(Frame(1)).name(id).as_deref(), Some("Mug"));
    let Outcome::Applied { inverse: Some(inv), .. } = o else { panic!("applied with an inverse") };
    let inv = Transaction::decode(&inv).expect("the inverse decodes");
    assert_eq!(inv.inline, vec![b"Cup".to_vec()], "undoing restores Cup, and carries its bytes");
    // A rename by SetField of the blob field.
    let field = Op::SetField {
        id: Target::Existing(id),
        component: Name::ID,
        instance: 0,
        field: 0,
        value: bytemuck::bytes_of(&BlobRef::of(b"Jug")).to_vec(),
    };
    both(&mut f, &tx(vec![field], vec![b"Jug".to_vec()]));
    assert_eq!(f.there.snapshot(Frame(1)).name(id).as_deref(), Some("Jug"));
}

#[test]
fn a_small_sim_blob_is_inline_too() {
    // The coordinator's (a), m84f3b5262a91: SimBlobRef values of 256 bytes or less are of the inline class.
    let mut f = fx();
    let mesh = vec![3u8; 200];
    let shape = Shape { mesh: BlobRef::of(&mesh), ..Default::default() };
    let create = Op::Create {
        new: 0,
        parent: Target::Root,
        index: None,
        rows: vec![row(Transform::at(Pos::ORIGIN)), row(shape)],
    };
    let o = both(&mut f, &tx(vec![create], vec![mesh.clone()]));
    assert!(matches!(o, Outcome::Applied { .. }), "{o:?}");
    assert_eq!(f.there.blobs().get(&BlobRef::of(&mesh)).as_deref(), Some(&mesh[..]));
}

#[test]
fn the_inline_class_and_the_empty_blob() {
    assert!(!is_inline(&BlobRef::NONE));
    assert!(!is_inline(&BlobRef::of(b"")), "every machine has the empty blob");
    assert!(is_inline(&BlobRef::of(&[1; MAX_INLINE_BLOB])));
    assert!(!is_inline(&BlobRef::of(&[1; MAX_INLINE_BLOB + 1])));
    assert_eq!(BlobStore::default().get(&BlobRef::of(b"")).as_deref(), Some(&[][..]));
}

#[test]
fn a_copy_introduces_nothing() {
    let mut f = fx();
    let id = created(&both(&mut f, &tx(vec![named(b"Cup")], vec![b"Cup".to_vec()])));
    let copy = Op::Copy { source: Target::Existing(id), new: 0, parent: Target::Root, index: None, size: 1 };
    assert_eq!(refused(&both(&mut f, &tx(vec![copy.clone()], vec![b"Cup".to_vec()]))), Reason::Invalid { field: None });
    assert!(matches!(both(&mut f, &tx(vec![copy], vec![])), Outcome::Applied { .. }));
}

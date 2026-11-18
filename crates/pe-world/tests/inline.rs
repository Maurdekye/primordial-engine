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
    let mut big = t.clone();
    big.inline = vec![vec![7; MAX_INLINE_BLOB + 1]];
    assert_eq!(Transaction::decode(&big.encode()), None, "over 256 bytes");
    // Without inline blobs the section is absent, so the payload is what it was before inline blobs existed; an
    // explicit empty section is a second encoding and is refused.
    let mut none = t;
    none.inline.clear();
    let bytes = none.encode();
    assert_eq!(Transaction::decode(&bytes), Some(none));
    let mut explicit = bytes;
    explicit.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(Transaction::decode(&explicit), None, "an empty section");
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
fn a_reference_without_its_bytes_is_refused_and_the_dry_run_agrees() {
    // Phase 3 (3.4): every inline-class reference an op introduces comes with its bytes, so every machine that
    // applies the entry has them; one that comes without is `Invalid`, on every machine, and changes nothing.
    let mut f = fx();
    let before = f.there.root_hash(pe_seams::DomainId(0));
    let bare = tx(vec![named(b"Cup")], vec![]);
    let e = entry(&mut f, &bare);
    let d = pe_world::Draft { author: e.entry.author, line: e.entry.line, chain: e.entry.chain, seq: e.entry.seq };
    assert_eq!(refused(&f.here.dry_run_draft(Frame(1), &d, &bare)), Reason::Invalid { field: None });
    let (a, _) = f.here.apply_edit(Frame(1), &e);
    let (b, _) = f.there.apply_edit(Frame(1), &e);
    assert_eq!(a, b);
    assert_eq!(refused(&a), Reason::Invalid { field: None });
    assert_eq!(f.there.root_hash(pe_seams::DomainId(0)), before);
    // Two names and only one's bytes: still refused.
    let mug = match named(b"Mug") {
        Op::Create { rows, parent, index, .. } => Op::Create { new: 1, parent, index, rows },
        _ => unreachable!(),
    };
    let half = tx(vec![named(b"Cup"), mug], vec![b"Cup".to_vec()]);
    assert_eq!(refused(&both(&mut f, &half)), Reason::Invalid { field: None });
    // A reference outside the inline class needs no bytes: the empty blob, or one over 256 bytes.
    let big = BlobRef { hash: [7; 32], len: MAX_INLINE_BLOB as u64 + 1 };
    let ops = vec![Op::Create {
        new: 0,
        parent: Target::Root,
        index: None,
        rows: vec![row(Transform::at(Pos::ORIGIN)), row(Name { text: big })],
    }];
    assert!(matches!(both(&mut f, &tx(ops, vec![])), Outcome::Applied { .. }));
    assert!(matches!(both(&mut f, &tx(vec![named(b"")], vec![])), Outcome::Applied { .. }));
}

#[test]
fn over_two_kibibytes_is_too_large() {
    // pe-world's hard cap is 2 KiB, so an undo that grew past pe-api's 1 KiB still fits (@coordinator-opus
    // m67c4366304d7, point 3); exactly 2,048 bytes apply and 2,049 do not (@engine-architect mc90307e8c499).
    let names = |extra: bool| -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = (0..8u8).map(|i| vec![b'a' + i; MAX_INLINE_BLOB]).collect();
        if extra {
            v.push(b"z".to_vec());
        }
        v
    };
    let ops = |names: &[Vec<u8>]| -> Vec<Op> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| match named(n) {
                Op::Create { rows, parent, index, .. } => Op::Create { new: i as u32, parent, index, rows },
                _ => unreachable!(),
            })
            .collect()
    };
    let at = names(false);
    assert_eq!(at.iter().map(Vec::len).sum::<usize>(), MAX_INLINE_BYTES);
    assert!(matches!(both(&mut fx(), &tx(ops(&at), sorted(at.clone()))), Outcome::Applied { .. }));
    let over = names(true);
    let o = both(&mut fx(), &tx(ops(&over), sorted(over.clone())));
    assert_eq!(refused(&o), Reason::TooLarge { limit: 2048 });
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

/// A whole-world import of `w`, its inline blobs as `Tree::export_blobs` gives them (S2 v1.8).
fn import_of(w: &World) -> pe_seams::CutImport {
    use pe_seams::{LeafKey, PhysicsImage, Tree};
    let d = pe_seams::DomainId(0);
    let t = w.tree(d);
    let mut keys: Vec<LeafKey> = w.store().objects().iter().map(|&id| LeafKey::Object(id)).collect();
    keys.extend((0..pe_world::tree::table::COUNT as u8).map(LeafKey::Table));
    keys.push(LeafKey::Config);
    pe_seams::CutImport {
        cut: pe_seams::Cut { at: pe_seams::CutRef { line: w.line(), height: 0, frame: w.frame() }, heads: Vec::new() },
        target_root: t.root(),
        records: t.export(&keys),
        absent: Vec::new(),
        physics: Some(PhysicsImage { domain: d, frame: w.frame(), global: pe_core::Hash::ZERO, bytes: Bytes::new() }),
        blobs: t.export_blobs(&keys),
    }
}

/// A joiner with its own, empty blob store.
fn joiner() -> World {
    World::new(0, BlobStore::default())
}

#[test]
fn a_joiner_gets_the_names_of_the_state_it_imports() {
    let mut f = fx();
    let id = created(&both(&mut f, &tx(vec![named(b"Cup")], vec![b"Cup".to_vec()])));
    let imp = import_of(&f.here);
    assert_eq!(imp.blobs, vec![Bytes::from_static(b"Cup")]);
    let got = joiner().import(pe_seams::DomainId(0), &imp).unwrap();
    assert_eq!(got.snapshot(Frame(1)).name(id).as_deref(), Some("Cup"));
    // Without the bytes a joiner that lacks them refuses the import (phase 3b), so the repair asks another donor:
    // nothing fetches an inline blob later, so the name would stay blank for good.
    let mut bare = imp.clone();
    bare.blobs.clear();
    assert_eq!(
        joiner().import(pe_seams::DomainId(0), &bare).err(),
        Some(pe_seams::ImportError::MissingBlob(pe_core::Hash(BlobRef::of(b"Cup").hash)))
    );
    // One that holds them already needs none.
    let held = joiner();
    held.blobs().put(b"Cup");
    let got = held.import(pe_seams::DomainId(0), &bare).unwrap();
    assert_eq!(got.snapshot(Frame(1)).name(id).as_deref(), Some("Cup"));
}

#[test]
fn an_import_refuses_a_blob_no_row_references_or_out_of_order() {
    let mut f = fx();
    both(&mut f, &tx(vec![named(b"Cup")], vec![b"Cup".to_vec()]));
    both(&mut f, &tx(vec![named(b"Mug")], vec![b"Mug".to_vec()]));
    let imp = import_of(&f.here);
    assert_eq!(imp.blobs.len(), 2);
    let mut stray = imp.clone();
    stray.blobs.push(Bytes::from_static(b"stray"));
    stray.blobs.sort_by_key(|b| BlobRef::of(b).hash);
    let at = stray.blobs.iter().position(|b| &b[..] == b"stray").unwrap();
    assert_eq!(joiner().import(pe_seams::DomainId(0), &stray).err(), Some(pe_seams::ImportError::BadBlob(at)));
    let mut swapped = imp;
    swapped.blobs.reverse();
    assert_eq!(joiner().import(pe_seams::DomainId(0), &swapped).err(), Some(pe_seams::ImportError::BadBlob(1)));
}

#[test]
fn an_import_without_a_small_sim_blob_is_refused() {
    let mut f = fx();
    let mesh = vec![3u8; 200];
    let create = Op::Create {
        new: 0,
        parent: Target::Root,
        index: None,
        rows: vec![row(Transform::at(Pos::ORIGIN)), row(Shape { mesh: BlobRef::of(&mesh), ..Default::default() })],
    };
    both(&mut f, &tx(vec![create], vec![mesh.clone()]));
    let mut imp = import_of(&f.here);
    assert_eq!(imp.blobs, vec![Bytes::from(mesh.clone())]);
    imp.blobs.clear();
    assert_eq!(
        joiner().import(pe_seams::DomainId(0), &imp).err(),
        Some(pe_seams::ImportError::MissingBlob(pe_core::Hash(BlobRef::of(&mesh).hash)))
    );
    // One that already has the bytes needs none.
    let here = World::new(0, f.there.blobs().clone());
    assert!(here.import(pe_seams::DomainId(0), &imp).is_ok());
}

#[test]
fn a_deleted_objects_name_travels_with_the_tombstones() {
    // Undelete restores a tombstone's kept rows, so their blobs are referenced (@engine-architect ma16d20a4d421).
    let mut f = fx();
    let id = created(&both(&mut f, &tx(vec![named(b"Cup")], vec![b"Cup".to_vec()])));
    both(&mut f, &tx(vec![Op::Delete { id: Target::Existing(id) }], vec![]));
    let imp = import_of(&f.here);
    assert_eq!(imp.blobs, vec![Bytes::from_static(b"Cup")]);
    let mut got = joiner().import(pe_seams::DomainId(0), &imp).unwrap();
    assert_eq!(got.blobs().get(&BlobRef::of(b"Cup")).as_deref(), Some(&b"Cup"[..]));
    // The tombstones are imported state, so the joiner can apply the undo, a bare `Undelete` that carries no bytes
    // (@coordinator-opus m67c4366304d7, point 1).
    let undo = tx(vec![Op::Undelete { id }], vec![]);
    let e = entry(&mut f, &undo);
    let (o, _) = got.apply_edit(Frame(1), &e);
    assert!(matches!(o, Outcome::Applied { .. }), "{o:?}");
    assert_eq!(got.snapshot(Frame(1)).name(id).as_deref(), Some("Cup"));
}

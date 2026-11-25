//! Carried simulation blobs (world-model.md 3.4; @coordinator-opus): a blob a `SimBlobRef` field
//! references, 257 bytes to 960 KiB (a mesh, a hull), rides inside the signed entry that introduces it, in the same
//! list as the inline class, and in a joiner's import. So every machine that has the entry or the state has the
//! bytes, and none builds a collider another can't. A display blob over 256 bytes still travels by reference only.

use bytes::Bytes;
use pe_core::{Frame, ObjectId, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, DomainId, ImportError, Outcome, Reason, SignedEntry};
use pe_sim::{Component, Transform};
use pe_world::components::{Name, Shape};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::inline::{MAX_CARRIED_BLOB, MAX_CARRIED_BYTES, MAX_INLINE_BLOB, MAX_INLINE_BYTES, is_carried};
use pe_world::{BlobRef, BlobStore, World, perm};

fn row<T: Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

fn tx(ops: Vec<Op>, inline: Vec<Vec<u8>>) -> Transaction {
    let mut inline = inline;
    inline.sort_by_key(|b| BlobRef::of(b).hash);
    Transaction { actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![], inline }
}

/// Machine A, where the edits are made, and machine B, which only applies A's entries; each has its own store.
struct Fx {
    keys: EntryBuilder,
    a: World,
    b: World,
}

fn fx() -> Fx {
    let keys = TestKeys::new(1, 2);
    let world = || {
        let mut w = World::new(1, BlobStore::default());
        w.set_member(keys.key(0), perm::EDIT);
        w
    };
    Fx { keys: EntryBuilder::new(&keys, 0), a: world(), b: world() }
}

fn entry(f: &mut Fx, t: &Transaction) -> SignedEntry {
    let bytes = t.encode();
    assert!(bytes.len() <= pe_world::edit::MAX_EDIT_BYTES, "{} bytes fit in one entry", bytes.len());
    f.keys.next(Class::Edit, Frame(1), Bytes::from(bytes))
}

/// The entry applied on both machines; both give the same outcome and the same root.
fn both(f: &mut Fx, t: &Transaction) -> Outcome {
    let e = entry(f, t);
    let (a, _) = f.a.apply_edit(Frame(1), &e);
    let (b, _) = f.b.apply_edit(Frame(1), &e);
    assert_eq!(a, b, "the same verdict on every machine");
    assert_eq!(f.a.root_hash(DomainId(0)), f.b.root_hash(DomainId(0)));
    a
}

/// A mesh blob of `n` bytes, distinct by `seed`.
fn mesh(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

fn shaped(new: u32, m: &[u8]) -> Op {
    Op::Create {
        new,
        parent: Target::Root,
        index: None,
        rows: vec![row(Transform::at(Pos::ORIGIN)), row(Shape { mesh: BlobRef::of(m), ..Default::default() })],
    }
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
fn the_carried_class() {
    let r = |n: usize| BlobRef::of(&vec![1; n]);
    assert!(is_carried(&r(MAX_INLINE_BLOB), false), "the inline class, display or simulation");
    assert!(is_carried(&r(MAX_INLINE_BLOB + 1), true));
    assert!(is_carried(&BlobRef { hash: [2; 32], len: MAX_CARRIED_BLOB as u64 }, true));
    assert!(!is_carried(&BlobRef { hash: [2; 32], len: MAX_CARRIED_BLOB as u64 + 1 }, true), "fetched, later");
    assert!(!is_carried(&r(MAX_INLINE_BLOB + 1), false), "a display blob over 256 bytes is by reference");
    assert!(!is_carried(&BlobRef::NONE, true));
}

#[test]
fn a_mesh_reaches_a_machine_that_never_had_it() {
    let mut f = fx();
    let m = mesh(300 * 1024, 1);
    let o = both(&mut f, &tx(vec![shaped(0, &m)], vec![m.clone()]));
    let id = created(&o);
    assert_eq!(f.b.blobs().get(&BlobRef::of(&m)).as_deref(), Some(&m[..]), "B holds A's mesh bytes");
    assert_eq!(f.b.store().get::<Shape>(id).unwrap().mesh, BlobRef::of(&m));
}

#[test]
fn a_mesh_reference_without_its_bytes_is_refused_and_the_dry_run_agrees() {
    let mut f = fx();
    let before = f.b.root_hash(DomainId(0));
    let m = mesh(4096, 2);
    let bare = tx(vec![shaped(0, &m)], vec![]);
    let e = entry(&mut f, &bare);
    let d = pe_world::Draft { author: e.entry.author, line: e.entry.line, chain: e.entry.chain, seq: e.entry.seq };
    assert_eq!(refused(&f.a.dry_run_draft(Frame(1), &d, &bare)), Reason::Invalid { field: None });
    let (a, _) = f.a.apply_edit(Frame(1), &e);
    let (b, _) = f.b.apply_edit(Frame(1), &e);
    assert_eq!(a, b);
    assert_eq!(refused(&a), Reason::Invalid { field: None });
    assert_eq!(f.b.root_hash(DomainId(0)), before);
    // A simulation reference past the carried class needs no bytes (it is fetched, later; 3.4's late entry).
    let huge = BlobRef { hash: [9; 32], len: MAX_CARRIED_BLOB as u64 + 1 };
    let far = Op::Create {
        new: 0,
        parent: Target::Root,
        index: None,
        rows: vec![row(Transform::at(Pos::ORIGIN)), row(Shape { mesh: huge, ..Default::default() })],
    };
    assert!(matches!(both(&mut f, &tx(vec![far], vec![])), Outcome::Applied { .. }));
}

#[test]
fn a_display_blob_over_256_bytes_is_never_carried() {
    let mut f = fx();
    let text = vec![b'a'; MAX_INLINE_BLOB + 1];
    let name = |t: &[u8]| Op::Create {
        new: 0,
        parent: Target::Root,
        index: None,
        rows: vec![row(Transform::at(Pos::ORIGIN)), row(Name { text: BlobRef::of(t) })],
    };
    assert_eq!(refused(&both(&mut f, &tx(vec![name(&text)], vec![text.clone()]))), Reason::Invalid { field: None });
    assert!(f.b.blobs().get(&BlobRef::of(&text)).is_none(), "a refused edit stores nothing");
    assert!(matches!(both(&mut f, &tx(vec![name(&text)], vec![])), Outcome::Applied { .. }), "by reference");
}

#[test]
fn over_960_kibibytes_carried_is_too_large_and_the_two_caps_are_apart() {
    let mut f = fx();
    let (m1, m2) = (mesh(500 * 1024, 3), mesh(500 * 1024, 4));
    let o = both(&mut f, &tx(vec![shaped(0, &m1), shaped(1, &m2)], vec![m1.clone(), m2.clone()]));
    assert_eq!(refused(&o), Reason::TooLarge { limit: MAX_CARRIED_BYTES as u32 });
    assert!(f.b.blobs().get(&BlobRef::of(&m1)).is_none(), "nothing stored");
    // A full carried allowance and a full inline one in the same entry: each against its own cap.
    let big = mesh(MAX_CARRIED_BYTES - 64 * 1024, 5);
    let mut ops = vec![shaped(0, &big)];
    let mut inline = vec![big.clone()];
    for k in 0..(MAX_INLINE_BYTES / MAX_INLINE_BLOB) as u32 {
        let small = mesh(MAX_INLINE_BLOB, 10 + k as u8);
        ops.push(shaped(1 + k, &small));
        inline.push(small);
    }
    assert!(matches!(both(&mut f, &tx(ops, inline)), Outcome::Applied { .. }));
    assert_eq!(f.b.blobs().get(&BlobRef::of(&big)).as_deref(), Some(&big[..]));
}

#[test]
fn the_codec_takes_a_carried_blob_up_to_the_cap() {
    let m = mesh(MAX_CARRIED_BLOB, 6);
    let t = tx(vec![shaped(0, &m)], vec![m]);
    assert_eq!(Transaction::decode(&t.encode()), Some(t.clone()));
    let mut over = t;
    over.inline = vec![mesh(MAX_CARRIED_BLOB + 1, 6)];
    assert_eq!(Transaction::decode(&over.encode()), None, "over 960 KiB");
}

#[test]
fn the_inverse_carries_the_old_mesh() {
    let mut f = fx();
    let (old, new) = (mesh(2000, 7), mesh(3000, 8));
    let id = created(&both(&mut f, &tx(vec![shaped(0, &old)], vec![old.clone()])));
    let set =
        Op::SetRow { id: Target::Existing(id), row: row(Shape { mesh: BlobRef::of(&new), ..Default::default() }) };
    let Outcome::Applied { inverse: Some(inv), .. } = both(&mut f, &tx(vec![set], vec![new.clone()])) else {
        panic!("applied with an inverse")
    };
    let inv = Transaction::decode(&inv).expect("the inverse decodes");
    assert_eq!(inv.inline, vec![old.clone()], "undoing restores the old mesh, and carries its bytes");
    assert!(matches!(both(&mut f, &inv), Outcome::Applied { .. }));
    assert_eq!(f.b.store().get::<Shape>(id).unwrap().mesh, BlobRef::of(&old));
}

/// A whole-world import of `w`, its blobs as `Tree::export_blobs` gives them (S2 v1.8).
fn import_of(w: &World) -> pe_seams::CutImport {
    use pe_seams::{LeafKey, PhysicsImage, Tree};
    let d = DomainId(0);
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

#[test]
fn a_joiner_gets_a_mesh_made_before_it_joined() {
    let mut f = fx();
    let m = mesh(64 * 1024, 9);
    let id = created(&both(&mut f, &tx(vec![shaped(0, &m)], vec![m.clone()])));
    let imp = import_of(&f.a);
    assert_eq!(imp.blobs, vec![Bytes::from(m.clone())]);
    let got = World::new(0, BlobStore::default()).import(DomainId(0), &imp).unwrap();
    assert_eq!(got.blobs().get(&BlobRef::of(&m)).as_deref(), Some(&m[..]));
    assert_eq!(got.store().get::<Shape>(id).unwrap().mesh, BlobRef::of(&m));
    // Without the bytes a joiner that lacks them refuses (it could step differently), so the repair asks another donor.
    let mut bare = imp.clone();
    bare.blobs.clear();
    assert_eq!(
        World::new(0, BlobStore::default()).import(DomainId(0), &bare).err(),
        Some(ImportError::MissingBlob(pe_core::Hash(BlobRef::of(&m).hash)))
    );
    assert!(World::new(0, f.b.blobs().clone()).import(DomainId(0), &bare).is_ok(), "one that holds them needs none");
}

#[test]
fn a_deleted_mesh_travels_with_the_tombstones() {
    let mut f = fx();
    let m = mesh(10_000, 11);
    let id = created(&both(&mut f, &tx(vec![shaped(0, &m)], vec![m.clone()])));
    both(&mut f, &tx(vec![Op::Delete { id: Target::Existing(id) }], vec![]));
    let imp = import_of(&f.a);
    assert_eq!(imp.blobs, vec![Bytes::from(m.clone())], "Undelete restores it, so the joiner needs it");
    let got = World::new(0, BlobStore::default()).import(DomainId(0), &imp).unwrap();
    assert_eq!(got.blobs().get(&BlobRef::of(&m)).as_deref(), Some(&m[..]));
}

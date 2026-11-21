//! F3's n = 2 branch (world-model.md 2.3; @diverge-eng's design v5, L3): the branch record, and
//! re-issued edits that keep their original's ids on the new line, each once, only by its author, only unchanged.

use bytes::Bytes;
use pe_core::{EntryRef, Frame, Hash, LineId, ObjectId, Offset, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, CutImport, CutRef, DomainId, LeafKey, Outcome, PhysicsImage, Reason, SignedEntry, Tree};
use pe_sim::{Component, Transform};
use pe_world::branch::{BranchError, Reissue, Sealed, Tail, decode_edit, payload_hash, reissue_payload};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::tree::table;
use pe_world::{BlobStore, World, perm};

const D: DomainId = DomainId(0);

fn row(v: Transform) -> Row {
    Row { component: Transform::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

fn at(x: f64) -> Transform {
    Transform::at(Pos::ORIGIN.checked_add(Offset([x, 0.0, 0.0])).unwrap())
}

fn tx(ops: Vec<Op>) -> Transaction {
    Transaction { inline: vec![], actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] }
}

fn create(x: f64) -> Transaction {
    tx(vec![Op::Create { new: 0, parent: Target::Root, index: None, rows: vec![row(at(x))] }])
}

fn edit(b: &mut EntryBuilder, t: &Transaction) -> SignedEntry {
    b.next(Class::Edit, Frame(1), Bytes::from(t.encode()))
}

fn raw(b: &mut EntryBuilder, payload: Vec<u8>) -> SignedEntry {
    b.next(Class::Edit, Frame(1), Bytes::from(payload))
}

fn created(o: &Outcome) -> Vec<ObjectId> {
    match o {
        Outcome::Applied { created, .. } => created.clone(),
        r => panic!("refused: {r:?}"),
    }
}

fn reference(e: &SignedEntry) -> Reissue {
    Reissue { line: e.entry.line, chain: e.entry.chain, seq: e.entry.seq, entry: e.id() }
}

fn sealed(e: &SignedEntry) -> Sealed {
    Sealed { chain: e.entry.chain, seq: e.entry.seq, entry: e.id(), payload: payload_hash(&e.entry.payload) }
}

/// The transforms of every object, in id order: the state an edit sets, apart from the versions naming entries.
fn transforms(w: &World) -> Vec<(ObjectId, Transform)> {
    w.store().objects().iter().map(|&id| (id, *w.store().get::<Transform>(id).unwrap())).collect()
}

fn line(n: u8) -> LineId {
    LineId(Hash([n; 32]))
}

/// A world on line L where A made X at seq 1 (the base, before the branch's frame f), then, above f: Y at seq 2,
/// an edit of X and Y and a new Z at seq 3. `base` is the world at f; `full` applies the whole of L.
struct Fx {
    keys: TestKeys,
    base: World,
    full: World,
    a: [SignedEntry; 3],
    x: ObjectId,
    y: ObjectId,
}

fn fx() -> Fx {
    let keys = TestKeys::new(1, 3);
    let mut w = World::new(1, BlobStore::default());
    w.switch_line(line(1));
    for i in 0..2 {
        w.set_member(keys.key(i), perm::EDIT);
    }
    let mut a = EntryBuilder::on_line(&keys, 0, line(1));
    let a1 = edit(&mut a, &create(1.0));
    let x = created(&w.apply_edit(Frame(1), &a1).0)[0];
    let base = w.clone();
    let a2 = edit(&mut a, &create(2.0));
    let y = created(&w.apply_edit(Frame(2), &a2).0)[0];
    let a3 = edit(
        &mut a,
        &tx(vec![
            Op::SetRow { id: Target::Existing(x), row: row(at(10.0)) },
            Op::SetRow { id: Target::Existing(y), row: row(at(20.0)) },
            Op::Create { new: 0, parent: Target::Existing(y), index: None, rows: vec![row(at(3.0))] },
        ]),
    );
    assert_eq!(created(&w.apply_edit(Frame(3), &a3).0).len(), 1);
    Fx { keys, base, full: w, a: [a1, a2, a3], x, y }
}

/// `f.base` continued on L' = line(2), from the record `Branch` gives: A's chain at head 1, and B's (empty).
fn branched(f: &Fx) -> World {
    let mut w = f.base.clone();
    w.switch_line(line(2));
    let b = EntryBuilder::on_line(&f.keys, 1, line(1));
    let tails = [
        Tail { chain: f.a[0].entry.chain, member: f.keys.key(0), head: 1 },
        Tail { chain: b.chain(), member: f.keys.key(1), head: 0 },
    ];
    w.begin_branch(line(1), &tails, &[sealed(&f.a[1]), sealed(&f.a[2])]).unwrap();
    w
}

fn reissue(b: &mut EntryBuilder, original: &SignedEntry) -> SignedEntry {
    raw(b, reissue_payload(&original.entry.payload, &reference(original)))
}

#[test]
fn a_re_issued_tail_makes_the_ids_and_state_its_original_made() {
    let f = fx();
    let mut w = branched(&f);
    assert_eq!(w.branch_parent(), Some(line(1)));
    let mut a2 = EntryBuilder::on_line(&f.keys, 0, line(2));
    let e2 = reissue(&mut a2, &f.a[1]);
    assert_eq!(created(&w.apply_edit(Frame(2), &e2).0), vec![f.y], "the create keeps its id on L'");
    let e3 = reissue(&mut a2, &f.a[2]);
    let z = created(&w.apply_edit(Frame(3), &e3).0);
    assert_eq!(z, vec![ObjectId::created(line(1), f.keys.key(0), f.a[2].entry.chain, 3, 0)], "Z has its L id");
    assert_eq!(
        transforms(&w),
        transforms(&f.full),
        "the edit of a pre-f object and of a re-issued one, and the new one"
    );
    assert!(w.store().objects().contains(&f.x));
    assert!(!w.is_sealed(f.a[1].entry.chain, 2) && !w.is_sealed(f.a[2].entry.chain, 3), "each is re-issued once");
    // A plain edit on L' takes L''s coordinates, as any edit does.
    let plain = edit(&mut a2, &create(4.0));
    let id = created(&w.apply_edit(Frame(4), &plain).0)[0];
    assert_eq!(id, ObjectId::created(line(2), f.keys.key(0), plain.entry.chain, plain.entry.seq, 0));
    // The decoder others use gives the original's coordinates for the re-issue, and the entry's for the plain one.
    let (_, o) = decode_edit(&e2.entry).unwrap();
    assert_eq!((o.line, o.chain, o.seq), (line(1), f.a[1].entry.chain, 2));
    let (_, o) = decode_edit(&plain.entry).unwrap();
    assert_eq!((o.line, o.chain, o.seq), (line(2), plain.entry.chain, plain.entry.seq));
}

#[test]
fn a_re_issue_is_refused_unless_sealed_unchanged_by_its_author_and_once() {
    let f = fx();
    let mut w = branched(&f);
    let mut a2 = EntryBuilder::on_line(&f.keys, 0, line(2));
    let mut b2 = EntryBuilder::on_line(&f.keys, 1, line(2));
    let refused = |w: &mut World, e: &SignedEntry| {
        let before = w.root_hash(D);
        let o = w.apply_edit(Frame(2), e).0;
        assert!(matches!(o, Outcome::Refused { op: None, reason: Reason::Invalid { field: None } }), "{o:?}");
        assert_eq!(w.root_hash(D), before, "a refused re-issue changes nothing");
    };
    // Another member re-issuing A's original.
    let by_b = reissue(&mut b2, &f.a[1]);
    refused(&mut w, &by_b);
    // Changed bytes under the original's reference.
    let changed = raw(&mut a2, reissue_payload(&create(9.0).encode(), &reference(&f.a[1])));
    refused(&mut w, &changed);
    // The wrong entry id, the wrong parent line, and an original in the base (not sealed).
    let wrong_entry = Reissue { entry: EntryRef(Hash([7; 32])), ..reference(&f.a[1]) };
    refused(&mut w, &raw(&mut a2, reissue_payload(&f.a[1].entry.payload, &wrong_entry)));
    let wrong_line = Reissue { line: line(9), ..reference(&f.a[1]) };
    refused(&mut w, &raw(&mut a2, reissue_payload(&f.a[1].entry.payload, &wrong_line)));
    refused(&mut w, &reissue(&mut a2, &f.a[0]));
    // Once: the second re-issue of the same original is refused.
    let first = reissue(&mut a2, &f.a[1]);
    created(&w.apply_edit(Frame(2), &first).0);
    refused(&mut w, &reissue(&mut a2, &f.a[1]));
    // A world that never branched refuses every re-issue.
    let mut plain = f.base.clone();
    refused(&mut plain, &reissue(&mut a2, &f.a[1]));
    // A re-issue too short to hold its reference is malformed.
    let short = raw(&mut a2, vec![pe_world::branch::REISSUE, 1, 2]);
    assert!(matches!(w.apply_edit(Frame(2), &short).0, Outcome::Refused { reason: Reason::Malformed, .. }));
}

#[test]
fn a_refused_re_issue_leaves_its_original_sealed() {
    let f = fx();
    let mut w = branched(&f);
    let mut a2 = EntryBuilder::on_line(&f.keys, 0, line(2));
    // On L', X was deleted before A re-issues the edit of it, so the re-issued edit is refused by the edit rules.
    let del = edit(&mut a2, &tx(vec![Op::Delete { id: Target::Existing(f.x) }]));
    assert!(matches!(w.apply_edit(Frame(2), &del).0, Outcome::Applied { .. }));
    created(&w.apply_edit(Frame(2), &reissue(&mut a2, &f.a[1])).0);
    let e3 = reissue(&mut a2, &f.a[2]);
    assert!(matches!(w.apply_edit(Frame(3), &e3).0, Outcome::Refused { .. }));
    assert!(w.is_sealed(f.a[2].entry.chain, 3), "a refused re-issue changes nothing, its original stays sealed");
}

#[test]
fn a_second_branch_re_issues_a_re_issued_edit_with_the_first_ids() {
    let f = fx();
    let mut w = branched(&f);
    let mut a2 = EntryBuilder::on_line(&f.keys, 0, line(2));
    let r2 = reissue(&mut a2, &f.a[1]);
    created(&w.apply_edit(Frame(2), &r2).0);
    let base2 = w.clone();
    let r3 = reissue(&mut a2, &f.a[2]);
    let z = created(&w.apply_edit(Frame(3), &r3).0);
    // L'' = line(3) from L' at A's L'-chain head 1: the re-issued r3 is the suffix, re-issued again.
    let mut w2 = base2;
    w2.switch_line(line(3));
    let tails = [Tail { chain: a2.chain(), member: f.keys.key(0), head: 1 }];
    w2.begin_branch(line(2), &tails, &[sealed(&r3)]).unwrap();
    let mut a3 = EntryBuilder::on_line(&f.keys, 0, line(3));
    let rr3 = reissue(&mut a3, &r3);
    assert_eq!(created(&w2.apply_edit(Frame(3), &rr3).0), z, "ids derive from the innermost original, on L");
    assert_eq!(transforms(&w2), transforms(&f.full));
    let (_, o) = decode_edit(&rr3.entry).unwrap();
    assert_eq!((o.line, o.chain, o.seq), (line(1), f.a[2].entry.chain, 3));
    // The first branch's record is replaced: L's originals can no longer be re-issued on L''.
    assert!(!w2.is_sealed(f.a[1].entry.chain, 2));
}

#[test]
fn a_branch_record_no_branch_can_bind_is_refused() {
    let f = fx();
    let chain = f.a[0].entry.chain;
    let tail = Tail { chain, member: f.keys.key(0), head: 1 };
    let mut w = f.base.clone();
    let root = w.root_hash(D);
    assert_eq!(w.begin_branch(w.line(), &[tail], &[]), Err(BranchError::SameLine));
    w.switch_line(line(2));
    let root2 = w.root_hash(D);
    assert_ne!(root, root2, "the line switch moves the root");
    assert_eq!(w.begin_branch(line(1), &[tail, tail], &[]), Err(BranchError::DuplicateChain(chain)));
    assert_eq!(w.begin_branch(line(1), &[tail], &[sealed(&f.a[0])]), Err(BranchError::NotInSuffix { chain, seq: 1 }));
    let s = sealed(&f.a[1]);
    assert_eq!(w.begin_branch(line(1), &[tail], &[s, s]), Err(BranchError::DuplicateSealed { chain, seq: 2 }));
    assert_eq!(w.root_hash(D), root2, "a refused record changes nothing");
    assert!(w.table_entries(table::LINES).is_empty(), "a world that never branched has an empty lines table");
    w.begin_branch(line(1), &[tail], &[s]).unwrap();
    assert_ne!(w.root_hash(D), root2, "the branch record is in the root");
    assert_eq!(w.table_entries(table::LINES).len(), 2, "the branch row and one sealed row");
}

#[test]
fn a_joiner_imports_the_branch_record_and_re_issues_check_alike() {
    let f = fx();
    let target = branched(&f);
    let t = target.tree(D);
    let mut keys: Vec<LeafKey> = target.store().objects().iter().map(|&id| LeafKey::Object(id)).collect();
    keys.extend((0..table::COUNT as u8).map(LeafKey::Table));
    keys.push(LeafKey::Config);
    let import = CutImport {
        blobs: vec![],
        cut: pe_seams::Cut { at: CutRef { line: target.line(), height: 0, frame: target.frame() }, heads: Vec::new() },
        target_root: t.root(),
        records: t.export(&keys),
        absent: Vec::new(),
        physics: Some(PhysicsImage { domain: D, frame: target.frame(), global: Hash::ZERO, bytes: Default::default() }),
    };
    let mut got = World::new(0, target.blobs().clone()).import(D, &import).unwrap();
    assert_eq!(got.root_hash(D), target.root_hash(D));
    assert_eq!(got.branch_parent(), Some(line(1)));
    let mut a2 = EntryBuilder::on_line(&f.keys, 0, line(2));
    assert_eq!(created(&got.apply_edit(Frame(2), &reissue(&mut a2, &f.a[1])).0), vec![f.y]);
}

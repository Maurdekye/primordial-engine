//! The edit path (world-model.md 7; acceptance 11 items 1-7 and 14).

use bytes::Bytes;
use pe_core::{Frame, ObjectId, Offset, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Note, Outcome, Reason, SignedEntry, Version};
use pe_sim::{ComponentId, Transform};
use pe_world::components::{Appearance, Body, Protection, ids};
use pe_world::edit::{Actor, Op, Precondition, Row, Target, Transaction, content_hash};
use pe_world::{BlobStore, Claim, Footprint, World, perm};

fn row<T: pe_sim::Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

fn tx(ops: Vec<Op>) -> Transaction {
    Transaction { actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] }
}

fn at(x: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, 0.0, 0.0])).unwrap()
}

struct Fx {
    keys: TestKeys,
    a: EntryBuilder,
    b: EntryBuilder,
    w: World,
}

fn fx() -> Fx {
    let keys = TestKeys::new(1, 3);
    let mut w = World::new(1, BlobStore::default());
    w.set_member(keys.key(0), perm::EDIT);
    w.set_member(keys.key(1), perm::EDIT);
    Fx { a: EntryBuilder::new(&keys, 0), b: EntryBuilder::new(&keys, 1), keys, w }
}

fn entry(b: &mut EntryBuilder, t: &Transaction) -> SignedEntry {
    b.next(Class::Edit, Frame(1), Bytes::from(t.encode()))
}

fn apply(w: &mut World, e: &SignedEntry) -> (Outcome, Footprint) {
    w.apply_edit(Frame(1), e)
}

fn created(o: &Outcome) -> Vec<ObjectId> {
    match o {
        Outcome::Applied { created, .. } => created.clone(),
        r => panic!("refused: {r:?}"),
    }
}

fn refused(o: &Outcome) -> (Option<u32>, Reason) {
    match o {
        Outcome::Refused { op, reason } => (*op, reason.clone()),
        a => panic!("applied: {a:?}"),
    }
}

/// A root table at x = 5 and a ball on it, by member A.
fn table_and_ball(f: &mut Fx) -> (ObjectId, ObjectId, SignedEntry) {
    let t = tx(vec![
        Op::Create { new: 0, parent: Target::Root, index: None, rows: vec![row(Transform::at(at(5.0)))] },
        Op::Create {
            new: 1,
            parent: Target::New(0),
            index: None,
            rows: vec![row(Transform::local([0.0, 1.0, 0.0])), row(Body::default())],
        },
    ]);
    let e = entry(&mut f.a, &t);
    let ids = created(&apply(&mut f.w, &e).0);
    (ids[0], ids[1], e)
}

fn ver(e: &SignedEntry, op: u32) -> Version {
    Version { entry: e.id(), op }
}

fn state(w: &World) -> u64 {
    w.store().state_hash()
}

#[test]
fn edits_alone_and_combined() {
    let mut f = fx();
    let (table, ball, e0) = table_and_ball(&mut f);
    let s = f.w.snapshot(Frame(0));
    assert_eq!(s.children(table), [ball]);
    assert_eq!(s.info(ball).unwrap().creator, f.keys.key(0));
    assert_eq!(s.info(ball).unwrap().version, ver(&e0, 1));
    assert_eq!(s.info(table).unwrap().structural, ver(&e0, 1), "a child's creation moves the parent's structure");
    assert!(s.info(ball).unwrap().types.contains(&"SimSize".to_string()), "every object gets a SimSize");
    // SetRow, SetField, RemoveRow and Reparent in one transaction.
    let red = Appearance { base: [1.0, 0.0, 0.0, 1.0], ..Default::default() };
    let t = tx(vec![
        Op::SetRow { id: Target::Existing(ball), row: row(red) },
        Op::SetField {
            id: Target::Existing(ball),
            component: ids::APPEARANCE,
            instance: 0,
            field: 2, // roughness (fields sorted by offset: base, emissive, roughness, ...)
            value: 0.25f32.to_le_bytes().to_vec(),
        },
        Op::RemoveRow { id: Target::Existing(ball), component: ids::BODY, instance: 0 },
        Op::Reparent { id: Target::Existing(ball), parent: Target::Root, index: None, keep_world: true },
    ]);
    let e1 = entry(&mut f.a, &t);
    let (o, fp) = apply(&mut f.w, &e1);
    assert!(matches!(o, Outcome::Applied { .. }), "{o:?}");
    let s = f.w.snapshot(Frame(0));
    let a = s.fields(ball, "Appearance").unwrap();
    assert_eq!(a.iter().find(|(k, _)| k == "roughness").unwrap().1, pe_world::Value::F(0.25));
    assert!(s.fields(ball, "Body").is_none());
    assert_eq!(s.parent(ball), None);
    assert_eq!(s.world_transform(ball).unwrap().pos, Pos::ORIGIN.checked_add(Offset([5.0, 1.0, 0.0])).unwrap());
    assert_eq!(s.info(ball).unwrap().version, ver(&e1, 3));
    assert_eq!(fp.rows, [(ball, ids::APPEARANCE), (ball, ids::APPEARANCE), (ball, ids::BODY)]);
    assert_eq!(fp.reparented, [ball]);
    // Delete.
    let e2 = entry(&mut f.a, &tx(vec![Op::Delete { id: Target::Existing(table) }]));
    let (_, fp) = apply(&mut f.w, &e2);
    assert_eq!(fp.removed, [table]);
    assert!(!f.w.store().exists(table) && f.w.store().exists(ball));
}

#[test]
fn all_or_nothing_and_dry_run() {
    let mut f = fx();
    let (_, ball, _) = table_and_ball(&mut f);
    let before = state(&f.w);
    let t = tx(vec![
        Op::SetRow { id: Target::Existing(ball), row: row(Appearance::default()) },
        Op::Create { new: 0, parent: Target::Root, index: None, rows: vec![] },
        Op::Delete { id: Target::Existing(ObjectId::from_u128(999)) },
    ]);
    let e = entry(&mut f.a, &t);
    let dry = f.w.dry_run(Frame(1), &e);
    assert_eq!(state(&f.w), before, "a dry run changes nothing");
    let (o, fp) = apply(&mut f.w, &e);
    assert_eq!(o, dry, "a dry run gives the live outcome");
    assert_eq!(refused(&o), (Some(2), Reason::Unknown(ObjectId::from_u128(999))));
    assert_eq!(fp, Footprint::default());
    assert_eq!(state(&f.w), before, "the failed last op undid the first two");
}

#[test]
fn rule_2_stale_preconditions() {
    let mut f = fx();
    let (_, ball, e0) = table_and_ball(&mut f);
    let read = ver(&e0, 1);
    let guarded = |is: Version| Transaction {
        preconditions: vec![Precondition::Version { id: ball, component: ComponentId::TRANSFORM, instance: 0, is }],
        ..tx(vec![Op::SetRow { id: Target::Existing(ball), row: row(Transform::local([0.0, 2.0, 0.0])) }])
    };
    // B edits the ball first; A's edit, based on the earlier read, is stale and names B's entry.
    let eb = entry(
        &mut f.b,
        &tx(vec![Op::SetRow { id: Target::Existing(ball), row: row(Transform::local([1.0, 1.0, 0.0])) }]),
    );
    created(&apply(&mut f.w, &eb).0);
    let ea = entry(&mut f.a, &guarded(read));
    let (op, reason) = refused(&apply(&mut f.w, &ea).0);
    assert_eq!(op, None);
    assert_eq!(reason, Reason::Stale { precondition: 0, current: ver(&eb, 0), winner: eb.id() });
    // With no intervening edit it applies.
    let ea2 = entry(&mut f.a, &guarded(ver(&eb, 0)));
    created(&apply(&mut f.w, &ea2).0);
    // A Content precondition covers every writer; an ObjectVersion one covers the whole object.
    let bytes = f.w.store().get_raw(ComponentId::TRANSFORM, ball).unwrap().to_vec();
    let content = Transaction {
        preconditions: vec![Precondition::Content {
            id: ball,
            component: ComponentId::TRANSFORM,
            instance: 0,
            hash: content_hash(&bytes),
        }],
        ..tx(vec![Op::SetRow { id: Target::Existing(ball), row: row(Appearance::default()) }])
    };
    let mut w2 = f.w.clone();
    assert!(matches!(apply(&mut w2, &entry(&mut f.a.clone(), &content)).0, Outcome::Applied { .. }));
    let obj =
        Transaction { preconditions: vec![Precondition::ObjectVersion { id: ball, is: ver(&eb, 0) }], ..tx(vec![]) };
    assert!(matches!(refused(&apply(&mut f.w.clone(), &entry(&mut f.a.clone(), &obj)).0).1, Reason::Stale { .. }));
}

#[test]
fn trusted_writes_never_make_a_version_stale() {
    let mut f = fx();
    let (_, ball, e0) = table_and_ball(&mut f);
    // A trusted write stands in for the physics write-back: the row changes, its version does not.
    let mut w = f.w.clone();
    let mut t = *w.store().get::<Transform>(ball).unwrap();
    t.pos[0] = 0.5;
    w.insert(ball, t).unwrap();
    let guarded = Transaction {
        preconditions: vec![Precondition::Version {
            id: ball,
            component: ComponentId::TRANSFORM,
            instance: 0,
            is: ver(&e0, 1),
        }],
        ..tx(vec![Op::SetRow { id: Target::Existing(ball), row: row(Appearance::default()) }])
    };
    assert!(matches!(apply(&mut w, &entry(&mut f.a, &guarded)).0, Outcome::Applied { .. }));
}

#[test]
fn rule_3_the_later_plain_write_stands() {
    let mut f = fx();
    let (_, ball, _) = table_and_ball(&mut f);
    let paint = |c: f32| {
        tx(vec![Op::SetRow { id: Target::Existing(ball), row: row(Appearance { base: [c; 4], ..Default::default() }) }])
    };
    let ea = entry(&mut f.a, &paint(0.25));
    let eb = entry(&mut f.b, &paint(0.75));
    created(&apply(&mut f.w, &ea).0);
    let (o, _) = apply(&mut f.w, &eb);
    let Outcome::Applied { overwrote, .. } = o else { panic!() };
    assert_eq!(overwrote, [ver(&ea, 0)], "the overwritten author is named");
    let a: &Appearance = f.w.store().get(ball).unwrap();
    assert_eq!(a.base, [0.75; 4]);
}

#[test]
fn delete_against_add_on_both_orders() {
    // world-model.md 7.5, the user's rule: delete first orphans the add-on; delete second destroys it.
    let mut f = fx();
    let (table, ball, _) = table_and_ball(&mut f);
    let add_on = |p: ObjectId| {
        tx(vec![Op::Create {
            new: 0,
            parent: Target::Existing(p),
            index: None,
            rows: vec![row(Transform::local([0.0, 0.0, 2.0]))],
        }])
    };
    let delete = tx(vec![Op::Delete { id: Target::Existing(table) }]);
    let base = f.w.clone();
    let (a0, b0) = (f.a.clone(), f.b.clone());
    let del = entry(&mut f.a, &delete);
    let add = entry(&mut f.b, &add_on(ball)); // under the table's child: a depth-2 descendant of the deleted table

    // Delete first: the add-on becomes a root at its parent's last place, with a note.
    let mut w1 = base.clone();
    apply(&mut w1, &del);
    let (o, _) = apply(&mut w1, &add);
    let Outcome::Applied { created, notes, .. } = o else { panic!("{o:?}") };
    assert_eq!(notes, [Note::Orphaned { object: created[0], deleted_by: del.id() }]);
    let s = w1.snapshot(Frame(0));
    assert_eq!(s.parent(created[0]), None);
    // The ball was at (5, 1, 0); the add-on's local (0, 0, 2) puts it at (5, 1, 2).
    assert_eq!(s.world_transform(created[0]).unwrap().pos, Pos::ORIGIN.checked_add(Offset([5.0, 1.0, 2.0])).unwrap());

    // Delete second: everything is destroyed.
    let mut w2 = base.clone();
    created_ok(&apply(&mut w2, &add).0);
    apply(&mut w2, &del);
    assert_eq!(w2.store().objects().len(), 0);

    // Both sides delete the table: the later transaction's other ops still apply.
    let mut w3 = base;
    let (mut a, mut b) = (a0, b0);
    apply(&mut w3, &entry(&mut a, &delete));
    let both = tx(vec![
        Op::Delete { id: Target::Existing(table) },
        Op::Create { new: 0, parent: Target::Root, index: None, rows: vec![] },
    ]);
    let e = entry(&mut b, &both);
    let (o, _) = apply(&mut w3, &e);
    let Outcome::Applied { created, notes, .. } = o else { panic!("{o:?}") };
    assert_eq!(created.len(), 1);
    assert!(matches!(notes[0], Note::AlreadyDeleted { object, .. } if object == table));
    // Other ops on a deleted object refuse the transaction.
    let set = tx(vec![Op::SetRow { id: Target::Existing(ball), row: row(Appearance::default()) }]);
    assert!(
        matches!(refused(&apply(&mut w3, &entry(&mut b, &set)).0).1, Reason::Deleted { object, .. } if object == ball)
    );
}

fn created_ok(o: &Outcome) {
    assert!(matches!(o, Outcome::Applied { .. }), "{o:?}");
}

#[test]
fn ids_do_not_depend_on_order() {
    let mut f = fx();
    let make = tx(vec![Op::Create { new: 0, parent: Target::Root, index: None, rows: vec![] }]);
    let ea = entry(&mut f.a, &make);
    let eb = entry(&mut f.b, &make);
    let (mut w1, mut w2) = (f.w.clone(), f.w.clone());
    let x = [created(&apply(&mut w1, &ea).0), created(&apply(&mut w1, &eb).0)];
    let y = [created(&apply(&mut w2, &eb).0), created(&apply(&mut w2, &ea).0)];
    assert_eq!((x[0][0], x[1][0]), (y[1][0], y[0][0]));
    assert_eq!(w1.store().objects(), w2.store().objects());
    // Replaying an entry is an id collision: refused.
    assert_eq!(refused(&apply(&mut w1, &ea).0), (Some(0), Reason::Exists(x[0][0])));
    // Two ops with one New index overlap.
    let dup = tx(vec![
        Op::Create { new: 3, parent: Target::Root, index: None, rows: vec![] },
        Op::Create { new: 3, parent: Target::Root, index: None, rows: vec![] },
    ]);
    assert_eq!(refused(&apply(&mut w1, &entry(&mut f.a, &dup)).0), (Some(1), Reason::NewIndexOverlap));
}

#[test]
fn holds_permissions_and_protections() {
    let mut f = fx();
    let (table, ball, _) = table_and_ball(&mut f);
    let (ka, kb) = (f.keys.key(0), f.keys.key(1));
    f.w.set_claim(ball, Claim { hand: table, member: kb, since: Frame(0) }).unwrap();
    // While held: delete of it or an ancestor, and pose edits, are refused; a recolour passes.
    let del = entry(&mut f.a, &tx(vec![Op::Delete { id: Target::Existing(table) }]));
    assert_eq!(refused(&apply(&mut f.w, &del).0).1, Reason::Held { by: kb });
    let pose = entry(&mut f.a, &tx(vec![Op::SetRow { id: Target::Existing(ball), row: row(Transform::IDENTITY) }]));
    assert_eq!(refused(&apply(&mut f.w, &pose).0).1, Reason::Held { by: kb });
    let paint = entry(&mut f.a, &tx(vec![Op::SetRow { id: Target::Existing(ball), row: row(Appearance::default()) }]));
    created_ok(&apply(&mut f.w, &paint).0);
    f.w.clear_claim(ball);
    // No EDIT, no edit.
    let mut c = EntryBuilder::new(&f.keys, 2);
    assert_eq!(refused(&apply(&mut f.w, &entry(&mut c, &tx(vec![]))).0).1, Reason::NoPermission);
    // Only the creator may protect, and only as themselves.
    let protect = |k| tx(vec![Op::SetRow { id: Target::Existing(table), row: row(Protection { owner: k }) }]);
    assert_eq!(refused(&apply(&mut f.w, &entry(&mut f.b, &protect(kb))).0).1, Reason::Protected { owner: ka });
    assert_eq!(refused(&apply(&mut f.w, &entry(&mut f.a, &protect(kb))).0).1, Reason::Protected { owner: ka });
    created_ok(&apply(&mut f.w, &entry(&mut f.a, &protect(ka))).0);
    // Nobody else may remove it, or add one inside the protected subtree.
    let unprotect = tx(vec![Op::RemoveRow { id: Target::Existing(table), component: ids::PROTECTION, instance: 0 }]);
    assert_eq!(refused(&apply(&mut f.w, &entry(&mut f.b, &unprotect)).0).1, Reason::Protected { owner: ka });
    let inner = tx(vec![Op::SetRow { id: Target::Existing(ball), row: row(Protection { owner: kb }) }]);
    assert_eq!(refused(&apply(&mut f.w, &entry(&mut f.b, &inner)).0).1, Reason::Protected { owner: ka });
    // Other edits stay under the ordinary permissions.
    created_ok(
        &apply(
            &mut f.w,
            &entry(&mut f.b, &tx(vec![Op::SetRow { id: Target::Existing(ball), row: row(Appearance::default()) }])),
        )
        .0,
    );
    // An agent acts as its principal only.
    let agent = |p| Transaction {
        actor: Actor::Agent { harness: "Claude Code".into(), principal: p, requested_by: None },
        ..tx(vec![])
    };
    created_ok(&apply(&mut f.w, &entry(&mut f.a, &agent(ka))).0);
    assert_eq!(refused(&apply(&mut f.w, &entry(&mut f.a, &agent(kb))).0).1, Reason::Invalid { field: None });
}

#[test]
fn edits_may_not_write_engine_rows_or_cycles() {
    let mut f = fx();
    let (table, ball, _) = table_and_ball(&mut f);
    for c in [ComponentId::NODE, ids::BODY_STATE, ids::HAND] {
        let t = tx(vec![Op::SetRow {
            id: Target::Existing(ball),
            row: Row { component: c, instance: 0, bytes: vec![0; 8] },
        }]);
        assert!(matches!(refused(&apply(&mut f.w, &entry(&mut f.a, &t)).0).1, Reason::Invalid { .. }));
    }
    let cycle = tx(vec![Op::Reparent {
        id: Target::Existing(table),
        parent: Target::Existing(ball),
        index: None,
        keep_world: false,
    }]);
    assert_eq!(refused(&apply(&mut f.w, &entry(&mut f.a, &cycle)).0).1, Reason::Cycle);
    let nan = tx(vec![Op::SetRow {
        id: Target::Existing(ball),
        row: row(Appearance { roughness: f32::NAN, ..Default::default() }),
    }]);
    assert!(matches!(refused(&apply(&mut f.w, &entry(&mut f.a, &nan)).0).1, Reason::Invalid { field: Some(_) }));
}

#[test]
fn codec_is_canonical_and_total() {
    let t = Transaction {
        actor: Actor::Agent { harness: "h".into(), principal: pe_core::MemberKey([3; 32]), requested_by: None },
        label: Some("make the balls blue".into()),
        preconditions: vec![Precondition::Exists(ObjectId::from_u128(5)), Precondition::Absent(ObjectId::from_u128(6))],
        ops: vec![
            Op::Create { new: 0, parent: Target::Root, index: Some(2), rows: vec![row(Appearance::default())] },
            Op::Reparent {
                id: Target::New(0),
                parent: Target::Existing(ObjectId::from_u128(5)),
                index: None,
                keep_world: true,
            },
            Op::SetField { id: Target::New(0), component: ids::APPEARANCE, instance: 0, field: 1, value: vec![0; 12] },
            Op::RemoveRow { id: Target::New(0), component: ids::LIGHT, instance: 0 },
            Op::Delete { id: Target::New(0) },
        ],
        blobs: vec![pe_world::BlobRef::of(b"x")],
    };
    let b = t.encode();
    assert_eq!(Transaction::decode(&b), Some(t.clone()));
    // Every truncation and every one-byte extension is refused; nothing panics.
    for n in 0..b.len() {
        assert_eq!(Transaction::decode(&b[..n]), None, "prefix {n}");
    }
    let mut long = b.clone();
    long.push(0);
    assert_eq!(Transaction::decode(&long), None);
    // Arbitrary bytes, and single-byte corruptions of a valid encoding: decode or refuse, and anything that decodes
    // re-encodes to the same bytes.
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..20_000 {
        let n = (next() % 200) as usize;
        let junk: Vec<u8> = (0..n).map(|_| next() as u8).collect();
        if let Some(t) = Transaction::decode(&junk) {
            assert_eq!(t.encode(), junk);
        }
        let mut c = b.clone();
        let i = (next() as usize) % c.len();
        c[i] ^= 1 << (next() % 8);
        if let Some(t) = Transaction::decode(&c) {
            assert_eq!(t.encode(), c);
        }
    }
    // And an applier fed garbage refuses it as malformed.
    let mut f = fx();
    let e = f.a.next(Class::Edit, Frame(1), Bytes::from_static(b"\x07garbage"));
    assert_eq!(refused(&f.w.apply_edit(Frame(1), &e).0), (None, Reason::Malformed));
}

fn inverse(o: &Outcome) -> Transaction {
    match o {
        Outcome::Applied { inverse: Some(b), .. } => Transaction::decode(b).expect("the inverse decodes"),
        o => panic!("no inverse: {o:?}"),
    }
}

#[test]
fn copy_takes_derived_ids_in_depth_first_order() {
    let mut f = fx();
    let (table, ball, _) = table_and_ball(&mut f);
    let t = tx(vec![
        Op::Copy { source: Target::Existing(table), new: 0, parent: Target::Root, index: None },
        // The copy's indices are 0 (the table) and 1 (the ball): a Create at 1 overlaps.
        Op::Create { new: 1, parent: Target::Root, index: None, rows: vec![] },
    ]);
    assert_eq!(refused(&apply(&mut f.w, &entry(&mut f.a, &t)).0), (Some(1), Reason::NewIndexOverlap));
    let t = tx(vec![Op::Copy { source: Target::Existing(table), new: 0, parent: Target::Root, index: None }]);
    let ids = created(&apply(&mut f.w, &entry(&mut f.a, &t)).0);
    assert_eq!(ids.len(), 2);
    let s = f.w.snapshot(Frame(0));
    assert_eq!(s.children(ids[0]), [ids[1]]);
    assert_eq!(s.world_transform(ids[1]).unwrap().pos, s.world_transform(ball).unwrap().pos);
    assert!(s.fields(ids[1], "Body").is_some(), "rows and all");
    // Only the owner copies a protected object.
    let protect = tx(vec![Op::SetRow { id: Target::Existing(table), row: row(Protection { owner: f.keys.key(0) }) }]);
    created_ok(&apply(&mut f.w, &entry(&mut f.a, &protect)).0);
    let t = tx(vec![Op::Copy { source: Target::Existing(ball), new: 0, parent: Target::Root, index: None }]);
    assert_eq!(refused(&apply(&mut f.w, &entry(&mut f.b, &t)).0).1, Reason::Protected { owner: f.keys.key(0) });
}

#[test]
fn revert_of_a_delete_restores_the_same_ids() {
    // Acceptance 6: delete a referenced object, apply the outcome's inverse, and the reference resolves again.
    let mut f = fx();
    let (table, ball, _) = table_and_ball(&mut f);
    let before = f.w.snapshot(Frame(0));
    let t = ComponentId::TRANSFORM;
    let row_version = f.w.store().row_version(t, ball);
    assert!(row_version.is_some_and(|v| v.op == 1), "written by the creating entry's op 1");
    let del = entry(&mut f.a, &tx(vec![Op::Delete { id: Target::Existing(table) }]));
    let (o, _) = apply(&mut f.w, &del);
    assert!(!f.w.store().exists(ball));
    let undo = entry(&mut f.a, &inverse(&o));
    let (o2, fp) = apply(&mut f.w, &undo);
    created_ok(&o2);
    assert_eq!(fp.created, [table, ball]);
    let s = f.w.snapshot(Frame(0));
    assert_eq!(s.children(table), [ball]);
    assert_eq!(s.world_transform(ball), before.world_transform(ball));
    assert_eq!(s.fields(ball, "Body"), before.fields(ball, "Body"));
    // Row versions come back as they were; object versions move to the Undelete.
    assert_eq!(f.w.store().row_version(t, ball), row_version);
    assert_eq!(s.info(ball).unwrap().version, ver(&undo, 0));
    assert!(f.w.tombstone(ball).is_none());
    // Undelete of a live object, or of one never deleted, is refused.
    let again = entry(&mut f.a, &tx(vec![Op::Undelete { id: ball }]));
    assert_eq!(refused(&apply(&mut f.w, &again).0).1, Reason::Exists(ball));
}

#[test]
fn revert_of_edits_restores_rows_and_is_stale_after_another_edit() {
    let mut f = fx();
    let (table, ball, _) = table_and_ball(&mut f);
    let t = tx(vec![
        Op::SetRow { id: Target::Existing(ball), row: row(Appearance { base: [0.5; 4], ..Default::default() }) },
        Op::SetRow { id: Target::Existing(ball), row: row(Transform::local([3.0, 0.0, 0.0])) },
        Op::Reparent { id: Target::Existing(ball), parent: Target::Root, index: None, keep_world: true },
        Op::Create { new: 0, parent: Target::Existing(table), index: None, rows: vec![] },
    ]);
    let (o, _) = apply(&mut f.w, &entry(&mut f.a, &t));
    let inv = inverse(&o);
    // Reverting at once restores every row and link (versions aside, which move to the revert).
    let mut w = f.w.clone();
    created_ok(&apply(&mut w, &entry(&mut f.a.clone(), &inv)).0);
    let s = w.snapshot(Frame(0));
    assert_eq!(s.parent(ball), Some(table));
    assert_eq!(s.children(table), [ball]);
    assert!(s.fields(ball, "Appearance").is_none());
    assert_eq!(w.store().get::<Transform>(ball), Some(&Transform::local([0.0, 1.0, 0.0])));
    // After someone else's edit to a row it set, the revert is stale rather than undoing their work.
    let other = tx(vec![Op::SetRow { id: Target::Existing(ball), row: row(Appearance::default()) }]);
    created_ok(&apply(&mut f.w, &entry(&mut f.b, &other)).0);
    assert!(matches!(refused(&apply(&mut f.w, &entry(&mut f.a, &inv)).0).1, Reason::Stale { .. }));
}

#[test]
fn tombstones_expire_after_a_minute() {
    let mut f = fx();
    let (table, _, _) = table_and_ball(&mut f);
    apply(&mut f.w, &entry(&mut f.a, &tx(vec![Op::Delete { id: Target::Existing(table) }])));
    let expires = f.w.tombstone(table).unwrap().expires;
    assert_eq!(expires, Frame(1 + 60 * 60), "one minute at 60 Hz from the delete's frame");
    f.w.end_frame(Frame(expires.0 - 1));
    assert!(f.w.tombstone(table).is_some());
    f.w.end_frame(expires);
    assert!(f.w.tombstone(table).is_none());
    // After expiry an op naming the id is refused as unknown, and an add-on no longer orphans.
    let add = tx(vec![Op::Create { new: 0, parent: Target::Existing(table), index: None, rows: vec![] }]);
    assert_eq!(refused(&apply(&mut f.w, &entry(&mut f.b, &add)).0).1, Reason::Unknown(table));
}

//! Folders (pe-world `components::Folder`, `templates`; design domain-eng d1, C5): everything under a folder is
//! stored, not played, at any depth; the folder itself stays live; adding or removing the row regroups its subtree.

use bytes::Bytes;
use pe_core::{Frame, MemberKey, ObjectId, Offset, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Outcome, Reason};
use pe_sim::{Component, Transform};
use pe_world::components::{Body, Folder, Name, Shape, body_kind, ids, shape_kind};
use pe_world::content::Content;
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::preset::home::home_on;
use pe_world::preset::{find, genesis_line};
use pe_world::{BlobStore, Footprint, GENESIS, World, perm};

fn oid(k: u128) -> ObjectId {
    ObjectId::from_u128(k)
}

fn row<T: Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

fn keys() -> TestKeys {
    TestKeys::new(3, 2)
}

fn edit(w: &mut World, b: &mut EntryBuilder, ops: Vec<Op>) -> (Outcome, Footprint) {
    let t =
        Transaction { inline: vec![], actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] };
    let e = b.next(Class::Edit, Frame(1), Bytes::from(t.encode()));
    w.apply_edit(Frame(1), &e)
}

/// Roots 1 (a folder crate with a body) and 5 (a shelf). Under 1: 2 (a ball) and 3 (a plain frame) holding 4 (a
/// ball). Under 5: 6 (a ball).
fn shelf() -> World {
    let mut w = World::new(1, BlobStore::default());
    w.register_folders();
    w.set_member(keys().key(0), perm::EDIT);
    let owner = MemberKey([1; 32]);
    let add = |w: &mut World, id: u128, parent: Option<u128>, body: bool| {
        w.create(oid(id), parent.map(oid), owner, GENESIS).expect("created");
        let at = if parent.is_some() {
            Transform::local([0.0, 0.5, 0.0])
        } else {
            Transform::at(Pos::ORIGIN.checked_add(Offset([id as f64, 0.0, 0.0])).unwrap())
        };
        w.insert(oid(id), at).expect("transform");
        let text = w.blobs().put(format!("o{id}").as_bytes());
        w.insert(oid(id), Name { text }).expect("name");
        if body {
            let dynamic = Body { kind: body_kind::DYNAMIC, gravity_scale: 1.0, grabbable: 1, ..Body::default() };
            w.insert(oid(id), dynamic).expect("body");
            w.insert(oid(id), Shape { kind: shape_kind::SPHERE, size: [0.1, 0.0, 0.0], ..Shape::default() })
                .expect("shape");
        }
    };
    add(&mut w, 1, None, true);
    add(&mut w, 2, Some(1), true);
    add(&mut w, 3, Some(1), false);
    add(&mut w, 4, Some(3), true);
    add(&mut w, 5, None, false);
    add(&mut w, 6, Some(5), true);
    w.insert(oid(1), Folder::default()).expect("folder");
    w
}

#[test]
fn a_folders_contents_are_inert_at_every_depth_and_the_folder_stays_live() {
    let w = shelf();
    assert!(w.is_folder(oid(1)) && !w.is_folder(oid(5)));
    assert!(!w.inert(oid(1)), "the folder itself is live");
    for k in [2, 3, 4] {
        assert!(w.inert(oid(k)), "{k} is in the folder");
    }
    for k in [5, 6] {
        assert!(!w.inert(oid(k)), "{k} is live");
    }
    assert_eq!(w.physical(), [oid(1), oid(6)]);
}

#[test]
fn adding_or_removing_a_folder_row_regroups_its_subtree() {
    let mut w = shelf();
    let mut b = EntryBuilder::new(&keys(), 0);
    let (o, fp) = edit(&mut w, &mut b, vec![Op::SetRow { id: Target::Existing(oid(5)), row: row(Folder::default()) }]);
    assert!(matches!(o, Outcome::Applied { .. }), "{o:?}");
    assert_eq!(fp.regrouped, [oid(5)]);
    assert_eq!(w.physical(), [oid(1)]);
    let tag = u128::from_le_bytes(w.store().row_version(ids::FOLDER, oid(5)).expect("the row's version").tag);
    let (o, fp) = edit(
        &mut w,
        &mut b,
        vec![Op::RemoveRow { id: Target::Existing(oid(5)), component: ids::FOLDER, instance: 0, tag }],
    );
    assert!(matches!(o, Outcome::Applied { .. }), "{o:?}");
    assert_eq!(fp.regrouped, [oid(5)]);
    assert_eq!(w.physical(), [oid(1), oid(6)]);
    // A colour or a move of the folder regroups nothing.
    let at = Transform::at(Pos::ORIGIN.checked_add(Offset([9.0, 0.0, 0.0])).unwrap());
    let (o, fp) = edit(&mut w, &mut b, vec![Op::SetRow { id: Target::Existing(oid(1)), row: row(at) }]);
    assert!(matches!(o, Outcome::Applied { .. }), "{o:?}");
    assert_eq!(fp.regrouped, []);
}

#[test]
fn moving_an_object_into_a_folder_takes_it_out_of_play() {
    let mut w = shelf();
    let mut b = EntryBuilder::new(&keys(), 0);
    let ops = vec![Op::Reparent {
        id: Target::Existing(oid(6)),
        parent: Target::Existing(oid(1)),
        index: None,
        keep_world: true,
    }];
    let (o, _) = edit(&mut w, &mut b, ops);
    assert!(matches!(o, Outcome::Applied { .. }), "{o:?}");
    assert!(w.inert(oid(6)));
    assert_eq!(w.physical(), [oid(1)]);
    // A world without the component registered refuses a folder row as any unknown component.
    let mut plain = World::new(1, BlobStore::default());
    plain.set_member(keys().key(0), perm::EDIT);
    plain.create(oid(1), None, MemberKey([1; 32]), GENESIS).unwrap();
    let mut b = EntryBuilder::new(&keys(), 0);
    let (o, _) =
        edit(&mut plain, &mut b, vec![Op::SetRow { id: Target::Existing(oid(1)), row: row(Folder::default()) }]);
    assert!(matches!(o, Outcome::Refused { reason: Reason::Invalid { .. }, .. }), "{o:?}");
}

#[test]
fn a_home_carries_folders_through_its_content() {
    let mut b = pe_world::content::Builder::default();
    let crate_rows = [
        row(Transform::local([0.0, 0.2, -2.0])),
        row(Folder::default()),
        row(Body { kind: body_kind::DYNAMIC, gravity_scale: 1.0, grabbable: 1, ..Body::default() }),
        row(Shape { kind: shape_kind::BOX, density: 300.0, size: [0.2; 3], ..Shape::default() }),
    ];
    let top = b.top(&crate_rows, "Crate");
    let ball = [
        row(Transform::local([0.0, 0.0, 0.0])),
        row(Body { kind: body_kind::DYNAMIC, gravity_scale: 1.0, grabbable: 1, ..Body::default() }),
        row(Shape { kind: shape_kind::SPHERE, density: 300.0, size: [0.1, 0.0, 0.0], ..Shape::default() }),
    ];
    b.child(top, &ball, "Stored ball");
    let content: Content = b.finish();
    let h = home_on(genesis_line(b"folders"), 1, BlobStore::default(), Pos::ORIGIN, &content).unwrap();
    let (c, s) = (find(&h.world, "Crate").unwrap(), find(&h.world, "Stored ball").unwrap());
    assert!(h.world.is_folder(c) && !h.world.inert(c) && h.world.inert(s));
}

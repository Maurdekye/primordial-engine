//! The friend test's flat plane (world-model.md 2.4, "The plane") and template folders (7.1, `pe_world::templates`;
//! @coordinator-opus with @fable-reviewer's four conditions).

use bytes::Bytes;
use pe_core::{Frame, LineId, MemberKey, ObjectId, Offset, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Outcome, Reason, SignedEntry};
use pe_sim::{Component, Transform};
use pe_world::components::{Body, Hand, Name, Shape, body_kind, ids, shape_kind};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::preset::{Preset, Session, find, m1_session, plane_names as n};
use pe_world::templates::{FOLDER, is_folder_name};
use pe_world::{BlobStore, Footprint, GENESIS, HandPose, Input, World, perm};

fn oid(k: u128) -> ObjectId {
    ObjectId::from_u128(k)
}

fn pos(x: f64, y: f64, z: f64) -> Pos {
    Pos::ORIGIN.checked_add(Offset([x, y, z])).expect("in range")
}

fn row<T: Component>(v: T) -> Row {
    Row { component: T::ID, instance: 0, bytes: bytemuck::bytes_of(&v).to_vec() }
}

fn tx(ops: Vec<Op>) -> Transaction {
    Transaction { inline: vec![], actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] }
}

fn keys() -> TestKeys {
    TestKeys::new(3, 2)
}

const LINE: LineId = LineId(pe_core::Hash([7; 32]));

fn session(preset: Preset) -> Session {
    let keys = keys();
    Session {
        seed: 5,
        origin: Pos::ORIGIN,
        preset,
        line: LINE,
        rate: pe_seams::SimRate::DEFAULT,
        gravity: pe_world::GRAVITY,
        agree: b"pe-agree's settings".to_vec(),
        members: vec![(keys.key(0), perm::ADMIN | perm::EDIT), (keys.key(1), perm::EDIT)],
    }
}

fn plane() -> World {
    m1_session(&session(Preset::Plane), BlobStore::default()).expect("the plane")
}

/// Applies `ops` as one edit, carrying the bytes of every name it writes inline (3.4).
fn edit(w: &mut World, b: &mut EntryBuilder, ops: Vec<Op>) -> (Outcome, Footprint) {
    let mut t = tx(ops);
    for op in &t.ops {
        if let Op::SetRow { row, .. } = op
            && row.component == Name::ID
        {
            let text: Name = bytemuck::pod_read_unaligned(&row.bytes);
            t.inline.push(w.blobs().get(&text.text).expect("the name's bytes").to_vec());
        }
    }
    t.inline.sort_by_key(|b| pe_world::BlobRef::of(b).hash);
    t.inline.dedup();
    let e = b.next(Class::Edit, Frame(1), Bytes::from(t.encode()));
    w.apply_edit(Frame(1), &e)
}

fn input(w: &mut World, b: &mut EntryBuilder, i: &Input) -> (Outcome, Footprint) {
    let e: SignedEntry = b.next(Class::Input, Frame(1), Bytes::from(i.encode()));
    w.apply_input(Frame(1), &e)
}

fn applied(o: &Outcome) -> Vec<ObjectId> {
    match o {
        Outcome::Applied { created, .. } => created.clone(),
        r => panic!("refused: {r:?}"),
    }
}

fn reason(o: &Outcome) -> Reason {
    match o {
        Outcome::Refused { reason, .. } => reason.clone(),
        a => panic!("applied: {a:?}"),
    }
}

fn named(w: &World, text: &str) -> Row {
    row(Name { text: w.blobs().put(text.as_bytes()) })
}

/// Every object of `top`'s subtree, `top` first.
fn subtree(w: &World, top: ObjectId) -> Vec<ObjectId> {
    let snap = w.snapshot(w.frame());
    let mut out = vec![];
    let mut stack = vec![top];
    while let Some(x) = stack.pop() {
        out.push(x);
        stack.extend(snap.children(x));
    }
    out
}

#[test]
fn the_plane_has_its_floor_an_empty_spawned_root_and_inert_templates() {
    let w = plane();
    let get = |name: &str| find(&w, name).unwrap_or_else(|| panic!("{name}"));
    let (root, floor, spawned, folder) = (get(n::ROOT), get(n::FLOOR), get(n::SPAWNED), get(n::TEMPLATES));
    let snap = w.snapshot(w.frame());
    let roots = snap.roots();
    for r in [root, spawned, folder] {
        assert!(roots.contains(&r), "{:?} is a root", snap.name(r));
    }
    assert_eq!(snap.parent(floor), Some(root));
    assert!(find(&w, n::LAMP).is_some_and(|l| snap.parent(l) == Some(root)));
    assert!(snap.children(spawned).is_empty(), "nothing is spawned at genesis");
    // The templates, in order: three single bodies and a stack of five.
    let names: Vec<_> = snap.children(folder).into_iter().map(|c| snap.name(c).expect("named")).collect();
    assert_eq!(names, [n::BOX, n::SPHERE, n::CAPSULE, n::STACK, n::BIG_CRATE, n::PLATFORM, n::SLAB]);
    // The heavy templates: box bodies far above the mover's 40 kg `heavy_mass`, so a player stands on them.
    for (label, half, density, _) in pe_world::preset::HEAVY {
        let id = get(label);
        let shape = w.store().get::<Shape>(id).expect("a shape");
        assert_eq!((shape.kind, shape.size, shape.density), (shape_kind::BOX, half, density));
        let kg = density * 8.0 * half[0] * half[1] * half[2];
        assert!(kg >= 150.0, "{label} weighs {kg} kg");
        let body = w.store().get::<Body>(id).expect("a body");
        assert_eq!((body.kind, body.grabbable), (pe_world::components::body_kind::DYNAMIC, 1));
    }
    let stack = get(n::STACK);
    assert!(w.store().get::<Body>(stack).is_none(), "the stack's parent is a plain frame");
    let boxes = snap.children(stack);
    assert_eq!(boxes.len(), n::STACK_BOXES);
    let kinds = |id| w.store().get::<Shape>(id).map(|s| s.kind);
    assert_eq!(
        [n::BOX, n::SPHERE, n::CAPSULE].map(|t| kinds(get(t))),
        [Some(shape_kind::BOX), Some(shape_kind::SPHERE), Some(shape_kind::CAPSULE)]
    );
    // Each template is a ready dynamic, grabbable body; and inert, as is the whole folder.
    let bodies: Vec<ObjectId> = [n::BOX, n::SPHERE, n::CAPSULE].map(get).into_iter().chain(boxes.clone()).collect();
    for &b in &bodies {
        let body = w.store().get::<Body>(b).expect("a body");
        assert_eq!((body.kind, body.grabbable, body.may_sleep), (body_kind::DYNAMIC, 1, 1));
        assert!(w.inert(b));
    }
    assert!(w.inert(folder) && w.inert(stack) && w.is_template_folder(folder));
    // Physical: the floor alone, nothing of the folder (the members' hands have no body).
    assert_eq!(w.physical(), [floor]);
    assert!(!w.inert(floor) && !w.inert(spawned));
    // The stack's boxes start apart, bottom first.
    let y = |id| (w.store().world_transform(id).expect("placed").pos - Pos::ORIGIN).0[1];
    assert!(boxes.windows(2).all(|p| y(p[1]) - y(p[0]) > 2.0 * f64::from(pe_world::preset::STACK_HALF)));
    // The folder is parked below the floor; the floor's shape, the templates' and each member's drawn body (8.2).
    assert!(y(folder) < -40.0);
    let heavy = pe_world::preset::HEAVY.len();
    assert_eq!(w.store().count(ids::SHAPE), 1 + 3 + n::STACK_BOXES + heavy + w.players().len());
}

#[test]
fn the_plane_s_root_is_the_same_on_every_machine_and_not_the_ball_pit_s() {
    let root = |s: &Session| m1_session(s, BlobStore::default()).expect("a world").root_hash(pe_seams::DomainId(0));
    let a = root(&session(Preset::Plane));
    assert_eq!(a, root(&session(Preset::Plane)), "deterministic");
    assert_ne!(a, root(&session(Preset::BallPit { balls: 8 })));
    let mut other = session(Preset::Plane);
    other.seed += 1;
    assert_ne!(a, root(&other), "the seed seeds the ids");
}

#[test]
fn a_copy_of_a_template_is_live_and_starts_at_rest_the_template_stays_inert() {
    let mut w = plane();
    let keys = keys();
    let mut b = EntryBuilder::on_line(&keys, 0, LINE);
    let (stack, spawned) = (find(&w, n::STACK).unwrap(), find(&w, n::SPAWNED).unwrap());
    let size = subtree(&w, stack).len() as u32;
    let ops = vec![
        Op::Copy { source: Target::Existing(stack), new: 0, parent: Target::Existing(spawned), index: None, size },
        Op::SetRow { id: Target::New(0), row: row(Transform::local([1.0, 0.0, -1.0])) },
    ];
    let (o, fp) = edit(&mut w, &mut b, ops);
    let made = applied(&o);
    assert_eq!(made.len(), 1 + n::STACK_BOXES);
    assert_eq!(fp.regrouped, [], "no root was renamed");
    let copy = made[0];
    assert_eq!(w.snapshot(w.frame()).parent(copy), Some(spawned));
    let live: Vec<ObjectId> = made[1..].to_vec();
    for &c in &live {
        assert!(!w.inert(c), "a copy out of the folder is live");
        assert!(w.store().get::<pe_world::components::BodyState>(c).is_none(), "a copy starts at rest");
    }
    let mut physical = w.physical();
    physical.retain(|id| live.contains(id));
    let mut want = live.clone();
    want.sort();
    assert_eq!(physical, want, "its five boxes are physical");
    assert!(subtree(&w, stack).iter().all(|&t| w.inert(t)), "the template is still inert");
}

/// A world with two roots named `Templates`, each holding a body; a root `Shelf` holding one; and a non-root named
/// `Templates` under the shelf, holding one.
fn folders() -> World {
    let mut w = World::new(1, BlobStore::default());
    w.set_member(keys().key(0), perm::EDIT);
    let owner = MemberKey([1; 32]);
    let add = |w: &mut World, id: u128, parent: Option<u128>, name: &str, body: bool| {
        w.create(oid(id), parent.map(oid), owner, GENESIS).expect("created");
        let at =
            if parent.is_some() { Transform::local([0.0, 0.5, 0.0]) } else { Transform::at(pos(id as f64, 0.0, 0.0)) };
        w.insert(oid(id), at).expect("transform");
        let text = w.blobs().put(name.as_bytes());
        w.insert(oid(id), Name { text }).expect("name");
        if body {
            let dynamic = Body { kind: body_kind::DYNAMIC, gravity_scale: 1.0, grabbable: 1, ..Body::default() };
            w.insert(oid(id), dynamic).expect("body");
            w.insert(oid(id), Shape { kind: shape_kind::SPHERE, size: [0.1, 0.0, 0.0], ..Shape::default() })
                .expect("s");
        }
    };
    add(&mut w, 1, None, FOLDER, false);
    add(&mut w, 2, Some(1), "a", true);
    add(&mut w, 3, None, FOLDER, false);
    add(&mut w, 4, Some(3), "b", true);
    add(&mut w, 5, None, "Shelf", false);
    add(&mut w, 6, Some(5), "c", true);
    add(&mut w, 7, Some(5), FOLDER, false);
    add(&mut w, 8, Some(7), "d", true);
    w
}

#[test]
fn every_root_named_templates_is_inert_even_two_and_only_roots_count() {
    let w = folders();
    assert!(is_folder_name(FOLDER) && !is_folder_name("templates") && !is_folder_name("Templates "));
    for k in [1, 2, 3, 4] {
        assert!(w.inert(oid(k)), "{k} is in a folder, though there are two");
    }
    assert!(!w.is_template_folder(oid(7)), "a non-root named Templates is not a folder");
    for k in [5, 6, 7, 8] {
        assert!(!w.inert(oid(k)), "{k} is live");
    }
    assert_eq!(w.physical(), [oid(6), oid(8)]);
    assert!(!w.inert(oid(99)), "nothing is not inert");
}

#[test]
fn renaming_a_root_to_or_from_templates_regroups_its_bodies_and_nothing_else_does() {
    let mut w = folders();
    let keys = keys();
    let mut b = EntryBuilder::new(&keys, 0);
    let rename = |w: &World, id: u128, to: &str| vec![Op::SetRow { id: Target::Existing(oid(id)), row: named(w, to) }];
    // Shelf becomes a folder: its bodies leave play.
    let ops = rename(&w, 5, FOLDER);
    let (o, fp) = edit(&mut w, &mut b, ops);
    applied(&o);
    assert_eq!(fp.regrouped, [oid(5)]);
    assert!(w.inert(oid(6)) && w.inert(oid(8)));
    assert_eq!(w.physical(), []);
    // Back: in play again.
    let ops = rename(&w, 5, "Shelf");
    let (o, fp) = edit(&mut w, &mut b, ops);
    applied(&o);
    assert_eq!(fp.regrouped, [oid(5)]);
    assert_eq!(w.physical(), [oid(6), oid(8)]);
    // Renames that change no folder regroup nothing: a live root to another name, a non-root to Templates, a folder
    // to Templates again.
    for (id, to) in [(5, "Rack"), (6, FOLDER), (1, FOLDER)] {
        let ops = rename(&w, id, to);
        let (o, fp) = edit(&mut w, &mut b, ops);
        applied(&o);
        assert_eq!(fp.regrouped, [], "{id} to {to:?}");
    }
    // Renaming one of two folders puts its subtree in play, and leaves the other's out.
    let ops = rename(&w, 3, "Box room");
    let (o, fp) = edit(&mut w, &mut b, ops);
    applied(&o);
    assert!(w.inert(oid(2)));
    assert_eq!(fp.regrouped, [oid(3)]);
    assert!(!w.inert(oid(4)));
}

/// [`folders`] plus a hand of member 0 at the shelf's body `c`.
fn with_hand() -> (World, MemberKey) {
    let mut w = folders();
    let k = keys().key(0);
    w.create(oid(20), None, k, GENESIS).expect("hand");
    let at = w.store().world_transform(oid(6)).expect("placed").pos;
    w.insert(oid(20), Transform::at(at)).expect("hand transform");
    w.insert(oid(20), Hand { owner: k, ..Hand::default() }).expect("hand row");
    (w, k)
}

#[test]
fn an_edit_that_would_make_a_held_object_inert_or_live_is_refused_held() {
    let (mut w, k) = with_hand();
    let keys = keys();
    let (mut b, mut bi) = (EntryBuilder::new(&keys, 0), EntryBuilder::new(&keys, 0));
    let o = input(&mut w, &mut bi, &Input::Grab { object: oid(6), hand: Some(oid(20)) }).0;
    assert!(matches!(o, Outcome::Applied { .. }), "{o:?}");
    // Renaming the held body's root to Templates, or moving its root under a folder: both refused.
    let rename = vec![Op::SetRow { id: Target::Existing(oid(5)), row: named(&w, FOLDER) }];
    assert_eq!(reason(&edit(&mut w, &mut b, rename).0), Reason::Held { by: k });
    let reparent = vec![Op::Reparent {
        id: Target::Existing(oid(5)),
        parent: Target::Existing(oid(1)),
        index: None,
        keep_world: true,
    }];
    assert_eq!(reason(&edit(&mut w, &mut b, reparent).0), Reason::Held { by: k });
    // A rename that keeps it live passes.
    let other = vec![Op::SetRow { id: Target::Existing(oid(5)), row: named(&w, "Rack") }];
    applied(&edit(&mut w, &mut b, other).0);
}

#[test]
fn a_template_is_out_of_reach_for_grabs_and_pushes_and_never_picked() {
    let (mut w, _) = with_hand();
    let keys = keys();
    let mut bi = EntryBuilder::new(&keys, 0);
    // The hand moves onto the folder's body `a`.
    let at = w.store().world_transform(oid(2)).expect("placed").pos;
    let pose = HandPose { pos: at, rot: [0.0, 0.0, 0.0, 1.0], grip: 0.5, engaged: false };
    let o = input(&mut w, &mut bi, &Input::Hand { hand: oid(20), poses: vec![pose], hold: 1 }).0;
    assert!(matches!(o, Outcome::Applied { .. }), "{o:?}");
    assert_eq!(w.pick_grab(Frame(1), keys.key(0), oid(20), at), None, "a template is never picked");
    let grab = Input::Grab { object: oid(2), hand: Some(oid(20)) };
    assert_eq!(reason(&input(&mut w, &mut bi, &grab).0), Reason::OutOfReach);
    let push = Input::Push { object: oid(2), impulse: [0.0, 0.1, 0.0], at: None };
    assert_eq!(reason(&input(&mut w, &mut bi, &push).0), Reason::OutOfReach);
}

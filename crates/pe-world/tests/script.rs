//! Scripts in the world (world-model.md 7.9, 9.2a; slice D): the tag, `Register` and `CommitScript`, re-keying, the
//! modules table, the logic stages' hooks and import.

use bytes::Bytes;
use pe_core::{Frame, Hash, ObjectId, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, CutImport, DomainId, LeafKey, Outcome, PhysicsImage, Reason, SignedEntry, Tree};
use pe_sim::{ComponentDesc, ComponentId, FieldDesc, FieldKind, Transform};
use pe_world::components::{Hand, Light, ids};
use pe_world::edit::{Actor, Op, Row, Target, Transaction};
use pe_world::script::{ModuleRow, TAG_NAME, script_component_id, tag_desc};
use pe_world::tree::table;
use pe_world::{BlobRef, BlobStore, Claim, Footprint, LogicReport, World, perm};

const ASSET: [u8; 32] = [7; 32];
const OTHER: [u8; 32] = [8; 32];
const LAMP: u128 = 100;
const HAND: u128 = 101;

fn oid(n: u128) -> ObjectId {
    ObjectId::from_u128(n)
}

/// Blink's fields in source version `src`: 1 and 2 have `period` and `phase` as f32 (2 adds `red`); 3 retypes `phase`
/// as f64 (a loss); 4 drops `phase` (a loss).
fn blink_fields(src: u8) -> Vec<FieldDesc> {
    let f32f = |n: &str, at| FieldDesc::new(n, FieldKind::F32, at, 1);
    match src {
        1 => vec![f32f("period", 0), f32f("phase", 4)],
        2 | 5 => vec![f32f("period", 0), f32f("phase", 4), f32f("red", 8)],
        3 => vec![
            f32f("period", 0),
            FieldDesc::new("_pad", FieldKind::U8, 4, 4),
            FieldDesc::new("phase", FieldKind::F64, 8, 1),
        ],
        _ => vec![f32f("period", 0)],
    }
}

fn v_of(src: u8) -> Hash {
    Hash([src; 32])
}

fn blink(src: u8) -> ComponentDesc {
    let id = script_component_id(&v_of(src), "Blink", blink_fields(src)).unwrap();
    ComponentDesc::new(id, "Blink", blink_fields(src)).unwrap()
}

fn tag(src: u8) -> ComponentId {
    tag_desc(&v_of(src)).id()
}

fn commit(asset: [u8; 32], base: Option<pe_seams::Version>, src: u8, toolchain: u8) -> Transaction {
    tx(vec![
        Op::Register { components: vec![tag_desc(&v_of(src)), blink(src)] },
        Op::CommitScript {
            script: asset,
            base,
            source: BlobRef { hash: v_of(src).0, len: 10 },
            toolchain: Hash([toolchain; 32]),
            sdk: Hash([2; 32]),
            author_module: Hash([3; 32]),
            activate_at: 1,
        },
    ])
}

fn tx(ops: Vec<Op>) -> Transaction {
    Transaction { actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] }
}

struct Fx {
    a: EntryBuilder,
    b: EntryBuilder,
    w: World,
}

fn fx() -> Fx {
    let keys = TestKeys::new(1, 3);
    let mut w = World::new(1, BlobStore::default());
    w.set_member(keys.key(0), perm::EDIT);
    w.set_member(keys.key(1), perm::EDIT);
    w.create(oid(LAMP), None, keys.key(0), pe_world::GENESIS).unwrap();
    w.insert(oid(LAMP), Transform::at(Pos::ORIGIN)).unwrap();
    w.create(oid(HAND), None, keys.key(0), pe_world::GENESIS).unwrap();
    w.insert(oid(HAND), Transform::at(Pos::ORIGIN)).unwrap();
    w.insert(oid(HAND), Hand { owner: keys.key(0), ..Hand::default() }).unwrap();
    Fx { a: EntryBuilder::new(&keys, 0), b: EntryBuilder::new(&keys, 1), w }
}

fn run(w: &mut World, b: &mut EntryBuilder, t: &Transaction) -> (Outcome, Footprint) {
    let e: SignedEntry = b.next(Class::Edit, Frame(1), Bytes::from(t.encode()));
    w.apply_edit(Frame(1), &e)
}

fn ok(r: &(Outcome, Footprint)) {
    assert!(matches!(r.0, Outcome::Applied { .. }), "refused: {:?}", r.0);
}

fn refused(r: &(Outcome, Footprint)) -> Reason {
    match &r.0 {
        Outcome::Refused { reason, .. } => reason.clone(),
        a => panic!("applied: {a:?}"),
    }
}

fn set(id: u128, c: ComponentId, bytes: Vec<u8>) -> Op {
    Op::SetRow { id: Target::Existing(oid(id)), row: Row { component: c, instance: 0, bytes } }
}

fn f32s(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn row(w: &World, c: ComponentId) -> Option<Vec<u8>> {
    w.store().get_raw(c, oid(LAMP)).map(<[u8]>::to_vec)
}

/// Commits version 1 and attaches it to the lamp with a Blink row (period 2, phase 0.5).
fn with_v1(f: &mut Fx) {
    ok(&run(&mut f.w, &mut f.a, &commit(ASSET, None, 1, 1)));
    ok(&run(&mut f.w, &mut f.a, &tx(vec![set(LAMP, tag(1), vec![]), set(LAMP, blink(1).id(), f32s(&[2.0, 0.5]))])));
}

#[test]
fn ids_derive_from_the_version_and_the_ops_round_trip() {
    let a = script_component_id(&v_of(1), "Blink", blink_fields(1)).unwrap();
    assert_eq!(a, script_component_id(&v_of(1), "Blink", blink_fields(1)).unwrap(), "deterministic");
    assert_ne!(a, script_component_id(&v_of(2), "Blink", blink_fields(1)).unwrap(), "another version");
    assert_ne!(a, script_component_id(&v_of(1), "Blink", blink_fields(2)).unwrap(), "another schema");
    assert!(a.0 >= 1 << 32 && tag(1).0 >= 1 << 32);
    assert_eq!(tag_desc(&v_of(1)).size(), 0);
    assert_eq!(tag_desc(&v_of(1)).name(), TAG_NAME);
    let t = commit(ASSET, Some(pe_seams::Version { entry: pe_core::EntryRef(Hash([9; 32])), op: 1 }), 3, 1);
    assert_eq!(Transaction::decode(&t.encode()), Some(t));
}

#[test]
fn a_commit_registers_its_version_and_writes_the_modules_row() {
    let mut f = fx();
    let before = f.w.root();
    ok(&run(&mut f.w, &mut f.a, &commit(ASSET, None, 1, 1)));
    let m = &f.w.modules()[&ASSET];
    assert_eq!(m.v(), v_of(1));
    let mut want = vec![tag(1), blink(1).id()];
    want.sort();
    assert_eq!(m.components, want);
    assert_eq!(m.faulted, None);
    assert_eq!(f.w.script_asset(blink(1).id()), Some(ASSET));
    assert_ne!(f.w.root(), before, "the registry and modules tables are in the root");
    assert_eq!(ModuleRow::from_bytes(&m.to_bytes()).as_ref(), Some(m));
    // A new script where one exists, a stamp that is not the frame, a Register without or after its commit.
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &commit(ASSET, None, 2, 1))), Reason::Stale { .. }));
    let mut late = commit(OTHER, None, 2, 1);
    if let Op::CommitScript { activate_at, .. } = &mut late.ops[1] {
        *activate_at = 0;
    }
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &late)), Reason::Invalid { .. }));
    let alone = tx(vec![Op::Register { components: vec![tag_desc(&v_of(2)), blink(2)] }]);
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &alone)), Reason::Invalid { .. }));
    let mut swapped = commit(OTHER, None, 2, 1);
    swapped.ops.swap(0, 1);
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &swapped)), Reason::Invalid { .. }));
    // An id that is not its derivation is refused.
    let mut forged = commit(OTHER, None, 2, 1);
    let bad = ComponentDesc::new(ComponentId((1 << 40) + 5), "Blink", blink_fields(2)).unwrap();
    forged.ops[0] = Op::Register { components: vec![tag_desc(&v_of(2)), bad] };
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &forged)), Reason::Invalid { .. }));
    // Another asset may not take a version any asset has had.
    assert!(matches!(refused(&run(&mut f.w, &mut f.b, &commit(OTHER, None, 1, 1))), Reason::Invalid { .. }));
}

#[test]
fn tags_attach_by_edit_but_never_to_a_hand() {
    let mut f = fx();
    with_v1(&mut f);
    assert_eq!(row(&f.w, tag(1)), Some(vec![]));
    let r = run(&mut f.w, &mut f.a, &tx(vec![set(HAND, tag(1), vec![])]));
    assert!(matches!(refused(&r), Reason::Invalid { .. }));
}

#[test]
fn a_new_version_rekeys_rows_and_supersedes_the_old_ids() {
    let mut f = fx();
    with_v1(&mut f);
    let base = f.w.modules()[&ASSET].version;
    let r = run(&mut f.w, &mut f.a, &commit(ASSET, Some(base), 2, 1));
    ok(&r);
    let rk = r.1.rekey.clone().expect("a re-key report");
    assert_eq!((rk.objects, rk.rows, rk.losses.is_empty()), (vec![oid(LAMP)], 2, true));
    assert_eq!(row(&f.w, blink(1).id()), None, "the old rows moved");
    assert_eq!(row(&f.w, tag(1)), None);
    assert_eq!(row(&f.w, tag(2)), Some(vec![]), "the tag moved too");
    assert_eq!(row(&f.w, blink(2).id()), Some(f32s(&[2.0, 0.5, 0.0])), "fields copied, the new one zero");
    let v = f.w.store().row_version(blink(2).id(), oid(LAMP)).unwrap();
    assert_eq!(v.op, 1, "the commit's op is the moved row's version");
    // An author who resolved names before the commit writes the old ids: Stale against the modules row.
    let stale = run(&mut f.w, &mut f.b, &tx(vec![set(LAMP, blink(1).id(), f32s(&[1.0, 1.0]))]));
    match refused(&stale) {
        Reason::Stale { current, .. } => assert_eq!(current, f.w.modules()[&ASSET].version),
        r => panic!("{r:?}"),
    }
}

#[test]
fn the_same_source_rewrites_only_the_modules_row() {
    let mut f = fx();
    with_v1(&mut f);
    let base = f.w.modules()[&ASSET].version;
    let rv = f.w.store().row_version(blink(1).id(), oid(LAMP));
    let r = run(&mut f.w, &mut f.a, &commit(ASSET, Some(base), 1, 9));
    ok(&r);
    assert_eq!(r.1.rekey, None);
    assert_eq!(f.w.store().row_version(blink(1).id(), oid(LAMP)), rv, "no row version moved");
    assert_eq!(f.w.modules()[&ASSET].toolchain, Hash([9; 32]));
}

#[test]
fn a_lossy_rekey_is_refused_on_others_held_objects_and_reported_elsewhere() {
    let mut f = fx();
    with_v1(&mut f);
    let base = f.w.modules()[&ASSET].version;
    // Held by member b: a retyped field (a loss) on it refuses a's commit.
    let mut held = f.w.clone();
    let kb = pe_seams::test_util::TestKeys::new(1, 3).key(1);
    held.set_claim(oid(LAMP), Claim { hand: oid(HAND), member: kb, since: Frame(0), until: Frame(1000) }).unwrap();
    let r = run(&mut held, &mut f.a, &commit(ASSET, Some(base), 3, 1));
    assert!(matches!(refused(&r), Reason::Invalid { .. }), "RekeyBlocked once pe-seams has it");
    // Not held: it applies, and reports the zeroed field.
    let r = run(&mut f.w, &mut f.a, &commit(ASSET, Some(base), 3, 1));
    ok(&r);
    let rk = r.1.rekey.unwrap();
    assert_eq!(rk.losses.len(), 1);
    assert_eq!((rk.losses[0].field.as_deref(), rk.losses[0].zeroed), (Some("phase"), true));
    assert_eq!(&row(&f.w, blink(3).id()).unwrap()[..4], &2.0f32.to_le_bytes(), "period copied");
}

#[test]
fn a_commit_rekeys_tombstones_so_undelete_brings_back_the_new_version() {
    // @chunks-reviewer's code M1: a tombstone left on the old ids would bring back rows no system binds.
    let mut f = fx();
    with_v1(&mut f);
    ok(&run(&mut f.w, &mut f.a, &tx(vec![Op::Delete { id: Target::Existing(oid(LAMP)) }])));
    let base = f.w.modules()[&ASSET].version;
    let r = run(&mut f.w, &mut f.a, &commit(ASSET, Some(base), 2, 1));
    ok(&r);
    let rk = r.1.rekey.unwrap();
    assert_eq!((rk.objects, rk.rows), (vec![oid(LAMP)], 2), "the tombstone's two rows count");
    let stone = f.w.tombstone(oid(LAMP)).unwrap();
    let cs: Vec<ComponentId> = stone.rows.iter().map(|(c, _, _)| *c).collect();
    assert!(cs.windows(2).all(|w| w[0] < w[1]), "rows stay in component-id order");
    assert!(cs.contains(&tag(2)) && !cs.contains(&tag(1)) && !cs.contains(&blink(1).id()));
    ok(&run(&mut f.w, &mut f.a, &tx(vec![Op::Undelete { id: oid(LAMP) }])));
    assert_eq!(row(&f.w, blink(2).id()), Some(f32s(&[2.0, 0.5, 0.0])), "the new version's row comes back");
    assert_eq!(row(&f.w, blink(1).id()), None);
    assert_eq!(row(&f.w, tag(2)), Some(vec![]));
}

#[test]
fn a_lossy_rekey_is_refused_on_another_owners_protected_tombstone() {
    let mut f = fx();
    with_v1(&mut f);
    let kb = pe_seams::test_util::TestKeys::new(1, 3).key(1);
    f.w.insert(oid(LAMP), pe_world::components::Protection { owner: kb }).unwrap();
    ok(&run(&mut f.w, &mut f.b, &tx(vec![Op::Delete { id: Target::Existing(oid(LAMP)) }])));
    let base = f.w.modules()[&ASSET].version;
    let r = run(&mut f.w, &mut f.a, &commit(ASSET, Some(base), 3, 1));
    assert!(matches!(refused(&r), Reason::Invalid { .. }), "RekeyBlocked once pe-seams has it");
    // The owner may lose their own data.
    ok(&run(&mut f.w, &mut f.b, &commit(ASSET, Some(base), 3, 1)));
}

#[test]
fn a_script_component_name_may_not_pose_as_an_engine_one() {
    // @chunks-reviewer's code Low 1: no `.` outside the tag.
    let mut f = fx();
    let v = v_of(1);
    let fields = blink_fields(1);
    let id = script_component_id(&v, "pe.Light", fields.clone()).unwrap();
    let posing = ComponentDesc::new(id, "pe.Light", fields).unwrap();
    let mut t = commit(ASSET, None, 1, 1);
    t.ops[0] = Op::Register { components: vec![tag_desc(&v), posing] };
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &t)), Reason::Invalid { .. }));
}

#[test]
fn the_logic_hooks_guard_check_tombstone_size_and_fault() {
    let mut f = fx();
    with_v1(&mut f);
    let kb = pe_seams::test_util::TestKeys::new(1, 3).key(1);
    f.w.create(oid(200), None, kb, pe_world::GENESIS).unwrap();
    f.w.insert(oid(200), Transform::at(Pos::ORIGIN)).unwrap();
    f.w.set_claim(oid(200), Claim { hand: oid(HAND), member: kb, since: Frame(0), until: Frame(1000) }).unwrap();
    let spawned = oid(300);
    let before = f.w.root();
    f.w.logic_stage(|store, guard, faulted, check| {
        assert!(guard(oid(200)) && !guard(oid(LAMP)), "held objects are guarded");
        assert!(faulted.is_empty());
        let t = bytemuck::bytes_of(&Transform::at(Pos::ORIGIN)).to_vec();
        assert!(check(ComponentId::TRANSFORM, oid(LAMP), &t).is_ok());
        assert!(check(ComponentId::TRANSFORM, oid(200), &t).is_err(), "a held pose is the holder's");
        assert!(check(ComponentId::TRANSFORM, oid(HAND), &t).is_err(), "never a hand");
        assert!(check(ids::SHAPE, oid(LAMP), &[0; 4]).is_err(), "not marked writable");
        let bad = Light { kind: pe_world::components::light_kind::POINT, inner_angle: 0.5, ..Light::default() };
        assert!(check(ids::LIGHT, oid(LAMP), bytemuck::bytes_of(&bad)).is_err(), "Light's angle rule");
        // The runner's work: a spawn with the tag, and a destroy of the lamp.
        store.create(spawned).unwrap();
        store.insert_raw(tag(1), spawned, &[]).unwrap();
        store.destroy(oid(LAMP)).unwrap();
    });
    let report = LogicReport {
        created: vec![(spawned, oid(LAMP))],
        removed: vec![oid(LAMP)],
        orphaned: vec![],
        written: vec![],
        faults: vec![(ASSET, 1)],
    };
    let fp = f.w.after_logic(Frame(1), &report);
    assert_eq!(fp.created, vec![spawned]);
    assert!(f.w.store().get::<pe_world::components::SimSize>(spawned).is_some());
    assert!(f.w.tombstone(oid(LAMP)).is_some(), "a script's destroy leaves a tombstone");
    assert_eq!(f.w.modules()[&ASSET].faulted, Some((Frame(1), 1)));
    assert_ne!(f.w.root(), before);
    // Undelete brings the lamp back with its rows.
    ok(&run(&mut f.w, &mut f.a, &tx(vec![Op::Undelete { id: oid(LAMP) }])));
    assert_eq!(row(&f.w, blink(1).id()), Some(f32s(&[2.0, 0.5])));
    // A later fault keeps the first; the next commit clears it.
    f.w.logic_stage(|_, _, faulted, _| assert!(faulted.contains(&ASSET)));
    f.w.after_logic(Frame(2), &LogicReport { faults: vec![(ASSET, 2)], ..LogicReport::default() });
    assert_eq!(f.w.modules()[&ASSET].faulted, Some((Frame(1), 1)));
    let base = f.w.modules()[&ASSET].version;
    ok(&run(&mut f.w, &mut f.a, &commit(ASSET, Some(base), 1, 4)));
    assert_eq!(f.w.modules()[&ASSET].faulted, None);
}

#[test]
fn a_joiner_imports_the_scripts_registry_and_modules() {
    let mut f = fx();
    with_v1(&mut f);
    let d = DomainId(1);
    let target = &f.w;
    let t = target.tree(d);
    let mut keys: Vec<LeafKey> = target.store().objects().iter().map(|&id| LeafKey::Object(id)).collect();
    keys.extend((0..table::COUNT as u8).map(LeafKey::Table));
    keys.push(LeafKey::Config);
    let imp = CutImport {
        cut: pe_seams::Cut {
            at: pe_seams::CutRef { line: target.line(), height: 0, frame: target.frame() },
            heads: Vec::new(),
        },
        target_root: t.root(),
        records: t.export(&keys),
        absent: Vec::new(),
        physics: Some(PhysicsImage { domain: d, frame: target.frame(), global: Hash::ZERO, bytes: Default::default() }),
    };
    let joiner = World::new(0, target.blobs().clone());
    let got = joiner.import(d, &imp).unwrap();
    assert_eq!(got.root_hash(d), target.root_hash(d));
    assert_eq!(got.modules(), target.modules());
    assert_eq!(got.script_asset(blink(1).id()), Some(ASSET));
    assert_eq!(got.store().get_raw(blink(1).id(), oid(LAMP)), target.store().get_raw(blink(1).id(), oid(LAMP)));
}

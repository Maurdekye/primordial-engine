//! Scripts in the world (world-model.md 7.9, 9.2a; slice D): the tag, `Register` and `CommitScript`, re-keying, the
//! modules table, the logic stages' hooks and import.

use bytes::Bytes;
use pe_core::{Frame, Hash, ObjectId, Pos};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, CutImport, DomainId, LeafKey, Outcome, PhysicsImage, Reason, SignedEntry, Tree};
use pe_sim::{ColorSpace, ComponentDesc, ComponentId, FieldDesc, FieldKind, Transform};
use pe_world::components::{Hand, Light, ids};
use pe_world::edit::{Actor, AssetTarget, Op, Row, Target, Transaction};
use pe_world::script::{ModuleRow, TAG_NAME, attest_window, pack_source, quorum, script_component_id, tag_desc};
use pe_world::tree::table;
use pe_world::{
    BlobRef, BlobStore, Blocked, BuildFailure, Claim, Footprint, FrameEnd, LogicReport, VoidKind, Voided, World, perm,
};

const LAMP: u128 = 100;
const HAND: u128 = 101;

fn oid(n: u128) -> ObjectId {
    ObjectId::from_u128(n)
}

/// Blink's fields in source version `src`: 1 and 2 have `period` and `phase` as f32 (2 adds `red`); 3 retypes `phase`
/// as f64 (a loss); 4 drops `phase` (a loss); 6 adds `tint`, a colour (3.6).
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
        6 => vec![f32f("period", 0), FieldDesc::new("tint", FieldKind::F32, 4, 3).with_color(ColorSpace::LinearSrgb)],
        _ => vec![f32f("period", 0)],
    }
}

/// Source version `src`'s archive (logic.md 4.1a): a crate whose `lib.rs` names the version.
fn archive(src: u8) -> bytes::Bytes {
    let lib = format!("// blink, source version {src}\n");
    let files: [(&str, &[u8]); 1] = [("src/lib.rs", lib.as_bytes())];
    pack_source(&files).unwrap().into()
}

/// V, source version `src`'s hash.
fn v_of(src: u8) -> Hash {
    Hash(BlobRef::of(&archive(src)).hash)
}

fn blink(src: u8) -> ComponentDesc {
    let id = script_component_id(&v_of(src), "Blink", blink_fields(src)).unwrap();
    ComponentDesc::new(id, "Blink", blink_fields(src)).unwrap()
}

fn tag(src: u8) -> ComponentId {
    tag_desc(&v_of(src)).id()
}

fn commit_to(target: AssetTarget, base: Option<pe_seams::Version>, src: u8, toolchain: u8) -> Transaction {
    tx(vec![
        Op::Register { components: vec![tag_desc(&v_of(src)), blink(src)] },
        Op::commit_script(target, base, archive(src), Hash([toolchain; 32]), Hash([2; 32]), Hash([3; 32]), 1),
    ])
}

/// A commit to the existing asset `asset`.
fn commit(asset: [u8; 32], base: Option<pe_seams::Version>, src: u8, toolchain: u8) -> Transaction {
    commit_to(AssetTarget::Existing(asset), base, src, toolchain)
}

/// A new asset's first version: its id is derived from the entry that carries it (7.9).
fn first(src: u8, toolchain: u8) -> Transaction {
    commit_to(AssetTarget::New(0), None, src, toolchain)
}

/// The id of the asset created by member `i`'s `seq`-th entry, as `New(0)`.
fn asset_of(i: usize, seq: u64) -> [u8; 32] {
    let keys = TestKeys::new(1, 3);
    let mut b = EntryBuilder::new(&keys, i);
    let e = (0..seq).map(|_| b.next(Class::Edit, Frame(1), Bytes::new())).last().expect("seq >= 1");
    let e = &e.entry;
    pe_world::script::asset_created(e.line, e.author, e.chain, e.seq, 0)
}

/// The tests' asset: the one A's first entry creates.
fn asset() -> [u8; 32] {
    asset_of(0, 1)
}

/// The asset a commit names: an existing one, or for a new one the tests' asset.
fn named(t: AssetTarget) -> [u8; 32] {
    match t {
        AssetTarget::Existing(a) => a,
        AssetTarget::New(_) => asset(),
    }
}

fn tx(ops: Vec<Op>) -> Transaction {
    Transaction { inline: vec![], actor: Actor::Person, label: None, preconditions: vec![], ops, blobs: vec![] }
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

fn run_at(w: &mut World, b: &mut EntryBuilder, t: &Transaction, at: u64) -> (Outcome, Footprint) {
    let e: SignedEntry = b.next(Class::Edit, Frame(at), Bytes::from(t.encode()));
    w.apply_edit(Frame(at), &e)
}

/// The attestation of `t`'s commit (its asset, V and author module).
fn attest_of(t: &Transaction) -> Transaction {
    let Some(Op::CommitScript { script, source, author_module, .. }) = t.ops.last() else { panic!("no commit") };
    tx(vec![Op::AttestBuild { script: named(*script), version: Hash(source.hash), module: *author_module }])
}

/// Lands commit `t` at frame 1 (logic.md 6.2a): one member commits it, the other attests, and the frame ends, which
/// activates it. Returns the commit's outcome and what the frame's end did.
fn land(f: &mut Fx, by_b: bool, t: &Transaction) -> ((Outcome, Footprint), FrameEnd) {
    let (by, other) = if by_b { (&mut f.b, &mut f.a) } else { (&mut f.a, &mut f.b) };
    let r = run(&mut f.w, by, t);
    if matches!(r.0, Outcome::Applied { .. }) {
        ok(&run(&mut f.w, other, &attest_of(t)));
    }
    let end = f.w.end_frame(Frame(1));
    (r, end)
}

/// The re-key report of the one activation `end` holds.
fn rekeyed(end: &FrameEnd) -> pe_world::RekeyReport {
    assert_eq!(end.activated.len(), 1, "{end:?}");
    end.activated[0].2.clone().expect("a re-key report")
}

/// A void of `asset()`'s commit `pv`, as `end_frame` reports it.
fn void(
    pv: pe_seams::Version,
    kind: VoidKind,
    blocked: Option<Blocked>,
    failed: Vec<(pe_core::MemberKey, BuildFailure)>,
) -> Voided {
    Voided { asset: asset(), version: pv, kind, blocked, failed }
}

/// `t`'s commit failed to build here, for `why` (logic.md 6.2a item 2).
fn failed_of(t: &Transaction, why: BuildFailure) -> Transaction {
    let Some(Op::CommitScript { script, source, .. }) = t.ops.last() else { panic!("no commit") };
    let module = (why == BuildFailure::Mismatch).then_some(Hash([9; 32]));
    tx(vec![Op::BuildFailed {
        script: named(*script),
        version: Hash(source.hash),
        reason: why,
        toolchain: Hash([1; 32]),
        sdk: Hash([2; 32]),
        module,
    }])
}

/// Commits version 1 and attaches it to the lamp with a Blink row (period 2, phase 0.5).
fn with_v1(f: &mut Fx) {
    let (r, _) = land(f, false, &first(1, 1));
    ok(&r);
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
    let t = commit(asset(), Some(pe_seams::Version { entry: pe_core::EntryRef(Hash([9; 32])), op: 1 }), 3, 1);
    assert_eq!(Transaction::decode(&t.encode()), Some(t));
}

#[test]
fn a_commit_registers_its_version_and_writes_the_modules_row() {
    let mut f = fx();
    let before = f.w.root();
    ok(&land(&mut f, false, &first(1, 1)).0);
    let m = &f.w.modules()[&asset()];
    assert_eq!(m.v(), v_of(1));
    let mut want = vec![tag(1), blink(1).id()];
    want.sort();
    assert_eq!(m.components, want);
    assert_eq!(m.faulted, None);
    assert_eq!(f.w.script_asset(blink(1).id()), Some(asset()));
    assert_ne!(f.w.root(), before, "the registry and modules tables are in the root");
    assert_eq!(ModuleRow::from_bytes(&m.to_bytes()).as_ref(), Some(m));
    // A new script where one exists, a stamp that is not the frame, a Register without or after its commit.
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &commit(asset(), None, 2, 1))), Reason::Stale { .. }));
    let mut late = first(2, 1);
    if let Op::CommitScript { activate_at, .. } = &mut late.ops[1] {
        *activate_at = 0;
    }
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &late)), Reason::Invalid { .. }));
    let alone = tx(vec![Op::Register { components: vec![tag_desc(&v_of(2)), blink(2)] }]);
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &alone)), Reason::Invalid { .. }));
    let mut swapped = first(2, 1);
    swapped.ops.swap(0, 1);
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &swapped)), Reason::Invalid { .. }));
    // An id that is not its derivation is refused.
    let mut forged = first(2, 1);
    let bad = ComponentDesc::new(ComponentId((1 << 40) + 5), "Blink", blink_fields(2)).unwrap();
    forged.ops[0] = Op::Register { components: vec![tag_desc(&v_of(2)), bad] };
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &forged)), Reason::Invalid { .. }));
    // Another asset may not take a version any asset has had.
    assert!(matches!(refused(&run(&mut f.w, &mut f.b, &first(1, 1))), Reason::Invalid { .. }));
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
    let base = f.w.modules()[&asset()].version;
    let (r, end) = land(&mut f, false, &commit(asset(), Some(base), 2, 1));
    ok(&r);
    assert_eq!(r.1.rekey, None, "the commit's entry re-keys nothing (6.2a)");
    let rk = rekeyed(&end);
    assert_eq!((rk.objects, rk.rows, rk.losses.is_empty()), (vec![oid(LAMP)], 2, true));
    f.w.verify_tree_cache(DomainId(1)).expect("the tree cache stays true after an activation's re-key");
    assert_eq!(row(&f.w, blink(1).id()), None, "the old rows moved");
    assert_eq!(row(&f.w, tag(1)), None);
    assert_eq!(row(&f.w, tag(2)), Some(vec![]), "the tag moved too");
    assert_eq!(row(&f.w, blink(2).id()), Some(f32s(&[2.0, 0.5, 0.0])), "fields copied, the new one zero");
    let v = f.w.store().row_version(blink(2).id(), oid(LAMP)).unwrap();
    assert_eq!(v.op, 1, "the commit's op is the moved row's version");
    // An author who resolved names before the commit writes the old ids: Stale against the modules row.
    let stale = run(&mut f.w, &mut f.b, &tx(vec![set(LAMP, blink(1).id(), f32s(&[1.0, 1.0]))]));
    match refused(&stale) {
        Reason::Stale { current, .. } => assert_eq!(current, f.w.modules()[&asset()].version),
        r => panic!("{r:?}"),
    }
}

#[test]
fn the_same_source_rewrites_only_the_modules_row() {
    let mut f = fx();
    with_v1(&mut f);
    let base = f.w.modules()[&asset()].version;
    let rv = f.w.store().row_version(blink(1).id(), oid(LAMP));
    let (r, end) = land(&mut f, false, &commit(asset(), Some(base), 1, 9));
    ok(&r);
    assert_eq!(end.activated.iter().map(|a| (a.0, a.2.clone())).collect::<Vec<_>>(), vec![(asset(), None)]);
    assert_eq!(f.w.store().row_version(blink(1).id(), oid(LAMP)), rv, "no row version moved");
    assert_eq!(f.w.modules()[&asset()].toolchain, Hash([9; 32]));
}

#[test]
fn a_lossy_rekey_is_refused_on_others_held_objects_and_reported_elsewhere() {
    let mut f = fx();
    with_v1(&mut f);
    let base = f.w.modules()[&asset()].version;
    // Held by member b: a retyped field (a loss) on it refuses a's commit.
    let mut held = f.w.clone();
    let kb = pe_seams::test_util::TestKeys::new(1, 3).key(1);
    held.set_claim(oid(LAMP), Claim { hand: oid(HAND), member: kb, since: Frame(0), until: Frame(1000) }).unwrap();
    let r = run(&mut held, &mut f.a, &commit(asset(), Some(base), 3, 1));
    assert_eq!(refused(&r), Reason::RekeyBlocked { objects: vec![oid(LAMP)], more: 0 });
    // Not held: it applies, and its activation reports the zeroed field.
    let (r, end) = land(&mut f, false, &commit(asset(), Some(base), 3, 1));
    ok(&r);
    let rk = rekeyed(&end);
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
    let base = f.w.modules()[&asset()].version;
    let (r, end) = land(&mut f, false, &commit(asset(), Some(base), 2, 1));
    ok(&r);
    let rk = rekeyed(&end);
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
    let base = f.w.modules()[&asset()].version;
    let r = run(&mut f.w, &mut f.a, &commit(asset(), Some(base), 3, 1));
    assert_eq!(refused(&r), Reason::RekeyBlocked { objects: vec![oid(LAMP)], more: 0 }, "the tombstone blocks");
    // The owner may lose their own data.
    let (r, end) = land(&mut f, true, &commit(asset(), Some(base), 3, 1));
    ok(&r);
    assert_eq!(rekeyed(&end).losses.len(), 1);
}

#[test]
fn a_script_component_name_may_not_pose_as_an_engine_one() {
    // @chunks-reviewer's code Low 1: no `.` outside the tag.
    let mut f = fx();
    let v = v_of(1);
    let fields = blink_fields(1);
    let id = script_component_id(&v, "pe.Light", fields.clone()).unwrap();
    let posing = ComponentDesc::new(id, "pe.Light", fields).unwrap();
    let mut t = first(1, 1);
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
        faults: vec![(asset(), 1)],
    };
    let fp = f.w.after_logic(Frame(1), &report);
    assert_eq!(fp.created, vec![spawned]);
    assert!(f.w.store().get::<pe_world::components::SimSize>(spawned).is_some());
    assert!(f.w.tombstone(oid(LAMP)).is_some(), "a script's destroy leaves a tombstone");
    assert_eq!(f.w.modules()[&asset()].faulted, Some((Frame(1), 1)));
    assert_ne!(f.w.root(), before);
    // Undelete brings the lamp back with its rows.
    ok(&run(&mut f.w, &mut f.a, &tx(vec![Op::Undelete { id: oid(LAMP) }])));
    assert_eq!(row(&f.w, blink(1).id()), Some(f32s(&[2.0, 0.5])));
    // A later fault keeps the first; the next commit clears it.
    f.w.logic_stage(|_, _, faulted, _| assert!(faulted.contains(&asset())));
    f.w.after_logic(Frame(2), &LogicReport { faults: vec![(asset(), 2)], ..LogicReport::default() });
    assert_eq!(f.w.modules()[&asset()].faulted, Some((Frame(1), 1)));
    let base = f.w.modules()[&asset()].version;
    ok(&land(&mut f, false, &commit(asset(), Some(base), 1, 4)).0);
    assert_eq!(f.w.modules()[&asset()].faulted, None);
}

#[test]
fn a_joiner_imports_the_scripts_registry_and_modules() {
    let mut f = fx();
    with_v1(&mut f);
    // Version 2 pending too: the modules table carries both (6.2a).
    let base = f.w.modules()[&asset()].version;
    ok(&run(&mut f.w, &mut f.a, &commit(asset(), Some(base), 2, 1)));
    assert!(f.w.pending().contains_key(&asset()));
    let d = DomainId(1);
    let target = &f.w;
    let t = target.tree(d);
    let mut keys: Vec<LeafKey> = target.store().objects().iter().map(|&id| LeafKey::Object(id)).collect();
    keys.extend((0..table::COUNT as u8).map(LeafKey::Table));
    keys.push(LeafKey::Config);
    let imp = CutImport {
        blobs: vec![],
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
    assert_eq!(got.pending(), target.pending());
    assert_eq!(got.script_asset(blink(1).id()), Some(asset()));
    assert_eq!(got.store().get_raw(blink(1).id(), oid(LAMP)), target.store().get_raw(blink(1).id(), oid(LAMP)));
}

/// A commit whose source archive is over the cap or not canonical is refused, and the old version keeps running
/// (logic.md 4.1a; @coordinator-opus m9f5e6b2ce690): the size first, from the length, then the form.
#[test]
fn a_commit_with_a_bad_source_is_refused_and_the_old_version_stays() {
    let mut f = fx();
    with_v1(&mut f);
    let before = (f.w.root(), f.w.modules()[&asset()].clone());
    let base = Some(f.w.modules()[&asset()].version);
    let with = |archive: Vec<u8>| {
        let mut t = commit(asset(), base, 2, 1);
        let src = Bytes::from(archive);
        t.ops[1] = Op::commit_script(
            AssetTarget::Existing(asset()),
            base,
            src,
            Hash([1; 32]),
            Hash([2; 32]),
            Hash([3; 32]),
            1,
        );
        t
    };
    let raw = |files: &[(&[u8], &[u8])]| {
        let mut out = b"pe.src.v0".to_vec();
        for (p, b) in files {
            for part in [*p, *b] {
                out.extend_from_slice(&(part.len() as u32).to_le_bytes());
                out.extend_from_slice(part);
            }
        }
        out
    };
    let cap = pe_world::script::MAX_SOURCE_BYTES;
    let over = raw(&[(b"src/lib.rs", "/".repeat(cap as usize - 26).as_bytes())]);
    assert_eq!(refused(&run(&mut f.w, &mut f.a, &with(over))), Reason::TooLarge { limit: cap });
    // Over the cap and malformed besides: the size is checked first.
    let over_bad = raw(&[(b"src/A.rs", "/".repeat(cap as usize).as_bytes())]);
    assert_eq!(refused(&run(&mut f.w, &mut f.a, &with(over_bad))), Reason::TooLarge { limit: cap });
    // A case-collision pair, and an archive with no lib.rs.
    let pair = raw(&[(b"src/A.rs", b""), (b"src/a.rs", b""), (b"src/lib.rs", b"")]);
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &with(pair))), Reason::Invalid { .. }));
    let no_lib = raw(&[(b"src/a.rs", b"")]);
    assert!(matches!(refused(&run(&mut f.w, &mut f.a, &with(no_lib))), Reason::Invalid { .. }));
    assert_eq!((f.w.root(), f.w.modules()[&asset()].clone()), before, "nothing changed");
    assert_eq!(row(&f.w, blink(1).id()), Some(f32s(&[2.0, 0.5])), "version 1's rows stay");
    // The same commit with its canonical source applies, and the machine keeps the source.
    ok(&land(&mut f, false, &commit(asset(), base, 2, 1)).0);
    let m = &f.w.modules()[&asset()];
    assert_eq!(m.v(), v_of(2));
    assert_eq!(f.w.blobs().get(&m.source).as_deref(), Some(&archive(2)[..]));
}

/// Machine B, with its own blob store, applies machine A's signed commit: B's store then holds the source, so B can
/// build the version (5.3; @coordinator-opus m3b68151991e0). A refused commit stores nothing.
#[test]
fn another_machine_holds_the_source_after_applying_the_commit() {
    // Two machines: each fx() builds the same world on its own, empty store.
    let mut a_keys = fx().a;
    let mut b_world = fx().w;
    let t = first(1, 1);
    let e: SignedEntry = a_keys.next(Class::Edit, Frame(1), Bytes::from(t.encode()));
    let src = BlobRef::of(&archive(1));
    assert_eq!(b_world.blobs().get(&src), None);
    assert!(matches!(b_world.apply_edit(Frame(1), &e).0, Outcome::Applied { .. }));
    assert_eq!(b_world.blobs().get(&src).as_deref(), Some(&archive(1)[..]));
    // A refused commit (another while one is pending) leaves B's store without its source.
    let again = commit(asset(), None, 2, 1);
    let e2: SignedEntry = a_keys.next(Class::Edit, Frame(1), Bytes::from(again.encode()));
    assert!(matches!(b_world.apply_edit(Frame(1), &e2).0, Outcome::Refused { .. }));
    assert_eq!(b_world.blobs().get(&BlobRef::of(&archive(2))), None);
}

/// The deadline's length and the quorum (logic.md 6.2a; agreement.md), pinned.
#[test]
fn the_attestation_window_and_quorum_are_pinned() {
    assert_eq!(attest_window(1_213, 60), 2_786, "the lamp: 2 x 23.213 s at 60 Hz, rounded up");
    assert_eq!(attest_window(262_144, 60), 34_098, "the cap: 2 x 284.144 s");
    assert_eq!(attest_window(0, 90), 3_960, "2 x 22 s at 90 Hz");
    let q: Vec<usize> = (1..=7).map(quorum).collect();
    assert_eq!(q, vec![1, 2, 2, 3, 4, 4, 5]);
}

/// A commit is pending until a quorum of the voters at its frame attests it (logic.md 6.2a): the author's own
/// counts; a wrong module, a non-voter's, a second one and one for another version do not; the second voter's
/// activates it at the end of that frame, and it runs from the next.
#[test]
fn a_commit_is_pending_until_a_quorum_attests_it() {
    let mut f = fx();
    let before = f.w.root();
    let t = first(1, 1);
    let r = run(&mut f.w, &mut f.a, &t);
    ok(&r);
    assert!(f.w.modules().is_empty(), "nothing runs yet");
    let p = f.w.pending()[&asset()].clone();
    let (ka, kb) = (pe_seams::test_util::TestKeys::new(1, 3).key(0), pe_seams::test_util::TestKeys::new(1, 3).key(1));
    assert_eq!(p.voters, {
        let mut v = vec![ka, kb];
        v.sort();
        v
    });
    assert_eq!(p.attested, vec![(ka, Frame(1))], "the commit is its author's attestation");
    assert_eq!((p.commit_frame, p.deadline), (Frame(1), Frame(1 + attest_window(p.source.len, 60))));
    assert_ne!(f.w.root(), before, "the pending commit is in the root");
    // A second commit while one is pending: Stale, the pending one the winner.
    match refused(&run(&mut f.w, &mut f.b, &commit(asset(), None, 2, 1))) {
        Reason::Stale { winner, .. } => assert_eq!(winner, p.version.entry),
        r => panic!("{r:?}"),
    }
    // One attestation of two: no quorum at the frame's end.
    assert_eq!(f.w.end_frame(Frame(1)), FrameEnd::default());
    // Not counted: another module, another version, a non-voter's, an attestation alongside another op.
    let mut wrong = attest_of(&t);
    if let Op::AttestBuild { module, .. } = &mut wrong.ops[0] {
        *module = Hash([7; 32]);
    }
    assert!(matches!(refused(&run_at(&mut f.w, &mut f.b, &wrong, 2)), Reason::Invalid { .. }));
    let mut other_v = attest_of(&t);
    if let Op::AttestBuild { version, .. } = &mut other_v.ops[0] {
        *version = v_of(2);
    }
    assert!(matches!(refused(&run_at(&mut f.w, &mut f.b, &other_v, 2)), Reason::Stale { .. }));
    let keys = pe_seams::test_util::TestKeys::new(1, 3);
    let mut outsider = EntryBuilder::new(&keys, 2);
    assert!(matches!(refused(&run_at(&mut f.w, &mut outsider, &attest_of(&t), 2)), Reason::Invalid { .. }));
    let mut two = attest_of(&t);
    two.ops.push(Op::Delete { id: Target::Existing(oid(LAMP)) });
    assert!(matches!(refused(&run_at(&mut f.w, &mut f.b, &two, 2)), Reason::Invalid { .. }));
    assert!(matches!(refused(&run_at(&mut f.w, &mut f.a, &attest_of(&t), 2)), Reason::Invalid { .. }), "a's twice");
    // b's completes the quorum, stamped 2: the commit activates at the end of frame 2 and runs from 3.
    ok(&run_at(&mut f.w, &mut f.b, &attest_of(&t), 2));
    assert!(f.w.modules().is_empty(), "not before the frame's end");
    assert_eq!(f.w.end_frame(Frame(2)).activated, vec![(asset(), p.version, None)]);
    f.w.verify_tree_cache(DomainId(1)).unwrap();
    assert!(f.w.pending().is_empty());
    let m = &f.w.modules()[&asset()];
    assert_eq!((m.version, m.v(), m.activate_at), (p.version, v_of(1), Frame(3)));
    // After activation an attestation is Stale.
    assert!(matches!(refused(&run_at(&mut f.w, &mut f.b, &attest_of(&t), 3)), Reason::Stale { .. }));
}

/// `AttestBuild` needs no `EDIT`: only a place among the voters at the commit's frame (logic.md 6.2a).
#[test]
fn a_voter_without_edit_attests() {
    let mut f = fx();
    let keys = pe_seams::test_util::TestKeys::new(1, 3);
    f.w.set_member(keys.key(1), perm::AGENT);
    let t = first(1, 1);
    ok(&run(&mut f.w, &mut f.a, &t));
    ok(&run(&mut f.w, &mut f.b, &attest_of(&t)));
    assert_eq!(f.w.end_frame(Frame(1)).activated.len(), 1);
}

/// No quorum by the deadline: the commit is voided at `end_frame(D)`, the old version stays, the void is recorded,
/// and the asset may commit again. A quorum completed at `D` itself activates instead (activation is tested first).
#[test]
fn an_unattested_commit_is_voided_at_its_deadline_unless_the_quorum_completes_then() {
    let mut f = fx();
    with_v1(&mut f);
    let base = f.w.modules()[&asset()].version;
    let t = commit(asset(), Some(base), 2, 1);
    let mut g = f.w.clone();
    ok(&run(&mut f.w, &mut f.a, &t));
    let d = f.w.pending()[&asset()].deadline;
    assert_eq!(f.w.end_frame(Frame(d.0 - 1)), FrameEnd::default());
    let root = f.w.root();
    let pv = f.w.pending()[&asset()].version;
    assert_eq!(f.w.end_frame(d).voided, vec![void(pv, VoidKind::Unattested, None, vec![])]);
    assert_ne!(f.w.root(), root, "the void is recorded in the root");
    assert_eq!(f.w.pending()[&asset()].void, Some((d, VoidKind::Unattested)));
    assert_eq!(f.w.modules()[&asset()].version, base, "the old version stays");
    assert_eq!(row(&f.w, blink(1).id()), Some(f32s(&[2.0, 0.5])), "nothing re-keyed");
    assert!(matches!(refused(&run_at(&mut f.w, &mut f.b, &attest_of(&t), d.0 + 1)), Reason::Stale { .. }));
    // The asset may commit again after a void.
    let mut again = commit(asset(), Some(base), 2, 1);
    if let Some(Op::CommitScript { activate_at, .. }) = again.ops.last_mut() {
        *activate_at = d.0 + 1;
    }
    ok(&run_at(&mut f.w, &mut f.a, &again, d.0 + 1));
    assert_eq!(f.w.pending()[&asset()].void, None);
    // In a copy, b's attestation stamped at D completes the quorum there: it activates.
    let mut a2 = EntryBuilder::new(&pe_seams::test_util::TestKeys::new(1, 3), 0);
    let mut b2 = EntryBuilder::new(&pe_seams::test_util::TestKeys::new(1, 3), 1);
    ok(&run(&mut g, &mut a2, &t));
    ok(&run_at(&mut g, &mut b2, &attest_of(&t), d.0));
    let end = g.end_frame(d);
    assert!(end.voided.is_empty() && end.activated.len() == 1, "{end:?}");
    g.verify_tree_cache(DomainId(1)).expect("the tree cache stays true after an activation's re-key");
}

/// The re-key is judged again at `F` (logic.md 6.2a): an object the committer may not edit, held by another member
/// since the commit, voids it there; nothing moves and the old version stays.
#[test]
fn a_rekey_blocked_by_the_time_of_activation_voids_the_commit() {
    let mut f = fx();
    with_v1(&mut f);
    let base = f.w.modules()[&asset()].version;
    let t = commit(asset(), Some(base), 3, 1);
    ok(&run(&mut f.w, &mut f.a, &t));
    let kb = pe_seams::test_util::TestKeys::new(1, 3).key(1);
    f.w.set_claim(oid(LAMP), Claim { hand: oid(HAND), member: kb, since: Frame(1), until: Frame(1000) }).unwrap();
    ok(&run(&mut f.w, &mut f.b, &attest_of(&t)));
    let pv = f.w.pending()[&asset()].version;
    let blocked = Some(Blocked { objects: vec![oid(LAMP)], more: 0 });
    assert_eq!(f.w.end_frame(Frame(1)).voided, vec![void(pv, VoidKind::RekeyBlockedAtActivation, blocked, vec![])]);
    assert_eq!(f.w.modules()[&asset()].version, base);
    assert_eq!(row(&f.w, blink(1).id()), Some(f32s(&[2.0, 0.5])), "nothing re-keyed");
}

/// An author whose `EDIT` is revoked between the commit and `F`: the commit is voided at `F`, the old version
/// stays (@coordinator-opus m41d775f5d9f9).
#[test]
fn an_author_revoked_before_activation_voids_the_commit() {
    let mut f = fx();
    with_v1(&mut f);
    let base = f.w.modules()[&asset()].version;
    let t = commit(asset(), Some(base), 2, 1);
    ok(&run(&mut f.w, &mut f.a, &t));
    let ka = pe_seams::test_util::TestKeys::new(1, 3).key(0);
    f.w.set_member(ka, perm::AGENT);
    ok(&run(&mut f.w, &mut f.b, &attest_of(&t)));
    let pv = f.w.pending()[&asset()].version;
    assert_eq!(f.w.end_frame(Frame(1)).voided, vec![void(pv, VoidKind::AuthorRevokedAtActivation, None, vec![])]);
    assert_eq!(f.w.modules()[&asset()].version, base);
    assert_eq!(row(&f.w, blink(1).id()), Some(f32s(&[2.0, 0.5])), "nothing re-keyed");
}

/// A blocked re-key names the first 16 blocking objects in id order, and how many more (7.9; S2 v1.6).
#[test]
fn a_blocked_rekey_names_the_first_16_objects_and_counts_the_rest() {
    let mut f = fx();
    with_v1(&mut f);
    let keys = pe_seams::test_util::TestKeys::new(1, 3);
    let kb = keys.key(1);
    let held: Vec<u128> = (400..418).rev().collect();
    for &id in &held {
        f.w.create(oid(id), None, keys.key(0), pe_world::GENESIS).unwrap();
        f.w.insert(oid(id), Transform::at(Pos::ORIGIN)).unwrap();
        ok(&run(&mut f.w, &mut f.a, &tx(vec![set(id, tag(1), vec![]), set(id, blink(1).id(), f32s(&[1.0, 0.5]))])));
        f.w.set_claim(oid(id), Claim { hand: oid(HAND), member: kb, since: Frame(0), until: Frame(1000) }).unwrap();
    }
    let base = f.w.modules()[&asset()].version;
    let mut want: Vec<ObjectId> = held.iter().map(|&id| oid(id)).collect();
    want.sort();
    match refused(&run(&mut f.w, &mut f.a, &commit(asset(), Some(base), 3, 1))) {
        Reason::RekeyBlocked { objects, more } => {
            assert_eq!(objects, want[..16].to_vec(), "the first 16, in id order");
            assert_eq!(more, 2);
        }
        r => panic!("{r:?}"),
    }
}

/// The modules table's value round-trips with a pending commit, attested and voided ones included.
#[test]
fn an_attestation_round_trips() {
    let t = attest_of(&first(1, 1));
    assert_eq!(Transaction::decode(&t.encode()), Some(t));
}

fn leave_entry() -> pe_core::EntryRef {
    pe_core::EntryRef(Hash([0x1e; 32]))
}

/// A commit pending across a 2 -> 1 leave (world-model.md 8; logic.md 6.2a): the leaver had not attested, so the
/// quorum of the voters at `c` (2 of 2) can't be reached; the shared void rule voids it at that frame's end, not at
/// `D`, and the old version stays.
#[test]
fn a_commit_pending_across_a_2_to_1_leave_is_voided_at_that_frames_end() {
    let mut f = fx();
    with_v1(&mut f);
    let base = f.w.modules()[&asset()].version;
    ok(&run(&mut f.w, &mut f.a, &commit(asset(), Some(base), 2, 1)));
    let pv = f.w.pending()[&asset()].version;
    let kb = TestKeys::new(1, 3).key(1);
    f.w.leave(kb, leave_entry()).unwrap();
    assert_eq!(f.w.pending()[&asset()].void, None, "the shared rule voids it at the frame's end");
    assert_eq!(f.w.end_frame(Frame(1)).voided, vec![void(pv, VoidKind::QuorumUnreachable, None, vec![])]);
    assert_eq!(f.w.pending()[&asset()].void, Some((Frame(1), VoidKind::QuorumUnreachable)));
    assert_eq!(f.w.end_frame(Frame(2)), FrameEnd::default(), "nothing activates or voids again");
    assert_eq!(f.w.modules()[&asset()].version, base);
}

/// 3 -> 2 (quorum 2 of the 3 voters at `c`): an attestation the leaver gave before leaving still counts, and a
/// commit the remaining voters can still attest is left pending.
#[test]
fn across_a_3_to_2_leave_the_leavers_attestation_counts_and_a_reachable_quorum_stays_pending() {
    let keys = TestKeys::new(1, 3);
    let (kb, kc) = (keys.key(1), keys.key(2));
    assert_eq!(quorum(3), 2);
    // The leaver attested before leaving: the commit activates at the frame's end.
    let mut f = fx();
    f.w.set_member(kc, perm::EDIT);
    with_v1(&mut f);
    let base = f.w.modules()[&asset()].version;
    let t = commit(asset(), Some(base), 2, 1);
    ok(&run(&mut f.w, &mut f.a, &t));
    ok(&run(&mut f.w, &mut f.b, &attest_of(&t)));
    f.w.leave(kb, leave_entry()).unwrap();
    assert_eq!(f.w.end_frame(Frame(1)).activated.len(), 1);
    assert_ne!(f.w.modules()[&asset()].version, base);
    // The leaver had not attested, but C still can: pending until C attests, then it activates.
    let mut f = fx();
    f.w.set_member(kc, perm::EDIT);
    with_v1(&mut f);
    let base = f.w.modules()[&asset()].version;
    let t = commit(asset(), Some(base), 2, 1);
    ok(&run(&mut f.w, &mut f.a, &t));
    f.w.leave(kb, leave_entry()).unwrap();
    assert_eq!(f.w.pending()[&asset()].void, None);
    let mut c = EntryBuilder::new(&keys, 2);
    ok(&run(&mut f.w, &mut c, &attest_of(&t)));
    assert_eq!(f.w.end_frame(Frame(1)).activated.len(), 1);
}

/// A failure's evidence must agree with itself (7.9; @chunks-reviewer's L1): a module exactly for a `Mismatch`, and
/// never the registered `author_module` it failed to match.
#[test]
fn a_failures_module_is_present_exactly_for_a_mismatch_and_is_not_the_authors() {
    let mut f = fx();
    let t = first(1, 1);
    ok(&run(&mut f.w, &mut f.a, &t));
    let with = |why: BuildFailure, module: Option<Hash>| {
        let mut x = failed_of(&t, why);
        if let Op::BuildFailed { reason, module: m, .. } = &mut x.ops[0] {
            (*reason, *m) = (why, module);
        }
        x
    };
    for bad in [
        with(BuildFailure::Compile, Some(Hash([9; 32]))),
        with(BuildFailure::Mismatch, None),
        with(BuildFailure::Mismatch, Some(Hash([3; 32]))),
    ] {
        assert!(matches!(refused(&run_at(&mut f.w, &mut f.b, &bad, 2)), Reason::Invalid { .. }), "{bad:?}");
    }
    assert!(f.w.pending()[&asset()].failed.is_empty());
    ok(&run_at(&mut f.w, &mut f.b, &with(BuildFailure::Mismatch, Some(Hash([9; 32]))), 2));
}

/// `BuildFailed` round-trips, with and without the module a `Mismatch` built, and an unknown reason does not decode.
#[test]
fn a_build_failure_round_trips() {
    let t = first(1, 1);
    let all =
        [BuildFailure::Compile, BuildFailure::Refused, BuildFailure::Cap, BuildFailure::Mismatch, BuildFailure::Local];
    for why in all {
        let f = failed_of(&t, why);
        assert_eq!(Transaction::decode(&f.encode()), Some(f));
    }
    // The reason byte is where a Compile and a Refused encoding differ.
    let mut bytes = failed_of(&t, BuildFailure::Compile).encode();
    let other = failed_of(&t, BuildFailure::Refused).encode();
    let diff: Vec<usize> = (0..bytes.len()).filter(|&i| bytes[i] != other[i]).collect();
    assert_eq!(diff.len(), 1);
    bytes[diff[0]] = 6;
    assert_eq!(Transaction::decode(&bytes), None);
}

/// At n = 2 (quorum 2), the other member's `BuildFailed` makes the quorum unreachable: the shared void rule voids the
/// commit at that frame's end, naming who could not build it and why (Q1 (c)), the old version stays, and nothing
/// counts on the void afterwards (logic.md 6.2a).
#[test]
fn a_failed_build_at_n_2_voids_the_commit_at_that_frames_end_naming_who_and_why() {
    let mut f = fx();
    with_v1(&mut f);
    let base = f.w.modules()[&asset()].version;
    let t = commit(asset(), Some(base), 2, 1);
    ok(&run(&mut f.w, &mut f.a, &t));
    let pv = f.w.pending()[&asset()].version;
    let kb = TestKeys::new(1, 3).key(1);
    let before = f.w.root();
    ok(&run(&mut f.w, &mut f.b, &failed_of(&t, BuildFailure::Compile)));
    assert_eq!(f.w.pending()[&asset()].failed, vec![(kb, Frame(1), BuildFailure::Compile)]);
    assert_ne!(f.w.root(), before, "a counted failure is in the root");
    assert_eq!(
        f.w.end_frame(Frame(1)).voided,
        vec![void(pv, VoidKind::QuorumUnreachable, None, vec![(kb, BuildFailure::Compile)])]
    );
    let p = &f.w.pending()[&asset()];
    assert_eq!(
        (p.void, p.failures()),
        (Some((Frame(1), VoidKind::QuorumUnreachable)), vec![(kb, BuildFailure::Compile)])
    );
    assert_eq!(f.w.modules()[&asset()].version, base);
    f.w.verify_tree_cache(DomainId(1)).unwrap();
    // Nothing changes a void: a later success or failure is Stale.
    assert!(matches!(refused(&run_at(&mut f.w, &mut f.b, &attest_of(&t), 2)), Reason::Stale { .. }));
    assert!(matches!(
        refused(&run_at(&mut f.w, &mut f.a, &failed_of(&t, BuildFailure::Local), 2)),
        Reason::Stale { .. }
    ));
    assert_eq!(f.w.end_frame(Frame(2)), FrameEnd::default());
}

/// n = 3 (quorum 2 of 3): a failure counts against the quorum but leaves it reachable; one per voter; a later success
/// by the same voter supersedes it while the commit is pending; a failure after a success is refused; a non-voter's,
/// another version's, and one beside another op are refused too.
#[test]
fn a_failure_is_one_per_voter_superseded_by_a_success_and_refused_after_one() {
    let keys = TestKeys::new(1, 3);
    let kc = keys.key(2);
    let mut f = fx();
    f.w.set_member(kc, perm::EDIT);
    with_v1(&mut f);
    let base = f.w.modules()[&asset()].version;
    let t = commit(asset(), Some(base), 2, 1);
    ok(&run(&mut f.w, &mut f.a, &t));
    let mut c = EntryBuilder::new(&keys, 2);
    ok(&run(&mut f.w, &mut f.b, &failed_of(&t, BuildFailure::Mismatch)));
    assert_eq!(f.w.end_frame(Frame(1)), FrameEnd::default(), "1 attested + C could = 2: still reachable");
    assert!(
        matches!(refused(&run_at(&mut f.w, &mut f.b, &failed_of(&t, BuildFailure::Local), 2)), Reason::Invalid { .. }),
        "a second"
    );
    let mut other_v = failed_of(&t, BuildFailure::Compile);
    if let Op::BuildFailed { version, .. } = &mut other_v.ops[0] {
        *version = v_of(3);
    }
    assert!(matches!(refused(&run_at(&mut f.w, &mut c, &other_v, 2)), Reason::Stale { .. }));
    let mut two = failed_of(&t, BuildFailure::Compile);
    two.ops.push(Op::Delete { id: Target::Existing(oid(LAMP)) });
    assert!(matches!(refused(&run_at(&mut f.w, &mut c, &two, 2)), Reason::Invalid { .. }));
    // A failure after the author's own attestation (the commit) is refused.
    assert!(matches!(
        refused(&run_at(&mut f.w, &mut f.a, &failed_of(&t, BuildFailure::Cap), 2)),
        Reason::Invalid { .. }
    ));
    // B's clean rebuild succeeds: its success supersedes its failure, and the quorum completes.
    ok(&run_at(&mut f.w, &mut f.b, &attest_of(&t), 2));
    assert!(f.w.pending()[&asset()].failed.is_empty());
    // C's failure after B's success counts, but can't stop the quorum B completed.
    ok(&run_at(&mut f.w, &mut c, &failed_of(&t, BuildFailure::Refused), 2));
    assert_eq!(f.w.pending()[&asset()].failed, vec![(kc, Frame(2), BuildFailure::Refused)]);
    let end = f.w.end_frame(Frame(2));
    assert!(end.voided.is_empty() && end.activated.len() == 1, "{end:?}");
    // A non-voter: a member admitted after `c` is not among its voters.
    let mut g = fx();
    with_v1(&mut g);
    let base = g.w.modules()[&asset()].version;
    let t = commit(asset(), Some(base), 2, 1);
    ok(&run(&mut g.w, &mut g.a, &t));
    g.w.set_member(kc, perm::EDIT);
    let mut c = EntryBuilder::new(&keys, 2);
    assert!(matches!(
        refused(&run_at(&mut g.w, &mut c, &failed_of(&t, BuildFailure::Compile), 2)),
        Reason::Invalid { .. }
    ));
}

/// A failure for the asset's current version is a record only (logic.md 6.2 step 3): the log's statement that this
/// member split off, which changes nothing in the world; a non-member's is refused.
#[test]
fn a_failure_for_the_active_version_is_a_record_only() {
    let mut f = fx();
    with_v1(&mut f);
    let active = first(1, 1);
    let root = f.w.root();
    ok(&run_at(&mut f.w, &mut f.b, &failed_of(&active, BuildFailure::Compile), 2));
    assert_eq!(f.w.root(), root, "nothing changes");
    assert!(f.w.pending().is_empty());
    let keys = TestKeys::new(1, 3);
    let mut outsider = EntryBuilder::new(&keys, 2);
    assert!(matches!(
        refused(&run_at(&mut f.w, &mut outsider, &failed_of(&active, BuildFailure::Compile), 2)),
        Reason::Invalid { .. }
    ));
    // A version the asset never had is Stale.
    assert!(matches!(
        refused(&run_at(&mut f.w, &mut f.b, &failed_of(&first(5, 1), BuildFailure::Compile), 2)),
        Reason::Stale { .. }
    ));
}

/// `set_activation` stamps every commit's `activate_at`, so a commit built with 0 applies at the frame it is set to
/// and is refused at any other (7.9; @engine-architect mbff4667dfe81).
#[test]
fn set_activation_stamps_the_commits_frame() {
    let mut f = fx();
    let mut t = first(1, 1);
    let Some(Op::CommitScript { activate_at, .. }) = t.ops.last_mut() else { panic!("no commit") };
    *activate_at = 0;
    assert!(matches!(refused(&run_at(&mut f.w, &mut f.a, &t, 7)), Reason::Invalid { .. }), "0 is not frame 7");
    t.set_activation(Frame(7));
    assert!(matches!(t.ops.last(), Some(Op::CommitScript { activate_at: 7, .. })));
    assert!(matches!(t.ops.first(), Some(Op::Register { .. })), "other ops untouched");
    ok(&run_at(&mut f.w, &mut f.a, &t, 7));
}

/// The author's prediction (logic.md 6.2a (i)): `activate_for_prediction` right after `end_frame(c)` gives exactly
/// the world a quorum completed at `c` gives, re-key included; with nothing pending it is `None`.
#[test]
fn a_prediction_equals_the_activation_a_quorum_at_the_same_frame_gives() {
    let mut f = fx();
    with_v1(&mut f);
    let base = f.w.modules()[&asset()].version;
    let t = commit(asset(), Some(base), 2, 1);
    ok(&run(&mut f.w, &mut f.a, &t));
    let mut real = f.w.clone();
    // The author's replica: no quorum at the frame's end, then the prediction.
    assert_eq!(f.w.end_frame(Frame(1)), FrameEnd::default());
    let predicted = f.w.activate_for_prediction(asset(), Frame(1)).expect("pending").expect("activates");
    // The real history: B attests at 1, and the frame's end activates.
    ok(&run(&mut real, &mut f.b, &attest_of(&t)));
    let end = real.end_frame(Frame(1));
    assert_eq!(end.activated.len(), 1);
    assert!(predicted.is_some(), "v1 -> v2 re-keys");
    assert_eq!(predicted, end.activated[0].2.clone());
    assert!(f.w.pending().is_empty());
    assert_eq!(f.w.modules(), real.modules());
    assert_eq!(row(&f.w, blink(2).id()), row(&real, blink(2).id()), "the same re-key");
    assert_eq!(f.w.store().get_raw(blink(1).id(), oid(LAMP)), None);
    f.w.verify_tree_cache(DomainId(1)).unwrap();
    assert_eq!(f.w.activate_for_prediction(asset(), Frame(2)), None, "nothing pending");
}

/// A prediction that fails changes nothing and leaves the commit pending (not voided): a blocked re-key here.
#[test]
fn a_failed_prediction_changes_nothing_and_leaves_the_commit_pending() {
    let mut f = fx();
    with_v1(&mut f);
    let base = f.w.modules()[&asset()].version;
    let t = commit(asset(), Some(base), 3, 1);
    ok(&run(&mut f.w, &mut f.a, &t));
    let kb = pe_seams::test_util::TestKeys::new(1, 3).key(1);
    f.w.set_claim(oid(LAMP), Claim { hand: oid(HAND), member: kb, since: Frame(1), until: Frame(1000) }).unwrap();
    let root = f.w.root();
    let blocked = Some(Blocked { objects: vec![oid(LAMP)], more: 0 });
    assert_eq!(
        f.w.activate_for_prediction(asset(), Frame(1)),
        Some(Err((VoidKind::RekeyBlockedAtActivation, blocked)))
    );
    assert_eq!(f.w.root(), root, "nothing changed");
    assert_eq!(f.w.pending()[&asset()].void, None, "still pending, not voided");
    assert_eq!(f.w.modules()[&asset()].version, base);
}

/// A new asset's id is derived from its creating entry (7.9; @coordinator-opus mf144e154826e): `New(k)` creates
/// `asset_created(line, author, chain, seq, k)`, and a first commit that names an id the world doesn't know is refused.
#[test]
fn a_new_assets_id_is_derived_from_its_entry_and_no_commit_names_a_new_one() {
    let mut f = fx();
    // Naming an id nobody has created: refused, whatever the id.
    for id in [[7; 32], asset_of(0, 1)] {
        let named = commit(id, None, 1, 1);
        assert!(matches!(refused(&run(&mut f.w, &mut f.b, &named)), Reason::Invalid { .. }), "{id:?}");
    }
    assert!(f.w.pending().is_empty());
    // B's third entry creates its own asset, at the id derived from that entry and k.
    let t = commit_to(AssetTarget::New(3), None, 1, 1);
    ok(&run(&mut f.w, &mut f.b, &t));
    let keys = TestKeys::new(1, 3);
    let mut b = EntryBuilder::new(&keys, 1);
    let e = (0..3).map(|_| b.next(Class::Edit, Frame(1), Bytes::new())).last().unwrap().entry;
    let id = pe_world::script::asset_created(e.line, e.author, e.chain, e.seq, 3);
    assert_eq!(f.w.pending().keys().copied().collect::<Vec<_>>(), vec![id]);
    assert_ne!(id, pe_world::script::asset_created(e.line, e.author, e.chain, e.seq, 0), "k is in the id");
    assert_ne!(id, asset_of(1, 2), "and so is the entry");
}

/// A first version that was voided leaves a pending record, so its asset exists: the author re-commits to it by
/// `Existing`, with no base, since nothing ever activated (Q1 (c): its source stays re-committable).
#[test]
fn a_voided_first_version_is_re_committed_to_the_same_asset() {
    let mut f = fx();
    let t = first(1, 1);
    ok(&run(&mut f.w, &mut f.a, &t));
    ok(&run(&mut f.w, &mut f.b, &failed_of(&t, BuildFailure::Compile)));
    assert_eq!(f.w.end_frame(Frame(1)).voided.len(), 1);
    assert!(f.w.modules().is_empty());
    let mut again = commit(asset(), None, 2, 1);
    if let Some(Op::CommitScript { activate_at, .. }) = again.ops.last_mut() {
        *activate_at = 2;
    }
    ok(&run_at(&mut f.w, &mut f.a, &again, 2));
    let p = &f.w.pending()[&asset()];
    assert_eq!((p.void, p.v()), (None, v_of(2)));
}

/// `CommitScript`'s target round-trips both ways, and an unknown target tag does not decode.
#[test]
fn a_commits_target_round_trips() {
    for t in [first(1, 1), commit_to(AssetTarget::New(7), None, 1, 1), commit([7; 32], None, 1, 1)] {
        assert_eq!(Transaction::decode(&t.encode()), Some(t.clone()));
    }
    let a = commit_to(AssetTarget::New(0), None, 1, 1).encode();
    let b = commit_to(AssetTarget::Existing([0; 32]), None, 1, 1).encode();
    let at = (0..a.len().min(b.len())).find(|&i| a[i] != b[i]).expect("the target tag");
    let mut bad = a.clone();
    bad[at] = 2;
    assert_eq!(Transaction::decode(&bad), None);
}

/// A script's colour field (3.6; @coordinator-opus m42dc307a66de): the colour space is part of its id, survives the
/// `Register` op's bytes and the registry's import, and a field without one keeps the id it had.
#[test]
fn a_colour_field_is_kept_by_the_op_the_registry_and_import() {
    let unmarked: Vec<FieldDesc> = blink_fields(6).into_iter().map(|f| FieldDesc { color: None, ..f }).collect();
    assert_ne!(blink(6).id(), script_component_id(&v_of(6), "Blink", unmarked).unwrap(), "the space is in the id");
    let t = first(6, 1);
    assert_eq!(Transaction::decode(&t.encode()), Some(t.clone()), "the op round-trips the space");
    let mut f = fx();
    ok(&land(&mut f, false, &t).0);
    let id = blink(6).id();
    let tint = |w: &World| w.store().registry().get(id).map(|d| d.fields()[1].color);
    assert_eq!(tint(&f.w), Some(Some(ColorSpace::LinearSrgb)));
    let d = DomainId(1);
    let t = f.w.tree(d);
    let mut keys: Vec<LeafKey> = f.w.store().objects().iter().map(|&id| LeafKey::Object(id)).collect();
    keys.extend((0..table::COUNT as u8).map(LeafKey::Table));
    keys.push(LeafKey::Config);
    let imp = CutImport {
        blobs: vec![],
        cut: pe_seams::Cut {
            at: pe_seams::CutRef { line: f.w.line(), height: 0, frame: f.w.frame() },
            heads: Vec::new(),
        },
        target_root: t.root(),
        records: t.export(&keys),
        absent: Vec::new(),
        physics: Some(PhysicsImage { domain: d, frame: f.w.frame(), global: Hash::ZERO, bytes: Default::default() }),
    };
    let got = World::new(0, f.w.blobs().clone()).import(d, &imp).unwrap();
    assert_eq!(got.root_hash(d), f.w.root_hash(d));
    assert_eq!(tint(&got), Some(Some(ColorSpace::LinearSrgb)));
}

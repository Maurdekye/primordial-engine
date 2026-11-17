//! Import (world-model.md 6.5, 6.6): records checked against the agreed root before anything applies, the absent
//! leaves removed, bound bodies only with the physics image.

use pe_core::{Frame, Hash, MemberKey, Pos};
use pe_seams::{Cut, CutImport, CutRef, DomainId, ImportError, LeafKey, PhysicsImage, Tree};
use pe_world::components::Name;
use pe_world::preset::{ball_pit, find, names};
use pe_world::tree::table;
use pe_world::{BlobStore, World, perm};

const D: DomainId = DomainId(1);

fn cut(target: &World) -> Cut {
    Cut { at: CutRef { line: target.line(), height: 0, frame: target.frame() }, heads: Vec::new() }
}

fn pit() -> World {
    let mut w = ball_pit(4, BlobStore::default(), Pos::ORIGIN);
    w.admit(w.line(), MemberKey([5; 32]), perm::ADMIN | perm::EDIT, Frame(0), Pos::ORIGIN).unwrap();
    w
}

/// A joiner's world before import (S2 v1.3): engine registry and default settings, as `PeWorld::empty` makes it.
fn bare(target: &World) -> World {
    World::new(0, target.blobs().clone())
}

/// Every object and table of `target`, and its config, as records, and its physics image stand-in.
fn full_import(target: &World) -> CutImport {
    let t = target.tree(D);
    let mut keys: Vec<LeafKey> = target.store().objects().iter().map(|&id| LeafKey::Object(id)).collect();
    keys.extend((0..table::COUNT as u8).map(LeafKey::Table));
    keys.push(LeafKey::Config);
    let physics = PhysicsImage { domain: D, frame: target.frame(), global: Hash::ZERO, bytes: Default::default() };
    CutImport {
        blobs: vec![],
        cut: cut(target),
        target_root: t.root(),
        records: t.export(&keys),
        absent: Vec::new(),
        physics: Some(physics),
    }
}

#[test]
fn a_joiner_imports_the_whole_world_from_its_base() {
    let mut target = pit();
    // Config values a bare world does not have: they reach the joiner only through the config record.
    target.set_agree_settings(b"quorum 2/3, cuts every 250 ms");
    target.set_depth_limit(77);
    target.set_gravity([0.5, -3.0, 0.25]);
    target.set_origin(Pos::ORIGIN.checked_add(pe_core::Offset([100.0, 2.0, -7.0])).unwrap());
    let s = pe_world::InputSettings { claim_ms: 900, cooldown_ms: 50, push_budget: 1234, reach_mm: 700, slack_mm: 30 };
    target.set_input_settings(s).unwrap();
    let base = bare(&target);
    assert_ne!(base.root_hash(D), target.root_hash(D));
    let mut got = base.import(D, &full_import(&target)).unwrap();
    assert_eq!(got.root_hash(D), target.root_hash(D));
    assert_eq!(got.config_bytes(), target.config_bytes());
    assert_eq!(got.store().seed(), target.store().seed());
    assert_eq!(got.agree_settings(), target.agree_settings());
    assert_eq!(got.settings().gravity, target.settings().gravity);
    assert_eq!(got.input_settings(), target.input_settings());
    assert_eq!(
        got.object_record(find(&got, "Ball 3").unwrap()),
        target.object_record(find(&target, "Ball 3").unwrap())
    );
    step_both(&mut target, &mut got, 3);
}

#[test]
fn a_tampered_record_is_refused_and_nothing_changes() {
    let target = pit();
    let base = bare(&target);
    let mut imp = full_import(&target);
    let i = 3;
    let mut bytes = imp.records[i].bytes.to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    imp.records[i].bytes = bytes.into();
    assert_eq!(base.import(D, &imp).err(), Some(ImportError::BadProof(i)));
    // A record checked against another root is refused too.
    let mut imp = full_import(&target);
    imp.target_root = Hash([9; 32]);
    assert_eq!(base.import(D, &imp).err(), Some(ImportError::BadProof(0)));
}

#[test]
fn a_bound_body_needs_the_physics_image() {
    let target = pit();
    let mut imp = full_import(&target);
    imp.physics = None;
    let base = bare(&target);
    assert!(matches!(base.import(D, &imp), Err(ImportError::NeedsPhysicsImage(_))));
    // Removing a bound body is a physics change too.
    let ball = find(&target, "Ball 0").unwrap();
    let imp = CutImport {
        blobs: vec![],
        cut: cut(&target),
        target_root: target.root_hash(D),
        records: Vec::new(),
        absent: vec![LeafKey::Object(ball)],
        physics: None,
    };
    assert_eq!(target.import(D, &imp).err(), Some(ImportError::NeedsPhysicsImage(ball)));
}

#[test]
fn a_physics_free_object_patches_alone() {
    // Rung 2 by object: the lamp has no body, so its record alone repairs it.
    let target = pit();
    let lamp = find(&target, names::LAMP).unwrap();
    let mut diverged = target.clone();
    let other = diverged.blobs().put(b"Lamp?");
    diverged.insert(lamp, Name { text: other }).unwrap();
    assert_ne!(diverged.root_hash(D), target.root_hash(D));
    let t = target.tree(D);
    let imp = CutImport {
        blobs: vec![],
        cut: cut(&target),
        target_root: t.root(),
        records: t.export(&[LeafKey::Object(lamp)]),
        absent: Vec::new(),
        physics: None,
    };
    let fixed = diverged.import(D, &imp).unwrap();
    assert_eq!(fixed.root_hash(D), target.root_hash(D));
}

#[test]
fn the_wrong_physics_image_is_refused() {
    let mut target = pit();
    target.write_physics(&[], Hash([4; 32]));
    let mut imp = full_import(&target);
    imp.physics.as_mut().unwrap().global = Hash([5; 32]);
    assert_eq!(bare(&target).import(D, &imp).err(), Some(ImportError::PhysicsCheck));
    imp.physics.as_mut().unwrap().global = Hash([4; 32]);
    let got = bare(&target).import(D, &imp).unwrap();
    assert_eq!(got.root_hash(D), target.root_hash(D));
}

#[test]
fn the_config_record_checks_against_the_world_node() {
    let target = pit();
    let t = target.tree(D);
    let [rec] = t.export(&[LeafKey::Config]).try_into().unwrap();
    assert_eq!(rec.proof.len(), 1);
    assert_eq!(&rec.bytes[..], &target.config_bytes()[..]);
    // Without the config record, a joiner's root differs: the caller's root check catches it.
    let mut imp = full_import(&target);
    imp.records.retain(|r| r.key != LeafKey::Config);
    let got = bare(&target).import(D, &imp).unwrap();
    assert_ne!(got.root_hash(D), target.root_hash(D));
    // A tampered config, or one proved by another world node, is refused.
    let mut imp = full_import(&target);
    let i = imp.records.iter().position(|r| r.key == LeafKey::Config).unwrap();
    let mut bytes = imp.records[i].bytes.to_vec();
    bytes[0] ^= 1;
    imp.records[i].bytes = bytes.into();
    assert_eq!(bare(&target).import(D, &imp).err(), Some(ImportError::BadProof(i)));
    let mut imp = full_import(&target);
    let other = bare(&target);
    imp.records[i].proof = other.tree(D).export(&[LeafKey::Config])[0].proof.clone();
    assert_eq!(bare(&target).import(D, &imp).err(), Some(ImportError::BadProof(i)));
}

/// 6.6 rate check at import (coordinator m1eb5e9623174 item 3): a 30 Hz world into a 60 Hz bare joiner is refused,
/// before anything applies; the same world into a 30 Hz joiner imports and matches.
#[test]
fn a_30_hz_world_is_refused_by_a_60_hz_joiner() {
    let mut target = pit();
    let hz30 = pe_seams::SimRate::new(30).unwrap();
    target.set_rate(hz30);
    let imp = full_import(&target);
    let joiner = bare(&target);
    assert_eq!(joiner.rate().hz(), 60);
    assert_eq!(joiner.import(D, &imp).err(), Some(ImportError::RateMismatch { world: hz30, mine: joiner.rate() }));
    let mut joiner30 = bare(&target);
    joiner30.set_rate(hz30);
    let mut got = joiner30.import(D, &imp).unwrap();
    assert_eq!(got.rate(), hz30);
    assert_eq!(got.root_hash(D), target.root_hash(D));
    step_both(&mut target, &mut got, 3);
}

/// Steps host and joiner alike (the adapter's convention: `end_frame(f)`, then the next frame) and compares roots
/// after each frame.
fn step_both(host: &mut World, joiner: &mut World, frames: u64) {
    for _ in 0..frames {
        for w in [&mut *host, &mut *joiner] {
            let f = w.frame();
            w.end_frame(f);
            w.set_frame(Frame(f.0 + 1));
        }
        assert_eq!(joiner.root_hash(D), host.root_hash(D));
    }
}

//! Primordial Engine fork: the reader's checks on records, against what the writer emits.

use super::joints::{ser_joint_sim, JOINT_SIM_MIN_BYTES};
use super::pod_bodies::{ser_body_sim, ser_body_state, BODY_SIM_MIN_BYTES, BODY_STATE_MIN_BYTES};
use super::pod_contacts::{ser_manifold, MANIFOLD_MIN_BYTES, MATERIAL_MIN_BYTES};
use super::pods::{des_hash_set, ser_hash_set};
use crate::geometry::default_surface_material;
use crate::recording::buffer::{RecBuffer, SnapReader};
use crate::snapshot_hostile_tests::scene;
use crate::table::{key_hash, HashSet, SetItem};
use crate::types::default_world_def;

fn size(write: impl FnOnce(&mut RecBuffer)) -> i32 {
    let mut buf = RecBuffer::new();
    write(&mut buf);
    buf.data.len() as i32
}

/// Every minimum record size `SnapReader::count` bounds a count with is at most what the writer
/// emits for a real record, so no honest image is refused for its counts.
#[test]
fn record_minimums_within_writer_sizes() {
    let world = scene(&default_world_def());
    let mut seen = [0; 4];
    let joint_sims = world.solver_sets.iter().flat_map(|s| &s.joint_sims);
    let joint_sims = joint_sims.chain(world.constraint_graph.colors.iter().flat_map(|c| &c.joint_sims));
    for s in world.solver_sets.iter().flat_map(|s| &s.body_sims) {
        assert!(size(|b| ser_body_sim(b, s)) >= BODY_SIM_MIN_BYTES);
        seen[0] += 1;
    }
    for s in world.solver_sets.iter().flat_map(|s| &s.body_states) {
        assert!(size(|b| ser_body_state(b, s)) >= BODY_STATE_MIN_BYTES);
        seen[1] += 1;
    }
    for s in joint_sims {
        assert!(size(|b| ser_joint_sim(b, s)) >= JOINT_SIM_MIN_BYTES);
        seen[2] += 1;
    }
    for m in world.contacts.iter().flat_map(|c| &c.manifolds) {
        assert!(size(|b| ser_manifold(b, m)) >= MANIFOLD_MIN_BYTES);
        seen[3] += 1;
    }
    assert!(seen.iter().all(|&n| n > 0), "scene lacks a record kind: {seen:?}");
    assert!(size(|b| b.append_material(default_surface_material())) >= MATERIAL_MIN_BYTES);
    println!(
        "RECORD sizes: body sim {} (min {BODY_SIM_MIN_BYTES}), body state {} (min {BODY_STATE_MIN_BYTES}), material {} (min {MATERIAL_MIN_BYTES}); records checked {seen:?}",
        size(|b| ser_body_sim(b, world.solver_sets.iter().flat_map(|s| &s.body_sims).next().unwrap())),
        size(|b| ser_body_state(b, world.solver_sets.iter().flat_map(|s| &s.body_states).next().unwrap())),
        size(|b| b.append_material(default_surface_material())),
    );
}

fn bytes_of(set: &HashSet) -> Vec<u8> {
    let mut buf = RecBuffer::new();
    ser_hash_set(&mut buf, set);
    buf.data
}

fn accepted(bytes: &[u8]) -> bool {
    let mut r = SnapReader::new(bytes);
    let _ = des_hash_set(&mut r);
    r.ok && r.cursor() == bytes.len()
}

/// The broad-phase pair set from an image is refused unless `find_slot` terminates and finds
/// every key (review P22R-2): no zero capacity, no full table, consistent counts and hashes.
#[test]
fn hostile_pair_sets_refused() {
    let world = scene(&default_world_def());
    let real = &world.broad_phase.pair_set;
    assert!(real.count() > 0);
    assert!(accepted(&bytes_of(real)), "the scene's pair set");

    let mut set = HashSet::new(16);
    for key in 1..=8u64 {
        set.add_key(key);
    }
    assert_eq!((set.capacity(), set.count()), (16, 8));
    assert!(accepted(&bytes_of(&set)), "half full, as add_key leaves it");

    let with = |f: &dyn Fn(&mut HashSet)| {
        let mut s = set.clone();
        f(&mut s);
        accepted(&bytes_of(&s))
    };
    assert!(!accepted(&[0; 8]), "capacity 0");
    assert!(!with(&|s| s.items.truncate(8)), "capacity 8");
    assert!(!with(&|s| s.items.resize(24, SetItem::default())), "capacity not a power of two");
    let full = |s: &mut HashSet| {
        for key in 9..=16u64 {
            let hash = key_hash(key);
            let slot = s.items.iter().position(|i| i.hash == 0).unwrap();
            s.items[slot] = SetItem { key, hash };
        }
    };
    assert!(!with(&|s| full(s)), "full table, count 8");
    assert!(!with(&|s| {
        full(s);
        s.count = 16;
    }), "full table, count 16");
    assert!(!with(&|s| s.count = 7), "count below the occupied slots");
    let first = set.items.iter().position(|i| i.hash != 0).unwrap();
    assert!(!with(&|s| s.items[first].hash ^= 1), "hash not the key's");
    assert!(!with(&|s| s.items[first].key = 0), "zero key");
    let empty = set.items.iter().position(|i| i.hash == 0).unwrap();
    assert!(!with(&|s| s.items[empty].key = 99), "bytes in an empty slot");
    // A key moved off its probe chain: the slot find_slot reaches for it is empty.
    let (from, to) = (0..16)
        .find_map(|i| {
            let item = set.items[i];
            let next = (i + 1) & 15;
            (item.hash != 0 && (item.hash & 15) as usize == i && set.items[next].hash == 0).then_some((i, next))
        })
        .expect("a key at its home slot followed by an empty one");
    assert!(!with(&|s| s.items.swap(from, to)), "key off its probe chain");
}

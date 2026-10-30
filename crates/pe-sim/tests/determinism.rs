//! Snapshot and restore round-trips, rollback re-simulation and the golden trace (P2-1's "done":
//! bit-exact round-trips; the golden digests must be equal on Windows and Linux).

mod scenario;

use pe_core::hash::Hasher;
use pe_core::{Frame, ObjectId};
use pe_sim::{SimStore, Snapshot, Transform};
use scenario::*;

/// Runs the scenario from a fresh world, returning the hash after each frame and snapshots at `keep`.
fn run(frames: u64, keep: &[u64]) -> (SimStore, Vec<u64>, Vec<Snapshot>) {
    let mut store = new_store();
    let mut sched = schedule();
    let mut hashes = Vec::new();
    let mut snaps = Vec::new();
    for _ in 0..frames {
        if keep.contains(&store.frame().0) {
            snaps.push(store.snapshot());
        }
        sched.tick(&mut store, |_, _| {});
        hashes.push(store.state_hash());
    }
    (store, hashes, snaps)
}

#[test]
fn rollback_resimulation_is_identical() {
    let (end, hashes, snaps) = run(600, &[100, 250, 599]);
    assert!(end.objects().len() > 50, "the scenario should build up a populated world");
    let mut sched = schedule();
    for s in &snaps {
        // Restore in place into the finished world, and into a fresh store from serialized bytes.
        let mut a = end.clone();
        a.restore(s).unwrap();
        let mut b = new_store();
        b.restore(&Snapshot::from_bytes(&s.to_bytes(), b.registry()).unwrap()).unwrap();
        assert_eq!(a.state_hash(), s.state_hash());
        assert_eq!(b.state_hash(), s.state_hash());
        let start = s.frame().0;
        if start > 0 {
            assert_eq!(s.state_hash(), hashes[start as usize - 1], "snapshot at {start} is the state after frame {}", start - 1);
        }
        for f in start..600 {
            sched.tick(&mut a, |_, _| {});
            sched.tick(&mut b, |_, _| {});
            assert_eq!(a.state_hash(), hashes[f as usize], "re-simulated frame {f} differs (restored in place)");
            assert_eq!(b.state_hash(), hashes[f as usize], "re-simulated frame {f} differs (from bytes)");
        }
        assert_eq!(a.snapshot().to_bytes(), end.snapshot().to_bytes());
    }
}

#[test]
fn snapshot_is_isolated_from_later_writes() {
    let (mut store, _, _) = run(300, &[]);
    let before = store.state_hash();
    let bytes = store.snapshot().to_bytes();
    let snap = store.snapshot();
    let mut sched = schedule();
    sched.run(&mut store, 50);
    // Direct writes through every mutable path.
    let first = store.objects()[0];
    if let Some(t) = store.get_mut::<Transform>(first) {
        t.pos[0] += 1.0;
    }
    store.column_mut::<Velocity>().1.iter_mut().for_each(|v| v.spin = 0.0);
    assert_ne!(store.state_hash(), before);
    assert_eq!(snap.state_hash(), before);
    assert_eq!(snap.to_bytes(), bytes);
    store.restore(&snap).unwrap();
    assert_eq!(store.state_hash(), before);
    assert_eq!(store.snapshot().to_bytes(), bytes);
}

#[test]
fn serialization_round_trip_is_bit_exact() {
    let (store, _, _) = run(450, &[]);
    let snap = store.snapshot();
    let bytes = snap.to_bytes();
    let back = Snapshot::from_bytes(&bytes, store.registry()).unwrap();
    assert_eq!(back.to_bytes(), bytes);
    assert_eq!(back.state_hash(), store.state_hash());
    let mut fresh = new_store();
    fresh.restore(&back).unwrap();
    assert_eq!(fresh.state_hash(), store.state_hash());
    // World transforms recomputed from restored state are bit-identical too.
    let bits = |s: &SimStore| -> Vec<u64> {
        s.world_transforms()
            .iter()
            .flat_map(|(id, w)| {
                let mut v = vec![id.to_u64()];
                v.extend(w.pos.chunk.iter().map(|&c| c as u64));
                v.extend(w.pos.local.iter().map(|x| x.to_bits()));
                v.extend(w.rot.iter().chain(&w.scale).map(|x| x.to_bits() as u64));
                v
            })
            .collect()
    };
    assert_eq!(bits(&fresh), bits(&store));
}

#[test]
fn corrupt_or_mismatched_bytes_are_refused() {
    let (store, _, _) = run(120, &[]);
    let bytes = store.snapshot().to_bytes();
    let reg = store.registry();
    assert!(Snapshot::from_bytes(&bytes[..bytes.len() - 1], reg).is_err());
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(Snapshot::from_bytes(&extra, reg).is_err());
    let mut magic = bytes.clone();
    magic[0] ^= 1;
    assert!(Snapshot::from_bytes(&magic, reg).is_err());
    // Swap the first two object ids: the table is no longer sorted.
    let mut swapped = bytes.clone();
    let at = 8 + 8 + 8 + 8;
    let (a, b) = (swapped[at..at + 8].to_vec(), swapped[at + 8..at + 16].to_vec());
    swapped[at..at + 8].copy_from_slice(&b);
    swapped[at + 8..at + 16].copy_from_slice(&a);
    assert!(Snapshot::from_bytes(&swapped, reg).is_err());
    // A registry without the scenario's components cannot read or restore it, and a failed restore leaves a
    // populated store unchanged.
    let bare = std::sync::Arc::new(pe_sim::Registry::new());
    assert!(Snapshot::from_bytes(&bytes, &bare).is_err());
    let mut other = SimStore::new(bare, SEED);
    for _ in 0..10 {
        let o = other.create(4).unwrap();
        other.insert(o, Transform::local([1.0, 2.0, 3.0])).unwrap();
    }
    let (h, b) = (other.state_hash(), other.snapshot().to_bytes());
    assert!(other.restore(&store.snapshot()).is_err());
    assert_eq!((other.state_hash(), other.snapshot().to_bytes()), (h, b));
    // A frame beyond 2^62 is refused (review P21-8).
    let mut far = bytes.clone();
    far[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(Snapshot::from_bytes(&far, reg).is_err());
}

#[test]
fn hash_ignores_history_only_state() {
    // The same final state reached by different operation orders hashes and serializes identically.
    let reg = registry();
    let ids = [ObjectId::new(2, 1), ObjectId::new(1, 3), ObjectId::new(1, 1), ObjectId::new(3, 7)];
    let build = |order: &[usize], comps_first: bool| {
        let mut s = SimStore::new(reg.clone(), SEED);
        for &i in order {
            s.create_with_id(ids[i]).unwrap();
        }
        let add = |s: &mut SimStore| {
            for &i in order.iter().rev() {
                s.insert(ids[i], Transform::local([i as f64, 0.0, 0.0])).unwrap();
                if i % 2 == 0 {
                    s.insert(ids[i], Velocity { v: [1.0, 2.0, 3.0], spin: i as f32 }).unwrap();
                }
            }
        };
        if comps_first {
            add(&mut s);
            s.set_parent(ids[0], Some(ids[1]), None).unwrap();
            s.set_parent(ids[2], Some(ids[1]), None).unwrap();
        } else {
            s.set_parent(ids[2], Some(ids[1]), None).unwrap();
            s.set_parent(ids[0], Some(ids[1]), Some(0)).unwrap();
            add(&mut s);
        }
        // A component added and removed again leaves no trace.
        s.insert_raw(HEAT, ids[3], &heat_row(1.0, 2, true)).unwrap();
        s.remove_raw(HEAT, ids[3]).unwrap();
        s
    };
    let a = build(&[0, 1, 2, 3], true);
    let b = build(&[3, 1, 0, 2], false);
    assert_eq!(a.state_hash(), b.state_hash());
    assert_eq!(a.snapshot().to_bytes(), b.snapshot().to_bytes());
}

/// Golden values for the cross-platform check (P2-1 "done": equal on Windows and Linux). They were
/// recorded on Windows x64; a different value on any target is a determinism bug, not a test to update.
const GOLDEN_TRACE: u64 = 0x820d14b183fb6434;
const GOLDEN_WORLD: u64 = 0xa15e17ef2b97bc12;

#[test]
fn golden_trace() {
    let (store, hashes, _) = run(600, &[]);
    let mut h = Hasher::new(0);
    for x in &hashes {
        h.write_u64(*x);
    }
    let trace = h.finish();
    let mut w = Hasher::new(0);
    for (id, t) in store.world_transforms() {
        w.write_u64(id.to_u64());
        for c in t.pos.chunk {
            w.write_u64(c as u64);
        }
        for x in t.pos.local {
            w.write_u64(x.to_bits());
        }
        for x in t.rot.iter().chain(&t.scale) {
            w.write_u32(x.to_bits());
        }
    }
    let world = w.finish();
    println!("RESULT golden objects={} frame={} trace={trace:#018x} world={world:#018x}", store.objects().len(), store.frame().0);
    assert_eq!(store.frame(), Frame(600));
    assert_eq!((trace, world), (GOLDEN_TRACE, GOLDEN_WORLD), "golden digests changed");
}

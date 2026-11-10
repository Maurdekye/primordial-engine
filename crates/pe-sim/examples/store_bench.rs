//! Snapshot, restore, serialization and hash costs of the sim store, against the spike's snapshot costs
//! (design 6.2). Run at low priority; timings on a busy machine are noisy:
//!
//!   cargo run -p pe-sim --release --example store_bench
//!
//! Each world has N objects, each with a hierarchy node, a Transform (80 B) and a 16 B velocity; half also
//! have a 16 B runtime-described component; a third are children of another object. Times are the median of
//! the repetitions.
//!
//! * `snapshot`: taking a snapshot (shares buffers).
//! * `first write`: after a snapshot, writing every Transform once. This pays the copy-on-write copy of the
//!   Transform column, which is the real cost a snapshot defers.
//! * `deep copy`: copying every buffer (what a snapshot without copy-on-write would cost).
//! * `restore`, `hash`, `to_bytes`, `from_bytes`.

use std::sync::Arc;
use std::time::Instant;

use bytemuck::{Pod, Zeroable};
use core::mem::offset_of;
use pe_core::coords::{Offset, Pos};
use pe_core::ObjectId;
use pe_core::rng::Rng;
use pe_sim::registry::{Component, ComponentDesc, ComponentId, FieldDesc, FieldKind, Registry};
use pe_sim::{SimStore, Snapshot, Transform};

#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
struct Velocity {
    v: [f32; 3],
    spin: f32,
}

impl Component for Velocity {
    const ID: ComponentId = ComponentId(1024);
    const NAME: &'static str = "Velocity";
    fn fields() -> Vec<FieldDesc> {
        vec![
            FieldDesc::new("v", FieldKind::F32, offset_of!(Velocity, v), 3),
            FieldDesc::new("spin", FieldKind::F32, offset_of!(Velocity, spin), 1),
        ]
    }
}

const EXTRA: ComponentId = ComponentId(1025);

fn world(n: usize) -> SimStore {
    let mut reg = Registry::new();
    reg.register_type::<Velocity>().unwrap();
    reg.register(
        ComponentDesc::new(EXTRA, "Extra", vec![FieldDesc::new("a", FieldKind::F64, 0, 1), FieldDesc::new("b", FieldKind::U64, 8, 1)])
            .unwrap(),
    )
    .unwrap();
    let mut s = SimStore::new(Arc::new(reg), 7);
    let mut rng = Rng::stream(1, 1);
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let id = ObjectId::from_u128(i as u128 + 1);
        s.create(id).unwrap();
        s.insert(id, Transform::at(Pos::ORIGIN.checked_add(Offset([rng.next_f64() * 1000.0, 0.0, rng.next_f64() * 1000.0])).unwrap())).unwrap();
        s.insert(id, Velocity { v: [rng.next_f32(), 0.0, rng.next_f32()], spin: 0.0 }).unwrap();
        if i % 2 == 0 {
            let mut row = [0u8; 16];
            row[..8].copy_from_slice(&(i as f64).to_le_bytes());
            s.insert_raw(EXTRA, id, &row).unwrap();
        }
        if i % 3 == 2 {
            s.set_parent(id, Some(ids[rng.below(ids.len() as u64) as usize]), None).unwrap();
        }
        ids.push(id);
    }
    s
}

fn median_us(reps: usize, mut f: impl FnMut() -> Option<u128>) -> f64 {
    let mut v: Vec<u128> = (0..reps).filter_map(|_| f()).collect();
    v.sort_unstable();
    v[v.len() / 2] as f64 / 1000.0
}

fn time<T>(f: impl FnOnce() -> T) -> (T, u128) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_nanos())
}

fn touch_transforms(s: &mut SimStore) {
    for t in s.column_mut::<Transform>().1 {
        t.pos[0] += 0.001;
    }
}

fn main() {
    let reps = 201;
    println!("RESULT store_bench reps={reps} (median, microseconds; low priority, noisy)");
    for n in [256usize, 1024, 2048, 8192] {
        let mut s = world(n);
        let snap = s.snapshot();
        let bytes = snap.to_bytes();
        let snapshot = median_us(reps, || Some(time(|| s.snapshot()).1));
        // Writes with no snapshot alive (the column is not shared), then right after one (it is copied).
        let write_unshared = median_us(reps, || Some(time(|| touch_transforms(&mut s)).1));
        let write_after_snapshot = median_us(reps, || {
            let keep = s.snapshot();
            let (_, ns) = time(|| touch_transforms(&mut s));
            drop(keep);
            Some(ns)
        });
        let deep_copy = median_us(reps, || Some(time(|| s.deep_clone()).1));
        let restore = median_us(reps, || Some(time(|| s.restore(&snap).unwrap()).1));
        let hash = median_us(reps, || Some(time(|| s.state_hash()).1));
        let to_bytes = median_us(reps, || Some(time(|| snap.to_bytes()).1));
        let from_bytes = median_us(reps, || Some(time(|| Snapshot::from_bytes(&bytes, s.registry()).unwrap()).1));
        println!(
            "RESULT store_bench objects={n} image_bytes={} data_bytes={} snapshot_us={snapshot:.3} write_transforms_unshared_us={write_unshared:.2} write_transforms_after_snapshot_us={write_after_snapshot:.2} deep_copy_us={deep_copy:.2} restore_us={restore:.3} hash_us={hash:.2} to_bytes_us={to_bytes:.2} from_bytes_us={from_bytes:.2}",
            bytes.len(),
            snap.data_bytes()
        );
    }
}

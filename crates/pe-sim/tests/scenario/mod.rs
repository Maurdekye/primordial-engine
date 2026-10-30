#![allow(dead_code)]
//! A scripted world for the determinism tests: objects spawn, move, spin, reparent and die, driven only
//! by the frame's RNG streams, so every run (and every re-simulation) must produce the same state.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use core::mem::offset_of;
use pe_core::clock::TickRate;
use pe_core::coords::WorldPos;
use pe_core::{dmath, ObjectId};
use pe_sim::registry::{Component, ComponentDesc, ComponentId, FieldDesc, FieldKind, Registry};
use pe_sim::{Schedule, SimStore, TickCtx, Transform};

#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct Velocity {
    pub v: [f32; 3],
    pub spin: f32,
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

/// A component known only by its runtime description, as a logic module's would be (16.5).
pub const HEAT: ComponentId = ComponentId(1025);

pub fn heat_desc() -> ComponentDesc {
    ComponentDesc::new(HEAT, "Heat", vec![
        FieldDesc::new("t", FieldKind::F64, 0, 1),
        FieldDesc::new("ticks", FieldKind::U32, 8, 1),
        FieldDesc::new("hot", FieldKind::Bool, 12, 1),
        FieldDesc::new("_pad", FieldKind::U8, 13, 3),
    ])
    .unwrap()
}

pub fn heat_row(t: f64, ticks: u32, hot: bool) -> [u8; 16] {
    let mut r = [0u8; 16];
    r[0..8].copy_from_slice(&t.to_le_bytes());
    r[8..12].copy_from_slice(&ticks.to_le_bytes());
    r[12] = hot as u8;
    r
}

pub fn registry() -> Arc<Registry> {
    let mut r = Registry::new();
    r.register_type::<Velocity>().unwrap();
    r.register(heat_desc()).unwrap();
    Arc::new(r)
}

fn pick(store: &SimStore, ctx: &TickCtx, stream: u64) -> Option<ObjectId> {
    let objs = store.objects();
    if objs.is_empty() {
        return None;
    }
    let mut rng = ctx.rng(stream);
    Some(objs[rng.below(objs.len() as u64) as usize])
}

fn spawn(store: &mut SimStore, ctx: &TickCtx) {
    if ctx.frame.0 % 3 != 0 {
        return;
    }
    let mut rng = ctx.rng(1);
    let creator = 1 + rng.below(3) as u32;
    let id = store.create(creator).unwrap();
    let parent = if rng.next_f32() < 0.4 { pick(store, ctx, 2).filter(|&p| p != id) } else { None };
    let t = match parent {
        Some(_) => Transform::local([rng.range_f32(-2.0, 2.0) as f64, rng.range_f32(0.0, 1.0) as f64, 0.5]),
        None => Transform::at(WorldPos::new(
            [rng.below(5) as i64 - 2, 0, -(rng.below(3) as i64)],
            [rng.range_f32(0.0, 1024.0) as f64, rng.next_f64() * 10.0, rng.range_f32(0.0, 1024.0) as f64],
        )),
    };
    let half = rng.range_f32(-1.5, 1.5) * 0.5;
    let (s, c) = dmath::sin_cos(half);
    store.insert(id, t.with_rot([0.0, s, 0.0, c]).with_scale([1.0, rng.range_f32(0.5, 2.0), 1.0])).unwrap();
    let v = Velocity { v: [rng.range_f32(-3.0, 3.0), 0.0, rng.range_f32(-3.0, 3.0)], spin: rng.range_f32(-2.0, 2.0) };
    store.insert(id, v).unwrap();
    if rng.next_f32() < 0.5 {
        store.insert_raw(HEAT, id, &heat_row(20.0, 0, false)).unwrap();
    }
    if let Some(p) = parent {
        store.set_parent(id, Some(p), Some(rng.below(4) as usize)).unwrap();
    }
}

fn integrate(store: &mut SimStore, ctx: &TickCtx) {
    let moving: Vec<(ObjectId, Velocity)> = store.iter::<Velocity>().map(|(id, v)| (id, *v)).collect();
    for (id, v) in moving {
        let Some(t) = store.get_mut::<Transform>(id) else { continue };
        for a in 0..3 {
            t.pos[a] += v.v[a] as f64 * ctx.dt as f64;
        }
        let (s, c) = dmath::sin_cos(v.spin * ctx.dt * 0.5);
        t.rot = pe_sim::transform::quat_mul(t.rot, [0.0, s, 0.0, c]);
    }
}

fn heat(store: &mut SimStore, ctx: &TickCtx) {
    let ids: Vec<ObjectId> = store.objects().to_vec();
    for id in ids {
        let Some(row) = store.get_raw(HEAT, id) else { continue };
        let t = f64::from_le_bytes(row[0..8].try_into().unwrap());
        let ticks = u32::from_le_bytes(row[8..12].try_into().unwrap());
        let t2 = t * 0.99 + dmath::exp(-(ticks as f32) * ctx.dt) as f64 + dmath::atan2(t as f32, 3.0) as f64;
        store.insert_raw(HEAT, id, &heat_row(t2, ticks + 1, t2 > 25.0)).unwrap();
    }
}

fn reshuffle(store: &mut SimStore, ctx: &TickCtx) {
    if ctx.frame.0 % 13 == 5 {
        if let (Some(c), Some(p)) = (pick(store, ctx, 3), pick(store, ctx, 4)) {
            let idx = ctx.rng(5).below(3) as usize;
            // Cycles are refused identically everywhere; that refusal is part of the scenario.
            let _ = store.set_parent(c, Some(p), Some(idx));
        }
    }
    if ctx.frame.0 % 29 == 11 {
        if let Some(c) = pick(store, ctx, 6) {
            store.set_parent(c, None, None).unwrap();
        }
    }
    if ctx.frame.0 % 11 == 7 {
        if let Some(d) = pick(store, ctx, 7) {
            store.destroy(d).unwrap();
        }
    }
    if ctx.frame.0 % 17 == 3 {
        if let Some(d) = pick(store, ctx, 8) {
            store.remove::<Velocity>(d).unwrap();
        }
    }
}

pub fn schedule() -> Schedule {
    Schedule::new(TickRate::HZ90)
        .with("spawn", spawn)
        .with("integrate", integrate)
        .with("heat", heat)
        .with("reshuffle", reshuffle)
}

pub const SEED: u64 = 0x5EED_0F_C0FFEE;

pub fn new_store() -> SimStore {
    SimStore::new(registry(), SEED)
}

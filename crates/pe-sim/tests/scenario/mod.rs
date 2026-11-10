#![allow(dead_code)]
//! A scripted world for the determinism tests: objects spawn, move, spin, reparent and die, driven only
//! by the frame's RNG streams, so every run (and every re-simulation) must produce the same state.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use core::mem::offset_of;
use pe_core::clock::TickRate;
use pe_core::coords::{Offset, Pos};
use pe_core::{Frame, LineId, ObjectId, dmath};
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
    ComponentDesc::new(
        HEAT,
        "Heat",
        vec![
            FieldDesc::new("t", FieldKind::F64, 0, 1),
            FieldDesc::new("ticks", FieldKind::U32, 8, 1),
            FieldDesc::new("hot", FieldKind::Bool, 12, 1),
            FieldDesc::new("_pad", FieldKind::U8, 13, 3),
        ],
    )
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
    if !ctx.frame.0.is_multiple_of(3) {
        return;
    }
    let mut rng = ctx.rng(1);
    let _creator = 1 + rng.below(3) as u32; // kept: the RNG stream's draws are unchanged
    // A logic spawn (world-model.md 2.1): one per frame here, so n = 0.
    let id = ObjectId::spawned(LineId::origin(), ObjectId::NONE, Frame(ctx.frame.0), 0);
    store.create(id).unwrap();
    let parent = if rng.next_f32() < 0.4 { pick(store, ctx, 2).filter(|&p| p != id) } else { None };
    let t = match parent {
        Some(_) => Transform::local([rng.range_f32(-2.0, 2.0) as f64, rng.range_f32(0.0, 1.0) as f64, 0.5]),
        None => Transform::at(wp(
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
    if ctx.frame.0 % 13 == 5
        && let (Some(c), Some(p)) = (pick(store, ctx, 3), pick(store, ctx, 4))
    {
        let idx = ctx.rng(5).below(3) as usize;
        // Cycles are refused identically everywhere; that refusal is part of the scenario.
        let _ = store.set_parent_keep_world(c, Some(p), Some(idx));
    }
    if ctx.frame.0 % 29 == 11
        && let Some(c) = pick(store, ctx, 6)
    {
        store.set_parent_keep_world(c, None, None).unwrap();
    }
    if ctx.frame.0 % 11 == 7
        && let Some(d) = pick(store, ctx, 7)
    {
        store.destroy(d).unwrap();
    }
    if ctx.frame.0 % 17 == 3
        && let Some(d) = pick(store, ctx, 8)
    {
        store.remove::<Velocity>(d).unwrap();
    }
}

pub fn schedule() -> Schedule {
    Schedule::new(TickRate::HZ90)
        .with("spawn", spawn)
        .with("integrate", integrate)
        .with("heat", heat)
        .with("reshuffle", reshuffle)
        .with("maintain_transforms", pe_sim::transform::maintain_transforms)
}

pub const SEED: u64 = 0x5EED_0FC0_FFEE;

pub fn new_store() -> SimStore {
    SimStore::new(registry(), SEED)
}

/// A test id: the old `(creator, counter)` packed into the low 64 bits.
pub fn oid(creator: u32, counter: u32) -> ObjectId {
    ObjectId::from_u128(((creator as u128) << 32) | counter as u128)
}

/// A test position: `chunk`'s corner moved by `local`.
pub fn wp(chunk: [i64; 3], local: [f64; 3]) -> Pos {
    Pos::at_chunk(chunk).unwrap().checked_add(Offset(local)).unwrap()
}

/// `a - b` as plain numbers (the wall's own operation).
pub fn dsub(a: Pos, b: Pos) -> [f64; 3] {
    (a - b).0
}

/// Test-only id allocation: the next `oid(creator, k)` after the largest live one.
pub trait Mk {
    fn mk(&mut self, creator: u32) -> ObjectId;
}

impl Mk for SimStore {
    fn mk(&mut self, creator: u32) -> ObjectId {
        let next = self
            .objects()
            .iter()
            .filter(|o| (o.to_u128() >> 32) as u32 == creator && o.to_u128() >> 64 == 0)
            .map(|o| o.to_u128() as u32)
            .max()
            .unwrap_or(0)
            + 1;
        let id = oid(creator, next);
        self.create(id).unwrap();
        id
    }
}

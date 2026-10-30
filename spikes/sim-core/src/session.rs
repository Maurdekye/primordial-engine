//! Session protocol core for the phase-1 spikes, revised after design review C1/C2/H4/H5 and the
//! user rulings of 2026-10-30: perturbations travel as **state + canonical frame**; same-frame
//! conflicts use a stable, non-grindable tie-break; programmable systems run locally and sync
//! only on perturbation; R14 cross-world transfers.
//!
//! * [`Entry`]: one perturbation (touch, hand target, transfer step, …). Its identity is the hash
//!   of its content, never `(member, seq)` alone. Each member's entries form a hash chain, so two
//!   different entries with the same `(member, seq)` are a transferable proof of equivocation.
//! * [`Ack`]: every member periodically states its *horizon* (it will author nothing stamped at
//!   or before it), the entries it holds (per author: contiguous seq and id), and the entries that
//!   reached it late. The **confirmed frame** is computed from acks only, so admission never
//!   depends on local arrival time (C1).
//! * [`WorldSim`]: one world's deterministic simulation with keyframe snapshots, rollback to the
//!   earliest changed frame, carried-state application, claims, and R14 escrow/arrival tables.
//!
//! Frame convention as in the rest of sim-core: the state at `frame` is the state at the *start*
//! of that frame; `hashes[f]` is the hash after frame `f` was stepped. `conf` is exclusive: every
//! frame `< conf` is confirmed and can never be rolled back.

use std::collections::{BTreeMap, BTreeSet};

use box3d_rust::body::{
    body_apply_linear_impulse_to_center, body_get_angular_velocity, body_get_linear_velocity,
    body_get_transform, body_set_angular_velocity, body_set_linear_velocity,
    body_set_target_transform, body_set_transform, create_body, destroy_body,
};
use box3d_rust::geometry::{Capsule, Sphere};
use box3d_rust::hull::make_box_hull;
use box3d_rust::id::BodyId;
use box3d_rust::math_functions::{Pos, Quat, Transform, Vec3, QUAT_IDENTITY};
use box3d_rust::recording::{
    deserialize_into_shell, hash_world_state, serialize_world, GeometryRegistry, RecBuffer,
    RegistrySlot,
};
use box3d_rust::shape::{create_capsule_shape, create_hull_shape, create_sphere_shape};
use box3d_rust::types::{default_body_def, default_shape_def, default_world_def, BodyType};
use box3d_rust::world::World;

use crate::{fnv_u64s, splitmix64, CLAIM_FRAMES, SUBSTEPS};

pub type MemberId = u32;
pub type EntryId = u64;
/// Member 0 is world logic: its actions are computed identically on every peer, never networked,
/// and never take claims (review M3).
pub const LOGIC: MemberId = 0;

// ---- object state ------------------------------------------------------------------------------

/// Exact bit pattern of a rigid body's sim state: position, rotation, linear and angular velocity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyState(pub [u32; 13]);

impl BodyState {
    pub fn read(world: &World, id: BodyId) -> Self {
        let t = body_get_transform(world, id);
        let v = body_get_linear_velocity(world, id);
        let w = body_get_angular_velocity(world, id);
        BodyState([
            t.p.x.to_bits(), t.p.y.to_bits(), t.p.z.to_bits(),
            t.q.v.x.to_bits(), t.q.v.y.to_bits(), t.q.v.z.to_bits(), t.q.s.to_bits(),
            v.x.to_bits(), v.y.to_bits(), v.z.to_bits(),
            w.x.to_bits(), w.y.to_bits(), w.z.to_bits(),
        ])
    }
    fn f(&self, i: usize) -> f32 {
        f32::from_bits(self.0[i])
    }
    pub fn pos(&self) -> Pos {
        Pos { x: self.f(0), y: self.f(1), z: self.f(2) }
    }
    pub fn rot(&self) -> Quat {
        Quat { v: Vec3 { x: self.f(3), y: self.f(4), z: self.f(5) }, s: self.f(6) }
    }
    pub fn lin(&self) -> Vec3 {
        Vec3 { x: self.f(7), y: self.f(8), z: self.f(9) }
    }
    pub fn ang(&self) -> Vec3 {
        Vec3 { x: self.f(10), y: self.f(11), z: self.f(12) }
    }
    /// Overwrite the body's state with these exact bits.
    pub fn write(&self, world: &mut World, id: BodyId) {
        body_set_transform(world, id, self.pos(), self.rot());
        body_set_linear_velocity(world, id, self.lin());
        body_set_angular_velocity(world, id, self.ang());
    }
    pub fn hash(&self) -> u64 {
        fnv_u64s(self.0.iter().map(|&b| b as u64))
    }
}

/// Everything another world needs to recreate an object (R14). `kind` selects the shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjRecord {
    pub global: u64,
    pub kind: u8,
    pub state: BodyState,
    pub src_world: u32,
    pub src_frame: u64,
}

// ---- entries, acks, messages ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// Impulse on an object. `grab` = a held grab, which renews its claim; a plain impulse claims
    /// once and never renews (review M3).
    Touch { obj: u32, impulse: [f32; 3], grab: bool },
    /// Kinematic hand target for this frame, in millimetres (quantized so every peer sees the
    /// same bits). `active = false` parks the hand.
    Hand { obj: u32, target_mm: [i32; 3], active: bool },
    /// R14, source world: carry `obj` out through the portal into escrow.
    Depart { transfer: u64, obj: u32, deadline: u64 },
    /// R14, destination world: the object arrives (relayed after the source confirmed Depart).
    Arrive { transfer: u64, record: ObjRecord },
    /// R14, source world: the destination's confirmed verdict (relayed after it confirmed).
    Verdict { transfer: u64, accepted: bool },
    /// R14 test fixture: open or close this world's arrival portal.
    Portal { open: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub member: MemberId,
    /// 1-based per member. A member that rejoins gets a new MemberId, so seqs never restart.
    pub seq: u32,
    /// Id of this member's previous entry (0 for seq 1): a hash chain.
    pub prev: EntryId,
    /// Canonical frame chosen by the author (its present frame when it acted).
    pub stamp: u64,
    pub payload: Payload,
    /// User ruling 09:30Z: the resulting state of the touched object after frame `stamp`.
    pub carried: Option<(u32, BodyState)>,
}

impl Entry {
    /// Spike: 64-bit FNV of the encoding. Production: BLAKE3 of the signed bytes.
    pub fn id(&self) -> EntryId {
        let mut w = Wr(Vec::with_capacity(96));
        w.entry(self);
        fnv_bytes(&w.0)
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Ack {
    pub member: MemberId,
    /// The sender will author nothing stamped at or before this frame.
    pub horizon: u64,
    /// Per author: highest contiguous seq held, and that entry's id.
    pub held: Vec<(MemberId, u32, EntryId)>,
    /// Entries that reached the sender later than `stamp + late_allowance` (frames), with the
    /// sender's clock frame at arrival. Absent = arrived on time (review H5).
    pub late: Vec<(EntryId, u64)>,
    /// Newest confirmed check frame and its full-state hash (review H4: every ack repeats it, so
    /// a lost ack loses nothing).
    pub check: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Entry(Entry),
    Ack(Ack),
    Fetch { author: MemberId, from: u32, to: u32 },
    /// Full-state hash of a confirmed check frame (review H4).
    Hash { frame: u64, hash: u64 },
    /// Vote to evict `target`, whose entries from `seq` on are void (equivocation proof held).
    Vote { target: MemberId, seq: u32 },
}

pub fn fnv_bytes(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h = (h ^ x as u64).wrapping_mul(0x100_0000_01B3);
    }
    h
}

// ---- wire encoding (also used in-process, so byte counts are real) ------------------------------

pub struct Wr(pub Vec<u8>);
impl Wr {
    fn u8(&mut self, v: u8) { self.0.push(v) }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn u64(&mut self, v: u64) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn i32(&mut self, v: i32) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn state(&mut self, s: &BodyState) { for b in s.0 { self.u32(b) } }
    fn entry(&mut self, e: &Entry) {
        self.u32(e.member);
        self.u32(e.seq);
        self.u64(e.prev);
        self.u64(e.stamp);
        match &e.payload {
            Payload::Touch { obj, impulse, grab } => {
                self.u8(1); self.u32(*obj); for c in impulse { self.u32(c.to_bits()) } self.u8(*grab as u8);
            }
            Payload::Hand { obj, target_mm, active } => {
                self.u8(2); self.u32(*obj); for c in target_mm { self.i32(*c) } self.u8(*active as u8);
            }
            Payload::Depart { transfer, obj, deadline } => {
                self.u8(3); self.u64(*transfer); self.u32(*obj); self.u64(*deadline);
            }
            Payload::Arrive { transfer, record } => {
                self.u8(4); self.u64(*transfer); self.u64(record.global); self.u8(record.kind);
                self.state(&record.state); self.u32(record.src_world); self.u64(record.src_frame);
            }
            Payload::Verdict { transfer, accepted } => { self.u8(5); self.u64(*transfer); self.u8(*accepted as u8) }
            Payload::Portal { open } => { self.u8(6); self.u8(*open as u8) }
        }
        match &e.carried {
            Some((obj, s)) => { self.u8(1); self.u32(*obj); self.state(s) }
            None => self.u8(0),
        }
    }
    pub fn msg(&mut self, m: &Msg) {
        match m {
            Msg::Entry(e) => { self.u8(1); self.entry(e) }
            Msg::Ack(a) => {
                self.u8(2); self.u32(a.member); self.u64(a.horizon);
                self.u32(a.held.len() as u32);
                for (q, s, id) in &a.held { self.u32(*q); self.u32(*s); self.u64(*id) }
                self.u32(a.late.len() as u32);
                for (id, f) in &a.late { self.u64(*id); self.u64(*f) }
                match a.check { Some((f, h)) => { self.u8(1); self.u64(f); self.u64(h) } None => self.u8(0) }
            }
            Msg::Fetch { author, from, to } => { self.u8(3); self.u32(*author); self.u32(*from); self.u32(*to) }
            Msg::Hash { frame, hash } => { self.u8(4); self.u64(*frame); self.u64(*hash) }
            Msg::Vote { target, seq } => { self.u8(5); self.u32(*target); self.u32(*seq) }
        }
    }
}

pub struct Rd<'a>(pub &'a [u8], pub usize);
impl Rd<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.0.get(self.1..self.1 + n)?;
        self.1 += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> { Some(self.take(1)?[0]) }
    fn u32(&mut self) -> Option<u32> { Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
    fn u64(&mut self) -> Option<u64> { Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?)) }
    fn i32(&mut self) -> Option<i32> { Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
    fn f32(&mut self) -> Option<f32> { Some(f32::from_bits(self.u32()?)) }
    fn state(&mut self) -> Option<BodyState> {
        let mut s = [0u32; 13];
        for b in &mut s { *b = self.u32()? }
        Some(BodyState(s))
    }
    fn entry(&mut self) -> Option<Entry> {
        let member = self.u32()?;
        let seq = self.u32()?;
        let prev = self.u64()?;
        let stamp = self.u64()?;
        let payload = match self.u8()? {
            1 => Payload::Touch { obj: self.u32()?, impulse: [self.f32()?, self.f32()?, self.f32()?], grab: self.u8()? != 0 },
            2 => Payload::Hand { obj: self.u32()?, target_mm: [self.i32()?, self.i32()?, self.i32()?], active: self.u8()? != 0 },
            3 => Payload::Depart { transfer: self.u64()?, obj: self.u32()?, deadline: self.u64()? },
            4 => {
                let transfer = self.u64()?;
                let record = ObjRecord {
                    global: self.u64()?, kind: self.u8()?, state: self.state()?, src_world: self.u32()?, src_frame: self.u64()?,
                };
                Payload::Arrive { transfer, record }
            }
            5 => Payload::Verdict { transfer: self.u64()?, accepted: self.u8()? != 0 },
            6 => Payload::Portal { open: self.u8()? != 0 },
            _ => return None,
        };
        let carried = match self.u8()? {
            1 => Some((self.u32()?, self.state()?)),
            _ => None,
        };
        Some(Entry { member, seq, prev, stamp, payload, carried })
    }
    pub fn msg(&mut self) -> Option<Msg> {
        Some(match self.u8()? {
            1 => Msg::Entry(self.entry()?),
            2 => {
                let member = self.u32()?;
                let horizon = self.u64()?;
                let n = self.u32()? as usize;
                let mut held = Vec::with_capacity(n.min(256));
                for _ in 0..n { held.push((self.u32()?, self.u32()?, self.u64()?)) }
                let n = self.u32()? as usize;
                let mut late = Vec::with_capacity(n.min(256));
                for _ in 0..n { late.push((self.u64()?, self.u64()?)) }
                let check = if self.u8()? == 1 { Some((self.u64()?, self.u64()?)) } else { None };
                Msg::Ack(Ack { member, horizon, held, late, check })
            }
            3 => Msg::Fetch { author: self.u32()?, from: self.u32()?, to: self.u32()? },
            4 => Msg::Hash { frame: self.u64()?, hash: self.u64()? },
            5 => Msg::Vote { target: self.u32()?, seq: self.u32()? },
            _ => return None,
        })
    }
}

pub fn encode_msg(m: &Msg) -> Vec<u8> {
    let mut w = Wr(Vec::with_capacity(128));
    w.msg(m);
    w.0
}
pub fn decode_msg(b: &[u8]) -> Option<Msg> {
    Rd(b, 0).msg()
}

/// Same-frame tie-break (user ruling 09:32Z): a per-frame pseudo-random permutation of members.
/// `MemberId`s are assigned in admission order by the membership log, never chosen by the peer,
/// and `seed` comes from the session/epoch genesis, so no key can be ground to win. All 32 bits of
/// the id are hashed (review L7).
pub fn tie_rank(seed: u64, frame: u64, member: MemberId) -> u64 {
    splitmix64(seed ^ frame.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (member as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
}

// ---- scenes --------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum SceneKind {
    /// The phase-1 pile: ground plus side*side*layers boxes/spheres/capsules.
    Pile { side: usize, layers: usize },
    /// A walled pit of small balls plus one kinematic hand per listed member.
    BallPit { balls: usize, hands: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Obj {
    pub body: BodyId,
    pub global: u64,
    pub kind: u8,
}

const BALL_R: f32 = 0.06;
pub const CLAIM_COOLDOWN: u64 = 10;
const HAND_R: f32 = 0.09;

fn add_shape(world: &mut World, body: BodyId, kind: u8) {
    let mut sd = default_shape_def();
    sd.density = 1.0;
    match kind {
        0 => {
            let h = make_box_hull(0.4, 0.4, 0.4);
            create_hull_shape(world, body, &sd, &h.base);
        }
        1 => {
            let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: 0.45 };
            create_sphere_shape(world, body, &sd, &s);
        }
        2 => {
            let c = Capsule {
                center1: Vec3 { x: -0.3, y: 0.0, z: 0.0 },
                center2: Vec3 { x: 0.3, y: 0.0, z: 0.0 },
                radius: 0.25,
            };
            create_capsule_shape(world, body, &sd, &c);
        }
        3 => {
            sd.density = 0.2;
            sd.base_material.rolling_resistance = 0.2;
            sd.base_material.friction = 0.6;
            let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: BALL_R };
            create_sphere_shape(world, body, &sd, &s);
        }
        _ => {
            let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: HAND_R };
            create_sphere_shape(world, body, &sd, &s);
        }
    }
}

fn static_box(world: &mut World, p: Pos, hx: f32, hy: f32, hz: f32) {
    let mut d = default_body_def();
    d.type_ = BodyType::Static;
    d.position = p;
    let b = create_body(world, &d);
    let hull = make_box_hull(hx, hy, hz);
    create_hull_shape(world, b, &default_shape_def(), &hull.base);
}

/// Builds the scene. `world_tag` makes global object ids unique across worlds (R14).
pub fn build_scene(kind: SceneKind, world_tag: u32) -> (World, Vec<Option<Obj>>) {
    let g = |i: usize| ((world_tag as u64) << 32) | i as u64;
    match kind {
        SceneKind::Pile { side, layers } => {
            let s = crate::build_pile_scene(side, layers);
            let objs = s.bodies.iter().enumerate().map(|(i, &b)| Some(Obj { body: b, global: g(i), kind: (i % 3) as u8 })).collect();
            (s.world, objs)
        }
        SceneKind::BallPit { balls, hands } => {
            let mut world = World::new(&default_world_def());
            static_box(&mut world, Pos { x: 0.0, y: -0.5, z: 0.0 }, 2.0, 0.5, 2.0);
            let (w, h) = (1.2f32, 0.6f32);
            static_box(&mut world, Pos { x: w + 0.1, y: h, z: 0.0 }, 0.1, h, w + 0.2);
            static_box(&mut world, Pos { x: -w - 0.1, y: h, z: 0.0 }, 0.1, h, w + 0.2);
            static_box(&mut world, Pos { x: 0.0, y: h, z: w + 0.1 }, w + 0.2, h, 0.1);
            static_box(&mut world, Pos { x: 0.0, y: h, z: -w - 0.1 }, w + 0.2, h, 0.1);
            let mut objs = Vec::with_capacity(balls + hands);
            let per_row = 16usize;
            let spacing = 2.0 * w / per_row as f32;
            for i in 0..balls {
                let (ix, iz, iy) = (i % per_row, (i / per_row) % per_row, i / (per_row * per_row));
                let jitter = (i as f32 * 0.618_034).fract() * 0.02 - 0.01;
                let mut d = default_body_def();
                d.type_ = BodyType::Dynamic;
                d.linear_damping = 0.1;
                d.angular_damping = 0.3;
                d.position = Pos {
                    x: -w + spacing * (ix as f32 + 0.5) + jitter,
                    y: BALL_R + 0.01 + iy as f32 * (2.0 * BALL_R + 0.01),
                    z: -w + spacing * (iz as f32 + 0.5) - jitter,
                };
                let b = create_body(&mut world, &d);
                add_shape(&mut world, b, 3);
                objs.push(Some(Obj { body: b, global: g(i), kind: 3 }));
            }
            for k in 0..hands {
                let mut d = default_body_def();
                d.type_ = BodyType::Kinematic;
                d.position = Pos { x: -0.8 + 1.6 * k as f32, y: 1.5, z: 0.0 };
                let b = create_body(&mut world, &d);
                add_shape(&mut world, b, 4);
                objs.push(Some(Obj { body: b, global: g(balls + k), kind: 4 }));
            }
            (world, objs)
        }
    }
}

// ---- rollback state ------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    pub member: MemberId,
    pub until: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscrowStatus {
    Pending,
    Committed,
    Returned,
    DepartRejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Escrow {
    pub obj: u32,
    pub record: Option<ObjRecord>,
    pub status: EscrowStatus,
    pub depart_frame: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    Accepted { obj: u32, frame: u64 },
    Rejected { frame: u64 },
}

/// Everything that rolls back together. The Box3D world image covers bodies, contacts, islands
/// and id pools; the tables cover engine state.
#[derive(Clone, Default)]
pub struct Tables {
    pub objs: Vec<Option<Obj>>,
    pub claims: BTreeMap<u32, Claim>,
    /// After a claim expires, its holder may not claim the object again for CLAIM_COOLDOWN frames,
    /// so repeated impulses cannot monopolize an object (review M3).
    pub cooldown: BTreeMap<u32, (MemberId, u64)>,
    pub escrow: BTreeMap<u64, Escrow>,
    pub arrivals: BTreeMap<u64, Arrival>,
    pub portal_open: bool,
}

#[derive(Clone)]
struct Keyframe {
    image: Vec<u8>,
    t: Tables,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub rollbacks: u64,
    pub resim_frames: u64,
    pub max_depth: u64,
    pub resim_us: u64,
    pub step_us: u64,
    pub steps: u64,
    pub overwrites: u64,
    pub paused_frames: u64,
    pub entries_rejected_violation: u64,
    pub equivocations: u64,
    pub checks_ok: u64,
    pub checks_bad: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct SimConfig {
    pub dt: f32,
    /// Speculation limit: a member stops advancing when `frame - conf` reaches this (frames).
    pub window: u64,
    pub keyframe_every: u64,
    /// Backdating allowance in frames (review H5).
    pub late_allowance: u64,
    pub seed: u64,
    pub check_every: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig { dt: 1.0 / 60.0, window: 120, keyframe_every: 4, late_allowance: 15, seed: 0x5052_494D_4F52_4449, check_every: 30 }
    }
}

/// Verdict of one entry when its frame was (re)simulated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    RejectedClaimed { by: MemberId, until: u64 },
    RejectedCooldown { until: u64 },
    RejectedGone,
    RejectedPortalClosed,
    RejectedDuplicate,
}

pub struct WorldSim {
    pub world_id: u32,
    pub me: MemberId,
    pub members: BTreeSet<MemberId>,
    pub epoch: u32,
    pub cfg: SimConfig,
    pub world: World,
    pub t: Tables,
    pub frame: u64,
    registry: GeometryRegistry,
    slots: Vec<RegistrySlot>,
    slot_count: usize,
    ring: BTreeMap<u64, Keyframe>,
    // ---- log
    entries: BTreeMap<EntryId, Entry>,
    by_author: BTreeMap<(MemberId, u32), EntryId>,
    contiguous: BTreeMap<MemberId, u32>,
    void: BTreeSet<EntryId>,
    void_from: BTreeMap<MemberId, u32>,
    eff: BTreeMap<EntryId, u64>,
    at_frame: BTreeMap<u64, BTreeSet<EntryId>>,
    /// My clock frame when each entry first reached me.
    arrived: BTreeMap<EntryId, u64>,
    /// Late reports from other members: entry -> member -> arrival frame.
    late_reports: BTreeMap<EntryId, BTreeMap<MemberId, u64>>,
    // ---- acks
    acks: BTreeMap<MemberId, Ack>,
    /// Per author: (horizon, own contiguous seq) from each of its acks, in order.
    horizons: BTreeMap<MemberId, Vec<(u64, u32)>>,
    pub conf: u64,
    /// My clock frame (my present), set by the harness each tick.
    pub now: u64,
    my_seq: u32,
    /// Test fixture (review H5): publish horizons this many frames behind the present.
    pub horizon_lag: u64,
    my_last: EntryId,
    votes: BTreeMap<(MemberId, u32), BTreeSet<MemberId>>,
    voted: BTreeSet<(MemberId, u32)>,
    fetch_asked: BTreeMap<MemberId, (u32, u64)>,
    // ---- results
    pub hashes: BTreeMap<u64, u64>,
    pub check_hashes: BTreeMap<u64, u64>,
    remote_checks: BTreeMap<u64, BTreeMap<MemberId, u64>>,
    evaluated: BTreeSet<(u64, MemberId)>,
    checks_sent: BTreeSet<u64>,
    pub outcomes: BTreeMap<EntryId, (u64, Outcome)>,
    pub watched: BTreeSet<u32>,
    /// (frame, obj) -> object state hash after that frame, for watched objects.
    pub obj_hist: BTreeMap<(u64, u32), u64>,
    dirty_from: Option<u64>,
    pub stats: Stats,
    pub events: Vec<String>,
}

pub fn now_us() -> u64 {
    // Monotonic microseconds since first call (review L6: never SystemTime for sim clocks).
    use std::sync::OnceLock;
    use std::time::Instant;
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64
}

impl WorldSim {
    pub fn new(world_id: u32, me: MemberId, members: &[MemberId], scene: SceneKind, cfg: SimConfig) -> Self {
        let (world, objs) = build_scene(scene, world_id);
        WorldSim {
            world_id,
            me,
            members: members.iter().copied().collect(),
            epoch: 0,
            cfg,
            world,
            t: Tables { objs, portal_open: true, ..Default::default() },
            frame: 0,
            registry: GeometryRegistry::new(),
            slots: Vec::new(),
            slot_count: 0,
            ring: BTreeMap::new(),
            entries: BTreeMap::new(),
            by_author: BTreeMap::new(),
            contiguous: BTreeMap::new(),
            void: BTreeSet::new(),
            void_from: BTreeMap::new(),
            eff: BTreeMap::new(),
            at_frame: BTreeMap::new(),
            arrived: BTreeMap::new(),
            late_reports: BTreeMap::new(),
            acks: BTreeMap::new(),
            horizons: BTreeMap::new(),
            conf: 0,
            now: 0,
            my_seq: 0,
            horizon_lag: 0,
            my_last: 0,
            votes: BTreeMap::new(),
            voted: BTreeSet::new(),
            fetch_asked: BTreeMap::new(),
            hashes: BTreeMap::new(),
            check_hashes: BTreeMap::new(),
            remote_checks: BTreeMap::new(),
            evaluated: BTreeSet::new(),
            checks_sent: BTreeSet::new(),
            outcomes: BTreeMap::new(),
            watched: BTreeSet::new(),
            obj_hist: BTreeMap::new(),
            dirty_from: None,
            stats: Stats::default(),
            events: Vec::new(),
        }
    }

    fn ev(&mut self, s: String) {
        self.events.push(format!("[w{} m{} f{} c{}] {s}", self.world_id, self.me, self.frame, self.conf));
    }

    // ---- snapshots -------------------------------------------------------------------------------

    fn save(&mut self) -> Keyframe {
        let mut buf = RecBuffer::new();
        serialize_world(&self.world, &mut buf, &mut self.registry);
        if self.registry.entries.len() != self.slot_count {
            self.slots = self.registry.to_slots();
            self.slot_count = self.registry.entries.len();
        }
        Keyframe { image: buf.data, t: self.t.clone() }
    }

    fn restore(&mut self, k: &Keyframe, frame: u64) {
        assert!(deserialize_into_shell(&k.image, &mut self.world, &mut self.slots), "restore failed");
        self.t = k.t.clone();
        self.frame = frame;
    }

    /// Hash of the full world image plus tables: used at confirmed check frames (review H4).
    pub fn image_hash(&mut self) -> u64 {
        let k = self.save();
        fnv_bytes(&k.image) ^ self.tables_hash().rotate_left(17)
    }

    fn tables_hash(&self) -> u64 {
        let mut v = vec![self.frame];
        for (i, o) in self.t.objs.iter().enumerate() {
            if let Some(o) = o {
                v.extend([i as u64, o.global, o.kind as u64]);
            }
        }
        for (o, c) in &self.t.claims {
            v.extend([*o as u64, c.member as u64, c.until]);
        }
        for (o, (m, u)) in &self.t.cooldown {
            v.extend([*o as u64, *m as u64, *u, 7]);
        }
        for (t, e) in &self.t.escrow {
            v.extend([*t, e.obj as u64, e.status as u64, e.depart_frame, e.record.map_or(0, |r| r.state.hash())]);
        }
        for (t, a) in &self.t.arrivals {
            match a {
                Arrival::Accepted { obj, frame } => v.extend([*t, 1, *obj as u64, *frame]),
                Arrival::Rejected { frame } => v.extend([*t, 2, *frame]),
            }
        }
        v.push(self.t.portal_open as u64);
        fnv_u64s(v)
    }

    /// Per-frame hash: Box3D body transforms + velocities, plus every engine table.
    pub fn state_hash(&self) -> u64 {
        hash_world_state(&self.world) ^ self.tables_hash().rotate_left(29)
    }

    pub fn obj_state(&self, obj: u32) -> Option<BodyState> {
        self.t.objs.get(obj as usize).copied().flatten().map(|o| BodyState::read(&self.world, o.body))
    }

    // ---- log -------------------------------------------------------------------------------------

    pub fn entry(&self, id: EntryId) -> Option<&Entry> {
        self.entries.get(&id)
    }

    pub fn effective_frame(&self, id: EntryId) -> Option<u64> {
        self.eff.get(&id).copied()
    }

    /// Effective frame (review H5): the stamp, unless at least half of the other members saw the
    /// entry later than `stamp + late_allowance`; then their median arrival minus the allowance.
    fn compute_eff(&self, id: EntryId) -> u64 {
        let e = &self.entries[&id];
        let others: Vec<MemberId> = self.members.iter().copied().filter(|&m| m != e.member).collect();
        if others.is_empty() {
            return e.stamp;
        }
        let ontime = e.stamp + self.cfg.late_allowance;
        let reports = self.late_reports.get(&id);
        let mut v: Vec<u64> = others
            .iter()
            .map(|m| {
                let r = if *m == self.me { self.arrived.get(&id).copied() } else { reports.and_then(|r| r.get(m)).copied() };
                r.filter(|&a| a > ontime).unwrap_or(ontime)
            })
            .collect();
        v.sort_unstable();
        // The lower median of the other members' arrivals: an entry moves only if at least half of
        // the other members saw it late.
        let k = others.len().div_ceil(2);
        e.stamp.max(v[k - 1] - self.cfg.late_allowance)
    }

    fn set_eff(&mut self, id: EntryId) {
        let new = if self.void.contains(&id) { None } else { Some(self.compute_eff(id)) };
        let old = self.eff.get(&id).copied();
        if old == new {
            return;
        }
        if let Some(o) = old {
            if let Some(s) = self.at_frame.get_mut(&o) {
                s.remove(&id);
            }
            self.eff.remove(&id);
            self.mark_dirty(o);
        }
        if let Some(n) = new {
            self.eff.insert(id, n);
            self.at_frame.entry(n).or_default().insert(id);
            self.mark_dirty(n);
            if old.is_some() {
                self.ev(format!("entry {id:016x} re-stamped {:?} -> {n}", old));
            }
        }
    }

    fn mark_dirty(&mut self, f: u64) {
        if f < self.frame {
            self.dirty_from = Some(self.dirty_from.map_or(f, |d| d.min(f)));
        }
    }

    /// Author an entry at the current frame. With `carry`, the entry carries the touched object's
    /// state after this frame: computed by stepping the frame once on a scratch copy.
    pub fn author(&mut self, payload: Payload, carry: Option<u32>) -> Entry {
        let stamp = if carry.is_some() {
            assert!(self.dirty_from.is_none() && self.frame >= self.now, "carrying state needs a caught-up sim");
            self.frame
        } else {
            self.frame.max(self.now)
        };
        let mut e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None };
        if let Some(obj) = carry {
            let scratch = self.save();
            let tmp_id = e.id();
            self.entries.insert(tmp_id, e.clone());
            self.at_frame.entry(stamp).or_default().insert(tmp_id);
            self.eff.insert(tmp_id, stamp);
            self.step_frame(false);
            let st = self.obj_state(obj);
            self.entries.remove(&tmp_id);
            self.at_frame.get_mut(&stamp).unwrap().remove(&tmp_id);
            self.eff.remove(&tmp_id);
            self.outcomes.remove(&tmp_id);
            self.restore(&scratch, stamp);
            e.carried = st.map(|s| (obj, s));
            self.watched.insert(obj);
        }
        self.my_seq += 1;
        let id = e.id();
        self.my_last = id;
        self.insert(e.clone(), self.now);
        e
    }

    /// Author a state-free entry stamped `ahead` frames after the present (input delay).
    pub fn author_ahead(&mut self, payload: Payload, ahead: u64) -> Entry {
        let stamp = self.frame.max(self.now) + ahead;
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None };
        self.my_seq += 1;
        self.my_last = e.id();
        self.insert(e.clone(), self.now);
        e
    }

    /// Test fixture for review H5: author with an arbitrary (backdated) stamp.
    pub fn author_backdated(&mut self, payload: Payload, back: u64) -> Entry {
        let stamp = self.frame.max(self.now).saturating_sub(back);
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None };
        self.my_seq += 1;
        self.my_last = e.id();
        self.insert(e.clone(), self.now);
        e
    }

    /// True if `e` breaks a horizon its author already published: an ack with a smaller own seq
    /// promised nothing would be stamped at or before that horizon (review H5).
    fn breaks_horizon(&self, e: &Entry) -> bool {
        self.horizons.get(&e.member).is_some_and(|h| h.iter().any(|&(hz, own)| own < e.seq && e.stamp <= hz))
    }

    /// Store a received entry. Returns messages to send (votes) if it proves equivocation.
    pub fn insert(&mut self, e: Entry, arrival_frame: u64) -> Vec<Msg> {
        let id = e.id();
        if self.entries.contains_key(&id) {
            return vec![];
        }
        if let Some(&first) = self.void_from.get(&e.member) {
            if e.seq >= first {
                return vec![];
            }
        }
        if let Some(&other) = self.by_author.get(&(e.member, e.seq)) {
            if other != id {
                // Two signed entries with the same (member, seq): transferable proof (review C2).
                self.entries.insert(id, e.clone());
                return self.equivocation(e.member, e.seq);
            }
        }
        if !self.members.contains(&e.member) && e.member != LOGIC {
            return vec![];
        }
        if e.member != self.me && self.breaks_horizon(&e) {
            self.stats.entries_rejected_violation += 1;
            self.ev(format!("violation: m{} s{} stamped {} at or before its own published horizon", e.member, e.seq, e.stamp));
            self.by_author.insert((e.member, e.seq), id);
            self.entries.insert(id, e.clone());
            self.void.insert(id);
            let mut c = self.contiguous.get(&e.member).copied().unwrap_or(0);
            while self.by_author.contains_key(&(e.member, c + 1)) {
                c += 1;
            }
            self.contiguous.insert(e.member, c);
            return vec![];
        }
        if e.stamp < self.conf {
            // The author promised (horizon) not to stamp here, and every member held all entries
            // up to `conf`: a protocol violation, refused identically by everyone.
            self.stats.entries_rejected_violation += 1;
            self.ev(format!("violation: entry m{} s{} stamped {} < confirmed {}", e.member, e.seq, e.stamp, self.conf));
            return vec![];
        }
        if let Some((obj, _)) = e.carried {
            self.watched.insert(obj);
        }
        if let Payload::Touch { obj, .. } = e.payload {
            self.watched.insert(obj);
        }
        self.by_author.insert((e.member, e.seq), id);
        self.entries.insert(id, e.clone());
        self.arrived.insert(id, arrival_frame);
        // Contiguous seq per author.
        let mut c = self.contiguous.get(&e.member).copied().unwrap_or(0);
        while self.by_author.contains_key(&(e.member, c + 1)) {
            c += 1;
        }
        self.contiguous.insert(e.member, c);
        self.set_eff(id);
        vec![]
    }

    fn equivocation(&mut self, member: MemberId, seq: u32) -> Vec<Msg> {
        self.stats.equivocations += 1;
        self.ev(format!("EQUIVOCATION proof: member {member} seq {seq} has two different signed entries"));
        self.void_member_from(member, seq);
        let key = (member, seq);
        if self.voted.insert(key) {
            self.record_vote(member, seq, self.me);
            return vec![Msg::Vote { target: member, seq }];
        }
        vec![]
    }

    fn void_member_from(&mut self, member: MemberId, seq: u32) {
        self.void_from.insert(member, seq.min(self.void_from.get(&member).copied().unwrap_or(u32::MAX)));
        let bad: Vec<EntryId> = self
            .entries
            .iter()
            .filter(|(_, e)| e.member == member && e.seq >= seq)
            .map(|(id, _)| *id)
            .collect();
        for id in bad {
            self.void.insert(id);
            self.set_eff(id);
        }
        let c = self.contiguous.get(&member).copied().unwrap_or(0).min(seq - 1);
        self.contiguous.insert(member, c);
    }

    fn record_vote(&mut self, target: MemberId, seq: u32, from: MemberId) {
        if !self.members.contains(&from) || from == target {
            return;
        }
        let n = self.members.len();
        let s = self.votes.entry((target, seq)).or_default();
        s.insert(from);
        if s.len() * 2 > n && self.members.contains(&target) {
            // Strict majority of the epoch's members: deterministic eviction (view change).
            self.members.remove(&target);
            self.epoch += 1;
            self.void_member_from(target, seq);
            self.ev(format!("member {target} evicted by majority vote (epoch {}); its entries from seq {seq} are void", self.epoch));
            for id in self.eff.keys().copied().collect::<Vec<_>>() {
                self.set_eff(id);
            }
        }
    }

    pub fn make_ack(&self) -> Ack {
        let mut held: Vec<(MemberId, u32, EntryId)> = Vec::new();
        for (&q, &s) in &self.contiguous {
            if s > 0 {
                held.push((q, s, self.by_author[&(q, s)]));
            }
        }
        let mut late = Vec::new();
        for (id, &a) in &self.arrived {
            if let Some(e) = self.entries.get(id) {
                if e.member != self.me && a > e.stamp + self.cfg.late_allowance && e.stamp + 2 * self.cfg.window >= self.conf {
                    late.push((*id, a));
                }
            }
        }
        let check = if self.dirty_from.is_none() {
            self.check_hashes.range(..self.conf.min(self.frame)).next_back().map(|(f, h)| (*f, *h))
        } else {
            None
        };
        Ack { member: self.me, horizon: self.now.saturating_sub(1 + self.horizon_lag), held, late, check }
    }

    /// Process an ack (mine included). Returns fetch requests for entries we lack.
    pub fn on_ack(&mut self, a: Ack) -> Vec<(MemberId, Msg)> {
        let mut out = Vec::new();
        if !self.members.contains(&a.member) {
            return out;
        }
        let own = a.held.iter().find(|(q, _, _)| *q == a.member).map_or(0, |x| x.1);
        let hist = self.horizons.entry(a.member).or_default();
        if hist.last().is_none_or(|&(h, s)| a.horizon >= h && own >= s) {
            hist.push((a.horizon, own));
            if hist.len() > 4096 {
                hist.drain(..2048);
            }
        }
        let backdated: Vec<EntryId> = self
            .entries
            .iter()
            .filter(|(id, e)| e.member == a.member && e.member != self.me && !self.void.contains(id) && self.breaks_horizon(e))
            .map(|(id, _)| *id)
            .collect();
        for id in backdated {
            let e = &self.entries[&id];
            let msg = format!("violation: m{} s{} stamped {} is at or before a horizon it published", e.member, e.seq, e.stamp);
            self.ev(msg);
            self.stats.entries_rejected_violation += 1;
            self.void.insert(id);
            self.set_eff(id);
        }
        for &(id, f) in &a.late {
            let changed = self.late_reports.entry(id).or_default().insert(a.member, f) != Some(f);
            if changed {
                self.ev(format!("late report from m{} for {id:016x}: arrived at its frame {f}", a.member));
            }
            if changed && self.entries.contains_key(&id) {
                self.set_eff(id);
            }
        }
        // Gap detection and equivocation suspicion.
        for &(q, s, id) in &a.held {
            if q == self.me {
                continue; // an author never fetches its own entries
            }
            let mine = self.contiguous.get(&q).copied().unwrap_or(0);
            if s > mine && !self.void_from.get(&q).is_some_and(|&v| v <= mine + 1) {
                out.push((a.member, Msg::Fetch { author: q, from: mine + 1, to: s }));
            } else if s <= mine && self.by_author.get(&(q, s)).is_some_and(|&x| x != id) {
                out.push((a.member, Msg::Fetch { author: q, from: s, to: s }));
            }
        }
        if let Some((f, h)) = a.check {
            if a.member != self.me {
                self.remote_checks.entry(f).or_default().insert(a.member, h);
            }
        }
        self.acks.insert(a.member, a);
        self.recompute_conf();
        self.evaluate_checks();
        out
    }

    /// Confirmed frame from acks only (review C1): frame f is confirmed when every member has
    /// promised not to author at or before f, and every member holds every author's entries that
    /// could be stamped at or before f, with identical ids.
    fn recompute_conf(&mut self) {
        let mut c = u64::MAX;
        for m in &self.members {
            match self.acks.get(m) {
                Some(a) => c = c.min(a.horizon + 1),
                None => return,
            }
        }
        for m in &self.members {
            let a = &self.acks[m];
            for q in &self.members {
                let held = a.held.iter().find(|x| x.0 == *q);
                let (hs, hid) = held.map_or((0, 0), |x| (x.1, x.2));
                if hs > 0 && self.by_author.get(&(*q, hs)).is_some_and(|&x| x != hid) {
                    return; // disagreement: possible equivocation, no confirmation
                }
                let hist = match self.horizons.get(q) {
                    Some(h) => h,
                    None => return,
                };
                // Largest horizon among q's acks whose own seq <= what m holds.
                let mut cover = None;
                for &(h, s) in hist.iter().rev() {
                    if s <= hs {
                        cover = Some(h + 1);
                        break;
                    }
                }
                match cover {
                    Some(cv) => c = c.min(cv),
                    None => return,
                }
            }
        }
        if c > self.conf && c != u64::MAX {
            self.conf = c;
        }
    }

    pub fn on_fetch(&self, author: MemberId, from: u32, to: u32) -> Vec<Msg> {
        (from..=to.min(from + 64))
            .filter_map(|s| self.by_author.get(&(author, s)).and_then(|id| self.entries.get(id)))
            .map(|e| Msg::Entry(e.clone()))
            .collect()
    }

    /// Fetch requests for authors whose acks show entries we lack (rate-limited per author).
    pub fn take_fetches(&mut self, reqs: Vec<(MemberId, Msg)>, now_ms: u64) -> Vec<(MemberId, Msg)> {
        let mut out = Vec::new();
        for (to, m) in reqs {
            if let Msg::Fetch { author, from, .. } = m {
                let last = self.fetch_asked.get(&author).copied();
                if last.is_some_and(|(f, t)| f == from && now_ms < t + 60) {
                    continue;
                }
                self.fetch_asked.insert(author, (from, now_ms));
            }
            out.push((to, m));
        }
        out
    }

    pub fn on_vote(&mut self, from: MemberId, target: MemberId, seq: u32) -> Vec<Msg> {
        self.record_vote(target, seq, from);
        vec![]
    }

    pub fn on_hash(&mut self, from: MemberId, frame: u64, hash: u64) {
        self.remote_checks.entry(frame).or_default().insert(from, hash);
        self.evaluate_checks();
    }

    fn evaluate_checks(&mut self) {
        let mut done = Vec::new();
        for (f, theirs) in &self.remote_checks {
            if let Some(mine) = self.check_hashes.get(f) {
                for (m, h) in theirs {
                    if !self.evaluated.insert((*f, *m)) {
                        continue;
                    }
                    if h == mine {
                        self.stats.checks_ok += 1;
                    } else {
                        self.stats.checks_bad += 1;
                        self.events.push(format!(
                            "[w{} m{}] NONDETERMINISM at confirmed frame {f}: mine {mine:016x} member {m} {h:016x}",
                            self.world_id, self.me
                        ));
                    }
                }
                done.push(*f);
            }
        }
        for f in done {
            self.remote_checks.remove(&f);
        }
    }

    /// Hash messages for confirmed check frames, but only once this member's simulation of them
    /// used the complete log (caught up, no re-simulation pending). The newest few are re-sent with
    /// every ack, so a lost hash message is repaired (review H4); receivers evaluate each
    /// (frame, member) pair once.
    pub fn due_checks(&mut self) -> Vec<Msg> {
        if self.dirty_from.is_some() {
            return vec![];
        }
        let fin = self.conf.min(self.frame);
        let ready: Vec<u64> = self.check_hashes.range(..fin).rev().take(4).map(|(f, _)| *f).collect();
        let mut out = Vec::new();
        for f in ready {
            self.checks_sent.insert(f);
            out.push(Msg::Hash { frame: f, hash: self.check_hashes[&f] });
        }
        self.evaluate_checks();
        out
    }

    // ---- simulation ------------------------------------------------------------------------------

    /// Sorted (entry id, entry) for frame `f`: soonest-wins order with the per-frame tie rank.
    fn frame_entries(&self, f: u64) -> Vec<(EntryId, Entry)> {
        let mut v: Vec<(EntryId, Entry)> = self
            .at_frame
            .get(&f)
            .map(|s| s.iter().filter(|id| !self.void.contains(id)).map(|id| (*id, self.entries[id].clone())).collect())
            .unwrap_or_default();
        let seed = self.cfg.seed;
        v.sort_by_key(|(_, e)| (tie_rank(seed, f, e.member), e.member, e.seq));
        v
    }

    fn live(&self, obj: u32) -> Option<Obj> {
        self.t.objs.get(obj as usize).copied().flatten()
    }

    fn apply(&mut self, id: EntryId, e: &Entry, f: u64, hands_set: &mut BTreeSet<u32>, carry: &mut Vec<(u32, BodyState)>) -> Outcome {
        match &e.payload {
            Payload::Touch { obj, impulse, grab } => {
                let Some(o) = self.live(*obj) else { return Outcome::RejectedGone };
                if e.member != LOGIC {
                    if let Some(c) = self.t.claims.get(obj) {
                        if c.member != e.member && c.until > f {
                            return Outcome::RejectedClaimed { by: c.member, until: c.until };
                        }
                    }
                    if let Some(&(m, u)) = self.t.cooldown.get(obj) {
                        if m == e.member && u > f && !self.t.claims.contains_key(obj) {
                            return Outcome::RejectedCooldown { until: u };
                        }
                    }
                }
                let imp = Vec3 { x: impulse[0], y: impulse[1], z: impulse[2] };
                body_apply_linear_impulse_to_center(&mut self.world, o.body, imp, true);
                if e.member != LOGIC {
                    match self.t.claims.get(obj).copied() {
                        Some(c) if c.member == e.member && c.until > f => {
                            if *grab {
                                self.t.claims.insert(*obj, Claim { member: e.member, until: f + CLAIM_FRAMES });
                            }
                        }
                        _ => {
                            self.t.claims.insert(*obj, Claim { member: e.member, until: f + CLAIM_FRAMES });
                        }
                    }
                }
                if let Some((cobj, st)) = e.carried {
                    if self.eff.get(&id) == Some(&e.stamp) {
                        carry.push((cobj, st));
                    }
                }
                Outcome::Accepted
            }
            Payload::Hand { obj, target_mm, active } => {
                let Some(o) = self.live(*obj) else { return Outcome::RejectedGone };
                let p = if *active {
                    Pos { x: target_mm[0] as f32 * 0.001, y: target_mm[1] as f32 * 0.001, z: target_mm[2] as f32 * 0.001 }
                } else {
                    Pos { x: 0.0, y: 1.5, z: 0.0 }
                };
                body_set_target_transform(&mut self.world, o.body, Transform { p, q: QUAT_IDENTITY }, self.cfg.dt, true);
                hands_set.insert(*obj);
                Outcome::Accepted
            }
            Payload::Depart { transfer, obj, .. } => {
                if self.t.escrow.contains_key(transfer) {
                    return Outcome::RejectedDuplicate;
                }
                let gone = Escrow { obj: *obj, record: None, status: EscrowStatus::DepartRejected, depart_frame: f };
                let Some(o) = self.live(*obj) else {
                    self.t.escrow.insert(*transfer, gone);
                    return Outcome::RejectedGone;
                };
                if let Some(c) = self.t.claims.get(obj) {
                    if c.member != e.member && c.until > f {
                        self.t.escrow.insert(*transfer, gone);
                        return Outcome::RejectedClaimed { by: c.member, until: c.until };
                    }
                }
                let record = ObjRecord { global: o.global, kind: o.kind, state: BodyState::read(&self.world, o.body), src_world: self.world_id, src_frame: f };
                destroy_body(&mut self.world, o.body);
                self.t.objs[*obj as usize] = None;
                self.t.claims.remove(obj);
                self.t.escrow.insert(*transfer, Escrow { obj: *obj, record: Some(record), status: EscrowStatus::Pending, depart_frame: f });
                Outcome::Accepted
            }
            Payload::Arrive { transfer, record } => {
                if self.t.arrivals.contains_key(transfer) {
                    return Outcome::RejectedDuplicate;
                }
                if !self.t.portal_open {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f });
                    return Outcome::RejectedPortalClosed;
                }
                let obj = self.spawn(record, Pos { x: 0.0, y: 2.0, z: 0.0 });
                self.t.arrivals.insert(*transfer, Arrival::Accepted { obj, frame: f });
                self.watched.insert(obj);
                Outcome::Accepted
            }
            Payload::Verdict { transfer, accepted } => {
                let Some(esc) = self.t.escrow.get(transfer).copied() else { return Outcome::RejectedGone };
                if esc.status != EscrowStatus::Pending {
                    return Outcome::RejectedDuplicate;
                }
                if *accepted {
                    self.t.escrow.insert(*transfer, Escrow { status: EscrowStatus::Committed, record: None, ..esc });
                } else {
                    let rec = esc.record.expect("pending escrow holds its record");
                    let back = self.spawn(&rec, rec.state.pos());
                    debug_assert_eq!(back as usize, self.t.objs.len() - 1);
                    self.t.escrow.insert(*transfer, Escrow { status: EscrowStatus::Returned, ..esc });
                }
                Outcome::Accepted
            }
            Payload::Portal { open } => {
                self.t.portal_open = *open;
                Outcome::Accepted
            }
        }
    }

    /// Recreate an object from a record at `at`, keeping its rotation and velocities.
    fn spawn(&mut self, r: &ObjRecord, at: Pos) -> u32 {
        let mut d = default_body_def();
        d.type_ = BodyType::Dynamic;
        d.position = at;
        d.rotation = r.state.rot();
        d.linear_velocity = r.state.lin();
        d.angular_velocity = r.state.ang();
        let b = create_body(&mut self.world, &d);
        add_shape(&mut self.world, b, r.kind);
        self.t.objs.push(Some(Obj { body: b, global: r.global, kind: r.kind }));
        (self.t.objs.len() - 1) as u32
    }

    /// Step one frame: keyframe, entries in canonical order, hands, Box3D step, carried states.
    fn step_frame(&mut self, record: bool) {
        let f = self.frame;
        if record && f % self.cfg.keyframe_every == 0 && !self.ring.contains_key(&f) {
            let k = self.save();
            self.ring.insert(f, k);
        }
        let mut hands_set = BTreeSet::new();
        let mut carry = Vec::new();
        for (id, e) in self.frame_entries(f) {
            let o = self.apply(id, &e, f, &mut hands_set, &mut carry);
            if record {
                self.outcomes.insert(id, (f, o));
            }
        }
        // Hands without a target this frame hold still (prediction = no motion).
        let hands: Vec<(u32, Obj)> =
            self.t.objs.iter().enumerate().filter_map(|(i, o)| o.filter(|o| o.kind == 4).map(|o| (i as u32, o))).collect();
        for (i, o) in hands {
            if !hands_set.contains(&i) {
                body_set_linear_velocity(&mut self.world, o.body, Vec3 { x: 0.0, y: 0.0, z: 0.0 });
            }
        }
        let t0 = now_us();
        self.world.step(self.cfg.dt, SUBSTEPS);
        if record {
            self.stats.step_us += now_us() - t0;
            self.stats.steps += 1;
        }
        // User ruling 09:30Z: the touched object takes the carried resulting state. A no-op when
        // this peer is on the author's world line; otherwise it pulls the object onto it.
        for (obj, st) in carry {
            if let Some(o) = self.live(obj) {
                if BodyState::read(&self.world, o.body) != st {
                    st.write(&mut self.world, o.body);
                    if record {
                        self.stats.overwrites += 1;
                    }
                }
            }
        }
        // Expire claims; their holders enter a short cooldown on that object.
        let next = f + 1;
        let expired: Vec<(u32, MemberId)> = self.t.claims.iter().filter(|(_, c)| c.until <= next).map(|(o, c)| (*o, c.member)).collect();
        for (o, m) in expired {
            self.t.claims.remove(&o);
            self.t.cooldown.insert(o, (m, next + CLAIM_COOLDOWN));
        }
        self.t.cooldown.retain(|_, (_, u)| *u > next);
        self.frame = next;
        if record {
            self.hashes.insert(f, self.state_hash());
            for &obj in &self.watched.clone() {
                if let Some(s) = self.obj_state(obj) {
                    self.obj_hist.insert((f, obj), s.hash());
                }
            }
            if f % self.cfg.check_every == 0 {
                let h = self.image_hash();
                self.check_hashes.insert(f, h);
            }
        }
    }

    /// Roll back if needed, then simulate forward to `target` (exclusive) unless the speculation
    /// limit is reached. Returns the number of new frames stepped.
    pub fn advance(&mut self, target: u64) -> u64 {
        if let Some(from) = self.dirty_from.take() {
            let now = self.frame;
            let (&kf, _) = self.ring.range(..=from).next_back().expect("keyframe at or before rollback frame");
            let k = self.ring[&kf].clone();
            for f in self.ring.range(kf + 1..).map(|(f, _)| *f).collect::<Vec<_>>() {
                self.ring.remove(&f);
            }
            self.restore(&k, kf);
            self.stats.rollbacks += 1;
            self.stats.max_depth = self.stats.max_depth.max(now - from);
            let t0 = now_us();
            while self.frame < now {
                self.step_frame(true);
                self.stats.resim_frames += 1;
            }
            self.stats.resim_us += now_us() - t0;
        }
        self.trim_ring();
        let mut n = 0;
        while self.frame < target {
            if self.frame >= self.conf + self.cfg.window {
                self.stats.paused_frames += 1;
                break;
            }
            self.step_frame(true);
            n += 1;
        }
        n
    }

    /// Drop keyframes this member can never need again: everything before the newest keyframe at
    /// or before min(confirmed frame, simulated frame), once no re-simulation is pending.
    fn trim_ring(&mut self) {
        if self.dirty_from.is_some() {
            return;
        }
        let fin = self.conf.min(self.frame);
        if let Some((&kf, _)) = self.ring.range(..=fin).next_back() {
            let drop: Vec<u64> = self.ring.range(..kf).map(|(f, _)| *f).collect();
            for f in drop {
                self.ring.remove(&f);
            }
        }
    }

    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    pub fn caught_up(&self) -> bool {
        self.dirty_from.is_none()
    }

    /// Entries this member holds whose effective frame is confirmed (R14 relays wait for this).
    pub fn is_confirmed(&self, id: EntryId) -> bool {
        self.eff.get(&id).is_some_and(|&f| f < self.conf)
    }

    pub fn all_entries(&self) -> Vec<Entry> {
        self.entries.values().cloned().collect()
    }

    pub fn void_ids(&self) -> &BTreeSet<EntryId> {
        &self.void
    }
}

// ---- offline reference ---------------------------------------------------------------------------

/// Re-simulate a world from scratch with a complete, final log and effective frames: the
/// single-world reference used to check that sharing a process with other worlds, rollbacks and
/// throttling changed nothing.
pub fn replay(world_id: u32, scene: SceneKind, cfg: SimConfig, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> BTreeMap<u64, u64> {
    let mut s = WorldSim::new(world_id, u32::MAX, members, scene, cfg);
    for (e, eff) in log {
        let id = e.id();
        s.by_author.insert((e.member, e.seq), id);
        s.entries.insert(id, e.clone());
        s.eff.insert(id, *eff);
        s.at_frame.entry(*eff).or_default().insert(id);
    }
    s.conf = u64::MAX / 2;
    while s.frame < frames {
        s.step_frame(true);
    }
    s.hashes
}

/// The log this member would hand to [`replay`]: every non-void entry with its effective frame.
pub fn final_log(s: &WorldSim) -> Vec<(Entry, u64)> {
    s.eff.iter().map(|(id, f)| (s.entries[id].clone(), *f)).collect()
}

pub fn digest(h: &BTreeMap<u64, u64>, frames: u64) -> u64 {
    fnv_u64s((0..frames).map(|f| h.get(&f).copied().unwrap_or(0)))
}

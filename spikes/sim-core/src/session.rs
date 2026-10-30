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
    /// The author's ack, as a chain entry (review NEW-1): it will author nothing stamped at or
    /// before `horizon`; per other author, the newest seq it holds contiguously and that entry's
    /// id; its newest confirmed check frame and full-state hash (review H4).
    Ack { horizon: u64, heads: Vec<(MemberId, u32, EntryId)>, check: Option<(u64, u64)> },
    /// Vote to evict `target`, which signed two versions of its entry `seq`. `frozen` is the head
    /// of target's chain this voter holds and will ever reference (review NEW-2).
    Vote { target: MemberId, seq: u32, frozen: (u32, EntryId) },
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

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    /// Any chain entry: a perturbation, an ack or a vote.
    Entry(Entry),
    /// Ask for `author`'s entries `from..=to` (gap or conflict detection).
    Fetch { author: MemberId, from: u32, to: u32 },
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
            Payload::Ack { horizon, heads, check } => {
                self.u8(7); self.u64(*horizon); self.u32(heads.len() as u32);
                for (q, s, id) in heads { self.u32(*q); self.u32(*s); self.u64(*id) }
                match check { Some((f, h)) => { self.u8(1); self.u64(*f); self.u64(*h) } None => self.u8(0) }
            }
            Payload::Vote { target, seq, frozen } => {
                self.u8(8); self.u32(*target); self.u32(*seq); self.u32(frozen.0); self.u64(frozen.1)
            }
        }
        match &e.carried {
            Some((obj, s)) => { self.u8(1); self.u32(*obj); self.state(s) }
            None => self.u8(0),
        }
    }
    pub fn msg(&mut self, m: &Msg) {
        match m {
            Msg::Entry(e) => { self.u8(1); self.entry(e) }
            Msg::Fetch { author, from, to } => { self.u8(3); self.u32(*author); self.u32(*from); self.u32(*to) }
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
            7 => {
                let horizon = self.u64()?;
                let n = self.u32()? as usize;
                let mut heads = Vec::with_capacity(n.min(256));
                for _ in 0..n { heads.push((self.u32()?, self.u32()?, self.u64()?)) }
                let check = if self.u8()? == 1 { Some((self.u64()?, self.u64()?)) } else { None };
                Payload::Ack { horizon, heads, check }
            }
            8 => Payload::Vote { target: self.u32()?, seq: self.u32()?, frozen: (self.u32()?, self.u64()?) },
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
            3 => Msg::Fetch { author: self.u32()?, from: self.u32()?, to: self.u32()? },
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
/// Dead-reckoning speed cap for a hand without an entry (m/s).
const HAND_COAST_VMAX: f32 = 3.0;

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
    /// Active hands: frames since the hand's last entry (dead reckoning, `SimConfig::hand_predict`).
    pub hand_idle: BTreeMap<u32, u64>,
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
    /// Keyframe saves and restores (counts and time), the worst single `advance` call, and the
    /// most image bytes the ring ever held.
    pub saves: u64,
    pub save_us: u64,
    pub restore_us: u64,
    pub max_advance_us: u64,
    pub advance_us: u64,
    pub max_ring_bytes: u64,
    pub overwrites: u64,
    pub paused_frames: u64,
    pub entries_rejected_violation: u64,
    pub equivocations: u64,
    pub checks_ok: u64,
    pub checks_bad: u64,
}

/// Which world images a member keeps between the base and its present (design 6.7). Purely local:
/// images are a cache of states the log already determines, so no setting changes any result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapPolicy {
    /// A keyframe at every multiple of `k` frames (the 3d0f98a behaviour with k = 4).
    Every(u64),
    /// Dense near the present (every `k` frames for the newest `4k`), then thinned so the spacing
    /// grows with age: about two keyframes per doubling of age.
    Log(u64),
    /// The base (newest image at or before the confirmed frame) plus one candidate, saved at the
    /// first multiple of `k` after the confirmed frame, that becomes the next base.
    Base(u64),
    /// The base plus keyframes every `k` frames for the newest `4k` frames only: a rollback
    /// deeper than that re-simulates from the base.
    Window(u64),
}

impl SnapPolicy {
    pub fn label(&self) -> String {
        match self {
            SnapPolicy::Every(k) => format!("every{k}"),
            SnapPolicy::Log(k) => format!("log{k}"),
            SnapPolicy::Base(k) => format!("base{k}"),
            SnapPolicy::Window(k) => format!("window{k}"),
        }
    }
}

/// Keyframe spacing kept at `age` frames behind the present under `SnapPolicy::Log(k)`.
pub fn log_grid(k: u64, age: u64) -> u64 {
    let r = age / k;
    if r < 4 {
        k
    } else {
        k << (62 - r.leading_zeros() as u64)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SimConfig {
    pub dt: f32,
    /// Speculation limit: a member stops advancing when `frame - conf` reaches this (frames).
    pub window: u64,
    pub snap: SnapPolicy,
    /// How far an ack's horizon may lag its own stamp, in frames: bounds backdating (review H5,
    /// user rule 14:31Z). Replaces the arrival-based late allowance; nothing is re-stamped.
    pub horizon_slack: u64,
    pub seed: u64,
    pub check_every: u64,
    /// Remote-hand dead reckoning: an active hand with no entry for a frame keeps its last
    /// commanded velocity for up to this many frames, then holds. 0 = hold at once.
    pub hand_predict: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            dt: 1.0 / 60.0,
            window: 120,
            snap: SnapPolicy::Every(4),
            horizon_slack: 2,
            seed: 0x5052_494D_4F52_4449,
            check_every: 30,
            hand_predict: 0,
        }
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
    /// Scratch world for canonical check hashes (review NEW-7).
    scratch: Option<World>,
    // ---- log (round 2: chains, stability, chain-local verdicts)
    known: BTreeSet<MemberId>,
    entries: BTreeMap<EntryId, Entry>,
    /// The version of each (member, seq) this member holds and references: the first one seen.
    by_author: BTreeMap<(MemberId, u32), EntryId>,
    /// Per author: the verified contiguous prefix (hash-linked) held.
    contiguous: BTreeMap<MemberId, u32>,
    /// Void by a chain-local rule (horizon violations): identical for every holder.
    violations: BTreeSet<EntryId>,
    /// Withdrawn by an equivocation proof or an eviction cut.
    disputed: BTreeSet<EntryId>,
    eff: BTreeMap<EntryId, u64>,
    at_frame: BTreeMap<u64, BTreeSet<EntryId>>,
    /// Per author: (max horizon declared so far, last ack stamp) along its verified prefix.
    chain_state: BTreeMap<MemberId, (Option<u64>, u64)>,
    /// Per author: ack seq -> max horizon declared up to that seq (valid acks only).
    ack_hist: BTreeMap<MemberId, BTreeMap<u32, u64>>,
    /// (referrer, author) -> seq -> id: what each member's acks said it holds of each chain.
    refs: BTreeMap<(MemberId, MemberId), BTreeMap<u32, EntryId>>,
    /// Equivocators: first disputed seq and the two versions.
    proofs: BTreeMap<MemberId, (u32, EntryId, EntryId)>,
    /// Heads this member froze when it learned of a proof.
    frozen: BTreeMap<MemberId, u32>,
    /// target -> voter -> the voter's frozen head of target's chain.
    votes: BTreeMap<MemberId, BTreeMap<MemberId, (u32, EntryId)>>,
    /// Evicted members: (cut seq, first frame they no longer count for).
    evicted: BTreeMap<MemberId, (u32, u64)>,
    wants: Vec<(MemberId, MemberId, u32, u32)>,
    pub conf: u64,
    /// My clock frame (my present), set by the harness each tick.
    pub now: u64,
    my_seq: u32,
    /// Test fixture (review H5): publish horizons this many frames behind the present.
    pub horizon_lag: u64,
    my_last: EntryId,
    fetch_asked: BTreeMap<(MemberId, MemberId), (u32, u64)>,
    // ---- results
    pub hashes: BTreeMap<u64, u64>,
    pub check_hashes: BTreeMap<u64, u64>,
    remote_checks: BTreeMap<u64, BTreeMap<MemberId, u64>>,
    evaluated: BTreeSet<(u64, MemberId)>,
    pub outcomes: BTreeMap<EntryId, (u64, Outcome)>,
    pub watched: BTreeSet<u32>,
    /// (frame, obj) -> object state hash after that frame, for watched objects.
    pub obj_hist: BTreeMap<(u64, u32), u64>,
    dirty_from: Option<u64>,
    /// The present a running re-simulation is heading for (keyframe thinning during re-sim).
    resim_to: u64,
    /// Record every object's position after each frame (observer measurements).
    pub track: bool,
    pub pos_hist: BTreeMap<u64, Vec<[f32; 3]>>,
    pub stats: Stats,
    pub events: Vec<String>,
}

/// Bytes needed to store `b` as a patch on `a`: every `block`-byte block of `b` that differs from
/// the block at the same offset in `a` (8 bytes of offset each), plus any tail beyond `a`.
pub fn block_delta(a: &[u8], b: &[u8], block: usize) -> u64 {
    let n = a.len().min(b.len());
    let mut bytes = 0u64;
    let mut i = 0;
    while i < n {
        let e = (i + block).min(n);
        if a[i..e] != b[i..e] {
            bytes += (e - i) as u64 + 8;
        }
        i = e;
    }
    bytes + b.len().saturating_sub(n) as u64
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
            scratch: None,
            known: members.iter().copied().collect(),
            entries: BTreeMap::new(),
            by_author: BTreeMap::new(),
            contiguous: BTreeMap::new(),
            violations: BTreeSet::new(),
            disputed: BTreeSet::new(),
            eff: BTreeMap::new(),
            at_frame: BTreeMap::new(),
            chain_state: BTreeMap::new(),
            ack_hist: BTreeMap::new(),
            refs: BTreeMap::new(),
            proofs: BTreeMap::new(),
            frozen: BTreeMap::new(),
            votes: BTreeMap::new(),
            evicted: BTreeMap::new(),
            wants: Vec::new(),
            conf: 0,
            now: 0,
            my_seq: 0,
            horizon_lag: 0,
            my_last: 0,
            fetch_asked: BTreeMap::new(),
            hashes: BTreeMap::new(),
            check_hashes: BTreeMap::new(),
            remote_checks: BTreeMap::new(),
            evaluated: BTreeSet::new(),
            outcomes: BTreeMap::new(),
            watched: BTreeSet::new(),
            obj_hist: BTreeMap::new(),
            dirty_from: None,
            resim_to: 0,
            track: false,
            pos_hist: BTreeMap::new(),
            stats: Stats::default(),
            events: Vec::new(),
        }
    }

    fn ev(&mut self, s: String) {
        self.events.push(format!("[w{} m{} f{} c{}] {s}", self.world_id, self.me, self.frame, self.conf));
    }

    // ---- snapshots -------------------------------------------------------------------------------

    fn save(&mut self) -> Keyframe {
        let t0 = now_us();
        let mut buf = RecBuffer::new();
        serialize_world(&self.world, &mut buf, &mut self.registry);
        if self.registry.entries.len() != self.slot_count {
            self.slots = self.registry.to_slots();
            self.slot_count = self.registry.entries.len();
        }
        self.stats.saves += 1;
        self.stats.save_us += now_us() - t0;
        Keyframe { image: buf.data, t: self.t.clone() }
    }

    fn restore(&mut self, k: &Keyframe, frame: u64) {
        let t0 = now_us();
        assert!(deserialize_into_shell(&k.image, &mut self.world, &mut self.slots), "restore failed");
        self.t = k.t.clone();
        self.frame = frame;
        self.stats.restore_us += now_us() - t0;
    }

    /// Bytes of world image the ring holds now (the memory side of the snapshot knob).
    pub fn ring_bytes(&self) -> u64 {
        self.ring.values().map(|k| k.image.len() as u64).sum()
    }

    /// What the ring would hold as one full base image plus block deltas: (full bytes, bytes if
    /// every later image were stored as a delta against the base, bytes if each were stored as a
    /// delta against the previous image), with 64-byte blocks.
    pub fn ring_delta_bytes(&self) -> (u64, u64, u64) {
        let imgs: Vec<&[u8]> = self.ring.values().map(|k| k.image.as_slice()).collect();
        let Some(base) = imgs.first() else { return (0, 0, 0) };
        let full = imgs.iter().map(|i| i.len() as u64).sum();
        let vs_base = base.len() as u64 + imgs[1..].iter().map(|i| block_delta(base, i, 64)).sum::<u64>();
        let vs_prev = base.len() as u64 + imgs.windows(2).map(|w| block_delta(w[0], w[1], 64)).sum::<u64>();
        (full, vs_base, vs_prev)
    }

    /// The memory/CPU knob (design 6.7): any value, any time. Takes effect at the next keyframe
    /// decision and trim; never changes a result.
    pub fn set_snap(&mut self, p: SnapPolicy) {
        self.cfg.snap = p;
    }

    fn wants_keyframe(&self, f: u64) -> bool {
        if self.ring.contains_key(&f) {
            return false;
        }
        if self.ring.is_empty() {
            return true;
        }
        match self.cfg.snap {
            SnapPolicy::Every(k) => f % k == 0,
            // During a re-simulation, skip images the thinning would drop on arrival.
            SnapPolicy::Log(k) => f % k == 0 && f % log_grid(k, self.resim_to.max(self.frame) - f) == 0,
            SnapPolicy::Base(k) => f % k == 0 && f > self.conf && self.ring.range(self.conf + 1..).next().is_none(),
            SnapPolicy::Window(k) => f % k == 0 && self.resim_to.max(self.frame) - f < 4 * k,
        }
    }

    /// Hash of the full world image plus tables: used at confirmed check frames (review H4).
    pub fn image_hash(&mut self) -> u64 {
        // A check-hash serialization is not a keyframe: keep it out of the snapshot counters.
        let (n, us) = (self.stats.saves, self.stats.save_us);
        let k = self.save();
        (self.stats.saves, self.stats.save_us) = (n, us);
        // Review NEW-7: after a destroy, a world that stepped continuously and one restored from a
        // keyframe hold the same physics but different image bytes. Hash the image of a scratch
        // world restored from this one instead, so both paths hash alike.
        let scratch = self.scratch.get_or_insert_with(|| World::new(&default_world_def()));
        assert!(deserialize_into_shell(&k.image, scratch, &mut self.slots), "canonical restore failed");
        let mut buf = RecBuffer::new();
        serialize_world(scratch, &mut buf, &mut self.registry);
        fnv_bytes(&buf.data) ^ self.tables_hash().rotate_left(17)
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
        if self.cfg.hand_predict > 0 {
            for (o, n) in &self.t.hand_idle {
                v.extend([*o as u64, *n, 11]);
            }
        }
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
    //
    // Round-2 protocol (design 10.8; reviews NEW-1, NEW-2; user rule 14:31Z):
    // * every input to a deterministic decision is an entry in its author's hash chain:
    //   perturbations, acks (horizon, holdings, check hash) and eviction votes;
    // * an entry's frame is its stamp. Nothing is ever re-stamped, so an author never sees its own
    //   interaction corrected; lateness is paid by the late receiver's rollback;
    // * validity is chain-local: an entry is void iff its own chain breaks a rule, so every holder
    //   decides the same;
    // * confirmation uses only stable data: a seq of a chain is stable once every member's chain
    //   references it with the same id (Wuu-Bernstein's 2-D time table, over signed hash chains).

    pub fn entry(&self, id: EntryId) -> Option<&Entry> {
        self.entries.get(&id)
    }

    pub fn effective_frame(&self, id: EntryId) -> Option<u64> {
        self.eff.get(&id).copied()
    }

    fn is_void(&self, id: EntryId) -> bool {
        self.violations.contains(&id) || self.disputed.contains(&id)
    }

    /// Apply or withdraw an entry's effect on the sim according to its current verdict.
    fn set_eff(&mut self, id: EntryId) {
        let e = &self.entries[&id];
        let acts = !matches!(e.payload, Payload::Ack { .. } | Payload::Vote { .. });
        let new = if acts && !self.is_void(id) { Some(e.stamp) } else { None };
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
        }
    }

    fn mark_dirty(&mut self, f: u64) {
        // Review NEW-2: confirmed frames are final. Getting here below `conf` is a protocol bug,
        // never a rollback.
        assert!(
            f >= self.conf,
            "protocol invariant violated: member {} would roll back to frame {f}, below its confirmed frame {}",
            self.me,
            self.conf
        );
        if f < self.frame {
            self.dirty_from = Some(self.dirty_from.map_or(f, |d| d.min(f)));
        }
    }

    fn publish(&mut self, e: Entry) -> Entry {
        self.my_seq += 1;
        self.my_last = e.id();
        let _ = self.insert(e.clone());
        e
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
        self.publish(e)
    }

    /// Author a state-free entry stamped `ahead` frames after the present (opt-in input delay).
    pub fn author_ahead(&mut self, payload: Payload, ahead: u64) -> Entry {
        let stamp = self.frame.max(self.now) + ahead;
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None };
        self.publish(e)
    }

    /// Test fixture for review H5: author with an arbitrary (backdated) stamp.
    pub fn author_backdated(&mut self, payload: Payload, back: u64) -> Entry {
        let stamp = self.frame.max(self.now).saturating_sub(back);
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None };
        self.publish(e)
    }

    /// Author this member's ack: a chain entry stating its horizon (it will author nothing stamped
    /// at or before it), the head it holds of every other chain, and its newest confirmed check
    /// hash (reviews NEW-1, H4).
    pub fn author_ack(&mut self) -> Entry {
        let mut heads = Vec::new();
        for (&q, &s) in &self.contiguous {
            if s > 0 && q != self.me {
                heads.push((q, s, self.by_author[&(q, s)]));
            }
        }
        let check = if self.dirty_from.is_none() {
            self.check_hashes.range(..self.conf.min(self.frame)).next_back().map(|(f, h)| (*f, *h))
        } else {
            None
        };
        let stamp = self.frame.max(self.now);
        let horizon = self.now.saturating_sub(1 + self.horizon_lag);
        let e = Entry {
            member: self.me,
            seq: self.my_seq + 1,
            prev: self.my_last,
            stamp,
            payload: Payload::Ack { horizon, heads, check },
            carried: None,
        };
        self.publish(e)
    }

    /// Store an entry (received, fetched or my own). Returns entries to broadcast: when it proves
    /// equivocation, both versions and this member's vote.
    pub fn insert(&mut self, e: Entry) -> Vec<Entry> {
        let id = e.id();
        if self.entries.contains_key(&id) || (!self.known.contains(&e.member) && e.member != LOGIC) {
            return vec![];
        }
        if let Some(&other) = self.by_author.get(&(e.member, e.seq)) {
            // Two signed entries for the same (member, seq): a transferable proof (review C2).
            self.entries.insert(id, e.clone());
            self.disputed.insert(id);
            let mut out = vec![self.entries[&other].clone(), e.clone()];
            out.extend(self.on_proof(e.member, e.seq, other, id));
            // A fetched version may be what a pending eviction's cut was waiting for.
            self.maybe_evict(e.member);
            return out;
        }
        self.by_author.insert((e.member, e.seq), id);
        if let Some((obj, _)) = e.carried {
            self.watched.insert(obj);
        }
        if let Payload::Touch { obj, .. } = e.payload {
            self.watched.insert(obj);
        }
        // Below the confirmed frame only a horizon violation can be new (design 10.8): never apply.
        if e.stamp < self.conf && !matches!(e.payload, Payload::Ack { .. } | Payload::Vote { .. }) {
            self.violations.insert(id);
        }
        let disputed = self.proofs.get(&e.member).is_some_and(|&(k, _, _)| e.seq >= k)
            || self.evicted.get(&e.member).is_some_and(|&(cut, _)| e.seq > cut);
        if disputed {
            self.disputed.insert(id);
        }
        let (member, seq) = (e.member, e.seq);
        self.entries.insert(id, e);
        self.set_eff(id);
        let mut out = Vec::new();
        self.extend_chain(member, &mut out);
        // An entry past a hole in its author's chain shows the hole: ask for it.
        let c = self.contiguous.get(&member).copied().unwrap_or(0);
        if member != self.me && seq > c + 1 {
            self.want_all(member, c + 1, seq - 1);
        }
        if self.votes.contains_key(&member) {
            self.maybe_evict(member);
        }
        out
    }

    /// Extend `m`'s verified contiguous prefix: each next entry must link to the previous by hash.
    fn extend_chain(&mut self, m: MemberId, out: &mut Vec<Entry>) {
        loop {
            let c = self.contiguous.get(&m).copied().unwrap_or(0);
            if self.frozen.get(&m).is_some_and(|&f| c >= f) {
                break;
            }
            let Some(&id) = self.by_author.get(&(m, c + 1)) else { break };
            let prev = if c == 0 { 0 } else { self.by_author[&(m, c)] };
            if self.entries[&id].prev != prev {
                // The chain continues from a version of seq c we do not hold: fetch it.
                self.want_all(m, c, c);
                break;
            }
            self.contiguous.insert(m, c + 1);
            self.validate(m, id, out);
        }
    }

    /// Chain-local rules, in chain order (reviews NEW-1, H5; user rule 14:31Z):
    /// * an ack is void if its stamp goes backwards, or its horizon lags its stamp by more than
    ///   `horizon_slack` frames (so no member can hold a backdating window open);
    /// * any other entry is void if stamped at or before a horizon its author declared earlier.
    fn validate(&mut self, m: MemberId, id: EntryId, out: &mut Vec<Entry>) {
        let e = self.entries[&id].clone();
        let (max_h, last_ack) = self.chain_state.get(&m).copied().unwrap_or((None, 0));
        let valid = match &e.payload {
            Payload::Ack { horizon, .. } => e.stamp >= last_ack && horizon + self.cfg.horizon_slack >= e.stamp,
            Payload::Vote { .. } => true,
            _ => max_h.is_none_or(|h| e.stamp > h),
        };
        if !valid {
            self.stats.entries_rejected_violation += 1;
            self.ev(format!("violation: m{m} s{} stamped {} breaks its chain's horizon rules: void for every member", e.seq, e.stamp));
            self.violations.insert(id);
            self.set_eff(id);
            return;
        }
        match e.payload {
            Payload::Ack { horizon, heads, check } => {
                let h = max_h.map_or(horizon, |x| x.max(horizon));
                self.chain_state.insert(m, (Some(h), e.stamp));
                self.ack_hist.entry(m).or_default().insert(e.seq, h);
                if m != self.me {
                    for &(q, s, hid) in &heads {
                        let mine = self.contiguous.get(&q).copied().unwrap_or(0);
                        if q != self.me && s > mine {
                            self.want_from(m, q, mine + 1, s);
                        } else if self.by_author.get(&(q, s)).is_some_and(|&x| x != hid) {
                            self.want_from(m, q, s, s);
                        }
                        let r = self.refs.entry((m, q)).or_default();
                        r.insert(s, hid);
                        if r.len() > 256 {
                            let k = *r.keys().next().unwrap();
                            r.remove(&k);
                        }
                    }
                    if let Some((f, hh)) = check {
                        self.remote_checks.entry(f).or_default().insert(m, hh);
                    }
                }
                self.recompute_conf();
                self.evaluate_checks();
            }
            Payload::Vote { target, frozen, .. } => {
                self.votes.entry(target).or_default().insert(m, frozen);
                self.maybe_evict(target);
            }
            _ => {}
        }
        let _ = out;
    }

    /// The highest seq of `m`'s chain that every member is known to hold with identical ids:
    /// each other member's newest reference at or below my own head, checked against my copy.
    fn stable_seq(&mut self, m: MemberId) -> u32 {
        let mine = self.contiguous.get(&m).copied().unwrap_or(0);
        let mut s = mine;
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for j in members {
            if j == self.me || j == m {
                continue;
            }
            let Some((rs, rid)) = self.refs.get(&(j, m)).and_then(|r| r.range(..=mine).next_back()).map(|(a, b)| (*a, *b)) else {
                return 0;
            };
            if self.by_author.get(&(m, rs)) != Some(&rid) {
                // j holds another version: wait for the proof (fetch it from j).
                self.want_from(j, m, rs, rs);
                return 0;
            }
            s = s.min(rs);
        }
        s
    }

    /// Confirmed frame from stable data only (reviews C1, NEW-1): frame f is confirmed once, for
    /// every member, a stable prefix of its chain declares a horizon at or after f. No decision
    /// depends on it except finality (SCM, ACL, R14 relays) and trimming.
    fn recompute_conf(&mut self) {
        let mut c = u64::MAX;
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for j in members {
            let s = self.stable_seq(j);
            match self.ack_hist.get(&j).and_then(|a| a.range(..=s).next_back()).map(|(_, h)| *h) {
                Some(h) => c = c.min(h + 1),
                None => return,
            }
        }
        if c != u64::MAX && c > self.conf {
            self.conf = c;
        }
    }

    fn on_proof(&mut self, m: MemberId, k: u32, a: EntryId, b: EntryId) -> Vec<Entry> {
        let first = !self.proofs.contains_key(&m);
        if first {
            self.stats.equivocations += 1;
            self.ev(format!("EQUIVOCATION proof: member {m} seq {k} has two signed versions {a:016x} / {b:016x}"));
        }
        let k0 = self.proofs.get(&m).map_or(k, |p| p.0.min(k));
        self.proofs.insert(m, (k0, a, b));
        let mut out = Vec::new();
        if first && m != self.me && self.members.contains(&m) {
            // Freeze what this member will ever reference of m's chain, and say so in a vote.
            let f = self.contiguous.get(&m).copied().unwrap_or(0);
            self.frozen.insert(m, f);
            let fid = if f == 0 { 0 } else { self.by_author[&(m, f)] };
            let v = Entry {
                member: self.me,
                seq: self.my_seq + 1,
                prev: self.my_last,
                stamp: self.frame.max(self.now),
                payload: Payload::Vote { target: m, seq: k0, frozen: (f, fid) },
                carried: None,
            };
            out.push(self.publish(v));
        }
        self.reconcile(m);
        out
    }

    /// Which of `m`'s entries count after an equivocation proof (review NEW-2): until the eviction
    /// is decided, every entry from the disputed seq on is withdrawn unless it is stable (held
    /// identically by every member, so possibly confirmed somewhere); after it, exactly the cut.
    fn reconcile(&mut self, m: MemberId) {
        let k = self.proofs.get(&m).map(|p| p.0);
        let decided = self.evicted.get(&m).map(|p| p.0);
        let stable = self.stable_seq(m);
        let ids: Vec<(u32, EntryId)> = self.entries.iter().filter(|(_, e)| e.member == m).map(|(id, e)| (e.seq, *id)).collect();
        for (seq, id) in ids {
            let mine = self.by_author.get(&(m, seq)) == Some(&id);
            let keep = match (decided, k) {
                (Some(cut), _) => mine && seq <= cut,
                (None, Some(k)) => mine && (seq < k || seq <= stable),
                (None, None) => mine,
            };
            let changed = if keep { self.disputed.remove(&id) } else { self.disputed.insert(id) };
            if changed {
                self.set_eff(id);
            }
        }
    }

    /// Follow `m`'s chain back from `head` to seq `to`; None if an entry on the way is missing.
    fn walk_back(&self, m: MemberId, head: (u32, EntryId), to: u32) -> Option<EntryId> {
        let (mut s, mut id) = head;
        while s > to {
            id = self.entries.get(&id)?.prev;
            s -= 1;
        }
        let _ = m;
        Some(id)
    }

    /// Evict `m` once every remaining member has voted (a strict majority of the epoch). The cut is
    /// the longest common prefix of the voters' frozen heads, so it contains every entry of m that
    /// could have been stable anywhere; m stops counting from the frame after its last valid
    /// horizon (virtual synchrony's flush, here decided from chain data alone).
    fn maybe_evict(&mut self, m: MemberId) {
        if self.evicted.contains_key(&m) || !self.members.contains(&m) {
            return;
        }
        let remaining: Vec<MemberId> = self.members.iter().copied().filter(|&j| j != m).collect();
        let Some(votes) = self.votes.get(&m).cloned() else { return };
        if remaining.len() * 2 <= self.members.len() || !remaining.iter().all(|j| votes.contains_key(j)) {
            return;
        }
        let mut cut = remaining.iter().map(|j| votes[j].0).min().unwrap_or(0);
        while cut > 0 {
            let ids: Vec<Option<EntryId>> = remaining.iter().map(|j| self.walk_back(m, votes[j], cut)).collect();
            if ids.iter().any(|x| x.is_none()) {
                for (j, x) in remaining.iter().zip(&ids) {
                    if x.is_none() {
                        self.want_from(*j, m, cut, votes[j].0);
                    }
                }
                return;
            }
            if ids.windows(2).all(|w| w[0] == w[1]) {
                break;
            }
            cut -= 1;
        }
        let v = self.ack_hist.get(&m).and_then(|a| a.range(..=cut).next_back()).map_or(0, |(_, h)| h + 1);
        self.evicted.insert(m, (cut, v));
        self.members.remove(&m);
        self.epoch += 1;
        self.ev(format!("member {m} evicted (epoch {}): its chain is cut at seq {cut}; it no longer counts from frame {v}", self.epoch));
        self.reconcile(m);
        self.recompute_conf();
    }

    fn want_from(&mut self, holder: MemberId, author: MemberId, from: u32, to: u32) {
        if holder != self.me && from <= to {
            self.wants.push((holder, author, from, to));
        }
    }

    fn want_all(&mut self, author: MemberId, from: u32, to: u32) {
        let known: Vec<MemberId> = self.known.iter().copied().collect();
        for j in known {
            self.want_from(j, author, from, to);
        }
    }

    pub fn on_fetch(&self, author: MemberId, from: u32, to: u32) -> Vec<Msg> {
        (from..=to.min(from + 64))
            .filter_map(|s| self.by_author.get(&(author, s)).and_then(|id| self.entries.get(id)))
            .map(|e| Msg::Entry(e.clone()))
            .collect()
    }

    /// Fetch requests from gap and conflict detection: (who to ask, request), rate-limited.
    pub fn take_fetches(&mut self, now_ms: u64) -> Vec<(MemberId, Msg)> {
        let mut out = Vec::new();
        for (to, author, from, upto) in std::mem::take(&mut self.wants) {
            let last = self.fetch_asked.get(&(to, author)).copied();
            if last.is_some_and(|(f, t)| f == from && now_ms < t + 60) {
                continue;
            }
            self.fetch_asked.insert((to, author), (from, now_ms));
            out.push((to, Msg::Fetch { author, from, to: upto }));
        }
        out
    }

    /// Compare check hashes only for confirmed frames this member has finished simulating with the
    /// complete log (viz-1: never while a re-simulation is pending).
    fn evaluate_checks(&mut self) {
        if self.dirty_from.is_some() {
            return;
        }
        let fin = self.conf.min(self.frame);
        let frames: Vec<u64> = self.remote_checks.range(..fin).map(|(f, _)| *f).collect();
        for f in frames {
            let Some(&mine) = self.check_hashes.get(&f) else {
                self.remote_checks.remove(&f);
                continue;
            };
            let theirs = self.remote_checks.remove(&f).unwrap_or_default();
            for (m, h) in theirs {
                if !self.evaluated.insert((f, m)) {
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
        }
    }

    // ---- simulation ------------------------------------------------------------------------------

    /// Sorted (entry id, entry) for frame `f`: soonest-wins order with the per-frame tie rank.
    fn frame_entries(&self, f: u64) -> Vec<(EntryId, Entry)> {
        let mut v: Vec<(EntryId, Entry)> = self
            .at_frame
            .get(&f)
            .map(|s| s.iter().filter(|id| !self.is_void(**id)).map(|id| (*id, self.entries[id].clone())).collect())
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
                if *active {
                    self.t.hand_idle.insert(*obj, 0);
                } else {
                    self.t.hand_idle.remove(obj);
                }
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
            // Protocol entries never reach the sim (set_eff schedules only perturbations).
            Payload::Ack { .. } | Payload::Vote { .. } => Outcome::Accepted,
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
        if record && self.wants_keyframe(f) {
            let k = self.save();
            self.ring.insert(f, k);
            self.stats.max_ring_bytes = self.stats.max_ring_bytes.max(self.ring_bytes());
        }
        let mut hands_set = BTreeSet::new();
        let mut carry = Vec::new();
        for (id, e) in self.frame_entries(f) {
            let o = self.apply(id, &e, f, &mut hands_set, &mut carry);
            if record {
                self.outcomes.insert(id, (f, o));
            }
        }
        // Hands without a target this frame hold still, or, with `hand_predict`, an active hand
        // keeps its last commanded velocity for up to that many frames (dead reckoning). Both are
        // deterministic rules of the sim: a speculative frame that lacks the author's entry
        // predicts with them, and the entry's arrival replaces the prediction by rollback.
        let hands: Vec<(u32, Obj)> =
            self.t.objs.iter().enumerate().filter_map(|(i, o)| o.filter(|o| o.kind == 4).map(|o| (i as u32, o))).collect();
        for (i, o) in hands {
            if hands_set.contains(&i) {
                continue;
            }
            let coast = match self.t.hand_idle.get_mut(&i) {
                Some(n) if self.cfg.hand_predict > 0 => {
                    *n += 1;
                    *n <= self.cfg.hand_predict
                }
                _ => false,
            };
            if !coast {
                body_set_linear_velocity(&mut self.world, o.body, Vec3 { x: 0.0, y: 0.0, z: 0.0 });
            } else {
                // Never coast faster than a hand moves: a jump (a teleport-like target) would
                // otherwise be extrapolated as a 40 m/s flight.
                let v = body_get_linear_velocity(&self.world, o.body);
                let sp = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
                if sp > HAND_COAST_VMAX {
                    let k = HAND_COAST_VMAX / sp;
                    body_set_linear_velocity(&mut self.world, o.body, Vec3 { x: v.x * k, y: v.y * k, z: v.z * k });
                }
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
            if self.track {
                let p: Vec<[f32; 3]> = self
                    .t
                    .objs
                    .iter()
                    .map(|o| o.map_or([f32::NAN; 3], |o| {
                        let x = body_get_transform(&self.world, o.body).p;
                        [x.x as f32, x.y as f32, x.z as f32]
                    }))
                    .collect();
                self.pos_hist.insert(f, p);
            }
        }
    }

    /// Roll back if needed, then simulate forward to `target` (exclusive) unless the speculation
    /// limit is reached. Returns the number of new frames stepped.
    pub fn advance(&mut self, target: u64) -> u64 {
        let t_adv = now_us();
        let n = self.advance_inner(target);
        let us = now_us() - t_adv;
        self.stats.advance_us += us;
        self.stats.max_advance_us = self.stats.max_advance_us.max(us);
        n
    }

    fn advance_inner(&mut self, target: u64) -> u64 {
        if let Some(from) = self.dirty_from.take() {
            let now = self.frame;
            self.resim_to = now;
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
            self.evaluate_checks();
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
        let base = match self.ring.range(..=fin).next_back() {
            Some((&kf, _)) => kf,
            None => return,
        };
        let now = self.frame;
        let drop: Vec<u64> = self
            .ring
            .keys()
            .copied()
            .filter(|&f| {
                f < base
                    || (f > base
                        && match self.cfg.snap {
                            SnapPolicy::Every(k) => f % k != 0,
                            SnapPolicy::Log(k) => f % log_grid(k, now - f) != 0,
                            // Keep only the first candidate after the confirmed frame.
                            SnapPolicy::Base(_) => self.ring.range(base + 1..f).next().is_some(),
                            SnapPolicy::Window(k) => f % k != 0 || now - f >= 4 * k,
                        })
            })
            .collect();
        for f in drop {
            self.ring.remove(&f);
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

    pub fn void_ids(&self) -> BTreeSet<EntryId> {
        self.violations.union(&self.disputed).copied().collect()
    }

    pub fn members_evicted(&self) -> &BTreeMap<MemberId, (u32, u64)> {
        &self.evicted
    }
}

// ---- offline reference ---------------------------------------------------------------------------

/// Re-simulate a world from scratch with a complete, final log and effective frames: the
/// single-world reference used to check that sharing a process with other worlds, rollbacks and
/// throttling changed nothing.
pub fn replay(world_id: u32, scene: SceneKind, cfg: SimConfig, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> BTreeMap<u64, u64> {
    replay_sim(world_id, scene, cfg, members, log, frames).hashes
}

pub fn replay_sim(world_id: u32, scene: SceneKind, cfg: SimConfig, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> WorldSim {
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
    s
}

/// The log this member would hand to [`replay`]: every non-void entry with its effective frame.
pub fn final_log(s: &WorldSim) -> Vec<(Entry, u64)> {
    s.eff.iter().map(|(id, f)| (s.entries[id].clone(), *f)).collect()
}

pub fn digest(h: &BTreeMap<u64, u64>, frames: u64) -> u64 {
    fnv_u64s((0..frames).map(|f| h.get(&f).copied().unwrap_or(0)))
}

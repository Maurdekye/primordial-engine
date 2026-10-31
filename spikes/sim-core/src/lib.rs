//! Shared deterministic-simulation harness for the Primordial Engine phase-1 spikes.
//!
//! * [`GameState`]: a Box3D (box3d-rust) world plus the engine-side claim table that
//!   implements "soonest interaction wins" (requirement 5).
//! * [`Snapshot`]: full rollback snapshot (world image + engine-side state).
//! * [`RollbackSim`]: fixed-step simulation with an input history and a snapshot ring that
//!   re-simulates from the earliest frame touched by a late interaction (requirement 3).
//!
//! Frame convention: `snapshot(f)` is the state at the *start* of frame `f`, before frame `f`'s
//! interactions are applied. `hashes[f]` is the hash of the state *after* frame `f` was stepped.

pub mod ownership;
pub mod provenance;
pub mod session;

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use box3d_rust::body::{body_apply_linear_impulse_to_center, body_is_valid, create_body};
use box3d_rust::geometry::{Capsule, Sphere};
use box3d_rust::hull::make_box_hull;
use box3d_rust::id::BodyId;
use box3d_rust::math_functions::{Pos, Vec3};
use box3d_rust::recording::{
    deserialize_into_shell, hash_world_state, serialize_world, GeometryRegistry, RecBuffer,
    RegistrySlot,
};
use box3d_rust::recycle_scale::ContactRecycleScaling;
use box3d_rust::shape::{create_capsule_shape, create_hull_shape, create_sphere_shape};
use box3d_rust::types::{default_body_def, default_shape_def, default_world_def, BodyType, WorldDef};
use box3d_rust::world::World;

/// Fixed simulation timestep (requirement 2). 60 Hz.
pub const DT: f32 = 1.0 / 60.0;
/// Box3D sub-steps per frame.
pub const SUBSTEPS: i32 = 4;
/// How long an accepted interaction "claims" an object, in frames (0.5 s).
pub const CLAIM_FRAMES: u64 = 30;

/// A player's physics interaction, stamped with the sim frame it applies at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Interaction {
    pub frame: u64,
    pub peer: u32,
    /// Per-peer sequence number; (peer, seq) is the interaction's identity.
    pub seq: u32,
    /// Index into the scene's dynamic-body list (a stable, engine-level object id).
    pub body: u32,
    pub impulse: [f32; 3],
}

impl Interaction {
    pub fn key(&self) -> (u32, u32) {
        (self.peer, self.seq)
    }
}

/// Outcome of resolving one interaction against the claim table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Accepted,
    /// Rejected because `by_peer` holds the object until `until_frame`.
    RejectedClaimed { by_peer: u32, until_frame: u64 },
    RejectedUnknownBody,
}

/// SplitMix64, used for deterministic per-frame tie-breaking.
pub fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Total order for interactions: earlier frame first; within a frame, a seeded hash of
/// (frame, peer) so no peer is permanently favoured; then (peer, seq) to make it total.
pub fn order_key(session_seed: u64, i: &Interaction) -> (u64, u64, u32, u32) {
    let tie = splitmix64(
        session_seed ^ i.frame.wrapping_mul(0x100_0000_01B3) ^ ((i.peer as u64) << 40),
    );
    (i.frame, tie, i.peer, i.seq)
}

pub struct Scene {
    pub world: World,
    /// Engine object id -> Box3D body id. Created in a fixed order, so identical on every peer.
    pub bodies: Vec<BodyId>,
}

/// The Primordial Engine world definition: Box3D defaults plus the fork's scale-dependent
/// contact recycling ("van der Waals simulation"), on by default with the HarmonicMean combine
/// (user ruling 2026-10-30). The fork crate's own `default_world_def` stays upstream.
pub fn engine_world_def() -> WorldDef {
    let mut def = default_world_def();
    def.contact_recycle_scaling = ContactRecycleScaling::enabled();
    def
}

/// Ground plus a mixed pile of boxes, spheres and capsules: `side * side * layers` bodies,
/// in an [`engine_world_def`] world.
pub fn build_pile_scene(side: usize, layers: usize) -> Scene {
    build_pile_scene_def(side, layers, &engine_world_def())
}

/// [`build_pile_scene`] with a caller-supplied world definition (e.g. upstream Box3D's
/// `default_world_def`, for comparisons).
pub fn build_pile_scene_def(side: usize, layers: usize, world_def: &WorldDef) -> Scene {
    let mut world = World::new(world_def);

    let mut ground_def = default_body_def();
    ground_def.type_ = BodyType::Static;
    ground_def.position = Pos { x: 0.0, y: -1.0, z: 0.0 };
    let ground = create_body(&mut world, &ground_def);
    let ground_hull = make_box_hull(40.0, 1.0, 40.0);
    create_hull_shape(&mut world, ground, &default_shape_def(), &ground_hull.base);

    let box_hull = make_box_hull(0.4, 0.4, 0.4);
    let mut shape_def = default_shape_def();
    shape_def.density = 1.0;

    let mut bodies = Vec::with_capacity(side * side * layers);
    for layer in 0..layers {
        for ix in 0..side {
            for iz in 0..side {
                let n = bodies.len();
                let mut def = default_body_def();
                def.type_ = BodyType::Dynamic;
                // Small deterministic jitter so the pile topples instead of stacking perfectly.
                let jitter = (n as f32 * 0.618_034).fract() * 0.2 - 0.1;
                def.position = Pos {
                    x: (ix as f32 - side as f32 * 0.5) * 1.1 + jitter,
                    y: 0.5 + layer as f32 * 1.2,
                    z: (iz as f32 - side as f32 * 0.5) * 1.1 - jitter,
                };
                let body = create_body(&mut world, &def);
                match n % 3 {
                    0 => {
                        create_hull_shape(&mut world, body, &shape_def, &box_hull.base);
                    }
                    1 => {
                        let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: 0.45 };
                        create_sphere_shape(&mut world, body, &shape_def, &s);
                    }
                    _ => {
                        let c = Capsule {
                            center1: Vec3 { x: -0.3, y: 0.0, z: 0.0 },
                            center2: Vec3 { x: 0.3, y: 0.0, z: 0.0 },
                            radius: 0.25,
                        };
                        create_capsule_shape(&mut world, body, &shape_def, &c);
                    }
                }
                bodies.push(body);
            }
        }
    }
    Scene { world, bodies }
}

/// Everything that must roll back together: physics world + engine-side claim table.
pub struct GameState {
    pub scene: Scene,
    pub frame: u64,
    /// object id -> (claiming peer, claim expires at this frame)
    pub claims: BTreeMap<u32, (u32, u64)>,
    pub session_seed: u64,
}

/// A full rollback snapshot.
#[derive(Clone)]
pub struct Snapshot {
    pub frame: u64,
    pub world_image: Vec<u8>,
    pub claims: BTreeMap<u32, (u32, u64)>,
}

pub struct FrameResult {
    pub verdicts: Vec<(Interaction, Verdict)>,
}

impl GameState {
    pub fn new(scene: Scene, session_seed: u64) -> Self {
        GameState { scene, frame: 0, claims: BTreeMap::new(), session_seed }
    }

    /// Apply this frame's interactions ("soonest wins" + claims), then step one fixed timestep.
    pub fn advance(&mut self, inputs: &[Interaction]) -> FrameResult {
        let mut sorted: Vec<Interaction> =
            inputs.iter().copied().filter(|i| i.frame == self.frame).collect();
        sorted.sort_by_key(|i| order_key(self.session_seed, i));
        let mut verdicts = Vec::with_capacity(sorted.len());
        for i in sorted {
            let v = self.resolve(&i);
            if v == Verdict::Accepted {
                let id = self.scene.bodies[i.body as usize];
                let imp = Vec3 { x: i.impulse[0], y: i.impulse[1], z: i.impulse[2] };
                body_apply_linear_impulse_to_center(&mut self.scene.world, id, imp, true);
                self.claims.insert(i.body, (i.peer, self.frame + CLAIM_FRAMES));
            }
            verdicts.push((i, v));
        }
        // Expire claims deterministically.
        let next = self.frame + 1;
        self.claims.retain(|_, (_, until)| *until > next);
        self.scene.world.step(DT, SUBSTEPS);
        self.frame += 1;
        FrameResult { verdicts }
    }

    fn resolve(&self, i: &Interaction) -> Verdict {
        if i.body as usize >= self.scene.bodies.len() {
            return Verdict::RejectedUnknownBody;
        }
        match self.claims.get(&i.body) {
            Some(&(owner, until)) if owner != i.peer && until > self.frame => {
                Verdict::RejectedClaimed { by_peer: owner, until_frame: until }
            }
            _ => Verdict::Accepted,
        }
    }

    /// Hash of all rollback state: Box3D body transforms + velocities, claims, frame.
    pub fn hash(&self) -> u64 {
        let mut h = hash_world_state(&self.scene.world);
        let mut mix = |v: u64| h = (h ^ v).wrapping_mul(0x100_0000_01B3);
        mix(self.frame);
        for (&body, &(peer, until)) in &self.claims {
            mix(body as u64);
            mix(peer as u64);
            mix(until);
        }
        h
    }

    /// Box3D-only hash (body transforms + velocities), as computed by the C reference.
    pub fn world_hash(&self) -> u64 {
        hash_world_state(&self.scene.world)
    }

    pub fn all_bodies_valid(&self) -> bool {
        self.scene.bodies.iter().all(|&b| body_is_valid(&self.scene.world, b))
    }
}

/// Serializes/restores the Box3D world. Geometry is interned once in a registry and shared by
/// every snapshot of the session (shapes do not change in these spikes).
pub struct Snapshotter {
    registry: GeometryRegistry,
    slots: Vec<RegistrySlot>,
    slot_count: usize,
}

impl Default for Snapshotter {
    fn default() -> Self {
        Self::new()
    }
}

impl Snapshotter {
    pub fn new() -> Self {
        Snapshotter { registry: GeometryRegistry::new(), slots: Vec::new(), slot_count: 0 }
    }

    pub fn save(&mut self, state: &GameState) -> Snapshot {
        let mut buf = RecBuffer::new();
        serialize_world(&state.scene.world, &mut buf, &mut self.registry);
        if self.registry.entries.len() != self.slot_count {
            self.slots = self.registry.to_slots();
            self.slot_count = self.registry.entries.len();
        }
        Snapshot { frame: state.frame, world_image: buf.data, claims: state.claims.clone() }
    }

    /// Restore in place: the same `World` value is overwritten, so engine-held `BodyId`s stay valid.
    pub fn restore(&mut self, state: &mut GameState, snap: &Snapshot) -> bool {
        let ok = deserialize_into_shell(&snap.world_image, &mut state.scene.world, &mut self.slots);
        state.frame = snap.frame;
        state.claims = snap.claims.clone();
        ok
    }

    /// Restore into a brand-new world (e.g. a late-joining peer receiving a snapshot).
    pub fn restore_fresh(
        &mut self,
        snap: &Snapshot,
        bodies: Vec<BodyId>,
        seed: u64,
    ) -> Option<GameState> {
        // The image carries the world config, recycling parameters included.
        let mut world = World::new(&engine_world_def());
        if !deserialize_into_shell(&snap.world_image, &mut world, &mut self.slots) {
            return None;
        }
        Some(GameState {
            scene: Scene { world, bodies },
            frame: snap.frame,
            claims: snap.claims.clone(),
            session_seed: seed,
        })
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct RollbackStats {
    pub rollbacks: u64,
    pub resimulated_frames: u64,
    pub max_depth: u64,
    pub too_late: u64,
}

/// Fixed-step simulation with input history + snapshot ring (requirement 3).
pub struct RollbackSim {
    pub state: GameState,
    pub snap: Snapshotter,
    inputs: BTreeMap<u64, Vec<Interaction>>,
    seen: BTreeSet<(u32, u32)>,
    ring: VecDeque<Snapshot>,
    /// hash after stepping frame f
    pub hashes: BTreeMap<u64, u64>,
    pub max_rollback: u64,
    dirty_from: Option<u64>,
    pub stats: RollbackStats,
    /// Latest verdicts per frame (for logging / sync).
    pub verdicts: BTreeMap<u64, Vec<(Interaction, Verdict)>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InputError {
    Duplicate,
    /// Older than the rollback window: must be handled by a resync, not a rollback.
    TooLate { frame: u64, oldest: u64 },
}

impl RollbackSim {
    pub fn new(state: GameState, max_rollback: u64) -> Self {
        RollbackSim {
            state,
            snap: Snapshotter::new(),
            inputs: BTreeMap::new(),
            seen: BTreeSet::new(),
            ring: VecDeque::new(),
            hashes: BTreeMap::new(),
            max_rollback,
            dirty_from: None,
            stats: RollbackStats::default(),
            verdicts: BTreeMap::new(),
        }
    }

    pub fn frame(&self) -> u64 {
        self.state.frame
    }

    pub fn oldest_restorable(&self) -> u64 {
        self.ring.front().map(|s| s.frame).unwrap_or(self.state.frame)
    }

    /// Every interaction known for frames `from..=to`.
    pub fn inputs_in(&self, from: u64, to: u64) -> Vec<Interaction> {
        self.inputs.range(from..=to).flat_map(|(_, v)| v.iter().copied()).collect()
    }

    /// Order-independent digest of the interactions known for one frame. Two peers whose
    /// state hashes differ at frame f but whose input digests also differ there are missing
    /// each other's interactions (fixable by exchanging inputs + rollback); equal digests with
    /// different state hashes mean true nondeterminism (needs a state transfer).
    pub fn input_digest(&self, frame: u64) -> u64 {
        let mut v: Vec<Interaction> = self.inputs.get(&frame).cloned().unwrap_or_default();
        v.sort_by_key(|i| (i.peer, i.seq));
        fnv_u64s(v.iter().flat_map(|i| {
            [
                i.frame,
                ((i.peer as u64) << 32) | i.seq as u64,
                i.body as u64,
                ((i.impulse[0].to_bits() as u64) << 32) | i.impulse[1].to_bits() as u64,
                i.impulse[2].to_bits() as u64,
            ]
        }))
    }

    /// Record an interaction. Returns Ok(true) if it is for a frame already simulated
    /// (a rollback is scheduled for the next `advance_to`).
    pub fn add_input(&mut self, i: Interaction) -> Result<bool, InputError> {
        if self.seen.contains(&i.key()) {
            return Err(InputError::Duplicate);
        }
        if i.frame < self.state.frame && i.frame < self.oldest_restorable() {
            self.stats.too_late += 1;
            return Err(InputError::TooLate { frame: i.frame, oldest: self.oldest_restorable() });
        }
        self.seen.insert(i.key());
        self.inputs.entry(i.frame).or_default().push(i);
        let late = i.frame < self.state.frame;
        if late {
            self.dirty_from = Some(self.dirty_from.map_or(i.frame, |d| d.min(i.frame)));
        }
        Ok(late)
    }

    fn step_one(&mut self) {
        let f = self.state.frame;
        let snap = self.snap.save(&self.state);
        self.ring.push_back(snap);
        while self.ring.len() as u64 > self.max_rollback {
            self.ring.pop_front();
        }
        let r = match self.inputs.get(&f) {
            Some(inputs) => self.state.advance(inputs),
            None => self.state.advance(&[]),
        };
        if !r.verdicts.is_empty() {
            self.verdicts.insert(f, r.verdicts);
        } else {
            self.verdicts.remove(&f);
        }
        self.hashes.insert(f, self.state.hash());
    }

    /// Apply any pending rollback, then simulate forward until `target` frames have been stepped.
    pub fn advance_to(&mut self, target: u64) {
        if let Some(from) = self.dirty_from.take() {
            let now = self.state.frame;
            let pos = self.ring.iter().position(|s| s.frame == from).expect("snapshot in ring");
            let snap = self.ring[pos].clone();
            self.ring.truncate(pos);
            assert!(self.snap.restore(&mut self.state, &snap), "snapshot restore failed");
            self.stats.rollbacks += 1;
            self.stats.max_depth = self.stats.max_depth.max(now - from);
            while self.state.frame < now {
                self.step_one();
                self.stats.resimulated_frames += 1;
            }
        }
        while self.state.frame < target {
            self.step_one();
        }
    }
}

/// The scripted interaction schedule used by the spikes: two peers, including a same-frame
/// conflict on one object and a later attempt on a claimed object.
pub fn scripted_inputs() -> Vec<Interaction> {
    let mk = |frame, peer, seq, body, impulse| Interaction { frame, peer, seq, body, impulse };
    vec![
        mk(60, 1, 0, 10, [0.0, 6.0, 0.0]),
        mk(90, 2, 0, 33, [3.0, 2.0, 0.0]),
        // Same frame, same object, two peers: exactly one wins on every peer.
        mk(120, 1, 1, 50, [0.0, 8.0, 2.0]),
        mk(120, 2, 1, 50, [-4.0, 8.0, 0.0]),
        // Within the claim window: rejected unless issued by the claim holder.
        mk(125, 2, 2, 50, [0.0, 9.0, 0.0]),
        mk(126, 1, 2, 50, [0.0, 3.0, 0.0]),
        mk(200, 2, 3, 77, [5.0, 5.0, 5.0]),
        mk(260, 1, 3, 120, [-6.0, 4.0, 1.0]),
        mk(330, 2, 4, 5, [0.0, 12.0, 0.0]),
        mk(400, 1, 4, 140, [2.0, 7.0, -3.0]),
        mk(470, 2, 5, 99, [-2.0, 6.0, 4.0]),
    ]
}

/// FNV-1a over a list of u64 values.
pub fn fnv_u64s(values: impl IntoIterator<Item = u64>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in values {
        for b in v.to_le_bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01B3);
        }
    }
    h
}

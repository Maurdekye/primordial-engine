//! The phase-1 spike's scene, interaction rules and rollback, rebuilt on pe-physics. Each piece
//! mirrors the spike's sim-core exactly (archived; at the tag archive/v0-prototype), so the committed
//! records in `tests/records/vdw/` must reproduce bit for bit.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

use pe_physics::{BodyDesc, BodyKind, Geometry, ObjectId, PhysicsConfig, PhysicsWorld, Push, ShapeDesc, WorldImage};

pub const DT: f32 = 1.0 / 60.0;
pub const CLAIM_FRAMES: u64 = 30;
pub const SEED: u64 = 0x5052_494D_4F52_4449; // "PRIMORDI"

/// The engine object id of the pile's n-th dynamic body (the ground is counter 1).
pub fn pile_object(n: usize) -> ObjectId {
    ObjectId::new(0, 2 + n as u32)
}

/// Ground plus `side * side * layers` mixed bodies, created in the spike's order with the
/// spike's shapes (`pe_sim_core::build_pile_scene_def`).
pub fn pile_world(config: PhysicsConfig, side: usize, layers: usize) -> (PhysicsWorld, Vec<ObjectId>) {
    let mut pw = PhysicsWorld::new(config).unwrap();
    let ground = BodyDesc::new(BodyKind::Static, [0.0, -1.0, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [40.0, 1.0, 40.0] }));
    pw.add_body(ObjectId::new(0, 1), &ground).unwrap();

    let mut objects = Vec::with_capacity(side * side * layers);
    for layer in 0..layers {
        for ix in 0..side {
            for iz in 0..side {
                let n = objects.len();
                let jitter = (n as f32 * 0.618_034).fract() * 0.2 - 0.1;
                // Computed in f32 like the spike, then widened: the same bits reach Box3D.
                let position = [
                    (ix as f32 - side as f32 * 0.5) * 1.1 + jitter,
                    0.5 + layer as f32 * 1.2,
                    (iz as f32 - side as f32 * 0.5) * 1.1 - jitter,
                ]
                .map(f64::from);
                let geometry = match n % 3 {
                    0 => Geometry::Box { half_extents: [0.4, 0.4, 0.4] },
                    1 => Geometry::Sphere { center: [0.0; 3], radius: 0.45 },
                    _ => Geometry::Capsule { center1: [-0.3, 0.0, 0.0], center2: [0.3, 0.0, 0.0], radius: 0.25 },
                };
                let desc = BodyDesc::new(BodyKind::Dynamic, position)
                    .with_shape(ShapeDesc::new(geometry).with_density(1.0));
                let id = pile_object(n);
                pw.add_body(id, &desc).unwrap();
                objects.push(id);
            }
        }
    }
    (pw, objects)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Interaction {
    pub frame: u64,
    pub peer: u32,
    pub seq: u32,
    pub body: u32,
    pub impulse: [f32; 3],
}

impl Interaction {
    fn key(&self) -> (u32, u32) {
        (self.peer, self.seq)
    }
}

pub fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn order_key(session_seed: u64, i: &Interaction) -> (u64, u64, u32, u32) {
    let tie = splitmix64(session_seed ^ i.frame.wrapping_mul(0x100_0000_01B3) ^ ((i.peer as u64) << 40));
    (i.frame, tie, i.peer, i.seq)
}

pub fn scripted_inputs() -> Vec<Interaction> {
    let mk = |frame, peer, seq, body, impulse| Interaction { frame, peer, seq, body, impulse };
    vec![
        mk(60, 1, 0, 10, [0.0, 6.0, 0.0]),
        mk(90, 2, 0, 33, [3.0, 2.0, 0.0]),
        mk(120, 1, 1, 50, [0.0, 8.0, 2.0]),
        mk(120, 2, 1, 50, [-4.0, 8.0, 0.0]),
        mk(125, 2, 2, 50, [0.0, 9.0, 0.0]),
        mk(126, 1, 2, 50, [0.0, 3.0, 0.0]),
        mk(200, 2, 3, 77, [5.0, 5.0, 5.0]),
        mk(260, 1, 3, 120, [-6.0, 4.0, 1.0]),
        mk(330, 2, 4, 5, [0.0, 12.0, 0.0]),
        mk(400, 1, 4, 140, [2.0, 7.0, -3.0]),
        mk(470, 2, 5, 99, [-2.0, 6.0, 4.0]),
    ]
}

pub fn fnv_u64s(values: impl IntoIterator<Item = u64>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in values {
        for b in v.to_le_bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100_0000_01B3);
        }
    }
    h
}

pub fn fnv_bytes(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h = (h ^ x as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// The spike's `GameState`: a physics world plus the "soonest wins" claim table.
pub struct Game {
    pub pw: PhysicsWorld,
    pub objects: Vec<ObjectId>,
    pub frame: u64,
    pub claims: BTreeMap<u32, (u32, u64)>,
    pub seed: u64,
}

#[derive(Clone)]
pub struct Snapshot {
    pub frame: u64,
    pub image: WorldImage,
    pub claims: BTreeMap<u32, (u32, u64)>,
}

impl Game {
    pub fn pile(config: PhysicsConfig) -> Game {
        let (pw, objects) = pile_world(config, 8, 4);
        Game { pw, objects, frame: 0, claims: BTreeMap::new(), seed: SEED }
    }

    pub fn advance(&mut self, inputs: &[Interaction]) {
        let mut sorted: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == self.frame).collect();
        sorted.sort_by_key(|i| order_key(self.seed, i));
        for i in sorted {
            if i.body as usize >= self.objects.len() {
                continue;
            }
            let claimed = matches!(self.claims.get(&i.body), Some(&(owner, until)) if owner != i.peer && until > self.frame);
            if !claimed {
                self.pw.push(self.objects[i.body as usize], Push::Impulse(i.impulse)).unwrap();
                self.claims.insert(i.body, (i.peer, self.frame + CLAIM_FRAMES));
            }
        }
        let next = self.frame + 1;
        self.claims.retain(|_, (_, until)| *until > next);
        self.pw.step(DT).unwrap();
        self.frame += 1;
    }

    /// The spike's state hash: Box3D's hash, then the frame and claims mixed in.
    pub fn hash(&self) -> u64 {
        let mut h = self.pw.box3d_hash().unwrap();
        let mut mix = |v: u64| h = (h ^ v).wrapping_mul(0x100_0000_01B3);
        mix(self.frame);
        for (&body, &(peer, until)) in &self.claims {
            mix(body as u64);
            mix(peer as u64);
            mix(until);
        }
        h
    }

    pub fn save(&mut self) -> Snapshot {
        Snapshot { frame: self.frame, image: self.pw.save_image().unwrap(), claims: self.claims.clone() }
    }

    pub fn restore(&mut self, s: &Snapshot) {
        self.pw.restore_image(&s.image).expect("restore");
        self.frame = s.frame;
        self.claims = s.claims.clone();
    }

    pub fn step_scripted(&mut self, inputs: &[Interaction], end: u64) -> Vec<u64> {
        let mut hashes = Vec::new();
        while self.frame < end {
            let f = self.frame;
            let now: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == f).collect();
            self.advance(&now);
            hashes.push(self.hash());
        }
        hashes
    }
}

/// The spike's reference run: (state hashes, Box3D hashes) after each frame.
pub fn reference_run(config: PhysicsConfig, frames: u64) -> (Vec<u64>, Vec<u64>) {
    let inputs = scripted_inputs();
    let mut g = Game::pile(config);
    let mut state = Vec::new();
    let mut world = Vec::new();
    while g.frame < frames {
        let f = g.frame;
        let now: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == f).collect();
        g.advance(&now);
        state.push(g.hash());
        world.push(g.pw.box3d_hash().unwrap());
    }
    (state, world)
}

#[derive(Debug, PartialEq, Eq)]
pub enum InputError {
    Duplicate,
    TooLate,
}

/// The spike's `RollbackSim`: input history plus a snapshot ring.
pub struct RollbackSim {
    pub game: Game,
    inputs: BTreeMap<u64, Vec<Interaction>>,
    seen: BTreeSet<(u32, u32)>,
    ring: VecDeque<Snapshot>,
    pub hashes: BTreeMap<u64, u64>,
    max_rollback: u64,
    dirty_from: Option<u64>,
    pub rollbacks: u64,
    pub resimulated: u64,
}

impl RollbackSim {
    pub fn new(game: Game, max_rollback: u64) -> RollbackSim {
        RollbackSim {
            game,
            inputs: BTreeMap::new(),
            seen: BTreeSet::new(),
            ring: VecDeque::new(),
            hashes: BTreeMap::new(),
            max_rollback,
            dirty_from: None,
            rollbacks: 0,
            resimulated: 0,
        }
    }

    fn oldest(&self) -> u64 {
        self.ring.front().map(|s| s.frame).unwrap_or(self.game.frame)
    }

    pub fn add_input(&mut self, i: Interaction) -> Result<bool, InputError> {
        if self.seen.contains(&i.key()) {
            return Err(InputError::Duplicate);
        }
        if i.frame < self.game.frame && i.frame < self.oldest() {
            return Err(InputError::TooLate);
        }
        self.seen.insert(i.key());
        self.inputs.entry(i.frame).or_default().push(i);
        let late = i.frame < self.game.frame;
        if late {
            self.dirty_from = Some(self.dirty_from.map_or(i.frame, |d| d.min(i.frame)));
        }
        Ok(late)
    }

    fn step_one(&mut self) {
        let f = self.game.frame;
        let snap = self.game.save();
        self.ring.push_back(snap);
        while self.ring.len() as u64 > self.max_rollback {
            self.ring.pop_front();
        }
        let inputs = self.inputs.get(&f).cloned().unwrap_or_default();
        self.game.advance(&inputs);
        self.hashes.insert(f, self.game.hash());
    }

    pub fn advance_to(&mut self, target: u64) {
        if let Some(from) = self.dirty_from.take() {
            let now = self.game.frame;
            let pos = self.ring.iter().position(|s| s.frame == from).expect("snapshot in ring");
            let snap = self.ring[pos].clone();
            self.ring.truncate(pos);
            self.game.restore(&snap);
            self.rollbacks += 1;
            while self.game.frame < now {
                self.step_one();
                self.resimulated += 1;
            }
        }
        while self.game.frame < target {
            self.step_one();
        }
    }
}

/// `crates/pe-physics/tests/records/vdw/<name>` in the repository: under `$PE_RECORDS` if set, else
/// next to this crate's source if that exists (a host build), else relative to the working directory
/// (a cross-built binary run from the repository root, e.g. the Linux run in WSL).
pub fn record(name: &str) -> PathBuf {
    if let Some(dir) = std::env::var_os("PE_RECORDS") {
        return PathBuf::from(dir).join(name);
    }
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/records/vdw");
    if here.is_dir() { here.join(name) } else { PathBuf::from("crates/pe-physics/tests/records/vdw").join(name) }
}

/// "release" or "debug": the committed records differ only in the snapshot images, whose
/// header carries a debug-only validation flag.
pub fn profile() -> &'static str {
    if cfg!(debug_assertions) { "debug" } else { "release" }
}

/// Engine or upstream configuration, and the name used in the record files.
pub fn modes() -> [(PhysicsConfig, &'static str); 2] {
    [(PhysicsConfig::engine(), "vdw"), (PhysicsConfig::upstream(), "off")]
}

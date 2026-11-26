//! Players (@item:primordial-engine/locomotion-l1-pe-physics-the-first-player-mover-m): a member's body, walked over
//! the world once a frame by [`PhysicsWorld::move_player`] with the fork's mover kit (Box3D's character mover:
//! collide, solve the planes, cast).
//!
//! * **The body** is kinematic and stands on its feet: the body origin is the feet, and its capsule
//!   ([`PlayerParams::capsule`]) floats [`PlayerParams::ride`] above them. One downward probe, a sphere cast from
//!   inside the bottom of the capsule, finds the ground (the closest hit not too steep to stand on), so steps and bumps
//!   lower than the ride pass beneath the capsule. The body's shape touches nothing in the solver and no query sees
//!   it, and everything the body does to the world is done here, in the move.
//! * **Players are walls to each other** (the friend test of 11-27, superseding 3a's pass-through):
//!   after its slide, a move steps the capsule out of every other player's it overlaps ([`PhysicsWorld::apart`]),
//!   across the floor, then slides that step too, so a wall still holds. Each move sees the others where they stood at
//!   the frame's start. A player is never ground: one that lands on a head slides off it.
//! * **Solid and light** (user 5c; @coordinator-opus): static and kinematic bodies, and dynamic bodies of
//!   at least [`PlayerParams::heavy_mass`] (each body's own mass, read when the move runs), are walls and ground: the
//!   capsule slides on them and the probe stands on them. Lighter dynamic bodies are neither. They are shoved, each by
//!   one capped impulse a move, when they are inside the player's push volume (the capsule and the push capsule below
//!   it, [`PlayerParams::push_capsule`]), in object-id order. What the move is told to ignore (what the player holds,
//!   and every player's hands) is neither solid nor shoved. **Nor is a light body by the player's own hands**
//!   ([`MoverInput::hands`], [`PlayerParams::hand_clear`]): leaning down to pick something up carries the capsule
//!   forward under the head, and it would shove away what the hand reaches for (the user point 3).
//! * **Out of walls** (the user's 6, "your body is pushed out the wall"): the capsule never stays inside something
//!   solid. Box3D's mover gives no plane for a hull whose shape the capsule's core is inside, so the move adds one, the
//!   hull's face that needs the least push.
//! * **What a player wants** is one pure function, [`walk`], of the tuning, the player's state, the ground under it at
//!   the frame's start and the input. Only [`PhysicsWorld::move_player`] calls it, so a trinket that changes how a
//!   player moves wraps it.
//! * **Weight:** on a solid dynamic body the player presses down with its weight, [`PlayerParams::mass`] times its
//!   gravity, at the probe's point, for the step.
//! * **No platform carry** (@coordinator-opus): the ground's motion is not followed sideways, so a
//!   player slides off anything that moves underneath it. A light plank or stack is not ground, so a player sinks
//!   through it (a known limit).
//! * **Deterministic:** serial, in the fork's arithmetic and IEEE square roots only, with every choice among the
//!   world's shapes made by shape id or object id, never by the order a query meets them. Each player's state is in
//!   the image, so the check hash covers it through the image's bytes, and [`PhysicsWorld::state_hash`] hashes it
//!   directly.

use std::collections::{BTreeMap, BTreeSet};

use box3d_rust::body::{
    body_apply_force, body_apply_linear_impulse, body_get_linear_velocity, body_get_mass, body_get_transform,
    body_get_type, body_set_target_transform, create_body,
};
use box3d_rust::distance::make_proxy;
use box3d_rust::geometry::{Capsule, CollisionPlane, ShapeType};
use box3d_rust::hull::get_hull_planes;
use box3d_rust::id::{BodyId, ShapeId};
use box3d_rust::math_functions::{
    PI, Plane, Pos, QUAT_IDENTITY, VEC3_ZERO, Vec3, WorldTransform as BoxWorldTransform, add, clamp_float,
    compute_cos_sin, dot, inv_rotate_vector, length_squared, max_float, min_float, mul_sv, neg, offset_pos,
    rotate_vector, sub, sub_pos,
};
use box3d_rust::mover::{clip_vector, solve_planes};
use box3d_rust::shape::{create_capsule_shape, shape_get_body, shape_get_hull, shape_get_type};
use box3d_rust::types::{BodyType, QueryFilter, default_body_def, default_query_filter, default_shape_def};
use box3d_rust::world::{World, world_cast_mover, world_cast_shape, world_collide_mover, world_overlap_shape};
use pe_core::ids::ObjectId;

use crate::desc::limits::{MAX_SIZE, MAX_SPEED};
use crate::desc::position;
use crate::error::PhysicsError;
use crate::world::{PhysicsWorld, Pose, from_pos, pos};

/// The collision category of players' shapes. Their mask is empty, so they touch nothing and no query sees them.
pub const PLAYER_CATEGORY: u64 = 1 << 1;

/// The most contact planes a slide pass solves, after sorting.
const MAX_PLANES: usize = 16;
/// Collide, solve and cast at most this many times a move.
const SLIDE_PASSES: usize = 5;
/// A slide pass that would move less than this, m, ends the slide.
const SLIDE_TOLERANCE: f32 = 1.0e-4;

/// The tuning of the mover, lengths in metres, times in seconds. A constant of the physics version ([`PLAYER`]),
/// passed into every move so that a later world row can carry its own (the pattern of the swept-bullet budget).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlayerParams {
    /// The capsule's radius.
    pub radius: f32,
    /// The body's full height, from the feet to the top of the capsule.
    pub height: f32,
    /// How high the capsule floats above the feet: the tallest step that passes beneath it.
    pub ride: f32,
    /// How far below the feet the ground may be and still hold the player (walking down steps and slopes).
    pub snap: f32,
    /// The probe's radius, at most the capsule's.
    pub probe_radius: f32,
    /// How fast the ride lifts the body onto higher ground and lowers it onto lower ground, m/s.
    pub rise_speed: f32,
    pub drop_speed: f32,
    /// The fastest walk the input may ask for, m/s.
    pub max_walk: f32,
    /// How fast the walk velocity moves toward the one asked for, on the ground and in the air, m/s².
    pub ground_accel: f32,
    pub air_accel: f32,
    /// Gravity on a player in the air, m/s² (its own, so the feel doesn't depend on the world's), and the fastest fall.
    pub gravity: f32,
    pub max_fall: f32,
    /// The upward speed a jump starts with, m/s: a jump rises `jump_speed² / (2 gravity)`.
    pub jump_speed: f32,
    /// How long after leaving the ground a jump still works (coyote time), and how long a press waits for the ground.
    pub coyote_time: f32,
    pub buffer_time: f32,
    /// The steepest ground that holds a player, as the least upward part of its normal (the cosine of the slope).
    pub min_ground_normal: f32,
    /// The farthest the room-scale offset may carry the body in one frame.
    pub max_room_step: f32,
    /// A dynamic body of at least this mass, kg, is solid to players; a lighter one is shoved. World data.
    pub heavy_mass: f32,
    /// The player's mass, kg: its weight on a solid dynamic body underfoot.
    pub mass: f32,
    /// How far above the feet the push capsule starts, so that it never touches the ground the player stands on.
    pub push_floor: f32,
    /// The caps on one shove: the speed it may give a light body, m/s, and its impulse, N s.
    pub max_shove_speed: f32,
    pub max_shove_impulse: f32,
    /// A light body whose centre, or whose touch with the push volume, is within this of one of the player's own
    /// hands ([`MoverInput::hands`]) is not shoved, m.
    pub hand_clear: f32,
}

/// The first mover's tuning [I, to tune with the user]: a 1.7 m player with a 0.25 m radius, steps up to 0.3 m, a
/// 5 m/s walk at most, a 0.9 m jump (5.2 m/s against 15 m/s²), 0.1 s of coyote time and of press buffer, slopes up to
/// about 50°, bodies from 40 kg solid, a 75 kg player, shoves of at most 10 m/s and 200 N s, and none within 0.35 m
/// of the player's own hands.
pub const PLAYER: PlayerParams = PlayerParams {
    radius: 0.25,
    height: 1.7,
    ride: 0.3,
    snap: 0.3,
    probe_radius: 0.2,
    rise_speed: 4.0,
    drop_speed: 6.0,
    max_walk: 5.0,
    ground_accel: 40.0,
    air_accel: 10.0,
    gravity: 15.0,
    max_fall: 30.0,
    jump_speed: 5.2,
    coyote_time: 0.1,
    buffer_time: 0.1,
    min_ground_normal: 0.64,
    max_room_step: 0.25,
    heavy_mass: 40.0,
    mass: 75.0,
    push_floor: 0.02,
    max_shove_speed: 10.0,
    max_shove_impulse: 200.0,
    hand_clear: 0.35,
};

impl PlayerParams {
    /// Refuse a tuning the mover can't use: every value finite and not negative, the probe within the capsule, the
    /// capsule above the ride, the push capsule below it, and lengths, speeds and masses within pe-physics's limits.
    pub fn validate(&self) -> Result<(), PhysicsError> {
        let p = self;
        let lengths = [p.radius, p.height, p.ride, p.snap, p.probe_radius, p.max_room_step, p.push_floor, p.hand_clear];
        let rates = [
            p.rise_speed,
            p.drop_speed,
            p.max_walk,
            p.ground_accel,
            p.air_accel,
            p.gravity,
            p.max_fall,
            p.jump_speed,
            p.heavy_mass,
            p.mass,
            p.max_shove_speed,
            p.max_shove_impulse,
        ];
        let ok = lengths.iter().all(|v| v.is_finite() && (0.0..=MAX_SIZE).contains(v))
            && rates.iter().all(|v| v.is_finite() && (0.0..=MAX_SPEED).contains(v))
            && [p.coyote_time, p.buffer_time].iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v))
            && p.radius > 0.0
            && p.probe_radius > 0.0
            && p.probe_radius <= p.radius
            && p.height >= p.ride + 2.0 * p.radius
            && p.push_floor <= p.ride
            && p.min_ground_normal > 0.0
            && p.min_ground_normal <= 1.0;
        if ok { Ok(()) } else { Err(PhysicsError::InvalidValue("player tuning")) }
    }

    /// The mover's capsule, relative to the feet: from the ride to the top of the body.
    pub fn capsule(&self) -> Capsule {
        Capsule {
            center1: Vec3 { x: 0.0, y: self.ride + self.radius, z: 0.0 },
            center2: Vec3 { x: 0.0, y: self.height - self.radius, z: 0.0 },
            radius: self.radius,
        }
    }

    /// The push capsule, relative to the feet: from [`Self::push_floor`] up into the mover's capsule. It only finds
    /// light bodies to shove; the mover never collides it.
    pub fn push_capsule(&self) -> Capsule {
        Capsule {
            center1: Vec3 { x: 0.0, y: self.push_floor + self.radius, z: 0.0 },
            center2: Vec3 { x: 0.0, y: self.ride + self.radius, z: 0.0 },
            radius: self.radius,
        }
    }
}

/// One frame's input to a player's move.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct MoverInput {
    /// The walk velocity asked for, in the physics world's x and z, m/s ([`walk_axes`] turns a stick into it); at
    /// most [`PlayerParams::max_walk`] is used.
    pub walk: [f32; 2],
    /// The room-scale offset to follow this frame, x and z, m: how far the head moved over the floor. At most
    /// [`PlayerParams::max_room_step`] is used.
    pub room: [f32; 2],
    /// Whether jump was pressed this frame (a press, not a hold: a held or re-sent sample never presses twice).
    pub jump: bool,
    /// Where the player's own hands are, in the physics world as the feet are: no light body within
    /// [`PlayerParams::hand_clear`] of one is shoved. `None` for a hand the player hasn't got.
    pub hands: [Option<[f64; 3]>; 2],
}

impl MoverInput {
    fn validate(&self) -> Result<(), PhysicsError> {
        let ok = self.walk.iter().chain(&self.room).all(|v| v.is_finite() && v.abs() <= MAX_SPEED);
        if !ok {
            return Err(PhysicsError::InvalidValue("move input"));
        }
        for h in self.hands.iter().flatten() {
            position(h, "move input hand")?;
        }
        Ok(())
    }
}

/// `v`, as (right, forward) in a frame turned `yaw` about world +y, in world (x, z). Yaw 0 faces -z, and a positive
/// yaw turns toward -x (counterclockwise seen from above); `yaw` wraps, 2^32 being one turn. It uses the fork's
/// cosine and sine (`compute_cos_sin`), written for cross-platform determinism, never the standard library's.
pub fn walk_axes(v: [f32; 2], yaw: u32) -> [f32; 2] {
    // The angle in [-pi, pi): the yaw's bits as a signed turn fraction.
    let radians = (yaw as i32) as f32 * (PI / 2_147_483_648.0);
    let cs = compute_cos_sin(radians);
    // right = (cos, -sin), forward = (-sin, -cos).
    [v[0] * cs.cosine - v[1] * cs.sine, -v[0] * cs.sine - v[1] * cs.cosine]
}

/// A player's state between moves, carried in the image and both hashes.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct PlayerState {
    /// The player's own velocity, m/s: walking, falling and jumping. The room-scale offset and the ride are not in it.
    pub velocity: [f32; 3],
    /// Whether the latest move began on ground that holds the player.
    pub grounded: bool,
    /// The coyote time left (a jump still works), s.
    pub coyote: f32,
    /// How long the latest press still waits for the ground, s.
    pub buffer: f32,
}

impl PlayerState {
    /// Its length in an image.
    pub(crate) const BYTES: usize = 21;

    /// [`Self::put`]'s bytes as an array.
    pub(crate) fn bytes(&self) -> [u8; Self::BYTES] {
        let mut out = Vec::with_capacity(Self::BYTES);
        self.put(&mut out);
        out.try_into().expect("a player state is BYTES long")
    }

    /// Its canonical little-endian bytes.
    pub(crate) fn put(&self, out: &mut Vec<u8>) {
        for v in self.velocity {
            out.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        out.push(u8::from(self.grounded));
        out.extend_from_slice(&self.coyote.to_bits().to_le_bytes());
        out.extend_from_slice(&self.buffer.to_bits().to_le_bytes());
    }

    /// [`Self::put`]'s bytes back, or `None` if they are not a state a move can leave.
    pub(crate) fn take(b: &[u8]) -> Option<PlayerState> {
        if b.len() != Self::BYTES {
            return None;
        }
        let f = |at: usize| f32::from_bits(u32::from_le_bytes(b[at..at + 4].try_into().unwrap()));
        let grounded = match b[12] {
            0 => false,
            1 => true,
            _ => return None,
        };
        let state = PlayerState { velocity: [f(0), f(4), f(8)], grounded, coyote: f(13), buffer: f(17) };
        let finite = state.velocity.iter().all(|v| v.is_finite() && v.abs() <= MAX_SPEED)
            && [state.coyote, state.buffer].iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v));
        finite.then_some(state)
    }
}

/// The ground a probe found under a player's feet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ground {
    /// How far the ground is above the feet, m: positive is a step up, negative is ground below them.
    pub rise: f32,
    /// The ground's unit normal, its upward part at least [`PlayerParams::min_ground_normal`].
    pub normal: [f32; 3],
}

/// What a player wants this frame ([`walk`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Walk {
    /// The velocity it wants, m/s, before the world has its say.
    pub velocity: [f32; 3],
    /// Whether the ground holds it this frame: false in the air and on the frame it jumps.
    pub grounded: bool,
    /// Its state after the frame, before the world clips the velocity.
    pub state: PlayerState,
}

/// `v` with its length capped at `most`.
fn cap(v: [f32; 2], most: f32) -> [f32; 2] {
    let l2 = v[0] * v[0] + v[1] * v[1];
    if l2 <= most * most {
        v
    } else {
        let s = most / l2.sqrt();
        [v[0] * s, v[1] * s]
    }
}

/// `v` moved toward `want` by at most `most`.
fn approach(v: [f32; 2], want: [f32; 2], most: f32) -> [f32; 2] {
    let d = [want[0] - v[0], want[1] - v[1]];
    let l2 = d[0] * d[0] + d[1] * d[1];
    if l2 <= most * most {
        want
    } else {
        let s = most / l2.sqrt();
        [v[0] + d[0] * s, v[1] + d[1] * s]
    }
}

/// What a player wants to do this frame: a pure function of the tuning, its state, the ground the probe found under
/// it at the frame's start (`None`: none that holds it) and the input, over `dt` seconds.
///
/// * **On the ground** (ground found, and not rising from a jump), the walk velocity moves toward the one asked for at
///   [`PlayerParams::ground_accel`], and the ride holds the body up, so its own vertical velocity is zero.
/// * **In the air** it moves toward it at [`PlayerParams::air_accel`], and gravity pulls it down, no faster than
///   [`PlayerParams::max_fall`].
/// * **A jump** starts when a press is still waiting ([`PlayerParams::buffer_time`]) while the ground holds the player
///   or held it lately ([`PlayerParams::coyote_time`]). It sets the vertical velocity to [`PlayerParams::jump_speed`]
///   and spends both.
pub fn walk(params: &PlayerParams, state: &PlayerState, ground: Option<&Ground>, input: &MoverInput, dt: f32) -> Walk {
    let grounded = ground.is_some() && state.velocity[1] <= 0.0;
    let mut coyote = if grounded { params.coyote_time } else { max_float(state.coyote - dt, 0.0) };
    let mut buffer = if input.jump { params.buffer_time } else { max_float(state.buffer - dt, 0.0) };
    let want = cap(input.walk, params.max_walk);
    let accel = if grounded { params.ground_accel } else { params.air_accel };
    let [vx, vz] = approach([state.velocity[0], state.velocity[2]], want, accel * dt);
    let mut vy = if grounded { 0.0 } else { max_float(state.velocity[1] - params.gravity * dt, -params.max_fall) };
    let mut on_ground = grounded;
    if buffer > 0.0 && coyote > 0.0 {
        vy = params.jump_speed;
        coyote = 0.0;
        buffer = 0.0;
        on_ground = false;
    }
    let velocity = [vx, vy, vz];
    Walk { velocity, grounded: on_ground, state: PlayerState { velocity, grounded: on_ground, coyote, buffer } }
}

/// The query filter of a player's queries. Players' own shapes have an empty mask, so no query sees them.
fn player_query_filter() -> QueryFilter {
    QueryFilter { mask_bits: !PLAYER_CATEGORY, ..default_query_filter() }
}

/// Which bodies are solid to one player's move: static, kinematic, and dynamic of at least `heavy_mass`, but not
/// the ones it ignores.
struct Solid<'a> {
    heavy_mass: f32,
    ignored: &'a BTreeSet<i32>,
}

impl Solid<'_> {
    /// Whether `body` is dynamic and light, and so shoved rather than solid (ignored bodies are neither).
    fn light(&self, world: &World, body: BodyId) -> bool {
        body_get_type(world, body) == BodyType::Dynamic && body_get_mass(world, body) < self.heavy_mass
    }

    fn body(&self, world: &World, body: BodyId) -> bool {
        !self.ignored.contains(&body.index1) && !self.light(world, body)
    }

    fn shape(&self, world: &World, shape: ShapeId) -> bool {
        self.body(world, shape_get_body(world, shape))
    }
}

/// A mover plane. A static body's is rigid. A moving body's (kinematic, or dynamic and heavy) pushes at most `limit` a
/// pass, so where the two disagree the static one wins: a crate pinning a player against a wall passes through the
/// player instead of pushing it into the wall. With `level_steep` (the player is on the ground or rising), a plane that
/// faces up but is too steep to stand on is turned into a wall, its normal made level with the same push along it, so
/// the capsule can't climb it by sliding. A falling player keeps the real plane, so its fall slides down the face
/// (@design-reviewer's M1 on cbe1603b).
fn mover_plane(plane: Plane, limit: f32, min_ground_normal: f32, level_steep: bool) -> CollisionPlane {
    let n = plane.normal;
    let level = (n.x * n.x + n.z * n.z).sqrt();
    let plane = if level_steep && n.y > 0.0 && n.y < min_ground_normal && level > 0.0 {
        Plane { normal: Vec3 { x: n.x / level, y: 0.0, z: n.z / level }, offset: plane.offset / level }
    } else {
        plane
    };
    CollisionPlane { plane, push_limit: limit, push: 0.0, clip_velocity: true }
}

/// The plane that pushes a capsule at `at` out of hull `shape` when its core is inside the hull, which Box3D's
/// mover leaves without one (hull/queries.rs, "deep overlap on hulls is intentionally not handled"): of the hull's
/// faces, the one whose push clears the capsule's deeper end by its radius is the least. `None` for other shapes.
fn push_out(world: &World, shape: ShapeId, at: Pos, capsule: &Capsule) -> Option<Plane> {
    // Only a hull has planes to push out along (a heavy sphere or capsule inside the core gives none).
    if shape_get_type(world, shape) != ShapeType::Hull {
        return None;
    }
    let hull = shape_get_hull(world, shape)?;
    let xf = body_get_transform(world, shape_get_body(world, shape));
    // The core's ends in the hull's frame (a hull shape is in its body's frame).
    let local = |c: Vec3| inv_rotate_vector(xf.q, add(sub_pos(at, xf.p), c));
    let (p1, p2) = (local(capsule.center1), local(capsule.center2));
    let mut best: Option<(f32, Vec3)> = None;
    for face in get_hull_planes(hull) {
        let inside = max_float(face.offset - dot(face.normal, p1), face.offset - dot(face.normal, p2));
        let push = inside + capsule.radius;
        if best.is_none_or(|(b, _)| push < b) {
            best = Some((push, face.normal));
        }
    }
    best.map(|(push, n)| Plane { normal: rotate_vector(xf.q, n), offset: push })
}

/// A probe's candidate hit: its fraction, its tie key (shape id, child, triangle), normal, point and body.
type ProbeHit = (f32, (i32, i32, i32), Vec3, Pos, BodyId);

/// What a probe found: the ground, the world point it touched, and the body it belongs to.
#[derive(Clone, Copy, Debug)]
struct Probe {
    ground: Ground,
    point: Pos,
    body: BodyId,
}

impl PhysicsWorld {
    /// Make `id`'s body a player standing with its feet at `feet`: a kinematic body whose one shape is the mover's
    /// capsule ([`PlayerParams::capsule`]), in [`PLAYER_CATEGORY`] with an empty mask, so it touches nothing in the
    /// solver and no query sees it (other players' moves step around it, [`Self::apart`]). Like [`Self::add_body`], refused for `ObjectId::NONE`, a bound object or a bad
    /// tuning, with nothing created.
    pub fn add_player(&mut self, id: ObjectId, feet: [f64; 3], params: &PlayerParams) -> Result<(), PhysicsError> {
        if id.is_none() {
            return Err(PhysicsError::NoneObject);
        }
        if self.bodies.contains_key(&id) {
            return Err(PhysicsError::AlreadyBound(id));
        }
        self.usable()?;
        params.validate()?;
        position(&feet, "player position")?;
        let mut def = default_body_def();
        def.type_ = BodyType::Kinematic;
        def.position = pos(feet);
        let body = create_body(&mut self.world, &def);
        let mut sd = default_shape_def();
        // Hit events on every shape, the rule of the physics version (see `add_body`), though this one never touches.
        sd.enable_hit_events = true;
        sd.filter.category_bits = PLAYER_CATEGORY;
        sd.filter.mask_bits = 0;
        create_capsule_shape(&mut self.world, body, &sd, &params.capsule());
        self.bodies.insert(id, body);
        self.objects.insert(body.index1, id);
        self.players.insert(id, PlayerState::default());
        Ok(())
    }

    /// Whether `id`'s body is a player.
    pub fn is_player(&self, id: ObjectId) -> bool {
        self.players.contains_key(&id)
    }

    /// The state `id`'s latest move left. Refused while poisoned.
    pub fn player(&self, id: ObjectId) -> Result<PlayerState, PhysicsError> {
        if self.poisoned {
            return Err(PhysicsError::Poisoned);
        }
        self.players.get(&id).copied().ok_or(PhysicsError::NotAPlayer(id))
    }

    /// Walk player `id` for this frame's step of `dt` seconds, and return the pose its body has after that step, within
    /// rounding: the step carries the body to it at the target's velocity, over its substeps (about 1e-5 m). Set the
    /// hand targets from the pose returned, and never compare it with the body's for equality.
    /// Called once a frame for each player, in object-id order, before the step. `ignore` lists the objects the move
    /// treats as absent: what this player holds, and every player's hands.
    ///
    /// 1. **The ground:** the probe looks under the feet as the world stands at the frame's start, this frame's fall
    ///    further down.
    /// 2. **The want:** [`walk`].
    /// 3. **The displacement:** the walk, plus the room-scale offset; on the ground, plus the slope's rise under the
    ///    walk and the ride's move toward the ground (at most [`PlayerParams::rise_speed`] up and
    ///    [`PlayerParams::drop_speed`] down); falling onto ground within this frame's fall, no deeper than the ground.
    /// 4. **The slide:** the capsule over what is solid to this player, Box3D's mover (collide, solve the planes,
    ///    cast) up to five times, the planes sorted by shape, steep ones levelled while it is on the ground or rising.
    ///    The player's velocity loses what goes into the planes it ends against.
    /// 5. **The body** gets the kinematic target of where the slide ended, which the step carries it to.
    /// 6. **Shoves and weight:** each light body in the push volume where the slide ended gets one capped impulse at
    ///    its deepest point, in object-id order; on solid dynamic ground the player's weight presses at the probe's
    ///    point for the step.
    pub fn move_player(
        &mut self,
        id: ObjectId,
        input: &MoverInput,
        ignore: &[ObjectId],
        params: &PlayerParams,
        dt: f32,
    ) -> Result<Pose, PhysicsError> {
        self.usable()?;
        params.validate()?;
        input.validate()?;
        if !(1.0e-3..=1.0).contains(&dt) {
            return Err(PhysicsError::InvalidValue("move time step"));
        }
        let state = *self.players.get(&id).ok_or(PhysicsError::NotAPlayer(id))?;
        let body = self.live_body(id)?;
        let ignored: BTreeSet<i32> = ignore.iter().filter_map(|o| self.bodies.get(o)).map(|b| b.index1).collect();
        let solid = Solid { heavy_mass: params.heavy_mass, ignored: &ignored };
        let start = body_get_transform(&self.world, body).p;

        let fall = max_float(0.0, params.gravity * dt - state.velocity[1]) * dt;
        let found = self.probe(start, params, &solid, params.snap + fall);
        let ground = found.filter(|p| p.ground.rise >= -params.snap);
        let want = walk(params, &state, ground.as_ref().map(|p| &p.ground), input, dt);
        let v = Vec3 { x: want.velocity[0], y: want.velocity[1], z: want.velocity[2] };
        let room = cap(input.room, params.max_room_step);
        let mut delta = Vec3 { x: v.x * dt + room[0], y: v.y * dt, z: v.z * dt + room[1] };
        match (want.grounded, ground, found) {
            (true, Some(p), _) => {
                let n = p.ground.normal;
                let slope = -(n[0] * delta.x + n[2] * delta.z) / n[1];
                delta.y = slope + clamp_float(p.ground.rise, -params.drop_speed * dt, params.rise_speed * dt);
            }
            (false, _, Some(p)) if v.y <= 0.0 => delta.y = max_float(delta.y, p.ground.rise),
            _ => {}
        }

        let level_steep = want.grounded || v.y > 0.0;
        let (mut end, mut planes) = self.slide(start, delta, params, &solid, level_steep);
        let apart = self.apart(id, end, params);
        if apart != VEC3_ZERO {
            (end, planes) = self.slide(end, apart, params, &solid, level_steep);
        }
        let velocity = clip_vector(v, &planes);
        let moved = sub_pos(end, start);
        let target = BoxWorldTransform { p: end, q: QUAT_IDENTITY };
        body_set_target_transform(&mut self.world, body, target, dt, moved != VEC3_ZERO);

        let hands: Vec<Pos> = input.hands.iter().flatten().map(|&h| pos(h)).collect();
        self.shove(end, mul_sv(1.0 / dt, moved), params, &ignored, &hands, dt);
        if let (true, Some(p)) = (want.grounded, ground)
            && body_get_type(&self.world, p.body) == BodyType::Dynamic
        {
            let weight = Vec3 { x: 0.0, y: -params.mass * params.gravity, z: 0.0 };
            body_apply_force(&mut self.world, p.body, weight, p.point, true);
        }

        let next = PlayerState { velocity: [velocity.x, velocity.y, velocity.z], ..want.state };
        self.players.insert(id, next);
        Ok(Pose { position: from_pos(end), rotation: [0.0, 0.0, 0.0, 1.0] })
    }

    /// How far across the floor player `me`'s capsule, with its feet at `feet`, must step to leave every other
    /// player's (the friend test of 11-27: players are walls to each other). For each other player whose capsule's
    /// height overlaps this one's and whose feet are nearer than two radii across the floor, in object-id order, the
    /// step out along the line between their feet (on the same spot, along +x for the lower id and -x for the higher).
    /// Each other player is
    /// where it stood at the frame's start: its body's pose, which the step hasn't moved yet.
    fn apart(&self, me: ObjectId, feet: Pos, params: &PlayerParams) -> Vec3 {
        let reach = 2.0 * params.radius;
        let mut out = VEC3_ZERO;
        for (&other, _) in self.players.iter() {
            if other == me {
                continue;
            }
            let Some(&body) = self.bodies.get(&other) else { continue };
            let there = body_get_transform(&self.world, body).p;
            let d = sub_pos(offset_pos(feet, out), there);
            // Both capsules span the same heights over their feet: they meet in height if the feet are within it.
            if d.y.abs() >= params.height - params.ride {
                continue;
            }
            let across = d.x * d.x + d.z * d.z;
            if across >= reach * reach {
                continue;
            }
            let len = across.sqrt();
            // Two on one spot step apart by id, the lower along +x and the higher along -x: each reads the other
            // where it stood at the frame's start, so a shared direction would carry both off together
            // (@chunks-reviewer's M1 on a306b59c).
            let (dir_x, dir_z) = match (len > 0.0, me < other) {
                (true, _) => (d.x / len, d.z / len),
                (false, true) => (1.0, 0.0),
                (false, false) => (-1.0, 0.0),
            };
            let push = reach - len;
            out = add(out, Vec3 { x: dir_x * push, y: 0.0, z: dir_z * push });
        }
        out
    }

    /// The ground under feet at `feet`, at most `below` under them: a sphere of [`PlayerParams::probe_radius`] cast
    /// straight down from inside the bottom of the capsule, over what is solid to this player. The ground is the
    /// closest hit not too steep to stand on, so a steep face nearer the feet (the foot of a steep slope) doesn't hide
    /// the floor below it. A shape the probe starts inside is passed through. Of two hits at the same distance the
    /// lower (shape id, child, triangle) wins, so the answer never depends on the order the cast meets them.
    fn probe(&self, feet: Pos, params: &PlayerParams, solid: &Solid, below: f32) -> Option<Probe> {
        let world = &self.world;
        // The probe's lowest point starts this far above the feet, inside the capsule's bottom.
        let top = params.ride + params.radius - params.probe_radius;
        let start = offset_pos(feet, Vec3 { x: 0.0, y: top + params.probe_radius, z: 0.0 });
        let reach = top + below;
        let proxy = make_proxy(&[VEC3_ZERO], params.probe_radius);
        let translation = Vec3 { x: 0.0, y: -reach, z: 0.0 };
        let mut best: Option<ProbeHit> = None;
        world_cast_shape(
            world,
            start,
            &proxy,
            translation,
            &player_query_filter(),
            |shape, point, normal, t, _, triangle, child| {
                if t > 0.0 && normal.y >= params.min_ground_normal && solid.shape(world, shape) {
                    let key = (shape.index1, child, triangle);
                    let closer = best.is_none_or(|(bt, bk, ..)| t < bt || (t == bt && key < bk));
                    if closer {
                        best = Some((t, key, normal, point, shape_get_body(world, shape)));
                    }
                }
                // Never clip the cast, so every shape on the way is seen and ties are settled by id.
                2.0
            },
        );
        let (_, _, n, point, body) = best?;
        // The ground's height is the touched point's, which is on the surface. The cast's own fraction is not: Box3D's
        // shape cast stops with a rounded shape about a linear slop inside what it hits, so a ride on it would sink.
        let rise = clamp_float(sub_pos(point, feet).y, -below, top);
        Some(Probe { ground: Ground { rise, normal: [n.x, n.y, n.z] }, point, body })
    }

    /// The solid planes a capsule at `at` touches, at most [`MAX_PLANES`]: Box3D's mover planes, plus [`push_out`]'s
    /// for the hulls the capsule's core is inside. When there are more, static bodies' planes are kept first. Sorted:
    /// moving bodies' first, then static ones, each by shape and then Box3D's order within the shape.
    fn planes_at(&self, at: Pos, capsule: &Capsule, solid: &Solid, steep: (f32, bool)) -> Vec<CollisionPlane> {
        let (min_ground_normal, level_steep) = steep;
        let world = &self.world;
        // A moving body pushes at most a capsule radius a pass (15 m/s at 60 Hz); a static one without limit.
        let limit = |shape: ShapeId| match body_get_type(world, shape_get_body(world, shape)) {
            BodyType::Static => f32::MAX,
            _ => capsule.radius,
        };
        let filter = player_query_filter();
        // Sorted with the static planes last, so that in each of the plane solver's passes they have the last word.
        let mut found: Vec<(bool, i32, i32, CollisionPlane)> = Vec::new();
        let fixed = |shape: ShapeId| body_get_type(world, shape_get_body(world, shape)) == BodyType::Static;
        world_collide_mover(world, at, capsule, &filter, |shape, results| {
            if solid.shape(world, shape) {
                for (k, r) in results.iter().enumerate() {
                    let plane = mover_plane(r.plane, limit(shape), min_ground_normal, level_steep);
                    found.push((fixed(shape), shape.index1, k as i32, plane));
                }
            }
            true
        });
        let core = make_proxy(&[capsule.center1, capsule.center2], 0.0);
        world_overlap_shape(world, at, &core, &filter, |shape| {
            if solid.shape(world, shape)
                && let Some(plane) = push_out(world, shape, at, capsule)
            {
                let plane = mover_plane(plane, limit(shape), min_ground_normal, level_steep);
                found.push((fixed(shape), shape.index1, -1, plane));
            }
            true
        });
        // Too many: the static planes stay, so a crowd of moving bodies can't push a wall out of the solve.
        if found.len() > MAX_PLANES {
            found.sort_by_key(|&(fixed, shape, k, _)| (!fixed, shape, k));
            found.truncate(MAX_PLANES);
        }
        found.sort_by_key(|&(fixed, shape, k, _)| (fixed, shape, k));
        found.into_iter().map(|(_, _, _, plane)| plane).collect()
    }

    /// Slide a player's capsule from feet at `feet` by `delta` over what is solid to it (Box3D's mover: collide,
    /// solve the planes, cast), returning where the feet end and the planes the capsule touches there, marked to clip.
    /// `level_steep`: steep upward planes are levelled into walls ([`mover_plane`]).
    fn slide(
        &self,
        feet: Pos,
        delta: Vec3,
        params: &PlayerParams,
        solid: &Solid,
        level_steep: bool,
    ) -> (Pos, Vec<CollisionPlane>) {
        let steep = (params.min_ground_normal, level_steep);
        let world = &self.world;
        let capsule = params.capsule();
        let filter = player_query_filter();
        let target = offset_pos(feet, delta);
        let mut at = feet;
        for _ in 0..SLIDE_PASSES {
            let mut planes = self.planes_at(at, &capsule, solid, steep);
            let step = solve_planes(sub_pos(target, at), &mut planes).delta;
            if length_squared(step) < SLIDE_TOLERANCE * SLIDE_TOLERANCE {
                break;
            }
            let mut keep = |shape: ShapeId| solid.shape(world, shape);
            let t = world_cast_mover(world, at, &capsule, step, &filter, Some(&mut keep));
            let moved = mul_sv(t, step);
            at = offset_pos(at, moved);
            if length_squared(moved) < SLIDE_TOLERANCE * SLIDE_TOLERANCE {
                break;
            }
        }
        // Every plane found touches the capsule where it ended (Box3D's mover lets it rest up to a slop inside, where
        // the solver would push nothing), so each one clips the velocity.
        let mut planes = self.planes_at(at, &capsule, solid, steep);
        for plane in &mut planes {
            plane.push = 1.0;
        }
        (at, planes)
    }

    /// Shove the light bodies in a player's push volume with its feet at `feet`, moving at `velocity`: each, in
    /// object-id order, gets one impulse at its deepest point, away from the player, enough to stop closing on it and
    /// to clear the overlap in `dt`, capped at [`PlayerParams::max_shove_speed`] and
    /// [`PlayerParams::max_shove_impulse`]. Ignored bodies are skipped, and so is a body by one of `hands` (its centre
    /// or its touch within [`PlayerParams::hand_clear`]).
    fn shove(
        &mut self,
        feet: Pos,
        velocity: Vec3,
        params: &PlayerParams,
        ignored: &BTreeSet<i32>,
        hands: &[Pos],
        dt: f32,
    ) {
        let world = &self.world;
        let filter = player_query_filter();
        let solid = Solid { heavy_mass: params.heavy_mass, ignored };
        let clear = params.hand_clear * params.hand_clear;
        let by_hand = |p: Pos| hands.iter().any(|&h| length_squared(sub_pos(p, h)) <= clear);
        // Per object: depth, shape, plane order, push direction and point; the deepest plane wins, ties by shape.
        let mut touched: BTreeMap<ObjectId, (f32, i32, i32, Vec3, Pos)> = BTreeMap::new();
        for capsule in [params.capsule(), params.push_capsule()] {
            world_collide_mover(world, feet, &capsule, &filter, |shape, results| {
                let body = shape_get_body(world, shape);
                if ignored.contains(&body.index1) || !solid.light(world, body) {
                    return true;
                }
                let Some(&object) = self.objects.get(&body.index1) else {
                    return true;
                };
                if by_hand(body_get_transform(world, body).p) {
                    return true;
                }
                for (k, r) in results.iter().enumerate() {
                    if by_hand(offset_pos(feet, r.point)) {
                        continue;
                    }
                    let key = (r.plane.offset, shape.index1, k as i32);
                    let deeper = touched
                        .get(&object)
                        .is_none_or(|&(d, s, o, ..)| key.0 > d || (key.0 == d && (key.1, key.2) < (s, o)));
                    if deeper {
                        touched.insert(object, (key.0, key.1, key.2, neg(r.plane.normal), offset_pos(feet, r.point)));
                    }
                }
                true
            });
        }
        for (object, (depth, _, _, away, point)) in touched {
            let body = self.bodies[&object];
            let closing = dot(sub(velocity, body_get_linear_velocity(&self.world, body)), away);
            let speed = min_float(max_float(closing, 0.0) + max_float(depth, 0.0) / dt, params.max_shove_speed);
            let impulse = min_float(body_get_mass(&self.world, body) * speed, params.max_shove_impulse);
            if impulse > 0.0 {
                body_apply_linear_impulse(&mut self.world, body, mul_sv(impulse, away), point, true);
            }
        }
    }
}

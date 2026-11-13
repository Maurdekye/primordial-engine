//! [`PhysicsWorld`]: one Box3D world, its bodies bound to sim objects, and its joints.

use std::collections::BTreeMap;

use box3d_rust::body::{
    body_apply_angular_impulse, body_apply_force, body_apply_force_to_center, body_apply_linear_impulse,
    body_apply_linear_impulse_to_center, body_apply_torque, body_get_angular_velocity, body_get_linear_velocity,
    body_get_shape_count, body_get_shapes, body_get_transform, body_get_type, body_is_awake, body_is_valid,
    body_reset_history, body_set_angular_velocity, body_set_awake, body_set_linear_velocity, body_set_target_transform,
    body_set_transform, create_body, destroy_body,
};
use box3d_rust::core::get_length_units_per_meter;
use box3d_rust::geometry::{Capsule, ShapeType, Sphere};
use box3d_rust::hull::{create_hull, get_hull_planes, make_box_hull};
use box3d_rust::id::{BodyId, JointId, ShapeId};
use box3d_rust::joint::{
    JointType, create_distance_joint, create_motor_joint, create_spherical_joint, create_weld_joint, destroy_joint,
    joint_get_body_a, joint_get_body_b, joint_get_local_frame_a, joint_get_local_frame_b, joint_is_valid,
    joint_set_local_frame_a, joint_wake_bodies, motor_joint_set_angular_velocity, motor_joint_set_linear_velocity,
};
use box3d_rust::math_functions::{
    Pos, Quat, Transform as BoxTransform, Vec3, WorldTransform as BoxWorldTransform, inv_rotate_vector,
    is_normalized_quat, rotate_vector, sub_pos, transform_world_point,
};
use box3d_rust::recording::hash_world_state;
use box3d_rust::shape::{
    create_capsule_shape, create_hull_shape, create_sphere_shape, shape_get_body, shape_get_capsule, shape_get_hull,
    shape_get_sphere, shape_get_type,
};
use box3d_rust::solver::make_soft;
use box3d_rust::types::{
    BodyType, default_body_def, default_distance_joint_def, default_motor_joint_def, default_query_filter,
    default_shape_def, default_spherical_joint_def, default_weld_joint_def,
};
use box3d_rust::world::{World, world_cast_mover, world_get_awake_body_count, world_set_worker_count};
use pe_core::ids::ObjectId;
use pe_sim::{SimStore, Transform, WorldTransform};

use crate::config::PhysicsConfig;
use crate::desc::limits::{MAX_DAMPING, MAX_PUSH, MAX_SIZE, MAX_SPEED};
use crate::desc::{BodyDesc, BodyKind, Geometry, bounded, position, unit_quat};
use crate::error::PhysicsError;

pub(crate) fn vec3(v: [f32; 3]) -> Vec3 {
    Vec3 { x: v[0], y: v[1], z: v[2] }
}

/// Box3D's `Pos` is `f32` unless the fork's `double-precision` feature is on; the engine API is
/// `f64` either way.
pub(crate) fn pos(v: [f64; 3]) -> Pos {
    Pos { x: v[0] as _, y: v[1] as _, z: v[2] as _ }
}

/// A pose's position bounded and its rotation exactly as given, which must be unit length as
/// Box3D judges it (`is_normalized_quat`): the exact setters never renormalize.
fn exact_rotation(pose: &Pose) -> Result<Quat, PhysicsError> {
    position(&pose.position, "pose position")?;
    let q = quat(pose.rotation);
    if !pose.rotation.iter().all(|c| c.is_finite()) || !is_normalized_quat(q) {
        return Err(PhysicsError::InvalidValue("pose rotation is not unit length"));
    }
    Ok(q)
}

fn quat(q: [f32; 4]) -> Quat {
    Quat { v: Vec3 { x: q[0], y: q[1], z: q[2] }, s: q[3] }
}

/// A body's pose, in the physics world's frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub position: [f64; 3],
    /// Unit quaternion `[x, y, z, w]`.
    pub rotation: [f32; 4],
}

/// One player's push on an object, applied at the start of a frame (design 7: interactions
/// become forces, impulses or joints). Vectors are in the physics world's frame, bounded by
/// [`crate::limits::MAX_PUSH`]; points by [`crate::limits::MAX_POSITION`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Push {
    /// A linear impulse (N s) through the centre of mass.
    Impulse([f32; 3]),
    /// A linear impulse at a world point; also changes the spin.
    ImpulseAt { impulse: [f32; 3], point: [f64; 3] },
    /// An angular impulse (N m s).
    AngularImpulse([f32; 3]),
    /// A force (N) through the centre of mass, for this frame's step only.
    Force([f32; 3]),
    /// A force at a world point, for this frame's step only.
    ForceAt { force: [f32; 3], point: [f64; 3] },
    /// A torque (N m), for this frame's step only.
    Torque([f32; 3]),
}

/// Engine handle of a joint, chosen by the caller from deterministic state (for example the id
/// of the interaction that made it), like an [`ObjectId`]. The joint table is part of the
/// physics state, so a rollback restores it with the world: after a rollback a key names
/// whatever joint the restored state has under it, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JointKey(pub u64);

/// Which [`JointDesc`] a joint was made from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum JointKind {
    Spherical = 0,
    Weld = 1,
    Distance = 2,
    Grab = 3,
}

impl JointKind {
    /// The Box3D joint type a joint of this kind is made as.
    pub(crate) fn box3d_type(self) -> JointType {
        match self {
            JointKind::Spherical => JointType::Spherical,
            JointKind::Weld => JointType::Weld,
            JointKind::Distance => JointType::Distance,
            JointKind::Grab => JointType::Motor,
        }
    }

    pub(crate) fn from_u8(v: u8) -> Option<JointKind> {
        Some(match v {
            0 => JointKind::Spherical,
            1 => JointKind::Weld,
            2 => JointKind::Distance,
            3 => JointKind::Grab,
            _ => return None,
        })
    }
}

/// A grab's spring: it pulls `b`'s anchor towards `a`'s (linear and angular, same hertz), so a
/// held object follows the hand instead of drifting from it. `hertz = 0` turns it off.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrabSpring {
    pub hertz: f32,
    pub damping_ratio: f32,
    pub max_force: f32,
    pub max_torque: f32,
}

/// How far from its place a grab's spring may see its object in one step ([`PhysicsWorld::step_holding`]), m.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HoldReach {
    /// In every direction but against gravity.
    pub reach: f32,
    /// Against gravity: the part of the error that pulls the object up, which holds it up.
    pub against_gravity: f32,
}

/// A joint between two bound objects (or one object and the world, `b = None`). Anchors are
/// in each body's local frame. The two ends must be different bodies.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JointDesc {
    /// Ball and socket: the anchors stay together.
    Spherical { a: ObjectId, b: Option<ObjectId>, anchor_a: [f32; 3], anchor_b: [f32; 3] },
    /// The two frames stay fixed relative to each other.
    Weld { a: ObjectId, b: Option<ObjectId>, anchor_a: [f32; 3], anchor_b: [f32; 3] },
    /// The anchors stay `length` apart.
    Distance { a: ObjectId, b: Option<ObjectId>, anchor_a: [f32; 3], anchor_b: [f32; 3], length: f32 },
    /// A grab, like a hand holding an object (D72 grabs): drives `b`'s velocity towards `a`'s
    /// plus a target velocity, with force caps, and optionally springs `anchor_b` towards
    /// `anchor_a`. `a = None` uses a static world anchor.
    Grab {
        a: Option<ObjectId>,
        b: ObjectId,
        anchor_a: [f32; 3],
        anchor_b: [f32; 3],
        max_force: f32,
        max_torque: f32,
        spring: Option<GrabSpring>,
    },
}

/// A joint in the table: its Box3D id and its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JointEntry {
    pub(crate) id: JointId,
    pub(crate) kind: JointKind,
}

/// A Box3D world whose bodies belong to sim objects.
///
/// * Bodies are created and destroyed only through this type, keyed by [`ObjectId`]. The
///   binding table is sorted by id, so every iteration over it is in the same order on every
///   peer.
/// * [`PhysicsWorld::raw`] exposes the Box3D world read-only. `raw_mut` (feature `raw`, for test
///   scenes and tools only) can break every invariant here: bodies created through it are not
///   bound, and settings or callbacks changed through it are not carried by images.
/// * Length units are process-wide in Box3D (`set_length_units_per_meter`), so every world in a
///   process shares them; the engine uses metres, and refuses to create or step a world
///   otherwise.
/// * A restore that fails after it has started writing poisons the world: every call that
///   reads or changes the simulation is refused until a good image is restored.
pub struct PhysicsWorld {
    pub(crate) world: World,
    pub(crate) config: PhysicsConfig,
    pub(crate) bodies: BTreeMap<ObjectId, BodyId>,
    pub(crate) joints: BTreeMap<JointKey, JointEntry>,
    /// The static body grabs with no `a` attach to (created on first use).
    pub(crate) anchor: Option<BodyId>,
    pub(crate) images: crate::image::ImageRegistry,
    pub(crate) poisoned: bool,
}

/// The fork's worker-count limit.
const MAX_WORKERS: i32 = 32;

/// A shape as a capsule for [`PhysicsWorld::static_in_way`], in its body's frame: its two centres and its radius. A
/// sphere or capsule is itself; a hull is the largest capsule inside it about its centre, along the longest side of
/// its bounding box (its radius the centre's distance to the nearest face, its length as far as every face allows).
/// `None` for shapes pe-physics never makes.
fn mover_of(world: &World, shape: ShapeId) -> Option<(Vec3, Vec3, f32)> {
    match shape_get_type(world, shape) {
        ShapeType::Sphere => {
            let s = shape_get_sphere(world, shape);
            Some((s.center, s.center, s.radius))
        }
        ShapeType::Capsule => {
            let c = shape_get_capsule(world, shape);
            Some((c.center1, c.center2, c.radius))
        }
        ShapeType::Hull => {
            let hull = shape_get_hull(world, shape)?;
            let c = hull.center;
            let planes = get_hull_planes(hull);
            let depth = |n: Vec3, offset: f32| offset - (n.x * c.x + n.y * c.y + n.z * c.z);
            let radius = planes.iter().map(|p| depth(p.normal, p.offset)).fold(f32::INFINITY, f32::min);
            if !(radius.is_finite() && radius > 0.0) {
                return None;
            }
            let (lo, hi) = (hull.aabb.lower_bound, hull.aabb.upper_bound);
            let half = [0.5 * (hi.x - lo.x), 0.5 * (hi.y - lo.y), 0.5 * (hi.z - lo.z)];
            let axis = (1..3).fold(0, |a, i| if half[i] > half[a] { i } else { a });
            let mut u = [0.0_f32; 3];
            u[axis] = 1.0;
            let mut length = half[axis];
            for p in planes {
                let along = (p.normal.x * u[0] + p.normal.y * u[1] + p.normal.z * u[2]).abs();
                if along > 1.0e-6 {
                    length = length.min((depth(p.normal, p.offset) - radius) / along);
                }
            }
            let length = length.max(0.0);
            let end = |k: f32| Vec3 { x: c.x + k * u[0], y: c.y + k * u[1], z: c.z + k * u[2] };
            Some((end(-length), end(length), radius))
        }
        _ => None,
    }
}

/// The part of `v` beyond `reach`: zero if `v` is within it.
fn beyond(v: Vec3, reach: f32) -> Vec3 {
    let length = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
    let k = if length > reach { 1.0 - reach / length } else { 0.0 };
    Vec3 { x: v.x * k, y: v.y * k, z: v.z * k }
}

fn check_length_units() -> Result<(), PhysicsError> {
    if get_length_units_per_meter() == 1.0 { Ok(()) } else { Err(PhysicsError::LengthUnits) }
}

impl PhysicsWorld {
    /// A new, empty world. Refuses an invalid configuration, and Box3D length units other than
    /// metres. The worker count is clamped to 1..=32.
    pub fn new(config: PhysicsConfig) -> Result<PhysicsWorld, PhysicsError> {
        config.validate()?;
        check_length_units()?;
        let config = PhysicsConfig { workers: config.workers.clamp(1, MAX_WORKERS), ..config };
        Ok(PhysicsWorld {
            world: World::new(&config.world_def()),
            config,
            bodies: BTreeMap::new(),
            joints: BTreeMap::new(),
            anchor: None,
            images: crate::image::ImageRegistry::default(),
            poisoned: false,
        })
    }

    /// Refuse calls while stepping or while poisoned.
    pub(crate) fn usable(&self) -> Result<(), PhysicsError> {
        if self.poisoned {
            return Err(PhysicsError::Poisoned);
        }
        if self.world.locked {
            return Err(PhysicsError::Locked);
        }
        Ok(())
    }

    /// A failed restore left this world unusable until a good image is restored.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn config(&self) -> &PhysicsConfig {
        &self.config
    }

    /// Change the worker-thread count (clamped to 1..=32). The results do not change (D2).
    pub fn set_workers(&mut self, workers: i32) {
        self.config.workers = workers.clamp(1, MAX_WORKERS);
        world_set_worker_count(&mut self.world, self.config.workers);
    }

    /// The Box3D world, read-only.
    pub fn raw(&self) -> &World {
        &self.world
    }

    /// The Box3D world, for test scenes and tools only; see the type docs.
    #[cfg(feature = "raw")]
    pub fn raw_mut(&mut self) -> &mut World {
        &mut self.world
    }

    // ---------------------------------------------------------------------------------------
    // Bodies
    // ---------------------------------------------------------------------------------------

    /// Create `id`'s body. Shapes are created in the order given, and the hull of a `Box` is
    /// shared by every body with the same half extents (Box3D's hull database interns it).
    pub fn add_body(&mut self, id: ObjectId, desc: &BodyDesc) -> Result<(), PhysicsError> {
        if id.is_none() {
            return Err(PhysicsError::NoneObject);
        }
        if self.bodies.contains_key(&id) {
            return Err(PhysicsError::AlreadyBound(id));
        }
        self.usable()?;
        let rotation = desc.validate()?;
        // Build hulls first so a bad hull refuses the call before anything is created.
        let mut hulls = Vec::new();
        for shape in &desc.shapes {
            hulls.push(match &shape.geometry {
                Geometry::Box { half_extents: h } => Some(make_box_hull(h[0], h[1], h[2]).base),
                Geometry::Hull { points } => {
                    let points: Vec<Vec3> = points.iter().map(|&p| vec3(p)).collect();
                    Some(create_hull(&points, 64).ok_or(PhysicsError::BadHull)?)
                }
                _ => None,
            });
        }

        let mut def = default_body_def();
        def.type_ = match desc.kind {
            BodyKind::Static => BodyType::Static,
            BodyKind::Kinematic => BodyType::Kinematic,
            BodyKind::Dynamic => BodyType::Dynamic,
        };
        def.position = pos(desc.position);
        def.rotation = quat(rotation);
        def.linear_velocity = vec3(desc.linear_velocity);
        def.angular_velocity = vec3(desc.angular_velocity);
        def.linear_damping = desc.linear_damping;
        def.angular_damping = desc.angular_damping;
        def.gravity_scale = desc.gravity_scale;
        def.is_bullet = desc.bullet;
        def.enable_sleep = desc.enable_sleep;
        let body = create_body(&mut self.world, &def);

        for (shape, hull) in desc.shapes.iter().zip(&hulls) {
            let mut sd = default_shape_def();
            sd.density = shape.density;
            sd.base_material.friction = shape.friction;
            sd.base_material.restitution = shape.restitution;
            sd.base_material.rolling_resistance = shape.rolling_resistance;
            match (&shape.geometry, hull) {
                (Geometry::Sphere { center, radius }, _) => {
                    create_sphere_shape(&mut self.world, body, &sd, &Sphere { center: vec3(*center), radius: *radius });
                }
                (Geometry::Capsule { center1, center2, radius }, _) => {
                    let capsule = Capsule { center1: vec3(*center1), center2: vec3(*center2), radius: *radius };
                    create_capsule_shape(&mut self.world, body, &sd, &capsule);
                }
                (_, Some(hull)) => {
                    create_hull_shape(&mut self.world, body, &sd, hull);
                }
                _ => unreachable!("hull built above"),
            }
        }
        self.bodies.insert(id, body);
        Ok(())
    }

    /// Destroy `id`'s body and the joints attached to it.
    pub fn remove_body(&mut self, id: ObjectId) -> Result<(), PhysicsError> {
        self.usable()?;
        let body = self.bodies.remove(&id).ok_or(PhysicsError::NotBound(id))?;
        destroy_body(&mut self.world, body);
        // Box3D destroyed the attached joints; forget their keys.
        let world = &self.world;
        self.joints.retain(|_, joint| joint_is_valid(world, joint.id));
        Ok(())
    }

    pub fn has_body(&self, id: ObjectId) -> bool {
        self.bodies.contains_key(&id)
    }

    /// The bound objects, in id order.
    pub fn objects(&self) -> impl Iterator<Item = ObjectId> + '_ {
        self.bodies.keys().copied()
    }

    pub fn body_count(&self) -> usize {
        self.bodies.len()
    }

    fn body(&self, id: ObjectId) -> Result<BodyId, PhysicsError> {
        self.bodies.get(&id).copied().ok_or(PhysicsError::NotBound(id))
    }

    /// Like `body`, for reads: also refused while poisoned.
    fn live_body(&self, id: ObjectId) -> Result<BodyId, PhysicsError> {
        if self.poisoned {
            return Err(PhysicsError::Poisoned);
        }
        self.body(id)
    }

    /// The Box3D body of `id` (for tools; engine code should not need it).
    pub fn body_id(&self, id: ObjectId) -> Option<BodyId> {
        self.bodies.get(&id).copied()
    }

    pub fn pose(&self, id: ObjectId) -> Result<Pose, PhysicsError> {
        let xf = body_get_transform(&self.world, self.live_body(id)?);
        Ok(Pose {
            position: [xf.p.x as f64, xf.p.y as f64, xf.p.z as f64],
            rotation: [xf.q.v.x, xf.q.v.y, xf.q.v.z, xf.q.s],
        })
    }

    /// Linear and angular velocity.
    pub fn velocity(&self, id: ObjectId) -> Result<([f32; 3], [f32; 3]), PhysicsError> {
        let body = self.live_body(id)?;
        let v = body_get_linear_velocity(&self.world, body);
        let w = body_get_angular_velocity(&self.world, body);
        Ok(([v.x, v.y, v.z], [w.x, w.y, w.z]))
    }

    pub fn is_awake(&self, id: ObjectId) -> Result<bool, PhysicsError> {
        Ok(body_is_awake(&self.world, self.live_body(id)?))
    }

    pub fn awake_body_count(&self) -> i32 {
        world_get_awake_body_count(&self.world)
    }

    /// Every bound body is still valid in Box3D.
    pub fn all_bodies_valid(&self) -> bool {
        self.bodies.values().all(|&b| body_is_valid(&self.world, b))
    }

    // ---------------------------------------------------------------------------------------
    // Interactions
    // ---------------------------------------------------------------------------------------

    /// Apply a push to `id`, waking it. Forces and torques last for the next step only.
    pub fn push(&mut self, id: ObjectId, push: Push) -> Result<(), PhysicsError> {
        self.usable()?;
        let body = self.body(id)?;
        let w = &mut self.world;
        match push {
            Push::Impulse(i) => {
                bounded(&i, MAX_PUSH, "impulse")?;
                body_apply_linear_impulse_to_center(w, body, vec3(i), true);
            }
            Push::ImpulseAt { impulse, point } => {
                bounded(&impulse, MAX_PUSH, "impulse")?;
                position(&point, "impulse point")?;
                body_apply_linear_impulse(w, body, vec3(impulse), pos(point), true);
            }
            Push::AngularImpulse(i) => {
                bounded(&i, MAX_PUSH, "angular impulse")?;
                body_apply_angular_impulse(w, body, vec3(i), true);
            }
            Push::Force(f) => {
                bounded(&f, MAX_PUSH, "force")?;
                body_apply_force_to_center(w, body, vec3(f), true);
            }
            Push::ForceAt { force, point } => {
                bounded(&force, MAX_PUSH, "force")?;
                position(&point, "force point")?;
                body_apply_force(w, body, vec3(force), pos(point), true);
            }
            Push::Torque(t) => {
                bounded(&t, MAX_PUSH, "torque")?;
                body_apply_torque(w, body, vec3(t), true);
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Carried state and targets (P2-3: a touch entry's resulting body state, hands)
    // ---------------------------------------------------------------------------------------

    /// A bound kinematic or dynamic body, for a setter.
    fn moving_body(&self, id: ObjectId) -> Result<BodyId, PhysicsError> {
        self.usable()?;
        let body = self.body(id)?;
        if body_get_type(&self.world, body) == BodyType::Static {
            return Err(PhysicsError::InvalidValue("a static body cannot be moved"));
        }
        Ok(body)
    }

    /// Apply carried state (P2-3: a touch entry's resulting body state): the pose and both
    /// velocities, exactly these bits, and the body awake with its sleep timer at zero.
    ///
    /// **Every peer applies it, the toucher included, at the same frame.** Writing a pose makes
    /// Box3D recompute the body's derived state (centre of mass, inertia, broad-phase proxy)
    /// from it, while the toucher's own body has those integrated; for a body whose centre of
    /// mass is off its origin they differ by rounding, and `box3d_hash` / `state_hash` do not
    /// show it until the next step (review SET-1). Sleep state is not carried: carried state is
    /// awake by definition, so a body asleep on one peer and awake on another, or with different
    /// sleep timers, ends the same everywhere (SET-2). Both checks run before anything is
    /// written. Refused for static bodies.
    ///
    /// The body's history is forgotten too (the fork's `body_reset_history`): its contacts are
    /// destroyed and recreated on the next step, so a recycled manifold (anchors from first
    /// touch, warm-start impulses) on one peer and a fresh one on another cannot make the next
    /// step differ. Speculatively a receiver's copy can differ from the toucher's at that frame;
    /// in the confirmed history every peer's world is identical there, so the reset is the same
    /// op everywhere. Touch-begin fires again for the body's contacts, and it has no warm start
    /// for one step.
    pub fn set_state(
        &mut self,
        id: ObjectId,
        pose: Pose,
        linear: [f32; 3],
        angular: [f32; 3],
    ) -> Result<(), PhysicsError> {
        let body = self.moving_body(id)?;
        let q = exact_rotation(&pose)?;
        bounded(&linear, MAX_SPEED, "linear velocity")?;
        bounded(&angular, MAX_SPEED, "angular velocity")?;
        let w = &mut self.world;
        body_set_transform(w, body, pos(pose.position), q);
        body_set_linear_velocity(w, body, vec3(linear));
        body_set_angular_velocity(w, body, vec3(angular));
        body_set_awake(w, body, true);
        body_reset_history(w, body);
        Ok(())
    }

    /// Overwrite a body's pose with exactly these bits. The rotation must be unit length as
    /// Box3D judges it (`is_normalized_quat`); it is taken as it is, never renormalized. Like
    /// Box3D's `body_set_transform`, this does not wake the body. Carried state goes through
    /// [`Self::set_state`] instead, on every peer. Refused for static bodies.
    pub fn set_pose(&mut self, id: ObjectId, pose: Pose) -> Result<(), PhysicsError> {
        let body = self.moving_body(id)?;
        let q = exact_rotation(&pose)?;
        body_set_transform(&mut self.world, body, pos(pose.position), q);
        Ok(())
    }

    /// Overwrite a body's linear and angular velocity with exactly these values (bounded by
    /// [`crate::limits::MAX_SPEED`]). Like Box3D's setters, a non-zero velocity wakes the body.
    /// Both are checked before either is written. Refused for static bodies.
    pub fn set_velocity(&mut self, id: ObjectId, linear: [f32; 3], angular: [f32; 3]) -> Result<(), PhysicsError> {
        let body = self.moving_body(id)?;
        bounded(&linear, MAX_SPEED, "linear velocity")?;
        bounded(&angular, MAX_SPEED, "angular velocity")?;
        body_set_linear_velocity(&mut self.world, body, vec3(linear));
        body_set_angular_velocity(&mut self.world, body, vec3(angular));
        Ok(())
    }

    /// Set a body's velocities so that it reaches `target` after `dt` seconds (Box3D's
    /// `body_set_target_transform`, for hands). `dt` is in [1 ms, 1 s], so the velocities stay
    /// finite. A sleeping body is woken only if `wake`. The rotation is normalized as in a
    /// [`BodyDesc`]; Box3D turns a body by at most a quarter of pi (45 degrees) per step, so a
    /// larger turn takes more than one step. Refused for static bodies.
    pub fn set_target(&mut self, id: ObjectId, target: Pose, dt: f32, wake: bool) -> Result<(), PhysicsError> {
        let body = self.moving_body(id)?;
        position(&target.position, "target position")?;
        let rotation = unit_quat(target.rotation, "target rotation is not a unit quaternion")?;
        if !(1.0e-3..=1.0).contains(&dt) {
            return Err(PhysicsError::InvalidValue("target time step"));
        }
        let target = BoxWorldTransform { p: pos(target.position), q: quat(rotation) };
        body_set_target_transform(&mut self.world, body, target, dt, wake);
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Joints
    // ---------------------------------------------------------------------------------------

    fn anchor_body(&mut self) -> BodyId {
        if let Some(anchor) = self.anchor {
            return anchor;
        }
        let mut def = default_body_def();
        def.type_ = BodyType::Static;
        let anchor = create_body(&mut self.world, &def);
        self.anchor = Some(anchor);
        anchor
    }

    /// Resolve a joint's two ends (`None` is the world anchor), refusing a joint from a body to
    /// itself. The bound ends are resolved before the anchor is created, so a refusal creates
    /// nothing.
    fn joint_ends(&mut self, a: Option<ObjectId>, b: Option<ObjectId>) -> Result<(BodyId, BodyId), PhysicsError> {
        if a.is_some() && a == b {
            return Err(PhysicsError::SameBody);
        }
        let bound_a = a.map(|a| self.body(a)).transpose()?;
        let bound_b = b.map(|b| self.body(b)).transpose()?;
        let body_a = bound_a.unwrap_or_else(|| self.anchor_body());
        let body_b = bound_b.unwrap_or_else(|| self.anchor_body());
        Ok((body_a, body_b))
    }

    /// Create a joint under `key` (see [`JointKey`]).
    pub fn add_joint(&mut self, key: JointKey, desc: &JointDesc) -> Result<(), PhysicsError> {
        self.usable()?;
        if self.joints.contains_key(&key) {
            return Err(PhysicsError::JointExists(key.0));
        }
        let frame = |p: [f32; 3]| BoxTransform { p: vec3(p), q: quat([0.0, 0.0, 0.0, 1.0]) };
        let anchors = |a: &[f32; 3], b: &[f32; 3]| {
            bounded(a, MAX_SIZE, "anchor")?;
            bounded(b, MAX_SIZE, "anchor")
        };
        let cap = |v: f32, max: f32, what| {
            if v.is_finite() && (0.0..=max).contains(&v) { Ok(()) } else { Err(PhysicsError::InvalidValue(what)) }
        };
        // Validate everything and resolve both ends before creating anything.
        let (id, kind) = match *desc {
            JointDesc::Spherical { a, b, anchor_a, anchor_b } => {
                anchors(&anchor_a, &anchor_b)?;
                let (body_a, body_b) = self.joint_ends(Some(a), b)?;
                let mut def = default_spherical_joint_def();
                def.base.body_id_a = body_a;
                def.base.body_id_b = body_b;
                def.base.local_frame_a = frame(anchor_a);
                def.base.local_frame_b = frame(anchor_b);
                (create_spherical_joint(&mut self.world, &def), JointKind::Spherical)
            }
            JointDesc::Weld { a, b, anchor_a, anchor_b } => {
                anchors(&anchor_a, &anchor_b)?;
                let (body_a, body_b) = self.joint_ends(Some(a), b)?;
                let mut def = default_weld_joint_def();
                def.base.body_id_a = body_a;
                def.base.body_id_b = body_b;
                def.base.local_frame_a = frame(anchor_a);
                def.base.local_frame_b = frame(anchor_b);
                (create_weld_joint(&mut self.world, &def), JointKind::Weld)
            }
            JointDesc::Distance { a, b, anchor_a, anchor_b, length } => {
                anchors(&anchor_a, &anchor_b)?;
                cap(length, MAX_SIZE, "joint length")?;
                let (body_a, body_b) = self.joint_ends(Some(a), b)?;
                let mut def = default_distance_joint_def();
                def.base.body_id_a = body_a;
                def.base.body_id_b = body_b;
                def.base.local_frame_a = frame(anchor_a);
                def.base.local_frame_b = frame(anchor_b);
                def.length = length;
                (create_distance_joint(&mut self.world, &def), JointKind::Distance)
            }
            JointDesc::Grab { a, b, anchor_a, anchor_b, max_force, max_torque, spring } => {
                anchors(&anchor_a, &anchor_b)?;
                cap(max_force, MAX_PUSH, "grab force")?;
                cap(max_torque, MAX_PUSH, "grab torque")?;
                if let Some(sp) = spring {
                    cap(sp.hertz, MAX_DAMPING, "grab spring hertz")?;
                    cap(sp.damping_ratio, MAX_DAMPING, "grab spring damping ratio")?;
                    cap(sp.max_force, MAX_PUSH, "grab spring force")?;
                    cap(sp.max_torque, MAX_PUSH, "grab spring torque")?;
                }
                let (body_a, body_b) = self.joint_ends(a, Some(b))?;
                let mut def = default_motor_joint_def();
                def.base.body_id_a = body_a;
                def.base.body_id_b = body_b;
                def.base.local_frame_a = frame(anchor_a);
                def.base.local_frame_b = frame(anchor_b);
                def.max_velocity_force = max_force;
                def.max_velocity_torque = max_torque;
                if let Some(sp) = spring {
                    def.linear_hertz = sp.hertz;
                    def.linear_damping_ratio = sp.damping_ratio;
                    def.max_spring_force = sp.max_force;
                    def.angular_hertz = sp.hertz;
                    def.angular_damping_ratio = sp.damping_ratio;
                    def.max_spring_torque = sp.max_torque;
                }
                (create_motor_joint(&mut self.world, &def), JointKind::Grab)
            }
        };
        self.joints.insert(key, JointEntry { id, kind });
        Ok(())
    }

    /// The kind of the joint under `key`.
    pub fn joint_kind(&self, key: JointKey) -> Option<JointKind> {
        self.joints.get(&key).map(|j| j.kind)
    }

    /// The objects the joint under `key` joins, its `a` end and then its `b` end, as it was made: `None` for an end
    /// at the world anchor (a joint made with `b = None`, or a grab with `a = None`). `None` if there is no such
    /// joint, or while the world is poisoned.
    pub fn joint_objects(&self, key: JointKey) -> Option<(Option<ObjectId>, Option<ObjectId>)> {
        if self.poisoned {
            return None;
        }
        let joint = self.joints.get(&key)?;
        let object = |body: BodyId| self.bodies.iter().find(|&(_, &b)| b == body).map(|(&id, _)| id);
        Some((object(joint_get_body_a(&self.world, joint.id)), object(joint_get_body_b(&self.world, joint.id))))
    }

    /// Set a grab's target velocities (the hand's motion relative to `a`), waking both bodies.
    pub fn set_grab_velocity(
        &mut self,
        key: JointKey,
        linear: [f32; 3],
        angular: [f32; 3],
    ) -> Result<(), PhysicsError> {
        self.usable()?;
        bounded(&linear, MAX_SPEED, "grab velocity")?;
        bounded(&angular, MAX_SPEED, "grab velocity")?;
        let joint = *self.joints.get(&key).ok_or(PhysicsError::UnknownJoint(key.0))?;
        if joint.kind != JointKind::Grab {
            return Err(PhysicsError::NotAGrab(key.0));
        }
        motor_joint_set_linear_velocity(&mut self.world, joint.id, vec3(linear));
        motor_joint_set_angular_velocity(&mut self.world, joint.id, vec3(angular));
        joint_wake_bodies(&mut self.world, joint.id);
        Ok(())
    }

    pub fn remove_joint(&mut self, key: JointKey) -> Result<(), PhysicsError> {
        self.usable()?;
        let joint = self.joints.remove(&key).ok_or(PhysicsError::UnknownJoint(key.0))?;
        destroy_joint(&mut self.world, joint.id, true);
        Ok(())
    }

    pub fn joint_count(&self) -> usize {
        self.joints.len()
    }

    // ---------------------------------------------------------------------------------------
    // Stepping and state
    // ---------------------------------------------------------------------------------------

    /// Advance by `dt` seconds (0 < dt <= 1) in the configuration's sub-steps ([`PhysicsConfig::sub_steps`]).
    pub fn step(&mut self, dt: f32) -> Result<(), PhysicsError> {
        self.step_holding(dt, &[])
    }

    /// Advance by `dt` like [`PhysicsWorld::step`], with each listed grab's spring seeing at most its [`HoldReach`] of
    /// error during the step. The error, from the object's anchor to its place on `a`, is split into its part along
    /// gravity and the rest, and each part is shortened: the part against gravity (pulling the object up) to
    /// `against_gravity`, everything else to `reach`. A grab whose object is farther from its place pulls it as if it
    /// were that far, in the same direction and through the same point of the object, so the speed the spring gives
    /// it is bounded (an object kept from its place, pinned by a wall say, comes back as if from its reach away),
    /// while the part against gravity keeps what the spring needs to hold the object up. For the step the
    /// spring's anchor on `a` is moved that much towards the object, its position only, and put back after it, so
    /// between steps every grab keeps the anchor it was made with (its object's place on `a`): images, digests and
    /// rollbacks never see a moved one, and the bound is a function of the state at the step's start. `a`'s anchor is
    /// moved rather than the object's because `a` is the hand's kinematic body (or the world anchor), which no joint
    /// force moves, so the object is still pulled through its own anchor, with no turn from the move. In a world
    /// without gravity the whole error takes `reach`. `reaches` names grab joints, each at most once, with reaches in
    /// `0..=MAX_SIZE`; a refusal changes nothing.
    pub fn step_holding(&mut self, dt: f32, reaches: &[(JointKey, HoldReach)]) -> Result<(), PhysicsError> {
        self.usable()?;
        check_length_units()?;
        if !(dt.is_finite() && dt > 0.0 && dt <= 1.0) {
            return Err(PhysicsError::InvalidValue("step time"));
        }
        let mut keys: Vec<JointKey> = reaches.iter().map(|&(key, _)| key).collect();
        keys.sort_unstable();
        if keys.windows(2).any(|w| w[0] == w[1]) {
            return Err(PhysicsError::InvalidValue("a grab listed twice"));
        }
        let [gx, gy, gz] = self.config.gravity;
        let g = (gx * gx + gy * gy + gz * gz).sqrt();
        let down = (g > 0.0).then(|| Vec3 { x: gx / g, y: gy / g, z: gz / g });
        // Validate everything and compute every moved anchor before changing anything.
        let mut moved = Vec::new();
        for &(key, reach) in reaches {
            let valid = |r: f32| r.is_finite() && (0.0..=MAX_SIZE).contains(&r);
            if !(valid(reach.reach) && valid(reach.against_gravity)) {
                return Err(PhysicsError::InvalidValue("grab reach"));
            }
            let joint = *self.joints.get(&key).ok_or(PhysicsError::UnknownJoint(key.0))?;
            if joint.kind != JointKind::Grab {
                return Err(PhysicsError::NotAGrab(key.0));
            }
            let home = joint_get_local_frame_a(&self.world, joint.id);
            let xa = body_get_transform(&self.world, joint_get_body_a(&self.world, joint.id));
            let xb = body_get_transform(&self.world, joint_get_body_b(&self.world, joint.id));
            let place = transform_world_point(xa, home.p);
            let anchor_b = transform_world_point(xb, joint_get_local_frame_b(&self.world, joint.id).p);
            let error = sub_pos(place, anchor_b);
            // The error's part along gravity, and whether it pulls the object up (against gravity).
            let (along, across, up) = match down {
                Some(d) => {
                    let t = error.x * d.x + error.y * d.y + error.z * d.z;
                    let along = Vec3 { x: t * d.x, y: t * d.y, z: t * d.z };
                    (along, Vec3 { x: error.x - along.x, y: error.y - along.y, z: error.z - along.z }, t < 0.0)
                }
                None => (Vec3 { x: 0.0, y: 0.0, z: 0.0 }, error, false),
            };
            let a = beyond(along, if up { reach.against_gravity } else { reach.reach });
            let c = beyond(across, reach.reach);
            if a.x != 0.0 || a.y != 0.0 || a.z != 0.0 || c.x != 0.0 || c.y != 0.0 || c.z != 0.0 {
                let excess = inv_rotate_vector(xa.q, Vec3 { x: a.x + c.x, y: a.y + c.y, z: a.z + c.z });
                let p = Vec3 { x: home.p.x - excess.x, y: home.p.y - excess.y, z: home.p.z - excess.z };
                moved.push((joint.id, home, BoxTransform { p, q: home.q }));
            }
        }
        for &(id, _, frame) in &moved {
            joint_set_local_frame_a(&mut self.world, id, frame);
        }
        self.world.step(dt, self.config.sub_steps);
        for &(id, home, _) in &moved {
            joint_set_local_frame_a(&mut self.world, id, home);
        }
        Ok(())
    }

    /// The speed `spring` asks for per metre of error, 1/s, in a step of `dt`: Box3D's soft-constraint bias rate,
    /// `omega / (2 zeta + h omega)` at the configured sub-step `h = dt / sub_steps`. A grab whose object is `d` from
    /// its place pulls it towards it at no more than about this times `d`.
    pub fn grab_spring_rate(&self, spring: GrabSpring, dt: f32) -> f32 {
        make_soft(spring.hertz, spring.damping_ratio, dt / self.config.sub_steps as f32).bias_rate
    }

    /// Whether static geometry lies in the way of `object` moving to `to`, for a placement that must not carry a held
    /// object through a wall (world-model.md 8.1). Each of the object's shapes is swept by the move of its own centre,
    /// keeping its rotation, with Box3D's mover cast against static shapes only: a sphere or capsule as itself, a hull
    /// (a box) as the largest capsule inside it, about the hull's centre along the longest side of its bounding box.
    /// The mover cast passes a shape the object already touches or overlaps unless the move goes into it, so an object
    /// resting on the floor may slide along it. The answer is only yes or no, so the order in which Box3D visits shapes
    /// cannot change it.
    pub fn static_in_way(&self, object: ObjectId, to: &Pose) -> Result<bool, PhysicsError> {
        let q = exact_rotation(to)?;
        let body = self.live_body(object)?;
        if self.world.locked {
            return Err(PhysicsError::Locked);
        }
        let from = body_get_transform(&self.world, body);
        let to = BoxWorldTransform { p: pos(to.position), q };
        let filter = default_query_filter();
        let world = &self.world;
        let count = usize::try_from(body_get_shape_count(world, body)).unwrap_or(0);
        for shape in body_get_shapes(world, body, count) {
            let Some((c1, c2, radius)) = mover_of(world, shape) else {
                continue;
            };
            let mid = Vec3 { x: 0.5 * (c1.x + c2.x), y: 0.5 * (c1.y + c2.y), z: 0.5 * (c1.z + c2.z) };
            let start = transform_world_point(from, mid);
            let translation = sub_pos(transform_world_point(to, mid), start);
            if translation.x == 0.0 && translation.y == 0.0 && translation.z == 0.0 {
                continue;
            }
            let mover = Capsule {
                center1: rotate_vector(from.q, Vec3 { x: c1.x - mid.x, y: c1.y - mid.y, z: c1.z - mid.z }),
                center2: rotate_vector(from.q, Vec3 { x: c2.x - mid.x, y: c2.y - mid.y, z: c2.z - mid.z }),
                radius,
            };
            let mut only_static = |id: ShapeId| body_get_type(world, shape_get_body(world, id)) == BodyType::Static;
            if world_cast_mover(world, start, &mover, translation, &filter, Some(&mut only_static)) < 1.0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The anchor of the grab under `key` on its `a` end, in `a`'s frame: where it holds its object (the object's
    /// anchor is pulled there).
    pub fn grab_anchor(&self, key: JointKey) -> Result<[f32; 3], PhysicsError> {
        if self.poisoned {
            return Err(PhysicsError::Poisoned);
        }
        let joint = *self.joints.get(&key).ok_or(PhysicsError::UnknownJoint(key.0))?;
        if joint.kind != JointKind::Grab {
            return Err(PhysicsError::NotAGrab(key.0));
        }
        let p = joint_get_local_frame_a(&self.world, joint.id).p;
        Ok([p.x, p.y, p.z])
    }

    /// Box3D's state hash: every body's transform and velocities (the C reference's hash,
    /// which the spikes' traces record). Refused while poisoned.
    pub fn box3d_hash(&self) -> Result<u64, PhysicsError> {
        self.usable()?;
        Ok(hash_world_state(&self.world))
    }

    /// The per-frame state hash (design 9.1): [`Self::box3d_hash`] (body transforms and
    /// velocities) plus the binding and joint tables. XXH64 (pe-core), so it composes with the
    /// sim store's hash. It does not cover sleep timers, contact caches, warm starts, islands or
    /// the settings; [`Self::check_hash`] covers the whole state. Refused while poisoned.
    pub fn state_hash(&self) -> Result<u64, PhysicsError> {
        let mut h = pe_core::hash::Hasher::new(0);
        h.write_u64(self.box3d_hash()?);
        h.write_u64(self.bodies.len() as u64);
        for (id, body) in &self.bodies {
            h.write(&id.to_u128().to_le_bytes());
            h.write_u32(body.index1 as u32);
            h.write_u32(body.generation as u32);
        }
        h.write_u64(self.joints.len() as u64);
        for (key, joint) in &self.joints {
            h.write_u64(key.0);
            h.write_u32(joint.id.index1 as u32);
            h.write_u32(joint.id.generation as u32);
            h.write_u32(joint.kind as u32);
        }
        Ok(h.finish())
    }

    // ---------------------------------------------------------------------------------------
    // Sim store
    // ---------------------------------------------------------------------------------------

    /// Write every bound body's pose into its object's `Transform` in `store`. The physics
    /// world's origin is the corner of level-0 chunk `origin`, so a body at physics position
    /// `p` is at world position `origin + p`.
    ///
    /// * A root object (no transformed ancestor) gets that position rebased into its own chunk.
    /// * An object under a frame gets the local transform that puts it there in its frame's
    ///   current world transform. Objects are written ancestors first (by depth, then id), so a
    ///   bound frame has moved before its children are placed in it.
    /// * Scale is kept as it is: physics does not read it (colliders come from [`BodyDesc`]).
    /// * Bound objects without a `Transform` are skipped.
    /// * An object whose pose the sim store refuses (outside its limits, for example under a
    ///   frame scaled near zero) keeps its transform and is listed in
    ///   [`WriteBack::refused`]; the others are still written (review P22R-7). Each transform
    ///   is written whole or not at all.
    pub fn write_root_transforms(&self, store: &mut SimStore, origin: [i64; 3]) -> Result<WriteBack, PhysicsError> {
        if self.poisoned {
            return Err(PhysicsError::Poisoned);
        }
        let depth = |store: &SimStore, mut id: ObjectId| {
            let mut d = 0u32;
            while let Some(p) = store.parent(id) {
                d += 1;
                id = p;
            }
            d
        };
        let mut order: Vec<(u32, ObjectId, BodyId)> = self
            .bodies
            .iter()
            .filter(|&(&id, _)| store.exists(id) && store.has::<Transform>(id))
            .map(|(&id, &body)| (depth(store, id), id, body))
            .collect();
        order.sort_unstable_by_key(|&(d, id, _)| (d, id));

        let mut out = WriteBack::default();
        for &(_, id, body) in &order {
            match self.place(store, id, body, origin) {
                Some(t) => {
                    *store.get_mut::<Transform>(id).expect("filtered above") = t;
                    out.written += 1;
                }
                None => out.refused.push(id),
            }
        }
        Ok(out)
    }

    /// The transform that puts `id` at `body`'s pose (see [`Self::write_root_transforms`]).
    fn place(&self, store: &SimStore, id: ObjectId, body: BodyId, origin: [i64; 3]) -> Option<Transform> {
        let current = *store.get::<Transform>(id).expect("checked by the caller");
        let xf = body_get_transform(&self.world, body);
        let p = [xf.p.x as f64, xf.p.y as f64, xf.p.z as f64];
        let rot = [xf.q.v.x, xf.q.v.y, xf.q.v.z, xf.q.s];
        let frame = store.frame_of(id);
        let t = match frame {
            None => Transform { chunk: origin, pos: p, rot, scale: current.scale, _pad: 0 }.rebased(),
            Some(f) => {
                let fw = store.world_transform(f).expect("a frame has a Transform");
                // `origin`'s corner moved by the body's region-frame position: the wall's own operations.
                let pos = pe_core::Pos::at_chunk(origin).ok()?.checked_add(pe_core::Offset(p)).ok()?;
                let target = WorldTransform { pos, rot, scale: [1.0; 3] };
                Transform { scale: current.scale, ..target.local_in(Some(&fw)) }
            }
        };
        t.validate(frame.is_some()).is_ok().then_some(t)
    }
}

/// What [`PhysicsWorld::write_root_transforms`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use = "refused objects keep their old transforms"]
pub struct WriteBack {
    /// Transforms written.
    pub written: usize,
    /// Objects whose pose the sim store refused, in write order; their transforms are unchanged.
    pub refused: Vec<ObjectId>,
}

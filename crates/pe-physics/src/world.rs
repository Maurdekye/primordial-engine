//! [`PhysicsWorld`]: one Box3D world, its bodies bound to sim objects, and its joints.

use std::collections::BTreeMap;

use box3d_rust::body::{
    body_apply_angular_impulse, body_apply_force, body_apply_force_to_center, body_apply_linear_impulse,
    body_apply_linear_impulse_to_center, body_apply_torque, body_get_angular_velocity,
    body_get_linear_velocity, body_get_transform, body_is_awake, body_is_valid, create_body, destroy_body,
};
use box3d_rust::core::get_length_units_per_meter;
use box3d_rust::geometry::{Capsule, Sphere};
use box3d_rust::hull::{create_hull, make_box_hull};
use box3d_rust::id::{BodyId, JointId};
use box3d_rust::joint::{
    create_distance_joint, create_motor_joint, create_spherical_joint, create_weld_joint, destroy_joint,
    joint_is_valid, joint_wake_bodies, motor_joint_set_angular_velocity, motor_joint_set_linear_velocity,
};
use box3d_rust::math_functions::{Pos, Quat, Transform as BoxTransform, Vec3};
use box3d_rust::recording::hash_world_state;
use box3d_rust::shape::{create_capsule_shape, create_hull_shape, create_sphere_shape};
use box3d_rust::types::{
    default_body_def, default_distance_joint_def, default_motor_joint_def, default_shape_def,
    default_spherical_joint_def, default_weld_joint_def, BodyType,
};
use box3d_rust::world::{world_get_awake_body_count, world_set_worker_count, World};
use pe_core::ids::ObjectId;
use pe_sim::{SimStore, Transform, WorldTransform};

use crate::config::PhysicsConfig;
use crate::desc::limits::{MAX_DAMPING, MAX_PUSH, MAX_SIZE, MAX_SPEED};
use crate::desc::{bounded, position, BodyDesc, BodyKind, Geometry};
use crate::error::PhysicsError;

pub(crate) fn vec3(v: [f32; 3]) -> Vec3 {
    Vec3 { x: v[0], y: v[1], z: v[2] }
}

/// Box3D's `Pos` is `f32` unless the fork's `double-precision` feature is on; the engine API is
/// `f64` either way.
pub(crate) fn pos(v: [f64; 3]) -> Pos {
    Pos { x: v[0] as _, y: v[1] as _, z: v[2] as _ }
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

    /// Set a grab's target velocities (the hand's motion relative to `a`), waking both bodies.
    pub fn set_grab_velocity(&mut self, key: JointKey, linear: [f32; 3], angular: [f32; 3]) -> Result<(), PhysicsError> {
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

    /// Advance by `dt` seconds (0 < dt <= 1) in [`crate::SUBSTEPS`] sub-steps.
    pub fn step(&mut self, dt: f32) -> Result<(), PhysicsError> {
        self.usable()?;
        check_length_units()?;
        if !(dt.is_finite() && dt > 0.0 && dt <= 1.0) {
            return Err(PhysicsError::InvalidValue("step time"));
        }
        self.world.step(dt, crate::config::SUBSTEPS);
        Ok(())
    }

    /// Box3D's state hash: every body's transform and velocities (the C reference's hash,
    /// which the spikes' traces record).
    pub fn box3d_hash(&self) -> u64 {
        hash_world_state(&self.world)
    }

    /// The per-frame state hash (design 9.1): [`Self::box3d_hash`] (body transforms and
    /// velocities) plus the binding and joint tables. XXH64 (pe-core), so it composes with the
    /// sim store's hash. It does not cover sleep timers, contact caches, warm starts, islands or
    /// the settings; [`Self::check_hash`] covers the whole state.
    pub fn state_hash(&self) -> u64 {
        let mut h = pe_core::hash::Hasher::new(0);
        h.write_u64(self.box3d_hash());
        h.write_u64(self.bodies.len() as u64);
        for (id, body) in &self.bodies {
            h.write_u32(id.creator);
            h.write_u32(id.counter);
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
        h.finish()
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
    ///
    /// All or nothing: if any pose is refused, every transform written so far is put back.
    /// Returns how many transforms were written.
    pub fn write_root_transforms(&self, store: &mut SimStore, origin: [i64; 3]) -> Result<usize, PhysicsError> {
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

        let mut old: Vec<(ObjectId, Transform)> = Vec::with_capacity(order.len());
        for &(_, id, body) in &order {
            match self.place(store, id, body, origin) {
                Ok(t) => {
                    let slot = store.get_mut::<Transform>(id).expect("filtered above");
                    old.push((id, *slot));
                    *slot = t;
                }
                Err(e) => {
                    for (id, t) in old.into_iter().rev() {
                        *store.get_mut::<Transform>(id).expect("written above") = t;
                    }
                    return Err(e);
                }
            }
        }
        Ok(old.len())
    }

    /// The transform that puts `id` at `body`'s pose (see [`Self::write_root_transforms`]).
    fn place(&self, store: &SimStore, id: ObjectId, body: BodyId, origin: [i64; 3]) -> Result<Transform, PhysicsError> {
        let current = *store.get::<Transform>(id).expect("checked by the caller");
        let xf = body_get_transform(&self.world, body);
        let p = [xf.p.x as f64, xf.p.y as f64, xf.p.z as f64];
        let rot = [xf.q.v.x, xf.q.v.y, xf.q.v.z, xf.q.s];
        let frame = store.frame_of(id);
        let t = match frame {
            None => Transform { chunk: origin, pos: p, rot, scale: current.scale, _pad: 0 }.rebased(),
            Some(f) => {
                let fw = store.world_transform(f).expect("a frame has a Transform");
                let target = WorldTransform { pos: pe_core::coords::WorldPos::new(origin, p), rot, scale: [1.0; 3] };
                Transform { scale: current.scale, ..target.local_in(Some(&fw)) }
            }
        };
        if t.validate(frame.is_some()).is_err() {
            return Err(PhysicsError::InvalidValue("body pose outside the sim store's limits"));
        }
        Ok(t)
    }
}

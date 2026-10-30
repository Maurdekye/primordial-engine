//! [`PhysicsWorld`]: one Box3D world, its bodies bound to sim objects, and its joints.

use std::collections::BTreeMap;

use box3d_rust::body::{
    body_apply_angular_impulse, body_apply_force, body_apply_force_to_center, body_apply_linear_impulse,
    body_apply_linear_impulse_to_center, body_apply_torque, body_get_angular_velocity,
    body_get_linear_velocity, body_get_transform, body_is_awake, body_is_valid, create_body, destroy_body,
};
use box3d_rust::geometry::{Capsule, Sphere};
use box3d_rust::hull::{create_hull, make_box_hull};
use box3d_rust::id::{BodyId, JointId};
use box3d_rust::joint::{
    create_distance_joint, create_motor_joint, create_spherical_joint, create_weld_joint, destroy_joint,
    joint_is_valid, motor_joint_set_angular_velocity, motor_joint_set_linear_velocity,
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
use pe_sim::{SimStore, Transform};

use crate::config::PhysicsConfig;
use crate::desc::{BodyDesc, BodyKind, Geometry};
use crate::error::PhysicsError;

pub(crate) fn vec3(v: [f32; 3]) -> Vec3 {
    Vec3 { x: v[0], y: v[1], z: v[2] }
}

pub(crate) fn pos(v: [f32; 3]) -> Pos {
    Pos { x: v[0] as _, y: v[1] as _, z: v[2] as _ }
}

fn quat(q: [f32; 4]) -> Quat {
    Quat { v: Vec3 { x: q[0], y: q[1], z: q[2] }, s: q[3] }
}

fn finite(v: &[f32], what: &'static str) -> Result<(), PhysicsError> {
    if v.iter().all(|c| c.is_finite()) { Ok(()) } else { Err(PhysicsError::InvalidValue(what)) }
}

/// A body's pose, in the physics world's frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub position: [f64; 3],
    /// Unit quaternion `[x, y, z, w]`.
    pub rotation: [f32; 4],
}

/// One player's push on an object, applied at the start of a frame (design 7: interactions
/// become forces, impulses or joints). Vectors are in the physics world's frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Push {
    /// A linear impulse (N s) through the centre of mass.
    Impulse([f32; 3]),
    /// A linear impulse at a world point; also changes the spin.
    ImpulseAt { impulse: [f32; 3], point: [f32; 3] },
    /// An angular impulse (N m s).
    AngularImpulse([f32; 3]),
    /// A force (N) through the centre of mass, for this frame's step only.
    Force([f32; 3]),
    /// A force at a world point, for this frame's step only.
    ForceAt { force: [f32; 3], point: [f32; 3] },
    /// A torque (N m), for this frame's step only.
    Torque([f32; 3]),
}

/// Engine handle of a joint. Keys are issued in order and never reused within a world.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JointKey(pub u32);

/// A joint between two bound objects (or one object and the world, `b = None`). Anchors are
/// in each body's local frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JointDesc {
    /// Ball and socket: the anchors stay together.
    Spherical { a: ObjectId, b: Option<ObjectId>, anchor_a: [f32; 3], anchor_b: [f32; 3] },
    /// The two frames stay fixed relative to each other.
    Weld { a: ObjectId, b: Option<ObjectId>, anchor_a: [f32; 3], anchor_b: [f32; 3] },
    /// The anchors stay `length` apart.
    Distance { a: ObjectId, b: Option<ObjectId>, anchor_a: [f32; 3], anchor_b: [f32; 3], length: f32 },
    /// A grab: drives `b`'s velocity towards `a`'s plus a target, with a force cap, like a
    /// hand holding an object (D72 grabs). `a = None` uses a static world anchor.
    Grab { a: Option<ObjectId>, b: ObjectId, max_force: f32, max_torque: f32 },
}

/// A Box3D world whose bodies belong to sim objects.
///
/// * Bodies are created and destroyed only through this type, keyed by [`ObjectId`]. The
///   binding table is sorted by id, so every iteration over it is in the same order on every
///   peer.
/// * [`PhysicsWorld::raw`] / [`PhysicsWorld::raw_mut`] expose the Box3D world for test scenes and
///   tools. Bodies created through them are not bound; do not mix the two in engine code.
/// * Length units are process-wide in Box3D (`set_length_units_per_meter`), so every world in a
///   process shares them; the engine uses metres.
pub struct PhysicsWorld {
    pub(crate) world: World,
    pub(crate) config: PhysicsConfig,
    pub(crate) bodies: BTreeMap<ObjectId, BodyId>,
    pub(crate) joints: BTreeMap<JointKey, JointId>,
    pub(crate) next_joint: u32,
    /// The static body grabs with no `a` attach to (created on first use).
    pub(crate) anchor: Option<BodyId>,
    pub(crate) images: crate::image::ImageRegistry,
}

impl PhysicsWorld {
    pub fn new(config: PhysicsConfig) -> PhysicsWorld {
        PhysicsWorld {
            world: World::new(&config.world_def()),
            config,
            bodies: BTreeMap::new(),
            joints: BTreeMap::new(),
            next_joint: 1,
            anchor: None,
            images: crate::image::ImageRegistry::default(),
        }
    }

    pub fn config(&self) -> &PhysicsConfig {
        &self.config
    }

    /// Change the worker-thread count. The results do not change (D2).
    pub fn set_workers(&mut self, workers: i32) {
        self.config.workers = workers;
        world_set_worker_count(&mut self.world, workers);
    }

    /// The Box3D world, read-only.
    pub fn raw(&self) -> &World {
        &self.world
    }

    /// The Box3D world, for test scenes and tools; see the type docs.
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
        if self.world.locked {
            return Err(PhysicsError::Locked);
        }
        desc.validate()?;
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
        def.rotation = quat(desc.rotation);
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
        if self.world.locked {
            return Err(PhysicsError::Locked);
        }
        let body = self.bodies.remove(&id).ok_or(PhysicsError::NotBound(id))?;
        destroy_body(&mut self.world, body);
        // Box3D destroyed the attached joints; forget their keys.
        let world = &self.world;
        self.joints.retain(|_, joint| joint_is_valid(world, *joint));
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

    /// The Box3D body of `id` (for tools; engine code should not need it).
    pub fn body_id(&self, id: ObjectId) -> Option<BodyId> {
        self.bodies.get(&id).copied()
    }

    pub fn pose(&self, id: ObjectId) -> Result<Pose, PhysicsError> {
        let xf = body_get_transform(&self.world, self.body(id)?);
        Ok(Pose {
            position: [xf.p.x as f64, xf.p.y as f64, xf.p.z as f64],
            rotation: [xf.q.v.x, xf.q.v.y, xf.q.v.z, xf.q.s],
        })
    }

    /// Linear and angular velocity.
    pub fn velocity(&self, id: ObjectId) -> Result<([f32; 3], [f32; 3]), PhysicsError> {
        let body = self.body(id)?;
        let v = body_get_linear_velocity(&self.world, body);
        let w = body_get_angular_velocity(&self.world, body);
        Ok(([v.x, v.y, v.z], [w.x, w.y, w.z]))
    }

    pub fn is_awake(&self, id: ObjectId) -> Result<bool, PhysicsError> {
        Ok(body_is_awake(&self.world, self.body(id)?))
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
        let body = self.body(id)?;
        let w = &mut self.world;
        match push {
            Push::Impulse(i) => {
                finite(&i, "impulse")?;
                body_apply_linear_impulse_to_center(w, body, vec3(i), true);
            }
            Push::ImpulseAt { impulse, point } => {
                finite(&impulse, "impulse")?;
                finite(&point, "impulse point")?;
                body_apply_linear_impulse(w, body, vec3(impulse), pos(point), true);
            }
            Push::AngularImpulse(i) => {
                finite(&i, "angular impulse")?;
                body_apply_angular_impulse(w, body, vec3(i), true);
            }
            Push::Force(f) => {
                finite(&f, "force")?;
                body_apply_force_to_center(w, body, vec3(f), true);
            }
            Push::ForceAt { force, point } => {
                finite(&force, "force")?;
                finite(&point, "force point")?;
                body_apply_force(w, body, vec3(force), pos(point), true);
            }
            Push::Torque(t) => {
                finite(&t, "torque")?;
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

    fn joint_end(&mut self, id: Option<ObjectId>) -> Result<BodyId, PhysicsError> {
        match id {
            Some(id) => self.body(id),
            None => Ok(self.anchor_body()),
        }
    }

    /// Create a joint. Its key is the next in this world's sequence.
    pub fn add_joint(&mut self, desc: &JointDesc) -> Result<JointKey, PhysicsError> {
        if self.world.locked {
            return Err(PhysicsError::Locked);
        }
        let frame = |p: [f32; 3]| BoxTransform { p: vec3(p), q: quat([0.0, 0.0, 0.0, 1.0]) };
        // Validate and resolve both ends before creating anything.
        let joint = match *desc {
            JointDesc::Spherical { a, b, anchor_a, anchor_b } => {
                finite(&anchor_a, "anchor")?;
                finite(&anchor_b, "anchor")?;
                let body_a = self.body(a)?;
                let body_b = self.joint_end(b)?;
                let mut def = default_spherical_joint_def();
                def.base.body_id_a = body_a;
                def.base.body_id_b = body_b;
                def.base.local_frame_a = frame(anchor_a);
                def.base.local_frame_b = frame(anchor_b);
                create_spherical_joint(&mut self.world, &def)
            }
            JointDesc::Weld { a, b, anchor_a, anchor_b } => {
                finite(&anchor_a, "anchor")?;
                finite(&anchor_b, "anchor")?;
                let body_a = self.body(a)?;
                let body_b = self.joint_end(b)?;
                let mut def = default_weld_joint_def();
                def.base.body_id_a = body_a;
                def.base.body_id_b = body_b;
                def.base.local_frame_a = frame(anchor_a);
                def.base.local_frame_b = frame(anchor_b);
                create_weld_joint(&mut self.world, &def)
            }
            JointDesc::Distance { a, b, anchor_a, anchor_b, length } => {
                finite(&anchor_a, "anchor")?;
                finite(&anchor_b, "anchor")?;
                if !(length.is_finite() && length >= 0.0) {
                    return Err(PhysicsError::InvalidValue("joint length"));
                }
                let body_a = self.body(a)?;
                let body_b = self.joint_end(b)?;
                let mut def = default_distance_joint_def();
                def.base.body_id_a = body_a;
                def.base.body_id_b = body_b;
                def.base.local_frame_a = frame(anchor_a);
                def.base.local_frame_b = frame(anchor_b);
                def.length = length;
                create_distance_joint(&mut self.world, &def)
            }
            JointDesc::Grab { a, b, max_force, max_torque } => {
                if !(max_force.is_finite() && max_force >= 0.0 && max_torque.is_finite() && max_torque >= 0.0) {
                    return Err(PhysicsError::InvalidValue("grab force or torque"));
                }
                let body_b = self.body(b)?;
                let body_a = self.joint_end(a)?;
                let mut def = default_motor_joint_def();
                def.base.body_id_a = body_a;
                def.base.body_id_b = body_b;
                def.max_velocity_force = max_force;
                def.max_velocity_torque = max_torque;
                create_motor_joint(&mut self.world, &def)
            }
        };
        let key = JointKey(self.next_joint);
        self.next_joint += 1;
        self.joints.insert(key, joint);
        Ok(key)
    }

    /// Set a grab's target velocities (the hand's motion relative to `a`).
    pub fn set_grab_velocity(&mut self, key: JointKey, linear: [f32; 3], angular: [f32; 3]) -> Result<(), PhysicsError> {
        finite(&linear, "grab velocity")?;
        finite(&angular, "grab velocity")?;
        let joint = *self.joints.get(&key).ok_or(PhysicsError::UnknownJoint(key.0))?;
        motor_joint_set_linear_velocity(&mut self.world, joint, vec3(linear));
        motor_joint_set_angular_velocity(&mut self.world, joint, vec3(angular));
        Ok(())
    }

    pub fn remove_joint(&mut self, key: JointKey) -> Result<(), PhysicsError> {
        if self.world.locked {
            return Err(PhysicsError::Locked);
        }
        let joint = self.joints.remove(&key).ok_or(PhysicsError::UnknownJoint(key.0))?;
        destroy_joint(&mut self.world, joint, true);
        Ok(())
    }

    pub fn joint_count(&self) -> usize {
        self.joints.len()
    }

    // ---------------------------------------------------------------------------------------
    // Stepping and state
    // ---------------------------------------------------------------------------------------

    /// Advance by `dt` seconds in [`crate::SUBSTEPS`] sub-steps.
    pub fn step(&mut self, dt: f32) {
        self.world.step(dt, crate::config::SUBSTEPS);
    }

    /// Box3D's state hash: every body's transform and velocities (the C reference's hash,
    /// which the spikes' traces record).
    pub fn box3d_hash(&self) -> u64 {
        hash_world_state(&self.world)
    }

    /// Hash of the whole physics state the engine owns: [`Self::box3d_hash`] plus the binding
    /// and joint tables. XXH64 (pe-core), so it composes with the sim store's hash.
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
            h.write_u32(key.0);
            h.write_u32(joint.index1 as u32);
            h.write_u32(joint.generation as u32);
        }
        h.write_u32(self.next_joint);
        h.finish()
    }

    // ---------------------------------------------------------------------------------------
    // Sim store
    // ---------------------------------------------------------------------------------------

    /// Write every bound body's pose into its object's `Transform` in `store`, for objects that
    /// are roots of the transform hierarchy (no ancestor with a `Transform`). The physics
    /// world's origin is the corner of level-0 chunk `origin`, so a body at physics position
    /// `p` lands at `origin + p`, rebased into its own chunk. Objects with a transformed
    /// ancestor are skipped: their pose is relative to that ancestor and owned by it. Scale is
    /// kept. Returns how many transforms were written.
    pub fn write_root_transforms(&self, store: &mut SimStore, origin: [i64; 3]) -> Result<usize, PhysicsError> {
        let mut written = 0;
        for (&id, &body) in &self.bodies {
            if !store.exists(id) || store.frame_of(id).is_some() {
                continue;
            }
            let Some(old) = store.get::<Transform>(id).copied() else {
                continue;
            };
            let xf = body_get_transform(&self.world, body);
            let t = Transform {
                chunk: origin,
                pos: [xf.p.x as f64, xf.p.y as f64, xf.p.z as f64],
                rot: [xf.q.v.x, xf.q.v.y, xf.q.v.z, xf.q.s],
                scale: old.scale,
                _pad: 0,
            }
            .rebased();
            if t.validate(false).is_err() {
                return Err(PhysicsError::InvalidValue("body pose outside the sim store's limits"));
            }
            *store.get_mut::<Transform>(id).expect("checked above") = t;
            written += 1;
        }
        Ok(written)
    }
}

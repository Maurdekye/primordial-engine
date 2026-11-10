//! Per-body physics digests: the half of the physics in a root that localizes a divergence to one body
//! (docs/spec/replica.md 5.1).
//!
//! A digest covers what one body carries: its pose bits, its velocities, its awake flag and sleep timer, and the
//! touching contacts it shares with every body of a higher object id, manifold by manifold, point by point. Each
//! pair is covered once, by its lower id. A contact with a body that has no object id (the grab anchor, which is
//! not bound) is in neither side's digest; [`PhysicsWorld::check_hash`] covers it.
//!
//! **Equal digests do not mean equal futures.** A step also reads state that belongs to no body:
//! - the id pools;
//! - the constraint graph's colours;
//! - solver-set membership and order;
//! - islands and the broad-phase trees;
//! - the caches of contacts that are not touching.
//!
//! Only [`PhysicsWorld::check_hash`] covers all of it. A replica's root carries both: the digests to name the bodies
//! that differ, the check hash to make the root complete.

use box3d_rust::body::{
    body_get_angular_velocity, body_get_contact_capacity, body_get_contact_data, body_get_linear_velocity,
    body_get_transform, body_is_awake,
};
use box3d_rust::core::NULL_INDEX;
use box3d_rust::events::ContactData;
use box3d_rust::math_functions::Vec3;
use box3d_rust::world::World;
use pe_core::ids::ObjectId;

use crate::error::PhysicsError;
use crate::world::PhysicsWorld;

/// BLAKE3 key-derivation context, so a body digest cannot be confused with another BLAKE3 use.
const BODY_DIGEST_CONTEXT: &str = "Primordial Engine 2026-11-10 pe-physics body digest";

/// The 256-bit digest of one body's physics state (see the module documentation for what it covers).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BodyDigest(pub [u8; 32]);

impl std::fmt::Display for BodyDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl std::fmt::Debug for BodyDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BodyDigest({self})")
    }
}

struct Writer(blake3::Hasher);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.update(&[v]);
    }
    fn u32(&mut self, v: u32) {
        self.0.update(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.update(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.update(&v.to_bits().to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.0.update(&v.to_bits().to_le_bytes());
    }
    fn vec3(&mut self, v: Vec3) {
        self.f32(v.x);
        self.f32(v.y);
        self.f32(v.z);
    }
    fn id(&mut self, id: ObjectId) {
        self.0.update(&id.to_u128().to_le_bytes());
    }
}

/// The position of shape `shape` in its body's shape list: the order the body's shapes were created in, which is
/// the same on every replica that built the body the same way.
fn shape_position(world: &World, shape: usize) -> u32 {
    let body = world.shapes[shape].body_id as usize;
    let mut s = world.bodies[body].head_shape_id;
    let mut position = 0;
    while s != NULL_INDEX {
        if s as usize == shape {
            return position;
        }
        position += 1;
        s = world.shapes[s as usize].next_shape_id;
    }
    u32::MAX
}

impl PhysicsWorld {
    /// Every bound body's [`BodyDigest`], in id order.
    ///
    /// The contacts a body's digest covers are its touching contacts with bodies of a higher id. They are taken in a
    /// canonical order: the other body's id, then this body's shape position, then the other's. Each carries:
    /// - which side this body is (Box3D's shape A or B);
    /// - per manifold: the normal and the friction, twist and rolling impulses;
    /// - per point: the anchors, separations, impulses, normal velocity, feature id, triangle index and the
    ///   persisted flag.
    ///
    /// Refused while poisoned or stepping.
    pub fn body_digests(&self) -> Result<Vec<(ObjectId, BodyDigest)>, PhysicsError> {
        self.usable()?;
        let world = &self.world;
        // Box3D body index to object id. Bodies that are not bound (the grab anchor) have none.
        let mut owner: Vec<Option<ObjectId>> = vec![None; world.bodies.len()];
        for (&id, body) in &self.bodies {
            if let Some(slot) = owner.get_mut((body.index1 - 1) as usize) {
                *slot = Some(id);
            }
        }
        let mut out = Vec::with_capacity(self.bodies.len());
        for (&id, &body) in &self.bodies {
            let index = (body.index1 - 1) as usize;
            let mut w = Writer(blake3::Hasher::new_derive_key(BODY_DIGEST_CONTEXT));
            w.id(id);
            let xf = body_get_transform(world, body);
            w.f64(xf.p.x as f64);
            w.f64(xf.p.y as f64);
            w.f64(xf.p.z as f64);
            w.vec3(xf.q.v);
            w.f32(xf.q.s);
            w.vec3(body_get_linear_velocity(world, body));
            w.vec3(body_get_angular_velocity(world, body));
            w.u8(body_is_awake(world, body) as u8);
            w.f32(world.bodies[index].sleep_time);

            let capacity = body_get_contact_capacity(world, body).max(0) as usize;
            let mut contacts: Vec<(ObjectId, u32, u32, bool, ContactData)> = Vec::new();
            for cd in body_get_contact_data(world, body, capacity) {
                let shape_a = (cd.shape_id_a.index1 - 1) as usize;
                let shape_b = (cd.shape_id_b.index1 - 1) as usize;
                let is_a = world.shapes[shape_a].body_id as usize == index;
                let (own, other) = if is_a { (shape_a, shape_b) } else { (shape_b, shape_a) };
                let Some(other_id) = owner.get(world.shapes[other].body_id as usize).copied().flatten() else {
                    continue;
                };
                if other_id <= id {
                    continue;
                }
                contacts.push((other_id, shape_position(world, own), shape_position(world, other), is_a, cd));
            }
            contacts.sort_by_key(|c| (c.0, c.1, c.2));
            w.u32(contacts.len() as u32);
            for (other_id, own_pos, other_pos, is_a, cd) in &contacts {
                w.id(*other_id);
                w.u32(*own_pos);
                w.u32(*other_pos);
                w.u8(*is_a as u8);
                w.u32(cd.manifolds.len() as u32);
                for m in &cd.manifolds {
                    w.vec3(m.normal);
                    w.f32(m.twist_impulse);
                    w.vec3(m.friction_impulse);
                    w.vec3(m.rolling_impulse);
                    w.i32(m.point_count);
                    for p in m.points.iter().take(m.point_count.clamp(0, m.points.len() as i32) as usize) {
                        w.vec3(p.anchor_a);
                        w.vec3(p.anchor_b);
                        w.f32(p.separation);
                        w.f32(p.base_separation);
                        w.f32(p.normal_impulse);
                        w.f32(p.total_normal_impulse);
                        w.f32(p.normal_velocity);
                        w.u32(p.feature_id);
                        w.i32(p.triangle_index);
                        w.u8(p.persisted as u8);
                    }
                }
            }
            out.push((id, BodyDigest(*w.0.finalize().as_bytes())));
        }
        Ok(out)
    }
}

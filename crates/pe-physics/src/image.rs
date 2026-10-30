//! World images: the physics part of a rollback snapshot and of a state transfer (6.1, 9.5).
//!
//! An image is Box3D's own world serialization plus the engine's binding and joint tables.
//! Geometry (hulls, meshes) is not repeated in every image: it is interned once per world in a
//! registry, and images refer to it by index. A peer that restores an image into a fresh world
//! (a late joiner) also needs the registry, as [`GeometryBlobs`].

use box3d_rust::id::{BodyId, JointId};
use box3d_rust::recording::{
    deserialize_into_shell, serialize_world, GeometryKind, GeometryRegistry, RecBuffer, RegistrySlot,
};
use box3d_rust::body::body_is_valid;
use box3d_rust::joint::joint_is_valid;
use box3d_rust::world::World;
use pe_core::ids::ObjectId;

use crate::config::PhysicsConfig;
use crate::error::PhysicsError;
use crate::world::{JointKey, PhysicsWorld};

/// Per-world geometry registry and the reader slots built from it.
#[derive(Default)]
pub(crate) struct ImageRegistry {
    registry: GeometryRegistry,
    slots: Vec<RegistrySlot>,
}

impl ImageRegistry {
    fn refresh_slots(&mut self) {
        if self.slots.len() != self.registry.entries.len() {
            self.slots = self.registry.to_slots();
        }
    }
}

/// The physics state of one world at one instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldImage {
    world: Vec<u8>,
    bodies: Vec<(ObjectId, BodyId)>,
    joints: Vec<(JointKey, JointId)>,
    next_joint: u32,
    anchor: Option<BodyId>,
}

/// The geometry a world's images refer to, in registry order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeometryBlobs {
    entries: Vec<(u8, Vec<u8>)>,
}

const IMAGE_MAGIC: u32 = 0x5045_5049; // "PEPI"
const IMAGE_VERSION: u32 = 1;

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_body(out: &mut Vec<u8>, b: BodyId) {
    put_u32(out, b.index1 as u32);
    out.extend_from_slice(&b.world0.to_le_bytes());
    out.extend_from_slice(&b.generation.to_le_bytes());
}

struct Reader<'a> {
    data: &'a [u8],
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], PhysicsError> {
        if self.data.len() < n {
            return Err(PhysicsError::BadImage("truncated"));
        }
        let (head, rest) = self.data.split_at(n);
        self.data = rest;
        Ok(head)
    }
    fn u32(&mut self) -> Result<u32, PhysicsError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u16(&mut self) -> Result<u16, PhysicsError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn len(&mut self) -> Result<usize, PhysicsError> {
        let n = self.u32()? as usize;
        if n > self.data.len() {
            return Err(PhysicsError::BadImage("length beyond the end"));
        }
        Ok(n)
    }
    fn body(&mut self) -> Result<BodyId, PhysicsError> {
        Ok(BodyId { index1: self.u32()? as i32, world0: self.u16()?, generation: self.u16()? })
    }
}

impl WorldImage {
    /// Box3D's world bytes (the part the spikes hash).
    pub fn world_bytes(&self) -> &[u8] {
        &self.world
    }

    /// Canonical little-endian bytes, for state transfer.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.world.len() + 16 * self.bodies.len() + 64);
        put_u32(&mut out, IMAGE_MAGIC);
        put_u32(&mut out, IMAGE_VERSION);
        put_u32(&mut out, self.world.len() as u32);
        out.extend_from_slice(&self.world);
        put_u32(&mut out, self.bodies.len() as u32);
        for &(id, body) in &self.bodies {
            put_u32(&mut out, id.creator);
            put_u32(&mut out, id.counter);
            put_body(&mut out, body);
        }
        put_u32(&mut out, self.joints.len() as u32);
        for &(key, joint) in &self.joints {
            put_u32(&mut out, key.0);
            put_u32(&mut out, joint.index1 as u32);
            out.extend_from_slice(&joint.world0.to_le_bytes());
            out.extend_from_slice(&joint.generation.to_le_bytes());
        }
        put_u32(&mut out, self.next_joint);
        match self.anchor {
            Some(b) => {
                out.push(1);
                put_body(&mut out, b);
            }
            None => out.push(0),
        }
        out
    }

    /// Parse [`Self::to_bytes`]. Checks the framing and that the tables are sorted and unique;
    /// Box3D checks the world bytes when the image is restored.
    pub fn from_bytes(data: &[u8]) -> Result<WorldImage, PhysicsError> {
        let mut r = Reader { data };
        if r.u32()? != IMAGE_MAGIC || r.u32()? != IMAGE_VERSION {
            return Err(PhysicsError::BadImage("not a pe-physics image of this version"));
        }
        let n = r.len()?;
        let world = r.take(n)?.to_vec();
        let count = r.u32()? as usize;
        if count > r.data.len() / 16 {
            return Err(PhysicsError::BadImage("body count beyond the end"));
        }
        let mut bodies = Vec::with_capacity(count);
        for _ in 0..count {
            let id = ObjectId { creator: r.u32()?, counter: r.u32()? };
            bodies.push((id, r.body()?));
        }
        let count = r.u32()? as usize;
        if count > r.data.len() / 12 {
            return Err(PhysicsError::BadImage("joint count beyond the end"));
        }
        let mut joints = Vec::with_capacity(count);
        for _ in 0..count {
            let key = JointKey(r.u32()?);
            let joint = JointId { index1: r.u32()? as i32, world0: r.u16()?, generation: r.u16()? };
            joints.push((key, joint));
        }
        let next_joint = r.u32()?;
        let anchor = match r.take(1)?[0] {
            0 => None,
            1 => Some(r.body()?),
            _ => return Err(PhysicsError::BadImage("bad anchor flag")),
        };
        if !r.data.is_empty() {
            return Err(PhysicsError::BadImage("trailing bytes"));
        }
        if bodies.iter().any(|(id, _)| id.is_none())
            || !bodies.windows(2).all(|w| w[0].0 < w[1].0)
            || !joints.windows(2).all(|w| w[0].0 < w[1].0)
            || joints.iter().any(|(k, _)| k.0 >= next_joint)
        {
            return Err(PhysicsError::BadImage("binding tables not sorted, unique and in range"));
        }
        Ok(WorldImage { world, bodies, joints, next_joint, anchor })
    }
}

impl GeometryBlobs {
    /// Canonical little-endian bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_u32(&mut out, self.entries.len() as u32);
        for (kind, bytes) in &self.entries {
            out.push(*kind);
            put_u32(&mut out, bytes.len() as u32);
            out.extend_from_slice(bytes);
        }
        out
    }

    pub fn from_bytes(data: &[u8]) -> Result<GeometryBlobs, PhysicsError> {
        let mut r = Reader { data };
        let count = r.u32()? as usize;
        if count > r.data.len() / 5 {
            return Err(PhysicsError::BadImage("geometry count beyond the end"));
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let kind = r.take(1)?[0];
            if kind > GeometryKind::Compound as u8 {
                return Err(PhysicsError::BadImage("unknown geometry kind"));
            }
            let n = r.len()?;
            entries.push((kind, r.take(n)?.to_vec()));
        }
        if !r.data.is_empty() {
            return Err(PhysicsError::BadImage("trailing bytes"));
        }
        Ok(GeometryBlobs { entries })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn kind_of(kind: u8) -> GeometryKind {
    match kind {
        0 => GeometryKind::Hull,
        1 => GeometryKind::Mesh,
        2 => GeometryKind::HeightField,
        _ => GeometryKind::Compound,
    }
}

impl PhysicsWorld {
    /// Save the physics state. Images of one world share its geometry registry.
    pub fn save_image(&mut self) -> WorldImage {
        let mut buf = RecBuffer::new();
        serialize_world(&self.world, &mut buf, &mut self.images.registry);
        WorldImage {
            world: buf.data,
            bodies: self.bodies.iter().map(|(&id, &b)| (id, b)).collect(),
            joints: self.joints.iter().map(|(&k, &j)| (k, j)).collect(),
            next_joint: self.next_joint,
            anchor: self.anchor,
        }
    }

    /// Restore an image saved from this world (or from a world restored from the same
    /// geometry). Box3D's world value is overwritten in place, so the worker threads and the
    /// configuration stay. On failure the world is unchanged only if Box3D refused the bytes
    /// before writing; callers treat a failed restore as fatal for the world.
    pub fn restore_image(&mut self, image: &WorldImage) -> Result<(), PhysicsError> {
        if self.world.locked {
            return Err(PhysicsError::Locked);
        }
        self.images.refresh_slots();
        restore_into(&mut self.world, &mut self.images.slots, image)?;
        self.set_tables(image)
    }

    /// The geometry this world's images refer to.
    pub fn geometry(&self) -> GeometryBlobs {
        GeometryBlobs {
            entries: self.images.registry.entries.iter().map(|e| (e.kind as u8, e.bytes.clone())).collect(),
        }
    }

    /// A new world from an image and its geometry (a late joiner's path). `config` supplies
    /// the worker count; everything that changes the simulation, contact recycling included,
    /// comes from the image.
    pub fn from_image(config: PhysicsConfig, geometry: &GeometryBlobs, image: &WorldImage) -> Result<PhysicsWorld, PhysicsError> {
        let mut pw = PhysicsWorld::new(config);
        for (kind, bytes) in &geometry.entries {
            let h = box3d_rust::core::hash64_non_zero(bytes);
            pw.images.registry.append(kind_of(*kind), h, bytes.clone());
        }
        pw.images.refresh_slots();
        restore_into(&mut pw.world, &mut pw.images.slots, image)?;
        pw.set_tables(image)?;
        Ok(pw)
    }

    fn set_tables(&mut self, image: &WorldImage) -> Result<(), PhysicsError> {
        let world = &self.world;
        if !image.bodies.iter().all(|&(_, b)| body_is_valid(world, b))
            || !image.joints.iter().all(|&(_, j)| joint_is_valid(world, j))
            || !image.anchor.is_none_or(|b| body_is_valid(world, b))
        {
            return Err(PhysicsError::BadImage("tables name bodies or joints the world does not have"));
        }
        self.bodies = image.bodies.iter().copied().collect();
        self.joints = image.joints.iter().copied().collect();
        self.next_joint = image.next_joint;
        self.anchor = image.anchor;
        Ok(())
    }
}

fn restore_into(world: &mut World, slots: &mut [RegistrySlot], image: &WorldImage) -> Result<(), PhysicsError> {
    if deserialize_into_shell(&image.world, world, slots) {
        Ok(())
    } else {
        Err(PhysicsError::BadImage("Box3D refused the world bytes"))
    }
}

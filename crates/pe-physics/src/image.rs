//! World images: the physics part of a rollback snapshot (6.1), and transfers: the physics
//! part of a state transfer to a joining peer (9.5).
//!
//! * A [`WorldImage`] is Box3D's own world serialization plus the engine's binding and joint
//!   tables. Geometry (hulls, meshes) is not repeated in every image: it is interned once per
//!   world in a registry that only grows, and images refer to it by index. Images restore only
//!   into the world that saved them (rollback). They are trusted local data: the only way to
//!   get one is [`PhysicsWorld::save_image`], and they are never read from bytes (outside the
//!   `raw` test feature).
//! * A [`Transfer`] is an image saved against a fresh registry, with exactly the geometry it
//!   uses, in shape order. Its bytes depend only on the physics state, not on the world's save
//!   history or the build profile, so [`Transfer::check_hash`] is the same on every honest
//!   peer with that state ([`PhysicsWorld::check_hash`]). A joiner restores a transfer only
//!   when its hash matches the confirmed one (design 9.1): the bytes come from an untrusted
//!   donor, and Box3D cannot check that every index in them is consistent. The check hash is
//!   BLAKE3 over all of the bytes, so a donor cannot find other bytes with the confirmed hash;
//!   the restored world must also save back to exactly the transfer's bytes.

use std::collections::BTreeSet;

use box3d_rust::body::body_is_valid;
use box3d_rust::id::{BodyId, JointId};
use box3d_rust::joint::{joint_get_type, joint_is_valid};
use box3d_rust::recording::{
    deserialize_into_shell, image_world_settings, serialize_world, world_settings, GeometryKind,
    GeometryRegistry, RecBuffer, RegistrySlot, SNAP_FLAG_VALIDATION,
};
use box3d_rust::world::World;
use pe_core::ids::ObjectId;

use crate::config::PhysicsConfig;
use crate::error::PhysicsError;
use crate::world::{JointEntry, JointKey, JointKind, PhysicsWorld};

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
    joints: Vec<(JointKey, JointId, JointKind)>,
    anchor: Option<BodyId>,
}

/// The full-state check hash (design 9.1) of a world: BLAKE3 (pure Rust, D16) of its
/// [`Transfer`] bytes, keyed for this use. Cryptographic, so a matching hash means matching
/// bytes even when the bytes come from a hostile peer (review P22R-1). XXH64 is enough to
/// detect divergence between honest peers ([`PhysicsWorld::state_hash`]) but not this.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CheckHash(pub [u8; 32]);

/// BLAKE3 key-derivation context, so the hash cannot be confused with another BLAKE3 use.
const CHECK_HASH_CONTEXT: &str = "Primordial Engine 2026-10-31 pe-physics transfer check hash";

impl CheckHash {
    /// The check hash of `transfer_bytes` ([`Transfer::to_bytes`]).
    pub fn of(transfer_bytes: &[u8]) -> CheckHash {
        let mut h = blake3::Hasher::new_derive_key(CHECK_HASH_CONTEXT);
        h.update(transfer_bytes);
        CheckHash(*h.finalize().as_bytes())
    }
}

impl std::fmt::Display for CheckHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl std::fmt::Debug for CheckHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CheckHash({self})")
    }
}

/// The physics state of a world for a joining peer: an image and the geometry it uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer {
    geometry: Vec<(u8, Vec<u8>)>,
    image: WorldImage,
}

const IMAGE_MAGIC: u32 = 0x5045_5049; // "PEPI"
const IMAGE_VERSION: u32 = 2;
const TRANSFER_MAGIC: u32 = 0x5045_5054; // "PEPT"
const TRANSFER_VERSION: u32 = 1;
/// Offset of the flags word in Box3D's world bytes (after magic, version, layout hash).
const SNAP_FLAGS_OFFSET: usize = 12;

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
    fn u8(&mut self) -> Result<u8, PhysicsError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, PhysicsError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, PhysicsError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, PhysicsError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    /// A byte length that fits in what is left.
    fn len(&mut self) -> Result<usize, PhysicsError> {
        let n = self.u32()? as usize;
        if n > self.data.len() {
            return Err(PhysicsError::BadImage("length beyond the end"));
        }
        Ok(n)
    }
    /// An element count whose elements, at `min_bytes` each, fit in what is left.
    fn count(&mut self, min_bytes: usize) -> Result<usize, PhysicsError> {
        let n = self.u32()? as usize;
        if n > self.data.len() / min_bytes {
            return Err(PhysicsError::BadImage("count beyond the end"));
        }
        Ok(n)
    }
    fn body(&mut self) -> Result<BodyId, PhysicsError> {
        Ok(BodyId { index1: self.u32()? as i32, world0: self.u16()?, generation: self.u16()? })
    }
    fn end(&self) -> Result<(), PhysicsError> {
        if self.data.is_empty() { Ok(()) } else { Err(PhysicsError::BadImage("trailing bytes")) }
    }
}

impl WorldImage {
    /// Box3D's world bytes (the part the spikes hash).
    pub fn world_bytes(&self) -> &[u8] {
        &self.world
    }

    /// Canonical little-endian bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.world.len() + 16 * self.bodies.len() + 17 * self.joints.len() + 32);
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
        for &(key, joint, kind) in &self.joints {
            out.extend_from_slice(&key.0.to_le_bytes());
            put_u32(&mut out, joint.index1 as u32);
            out.extend_from_slice(&joint.world0.to_le_bytes());
            out.extend_from_slice(&joint.generation.to_le_bytes());
            out.push(kind as u8);
        }
        match self.anchor {
            Some(b) => {
                out.push(1);
                put_body(&mut out, b);
            }
            None => out.push(0),
        }
        out
    }

    /// Parse [`Self::to_bytes`], for tests and tools only (feature `raw`): images are local
    /// data, and a restored image is not hash-checked (review P22R-5). Checks the framing and
    /// the table invariants (see [`Self::check_tables`]); Box3D's world bytes are only checked
    /// when restored, and only for being readable.
    #[cfg(feature = "raw")]
    pub fn from_bytes(data: &[u8]) -> Result<WorldImage, PhysicsError> {
        let mut r = Reader { data };
        let image = WorldImage::read(&mut r)?;
        r.end()?;
        Ok(image)
    }

    fn read(r: &mut Reader) -> Result<WorldImage, PhysicsError> {
        if r.u32()? != IMAGE_MAGIC || r.u32()? != IMAGE_VERSION {
            return Err(PhysicsError::BadImage("not a pe-physics image of this version"));
        }
        let n = r.len()?;
        let world = r.take(n)?.to_vec();
        let count = r.count(16)?;
        let mut bodies = Vec::with_capacity(count);
        for _ in 0..count {
            let id = ObjectId { creator: r.u32()?, counter: r.u32()? };
            bodies.push((id, r.body()?));
        }
        let count = r.count(17)?;
        let mut joints = Vec::with_capacity(count);
        for _ in 0..count {
            let key = JointKey(r.u64()?);
            let joint = JointId { index1: r.u32()? as i32, world0: r.u16()?, generation: r.u16()? };
            let kind = JointKind::from_u8(r.u8()?).ok_or(PhysicsError::BadImage("unknown joint kind"))?;
            joints.push((key, joint, kind));
        }
        let anchor = match r.u8()? {
            0 => None,
            1 => Some(r.body()?),
            _ => return Err(PhysicsError::BadImage("bad anchor flag")),
        };
        let image = WorldImage { world, bodies, joints, anchor };
        image.check_tables()?;
        Ok(image)
    }

    /// The table invariants: object ids and joint keys sorted and unique, no `NONE` object, no
    /// two objects or joints sharing a Box3D id, the anchor not a bound body, and every id in
    /// world 0 (the only Box3D world id the fork uses).
    fn check_tables(&self) -> Result<(), PhysicsError> {
        let bad = |why| Err(PhysicsError::BadImage(why));
        if self.bodies.iter().any(|(id, _)| id.is_none()) || !self.bodies.windows(2).all(|w| w[0].0 < w[1].0) {
            return bad("object ids not sorted and unique");
        }
        if !self.joints.windows(2).all(|w| w[0].0 < w[1].0) {
            return bad("joint keys not sorted and unique");
        }
        let mut body_ids = BTreeSet::new();
        for &(_, b) in &self.bodies {
            if b.world0 != 0 || !body_ids.insert(b.index1) {
                return bad("body ids repeated or not in world 0");
            }
        }
        if self.anchor.is_some_and(|a| a.world0 != 0 || body_ids.contains(&a.index1)) {
            return bad("anchor is a bound body or not in world 0");
        }
        let mut joint_ids = BTreeSet::new();
        for &(_, j, _) in &self.joints {
            if j.world0 != 0 || !joint_ids.insert(j.index1) {
                return bad("joint ids repeated or not in world 0");
            }
        }
        Ok(())
    }
}

fn put_geometry(out: &mut Vec<u8>, geometry: &[(u8, Vec<u8>)]) {
    put_u32(out, geometry.len() as u32);
    for (kind, bytes) in geometry {
        out.push(*kind);
        put_u32(out, bytes.len() as u32);
        out.extend_from_slice(bytes);
    }
}

fn kind_of(kind: u8) -> Option<GeometryKind> {
    Some(match kind {
        0 => GeometryKind::Hull,
        1 => GeometryKind::Mesh,
        2 => GeometryKind::HeightField,
        3 => GeometryKind::Compound,
        _ => return None,
    })
}

impl Transfer {
    /// Canonical little-endian bytes: what [`Self::check_hash`] hashes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_u32(&mut out, TRANSFER_MAGIC);
        put_u32(&mut out, TRANSFER_VERSION);
        put_geometry(&mut out, &self.geometry);
        out.extend_from_slice(&self.image.to_bytes());
        out
    }

    /// Parse [`Self::to_bytes`], checking the framing and the image's tables. `to_bytes` of the
    /// result gives `data` back exactly.
    pub fn from_bytes(data: &[u8]) -> Result<Transfer, PhysicsError> {
        let mut r = Reader { data };
        if r.u32()? != TRANSFER_MAGIC || r.u32()? != TRANSFER_VERSION {
            return Err(PhysicsError::BadImage("not a pe-physics transfer of this version"));
        }
        let count = r.count(5)?;
        let mut geometry = Vec::with_capacity(count);
        for _ in 0..count {
            let kind = r.u8()?;
            kind_of(kind).ok_or(PhysicsError::BadImage("unknown geometry kind"))?;
            let n = r.len()?;
            geometry.push((kind, r.take(n)?.to_vec()));
        }
        let image = WorldImage::read(&mut r)?;
        r.end()?;
        Ok(Transfer { geometry, image })
    }

    /// [`CheckHash::of`] [`Self::to_bytes`]: the full-state check hash (design 9.1). Equal on
    /// every peer with the same physics state, whatever its save history or build profile.
    pub fn check_hash(&self) -> CheckHash {
        CheckHash::of(&self.to_bytes())
    }

    /// The number of geometry blobs it carries.
    pub fn geometry_count(&self) -> usize {
        self.geometry.len()
    }
}

impl PhysicsWorld {
    /// Save the physics state for rollback. Images of one world share its geometry registry.
    /// Refused while poisoned: a half-restored world must not enter the ring (review P22R-4).
    pub fn save_image(&mut self) -> Result<WorldImage, PhysicsError> {
        self.usable()?;
        let mut buf = RecBuffer::new();
        serialize_world(&self.world, &mut buf, &mut self.images.registry);
        Ok(self.image_with(buf.data))
    }

    fn image_with(&self, world: Vec<u8>) -> WorldImage {
        WorldImage {
            world,
            bodies: self.bodies.iter().map(|(&id, &b)| (id, b)).collect(),
            joints: self.joints.iter().map(|(&k, j)| (k, j.id, j.kind)).collect(),
            anchor: self.anchor,
        }
    }

    /// Restore an image saved from this world (or from a world restored from the same
    /// transfer): local rollback only. Images are trusted, so nothing here is hash-checked;
    /// state from another peer arrives as a [`Transfer`]. Box3D's world value is overwritten in
    /// place, so the worker threads and the configuration stay. An image whose settings differ
    /// from this world's is refused before anything is written. If Box3D refuses the bytes
    /// after it has started writing, the world is poisoned ([`PhysicsError::Poisoned`]) until a
    /// good image is restored (the one call a poisoned world accepts).
    pub fn restore_image(&mut self, image: &WorldImage) -> Result<(), PhysicsError> {
        if self.world.locked {
            return Err(PhysicsError::Locked);
        }
        check_settings(&self.world, image)?;
        self.images.refresh_slots();
        self.poisoned = true;
        restore_into(&mut self.world, &mut self.images.slots, image)?;
        self.set_tables(image)?;
        self.poisoned = false;
        Ok(())
    }

    /// How many geometry blobs this world's image registry holds. It only grows as images are
    /// saved (each hull a body ever had stays interned) until [`Self::compact_images`].
    pub fn registry_len(&self) -> usize {
        self.images.registry.entries.len()
    }

    /// Rebuild the image registry from the images still kept, so that geometry only rolled-back
    /// or destroyed state used is dropped (review P22-12; with P2-3's keyframe ring).
    ///
    /// Each kept image is restored and saved again against a fresh registry, in the order given;
    /// then the live state is restored and saved into it too, so later saves intern into the new
    /// registry. Afterwards every kept image restores exactly as before and the live world is in
    /// the state it was in. Registry indices appear only in images, never in what is hashed or
    /// stepped, so compacting at any frame is invisible to the simulation: `state_hash` and
    /// `check_hash` do not move.
    ///
    /// Nothing changes if a kept image is refused (another world's, or other settings): the live
    /// state is restored and the error returned. If even that restore fails, the world is
    /// poisoned, as with [`Self::restore_image`]. Refused while stepping or poisoned.
    pub fn compact_images(&mut self, kept: &mut [&mut WorldImage]) -> Result<(), PhysicsError> {
        self.usable()?;
        let live = self.save_image()?;
        let mut fresh = GeometryRegistry::new();
        let mut rewritten = Vec::with_capacity(kept.len());
        let mut refused = None;
        for image in kept.iter() {
            if let Err(e) = self.restore_image(image) {
                refused = Some(e);
                break;
            }
            let mut buf = RecBuffer::new();
            serialize_world(&self.world, &mut buf, &mut fresh);
            rewritten.push(WorldImage { world: buf.data, ..WorldImage::clone(image) });
        }
        self.restore_image(&live)?;
        if let Some(e) = refused {
            return Err(e);
        }
        // The live world's own geometry, so the next save finds it.
        serialize_world(&self.world, &mut RecBuffer::new(), &mut fresh);
        for (image, new) in kept.iter_mut().zip(rewritten) {
            **image = new;
        }
        self.images = ImageRegistry { slots: fresh.to_slots(), registry: fresh };
        Ok(())
    }

    /// The physics state for a joining peer (see [`Transfer`]). Refused while poisoned.
    pub fn save_transfer(&self) -> Result<Transfer, PhysicsError> {
        self.usable()?;
        Ok(self.transfer())
    }

    fn transfer(&self) -> Transfer {
        let mut registry = GeometryRegistry::new();
        let mut buf = RecBuffer::new();
        serialize_world(&self.world, &mut buf, &mut registry);
        // The validation flag says only which build profile wrote the bytes; the reader
        // ignores it. Clear it so debug and release peers write the same bytes.
        let flags = SNAP_FLAGS_OFFSET..SNAP_FLAGS_OFFSET + 4;
        let word = u32::from_le_bytes(buf.data[flags.clone()].try_into().unwrap()) & !SNAP_FLAG_VALIDATION;
        buf.data[flags].copy_from_slice(&word.to_le_bytes());
        Transfer {
            geometry: registry.entries.iter().map(|e| (e.kind as u8, e.bytes.clone())).collect(),
            image: self.image_with(buf.data),
        }
    }

    /// The full-state check hash ([`Transfer::check_hash`] of [`Self::save_transfer`]).
    /// Refused while poisoned.
    pub fn check_hash(&self) -> Result<CheckHash, PhysicsError> {
        Ok(self.save_transfer()?.check_hash())
    }

    /// A new world from a transfer (a joiner's path). Refused unless the transfer's check hash
    /// is `confirmed`, the check hash the session confirmed for that state: only then is the
    /// donor's data trusted. `config` must be the world's configuration; the transfer's
    /// settings must equal it, and the worker count comes from it. The new world must save back
    /// to exactly the transfer, so no byte of it went unread: every geometry blob is used once,
    /// in order, and Box3D consumed all of its world bytes.
    pub fn from_transfer(config: PhysicsConfig, transfer: &Transfer, confirmed: &CheckHash) -> Result<PhysicsWorld, PhysicsError> {
        if transfer.check_hash() != *confirmed {
            return Err(PhysicsError::BadImage("check hash differs from the confirmed one"));
        }
        let mut pw = PhysicsWorld::new(config)?;
        check_settings(&pw.world, &transfer.image)?;
        for (kind, bytes) in &transfer.geometry {
            let h = box3d_rust::core::hash64_non_zero(bytes);
            pw.images.registry.append(kind_of(*kind).expect("checked by from_bytes"), h, bytes.clone());
        }
        pw.images.refresh_slots();
        restore_into(&mut pw.world, &mut pw.images.slots, &transfer.image)?;
        pw.set_tables(&transfer.image)?;
        if pw.transfer() != *transfer {
            return Err(PhysicsError::BadImage("transfer is not canonical: it does not save back to itself"));
        }
        Ok(pw)
    }

    fn set_tables(&mut self, image: &WorldImage) -> Result<(), PhysicsError> {
        let world = &self.world;
        if !image.bodies.iter().all(|&(_, b)| body_is_valid(world, b))
            || !image.joints.iter().all(|&(_, j, _)| joint_is_valid(world, j))
            || !image.anchor.is_none_or(|b| body_is_valid(world, b))
        {
            return Err(PhysicsError::BadImage("tables name bodies or joints the world does not have"));
        }
        // A grab must be Box3D's motor joint, or set_grab_velocity would panic (P22R-3).
        if !image.joints.iter().all(|&(_, j, kind)| joint_get_type(world, j) == kind.box3d_type()) {
            return Err(PhysicsError::BadImage("a joint's kind differs from its Box3D type"));
        }
        self.bodies = image.bodies.iter().copied().collect();
        self.joints = image.joints.iter().map(|&(k, id, kind)| (k, JointEntry { id, kind })).collect();
        self.anchor = image.anchor;
        Ok(())
    }
}

/// Refuse an image whose world settings differ from `world`'s (P22-2): an image cannot change
/// gravity, the solver or contact recycling, or set values `World::new` would clamp.
fn check_settings(world: &World, image: &WorldImage) -> Result<(), PhysicsError> {
    match image_world_settings(&image.world) {
        Some(s) if s == world_settings(world) => Ok(()),
        Some(_) => Err(PhysicsError::BadImage("world settings differ from this world's")),
        None => Err(PhysicsError::BadImage("world bytes too short")),
    }
}

fn restore_into(world: &mut World, slots: &mut [RegistrySlot], image: &WorldImage) -> Result<(), PhysicsError> {
    if deserialize_into_shell(&image.world, world, slots) {
        Ok(())
    } else {
        Err(PhysicsError::BadImage("Box3D refused the world bytes"))
    }
}

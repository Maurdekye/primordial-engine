//! The calls pe-replica's `WorldModel` adapter makes on a [`World`] (replica.md 8.1; agreed with @vdw-physics,
//! m9249bf73037f): what the physics reads, what the replica writes back, the settings, and a stand-in root until the
//! BLAKE3 tree (6) replaces it behind the same call.

use pe_core::commit::Hasher;
use pe_core::{Hash, ObjectId, Pos};
use pe_seams::SimRate;
use pe_sim::transform::quat_normalize;
use pe_sim::{Transform, WorldTransform};

use crate::components::{Body, BodyState, Shape};
use crate::world::{Claim, World, depth};

/// The world settings the physics takes (replica.md 8.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub rate: SimRate,
    /// Metres per second squared, in the world frame.
    pub gravity: [f32; 3],
    /// The domain's physics origin (4.2): bodies are simulated at `pos - origin`.
    pub origin: Pos,
}

/// The default gravity: 9.81 m/s² down the world's y axis.
pub const GRAVITY: [f32; 3] = [0.0, -9.81, 0.0];

impl World {
    /// The claims (7.2), in the held objects' id order.
    pub fn claims(&self) -> impl Iterator<Item = (ObjectId, Claim)> + '_ {
        self.claims.iter().map(|(&o, &c)| (o, c))
    }

    /// Every physical object, in id order: one with a `Body` and at least one shape, its own or a compound child's
    /// (a descendant reached without passing another `Body`).
    pub fn physical(&self) -> Vec<ObjectId> {
        self.store.iter::<Body>().map(|(id, _)| id).filter(|&id| self.has_shape(id)).collect()
    }

    fn has_shape(&self, body: ObjectId) -> bool {
        let mut stack = vec![body];
        while let Some(x) = stack.pop() {
            if self.store.has::<Shape>(x) {
                return true;
            }
            stack.extend(self.store.children(x).filter(|&c| !self.store.has::<Body>(c)));
        }
        false
    }

    /// Repairs out-of-limit transforms that trusted code wrote (3.3), and returns how many: nonzero is a fault.
    pub fn maintain_transforms(&mut self) -> usize {
        self.store.maintain_transforms()
    }

    /// The replica's write-back (replica.md 3, step 7): each body's new world pose. Each is converted to a local
    /// transform against its frame's final world pose, parents first, so the result does not depend on the order
    /// of `poses`; local scale is kept exactly. A trusted write: row versions do not move (7.3). Returns, in id
    /// order, the objects nothing was written for: no such object or `Transform`, or a pose its frame cannot hold
    /// in range (4.3).
    pub fn write_poses(&mut self, poses: &[(ObjectId, Pos, [f32; 4])]) -> Vec<ObjectId> {
        let mut order: Vec<(u32, ObjectId, Pos, [f32; 4])> =
            poses.iter().map(|&(id, p, r)| (depth(&self.store, id), id, p, r)).collect();
        order.sort_by_key(|&(d, id, ..)| (d, id));
        let mut refused = Vec::new();
        // An id given twice is refused, both times, so the result never depends on the order of `poses`
        // (@chunks-reviewer's Low 1).
        let mut repeated: Vec<ObjectId> = order.windows(2).filter(|w| w[0].1 == w[1].1).map(|w| w[0].1).collect();
        repeated.dedup();
        order.retain(|&(_, id, ..)| repeated.binary_search(&id).is_err());
        refused.extend(repeated);
        for (_, id, pos, rot) in order {
            let Some(old) = self.store.get::<Transform>(id).copied() else {
                refused.push(id);
                continue;
            };
            let frame = self.store.frame_of(id);
            let frame_world = match frame {
                Some(f) => match self.store.world_transform(f) {
                    Some(w) => Some(w),
                    None => {
                        refused.push(id);
                        continue;
                    }
                },
                None => None,
            };
            let placed = WorldTransform { pos, rot: quat_normalize(rot), scale: old.scale };
            let mut local = placed.local_in(frame_world.as_ref());
            local.scale = old.scale;
            if local.validate(frame.is_some()).is_err() || self.store.insert(id, local).is_err() {
                refused.push(id);
            }
        }
        refused.sort_unstable();
        refused
    }

    /// At a cut frame, before the root (replica.md 3.4): every bound body's `BodyState`, and the domain's
    /// `physics_global`. It replaces every `BodyState` row: an object missing from `rows` has none afterwards. A
    /// trusted write. Rows for objects that do not exist are skipped; of repeated ids, the last row stands.
    ///
    /// Linear in the bodies (@chunks-reviewer's Medium on f3f64f69, which measured the first version quadratic):
    /// rows are written in id order, so existing rows are overwritten in place and new ones mostly append, and the
    /// rows no longer wanted are removed from the highest id down, so no removal shifts the rows after it.
    pub fn write_physics(&mut self, rows: &[(ObjectId, BodyState)], global: Hash) {
        let mut want: Vec<(ObjectId, BodyState)> = rows.to_vec();
        want.sort_by_key(|&(id, _)| id);
        want.reverse();
        want.dedup_by_key(|&mut (id, _)| id); // keeps the last row given for an id
        want.reverse();
        let gone: Vec<ObjectId> = self
            .store
            .iter::<BodyState>()
            .map(|(id, _)| id)
            .filter(|id| want.binary_search_by_key(id, |&(w, _)| w).is_err())
            .collect();
        for id in gone.into_iter().rev() {
            let _ = self.store.remove_raw(crate::components::ids::BODY_STATE, id);
        }
        for (id, row) in want {
            if self.store.exists(id) {
                let _ = self.store.insert(id, row);
            }
        }
        self.physics_global = global;
    }

    /// The domain's `physics_global` record, as the last [`World::write_physics`] set it.
    pub fn physics_global(&self) -> Hash {
        self.physics_global
    }

    pub fn settings(&self) -> Settings {
        Settings { rate: self.rate, gravity: self.gravity, origin: self.origin }
    }

    pub fn set_gravity(&mut self, gravity: [f32; 3]) {
        self.gravity = gravity;
    }

    pub fn set_origin(&mut self, origin: Pos) {
        self.origin = origin;
    }

    /// The root of the current state. A STAND-IN until the BLAKE3 tree (6.2) replaces it behind this call:
    /// `BLAKE3("pe.root.v0" ‖ …)` over the store's state hash (frame, seed, objects, every column and row version)
    /// and every world table pe-world keeps (claims, members, tombstones, the rate, the depth limit, the settings,
    /// `physics_global`), so equal roots mean equal outcomes from here (12.11). Deterministic on every machine.
    /// Its strength for the store is that of the 64-bit `state_hash` inside it: enough against accidents, not
    /// against an adversary, until the tree lands (@chunks-reviewer's Low 3).
    pub fn root(&self) -> Hash {
        let mut h = Hasher::new("pe.root.v0");
        h.write_u64(self.store.state_hash());
        h.write_u64(self.claims.len() as u64);
        for (o, c) in self.claims.iter() {
            h.write_u128(o.to_u128()).write_u128(c.hand.to_u128()).write(&c.member.0).write_u64(c.since.0);
        }
        h.write_u64(self.members.len() as u64);
        for (k, p) in self.members.iter() {
            h.write(&k.0).write_u32(*p);
        }
        h.write_u64(self.tombstones.len() as u64);
        for (id, t) in self.tombstones.iter() {
            h.write_u128(id.to_u128()).write_hash(&t.by.0).write_u64(t.frame.0).write_u64(t.expires.0);
            h.write_u128(t.parent.map_or(0, |p| p.to_u128())).write_u32(t.place);
            match &t.world {
                None => {
                    h.write_u8(0);
                }
                Some(w) => {
                    let mut b = Vec::new();
                    w.pos.encode(&mut b);
                    h.write_u8(1).write(&b).write(bytemuck::cast_slice(&w.rot)).write(bytemuck::cast_slice(&w.scale));
                }
            }
            h.write(bytemuck::bytes_of(&t.node));
            h.write_u64(t.rows.len() as u64);
            for (c, bytes, v) in &t.rows {
                h.write_u128(c.0).write_len_prefixed(bytes).write(bytemuck::bytes_of(v));
            }
        }
        let mut origin = Vec::new();
        self.origin.encode(&mut origin);
        h.write_u32(self.rate.hz()).write_u32(self.depth_limit);
        h.write(bytemuck::cast_slice(&self.gravity)).write(&origin);
        h.write_hash(&self.physics_global);
        h.finish()
    }
}

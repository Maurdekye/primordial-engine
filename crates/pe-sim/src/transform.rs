//! The built-in [`Transform`] component and world-transform propagation (design 16.2, 25.2).
//!
//! * Position is `f64`, rotation and scale `f32` (25.2: the same split as box3d's double-precision mode).
//! * An object's *frame* is its nearest ancestor that has a `Transform`. An object with no frame is placed
//!   as a root: its `chunk` (level-0 chunk, `i64`) plus `pos` from that chunk's corner. An object with a
//!   frame is placed relative to it, and its `chunk` must be zero.
//! * **Precision.** A root has the chunk system's precision everywhere. A child's position is an `f64`
//!   offset from its frame, so its precision is that offset's ulp: about 1e-13 m at 1 km, 1e-10 m at
//!   1,000 km, 1 um out to about 4e9 m. Keep children within about 10^6 km of their frame; far content
//!   belongs in its own root.
//! * **Plain edits are data edits** (review P21R-3), each costing O(depth) for its frame lookups (review
//!   P21S-4): `insert`/`insert_raw` and `remove` of a `Transform`,
//!   and [`SimStore::set_parent`], store exactly what they are given and never rewrite other objects. So a
//!   state rebuilt by a loader is the same whatever order it inserts in. An edit that would leave an object
//!   with a frame and a non-zero `chunk` is refused.
//! * **Keep-world edits are explicit** (review P21-6): [`SimStore::set_parent_keep_world`],
//!   [`SimStore::insert_transform_keep_world`] and [`SimStore::remove_transform_keep_world`] rewrite the
//!   affected local transforms so their world transforms stay where they were (up to rounding), with the
//!   root chunk recomputed exactly. Each computes the old and new frame once, so its cost is linear in the
//!   objects it moves (review P21R-2). An edit whose rewritten transforms would break the limits is refused,
//!   and nothing changes.
//! * **Limits** (review P21-2), checked on insert, in `check_values` and on loading bytes, on the stored form:
//!   `|pos| <= 2^50` m; a root's `|chunk| <= 2^61`, so a root's position never saturates; scale components
//!   non-zero with magnitude in [2^-40, 2^40]; the rotation's squared norm within 2% of 1; `_pad` zero.
//!   Deep chains of maximal scales and offsets can still saturate a *world* position at the chunk limit;
//!   that is deterministic.
//! * **Maintenance** (reviews P21-7, P21R-5): [`SimStore::maintain_transforms`] (also the system
//!   [`maintain_transforms`]) rebases root positions into their chunk when the rebased form is within the
//!   limits, and repairs out-of-limit values written through `get_mut` (clamped, renormalized, zeroed), so
//!   the state stays loadable by joiners.
//! * Composition: `world.pos = frame.pos + rotate(frame.rot, frame.scale * local.pos)` in `f64`, then
//!   renormalized into its chunk; `world.rot = normalize(frame.rot * local.rot)` and `world.scale =
//!   frame.scale * local.scale` (clamped to [2^-100, 2^100] in magnitude) in `f32`. `rotate` normalizes the
//!   quaternion in `f64` (review P21R-4). Non-uniform scale under rotation does not shear children; it
//!   multiplies per axis, as in most engines. Every step is plain IEEE arithmetic in a fixed order and never
//!   panics, so [`SimStore::world_transform`] and [`SimStore::world_transforms`] give the same bits
//!   everywhere.

use bytemuck::{Pod, Zeroable};
use core::mem::offset_of;
use pe_core::coords::{Offset, Pos};
use pe_core::{Error, ObjectId};

use crate::registry::{Component, ComponentId, FieldDesc, FieldKind};
use crate::schedule::TickCtx;
use crate::store::SimStore;

/// The largest local position component, in metres: 2^50.
pub const POS_LIMIT: f64 = 1_125_899_906_842_624.0;
/// The largest root chunk index: 2^61, so a root plus its offset stays far from the chunk limit (2^62).
pub const ROOT_CHUNK_LIMIT: i64 = 1 << 61;
/// Scale component magnitudes: [2^-40, 2^40].
pub const SCALE_MIN: f32 = f32::from_bits((127 - 40) << 23);
pub const SCALE_MAX: f32 = f32::from_bits((127 + 40) << 23);
const WORLD_SCALE_MIN: f32 = f32::from_bits((127 - 100) << 23);
const WORLD_SCALE_MAX: f32 = f32::from_bits((127 + 100) << 23);

/// A local transform. 80 bytes, no implicit padding.
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct Transform {
    /// Objects without a frame only: the level-0 chunk the position is measured from.
    pub chunk: [i64; 3],
    pub pos: [f64; 3],
    /// Unit quaternion `[x, y, z, w]`.
    pub rot: [f32; 4],
    pub scale: [f32; 3],
    /// Always zero.
    pub _pad: u32,
}

impl Default for Transform {
    fn default() -> Self {
        Transform::IDENTITY
    }
}

impl Transform {
    pub const IDENTITY: Transform =
        Transform { chunk: [0; 3], pos: [0.0; 3], rot: [0.0, 0.0, 0.0, 1.0], scale: [1.0; 3], _pad: 0 };

    /// A transform at `pos` relative to the object's frame.
    pub fn local(pos: [f64; 3]) -> Transform {
        Transform { pos, ..Transform::IDENTITY }
    }

    /// A root transform at a canonical world position.
    /// A root placement at `p`: its chunk index, and the offset from that chunk's corner (`p - corner`, exact).
    pub fn at(p: Pos) -> Transform {
        let (chunk, local) = split(p);
        Transform { chunk, pos: local, ..Transform::IDENTITY }
    }

    pub fn with_rot(mut self, rot: [f32; 4]) -> Transform {
        self.rot = rot;
        self
    }

    pub fn with_scale(mut self, scale: [f32; 3]) -> Transform {
        self.scale = scale;
        self
    }

    /// Checks the limits in the module docs. `has_frame` says whether the object has a transformed
    /// ancestor. Returns the offending field's name.
    pub fn validate(&self, has_frame: bool) -> Result<(), &'static str> {
        if has_frame {
            if self.chunk != [0; 3] {
                return Err("chunk");
            }
        } else if self.chunk.iter().any(|c| c.unsigned_abs() > ROOT_CHUNK_LIMIT as u64) {
            return Err("chunk");
        }
        if !self.pos.iter().all(|p| p.is_finite() && p.abs() <= POS_LIMIT) {
            return Err("pos");
        }
        let n2: f32 = self.rot.iter().map(|q| q * q).sum();
        if !(self.rot.iter().all(|q| q.is_finite()) && (0.98..=1.02).contains(&n2)) {
            return Err("rot");
        }
        if !self.scale.iter().all(|s| s.is_finite() && (SCALE_MIN..=SCALE_MAX).contains(&s.abs())) {
            return Err("scale");
        }
        if self._pad != 0 {
            return Err("_pad");
        }
        Ok(())
    }

    /// The position a root placement denotes: `chunk`'s corner moved by `pos`. `None` if out of range.
    pub fn root_pos(&self) -> Option<Pos> {
        Pos::at_chunk(self.chunk).ok()?.checked_add(Offset(self.pos)).ok()
    }

    /// The same root placement with `pos` moved inside its chunk (exact). Unchanged if out of range.
    pub fn rebased(&self) -> Transform {
        match self.root_pos() {
            Some(p) => {
                let (chunk, local) = split(p);
                Transform { chunk, pos: local, ..*self }
            }
            None => *self,
        }
    }
}

impl Component for Transform {
    const ID: ComponentId = ComponentId::TRANSFORM;
    const NAME: &'static str = "Transform";
    fn fields() -> Vec<FieldDesc> {
        vec![
            FieldDesc::new("chunk", FieldKind::I64, offset_of!(Transform, chunk), 3),
            FieldDesc::new("pos", FieldKind::F64, offset_of!(Transform, pos), 3),
            FieldDesc::new("rot", FieldKind::F32, offset_of!(Transform, rot), 4),
            FieldDesc::new("scale", FieldKind::F32, offset_of!(Transform, scale), 3),
            FieldDesc::new("_pad", FieldKind::U32, offset_of!(Transform, _pad), 1),
        ]
    }
}

/// A `Pos` as a chunk index and the offset from that chunk's corner. Only the wall's own operations are used:
/// the integer cell at level 0, and `p - corner` (world-model.md 4.2).
fn split(p: Pos) -> ([i64; 3], [f64; 3]) {
    let chunk = p.chunk_index();
    let corner = Pos::at_chunk(chunk).expect("a valid Pos's chunk is valid");
    (chunk, (p - corner).0)
}

/// A world transform: derived from the hierarchy and the local transforms, never stored or hashed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldTransform {
    pub pos: Pos,
    pub rot: [f32; 4],
    pub scale: [f32; 3],
}

/// Hamilton product `a * b` (apply `b`, then `a`).
pub fn quat_mul(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

/// `q / |q|`; the identity if `q` is zero or not finite, so propagation never produces NaN.
pub fn quat_normalize(q: [f32; 4]) -> [f32; 4] {
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if !(n.is_finite() && n > 0.0) {
        return [0.0, 0.0, 0.0, 1.0];
    }
    [q[0] / n, q[1] / n, q[2] / n, q[3] / n]
}

fn quat_conj(q: [f32; 4]) -> [f32; 4] {
    [-q[0], -q[1], -q[2], q[3]]
}

/// Rotates `v` by the quaternion `q`, normalized in `f64` first (review P21R-4: an `f32` quaternion is
/// unit only to about 1e-7): `v + 2w (u x v) + 2 u x (u x v)`.
pub fn rotate(q: [f32; 4], v: [f64; 3]) -> [f64; 3] {
    let (x, y, z, w) = (q[0] as f64, q[1] as f64, q[2] as f64, q[3] as f64);
    let n = (x * x + y * y + z * z + w * w).sqrt();
    if !(n.is_finite() && n > 0.0) {
        return v;
    }
    let (ux, uy, uz, w) = (x / n, y / n, z / n, w / n);
    let cross =
        |a: [f64; 3], b: [f64; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let u = [ux, uy, uz];
    let t = cross(u, v);
    let t2 = cross(u, t);
    [v[0] + 2.0 * (w * t[0] + t2[0]), v[1] + 2.0 * (w * t[1] + t2[1]), v[2] + 2.0 * (w * t[2] + t2[2])]
}

/// Clamps a world scale component's magnitude to [2^-100, 2^100], keeping its sign; NaN becomes 1.
fn clamp_scale(s: f32) -> f32 {
    if s.is_nan() {
        return 1.0;
    }
    let m = s.abs().clamp(WORLD_SCALE_MIN, WORLD_SCALE_MAX);
    if s < 0.0 { -m } else { m }
}

impl WorldTransform {
    /// The world transform of an object without a frame. `None` if its position is out of range (a stored root
    /// within the limits never is).
    pub fn root(t: &Transform) -> Option<WorldTransform> {
        Some(WorldTransform { pos: t.root_pos()?, rot: quat_normalize(t.rot), scale: t.scale.map(clamp_scale) })
    }

    /// The world transform of an object with local transform `t` in this frame. `None` if the composed position
    /// leaves the range: refused, never clamped (world-model.md 4.3).
    pub fn compose(&self, t: &Transform) -> Option<WorldTransform> {
        let scaled =
            [self.scale[0] as f64 * t.pos[0], self.scale[1] as f64 * t.pos[1], self.scale[2] as f64 * t.pos[2]];
        Some(WorldTransform {
            pos: self.pos.checked_add(Offset(rotate(self.rot, scaled))).ok()?,
            rot: quat_normalize(quat_mul(self.rot, t.rot)),
            scale: [
                clamp_scale(self.scale[0] * t.scale[0]),
                clamp_scale(self.scale[1] * t.scale[1]),
                clamp_scale(self.scale[2] * t.scale[2]),
            ],
        })
    }

    /// The local transform that places an object at `self` in the frame `frame` (the inverse of
    /// [`WorldTransform::compose`], up to rounding), or as a root if `frame` is `None`.
    pub fn local_in(&self, frame: Option<&WorldTransform>) -> Transform {
        let Some(f) = frame else {
            let (chunk, local) = split(self.pos);
            return Transform { chunk, pos: local, rot: self.rot, scale: self.scale, _pad: 0 };
        };
        let inv = quat_conj(f.rot);
        let d = rotate(inv, (self.pos - f.pos).0);
        Transform {
            chunk: [0; 3],
            pos: [d[0] / f.scale[0] as f64, d[1] / f.scale[1] as f64, d[2] / f.scale[2] as f64],
            rot: quat_normalize(quat_mul(inv, self.rot)),
            scale: [self.scale[0] / f.scale[0], self.scale[1] / f.scale[1], self.scale[2] / f.scale[2]],
            _pad: 0,
        }
    }
}

fn invalid(object: ObjectId, field: &str) -> Error {
    Error::InvalidValue { component: ComponentId::TRANSFORM.0, object, field: field.to_string() }
}

/// `q` normalized in `f64` (review P21S-1: in `f32`, a tiny `q` has a subnormal `q.q` and normalizes to a
/// norm far from 1), or the identity if the result still fails the norm limit.
fn repaired_rot(q: [f32; 4]) -> [f32; 4] {
    let d = q.map(|x| x as f64);
    let n = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2] + d[3] * d[3]).sqrt();
    if n.is_finite() && n > 0.0 {
        let r = d.map(|x| (x / n) as f32);
        let n2: f32 = r.iter().map(|x| x * x).sum();
        if (0.98..=1.02).contains(&n2) {
            return r;
        }
    }
    [0.0, 0.0, 0.0, 1.0]
}

/// The nearest value to `t` that passes [`Transform::validate`] (`maintain_transforms`' repair).
fn repaired(t: &Transform, has_frame: bool) -> Transform {
    let mut r = *t;
    if has_frame {
        r.chunk = [0; 3];
    } else {
        r.chunk = r.chunk.map(|c| c.clamp(-ROOT_CHUNK_LIMIT, ROOT_CHUNK_LIMIT));
    }
    r.pos = r.pos.map(|p| if p.is_finite() { p.clamp(-POS_LIMIT, POS_LIMIT) } else { 0.0 });
    r.rot = repaired_rot(r.rot);
    r.scale = r.scale.map(|v| {
        if !v.is_finite() || v == 0.0 {
            1.0
        } else {
            let m = v.abs().clamp(SCALE_MIN, SCALE_MAX);
            if v < 0.0 { -m } else { m }
        }
    });
    r._pad = 0;
    r
}

impl SimStore {
    /// The nearest ancestor of `id` that has a `Transform` (its frame).
    pub fn frame_of(&self, id: ObjectId) -> Option<ObjectId> {
        let mut at = self.parent(id);
        while let Some(a) = at {
            if self.has::<Transform>(a) {
                return Some(a);
            }
            at = self.parent(a);
        }
        None
    }

    /// `id` itself if it has a `Transform`, else its frame.
    pub(crate) fn frame_or_self(&self, id: ObjectId) -> Option<ObjectId> {
        if self.has::<Transform>(id) { Some(id) } else { self.frame_of(id) }
    }

    /// The transformed objects whose frame is `id` or would be `id` if it had a `Transform`: descendants
    /// reached without passing another transformed object. In depth-first order; linear in what it visits.
    pub(crate) fn frame_children(&self, id: ObjectId) -> Vec<ObjectId> {
        let mut out = Vec::new();
        let mut stack: Vec<ObjectId> = self.children(id).collect::<Vec<_>>().into_iter().rev().collect();
        while let Some(x) = stack.pop() {
            if self.has::<Transform>(x) {
                out.push(x);
            } else {
                stack.extend(self.children(x).collect::<Vec<_>>().into_iter().rev());
            }
        }
        out
    }

    /// For every row of the Transform column, in column order, whether the object has a frame. One
    /// depth-first pass from the roots (review P21R-2).
    pub(crate) fn transform_frame_flags(&self) -> Vec<bool> {
        let ids = self.column::<Transform>().0;
        let mut flags = vec![false; ids.len()];
        let roots: Vec<ObjectId> = self.roots().collect();
        let mut stack: Vec<(ObjectId, bool)> = roots.into_iter().map(|r| (r, false)).collect();
        while let Some((id, above)) = stack.pop() {
            let here = match ids.binary_search(&id) {
                Ok(i) => {
                    flags[i] = above;
                    true
                }
                Err(_) => false,
            };
            for k in self.children(id) {
                stack.push((k, above || here));
            }
        }
        flags
    }

    /// The world transform of one object, composed from the top down. `None` if it has no `Transform`, or if its
    /// world position is out of range (an edit can never produce one: pe-world refuses it).
    pub fn world_transform(&self, id: ObjectId) -> Option<WorldTransform> {
        let t = self.get::<Transform>(id)?;
        // The transformed ancestors, nearest first; then compose from the top down (no recursion, so any
        // depth works).
        let mut chain = vec![t];
        let mut at = self.parent(id);
        while let Some(a) = at {
            if let Some(ta) = self.get::<Transform>(a) {
                chain.push(ta);
            }
            at = self.parent(a);
        }
        let mut w = WorldTransform::root(chain.pop().unwrap())?;
        while let Some(t) = chain.pop() {
            w = w.compose(t)?;
        }
        Some(w)
    }

    /// Every object's world transform, in depth-first order (objects without a `Transform` are skipped).
    /// One pass: each frame is computed once. An object whose world position is out of range is skipped with
    /// its whole subtree (deterministic; an edit can never produce one).
    pub fn world_transforms(&self) -> Vec<(ObjectId, WorldTransform)> {
        let mut out = Vec::with_capacity(self.count(ComponentId::TRANSFORM));
        let roots: Vec<ObjectId> = self.roots().collect();
        let mut stack: Vec<(ObjectId, Option<WorldTransform>)> = roots.into_iter().rev().map(|r| (r, None)).collect();
        while let Some((id, frame)) = stack.pop() {
            let frame = match self.get::<Transform>(id) {
                Some(t) => {
                    let w = match &frame {
                        Some(p) => p.compose(t),
                        None => WorldTransform::root(t),
                    };
                    let Some(w) = w else { continue };
                    out.push((id, w));
                    Some(w)
                }
                None => frame,
            };
            let kids: Vec<ObjectId> = self.children(id).collect();
            stack.extend(kids.into_iter().rev().map(|k| (k, frame)));
        }
        out
    }

    /// New local transforms that keep `objects`, which all have the frame `old_frame` now, at their world
    /// transforms in the frame `new_frame` (`None`: placed as roots). Each frame's world transform is
    /// computed once. Validated; nothing is written.
    pub(crate) fn relocalize(
        &self,
        objects: &[ObjectId],
        old_frame: Option<ObjectId>,
        new_frame: Option<ObjectId>,
    ) -> Result<Vec<(ObjectId, Transform)>, Error> {
        let wt = |f: ObjectId| self.world_transform(f).ok_or(Error::OutOfRange { object: f });
        let old = old_frame.map(wt).transpose()?;
        let new = new_frame.map(wt).transpose()?;
        self.relocalize_in(objects, old.as_ref(), new.as_ref())
    }

    fn relocalize_in(
        &self,
        objects: &[ObjectId],
        old: Option<&WorldTransform>,
        new: Option<&WorldTransform>,
    ) -> Result<Vec<(ObjectId, Transform)>, Error> {
        objects
            .iter()
            .map(|&o| {
                let t = self.get::<Transform>(o).expect("relocalized objects have a Transform");
                let w = match old {
                    Some(f) => f.compose(t),
                    None => WorldTransform::root(t),
                }
                .ok_or(Error::OutOfRange { object: o })?;
                let lt = w.local_in(new);
                lt.validate(new.is_some()).map_err(|field| invalid(o, field))?;
                Ok((o, lt))
            })
            .collect()
    }

    /// Validates and writes a `Transform` as given (the path behind `insert::<Transform>` and `insert_raw`).
    /// A data edit: nothing else is rewritten, so objects below that take this object as their new frame
    /// keep their local values. Refused if one of them would then have a frame and a non-zero `chunk`.
    pub(crate) fn insert_transform(&mut self, id: ObjectId, t: Transform) -> Result<(), Error> {
        t.validate(self.frame_of(id).is_some()).map_err(|field| invalid(id, field))?;
        if !self.has::<Transform>(id) && self.frame_of(id).is_none() {
            // Objects below that had no frame gain one.
            for k in self.frame_children(id) {
                if self.get::<Transform>(k).unwrap().chunk != [0; 3] {
                    return Err(Error::ChunkUnderFrame { object: k });
                }
            }
        }
        self.put_row(ComponentId::TRANSFORM, id, bytemuck::bytes_of(&t));
        Ok(())
    }

    /// Removes a `Transform` (a data edit: objects below keep their local values in the frame above).
    pub(crate) fn remove_transform(&mut self, id: ObjectId) -> Result<bool, Error> {
        Ok(self.drop_row(ComponentId::TRANSFORM, id))
    }

    /// Inserts or overwrites a `Transform`; if the object had none, the transformed objects below it keep
    /// their world transforms in the new frame.
    pub fn insert_transform_keep_world(&mut self, id: ObjectId, t: Transform) -> Result<(), Error> {
        if !self.exists(id) {
            return Err(Error::UnknownObject(id));
        }
        let frame = self.frame_of(id);
        t.validate(frame.is_some()).map_err(|field| invalid(id, field))?;
        if self.has::<Transform>(id) {
            self.put_row(ComponentId::TRANSFORM, id, bytemuck::bytes_of(&t));
            return Ok(());
        }
        let old = frame.map(|f| self.world_transform(f).ok_or(Error::OutOfRange { object: f })).transpose()?;
        let new = match &old {
            Some(f) => f.compose(&t),
            None => WorldTransform::root(&t),
        }
        .ok_or(Error::OutOfRange { object: id })?;
        let kids = self.frame_children(id);
        let rewritten = self.relocalize_in(&kids, old.as_ref(), Some(&new))?;
        self.put_row(ComponentId::TRANSFORM, id, bytemuck::bytes_of(&t));
        for (k, lt) in rewritten {
            self.put_row(ComponentId::TRANSFORM, k, bytemuck::bytes_of(&lt));
        }
        Ok(())
    }

    /// Removes a `Transform`; the transformed objects below keep their world transforms in the frame above.
    pub fn remove_transform_keep_world(&mut self, id: ObjectId) -> Result<bool, Error> {
        if !self.has::<Transform>(id) {
            return Ok(false);
        }
        let kids = self.frame_children(id);
        let rewritten = self.relocalize(&kids, Some(id), self.frame_of(id))?;
        self.drop_row(ComponentId::TRANSFORM, id);
        for (k, lt) in rewritten {
            self.put_row(ComponentId::TRANSFORM, k, bytemuck::bytes_of(&lt));
        }
        Ok(true)
    }

    /// Rebases root positions into their chunk (exact; review P21-7) where the rebased form is within the
    /// limits, and repairs any `Transform` outside them (written through `get_mut`; review P21R-5). Returns
    /// how many rows it changed. Linear in the objects.
    ///
    /// As a system it must run **after** every system that writes transforms through `get_mut` or
    /// `column_mut`: schedule it last (review P21S-3), so every state that is snapshotted or sent to a
    /// joiner has been repaired.
    pub fn maintain_transforms(&mut self) -> usize {
        let flags = self.transform_frame_flags();
        let ids: Vec<ObjectId> = self.column::<Transform>().0.to_vec();
        let mut changed = 0;
        for (i, id) in ids.into_iter().enumerate() {
            let t = *self.get::<Transform>(id).unwrap();
            let mut n = if t.validate(flags[i]).is_ok() { t } else { repaired(&t, flags[i]) };
            if !flags[i] {
                let r = n.rebased();
                if r.validate(false).is_ok() {
                    n = r;
                }
            }
            if n != t {
                self.put_row(ComponentId::TRANSFORM, id, bytemuck::bytes_of(&n));
                changed += 1;
            }
        }
        changed
    }
}

/// [`SimStore::maintain_transforms`] as a system, for schedules whose systems write transforms through
/// `get_mut` or `column_mut`. Add it **last** (review P21S-3).
pub fn maintain_transforms(store: &mut SimStore, _ctx: &TickCtx) {
    store.maintain_transforms();
}

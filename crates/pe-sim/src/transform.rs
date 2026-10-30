//! The built-in [`Transform`] component and world-transform propagation (design 16.2, 25.2).
//!
//! * Position is `f64`, rotation and scale `f32` (25.2: the same split as box3d's double-precision mode).
//! * An object's *frame* is its nearest ancestor that has a `Transform`. An object with no frame is placed
//!   as a root: its `chunk` (level-0 chunk, `i64`) plus `pos` from that chunk's corner. An object with a
//!   frame is placed relative to it, and its `chunk` must be zero.
//! * **Hierarchy edits keep world transforms** (review P21-6): reparenting an object, or giving or taking
//!   a `Transform` from one of its ancestors, rewrites the affected local transforms so their world
//!   transforms stay where they were (up to rounding), with the root chunk recomputed exactly. An edit
//!   whose rewritten transform would fall outside the limits below is refused.
//! * **Root positions are kept inside their chunk** (review P21-7): inserting a root transform rebases it
//!   exactly, and [`rebase_roots`] does the same for roots that sim code has moved through `get_mut`.
//! * **Limits** (review P21-2), checked on insert, in `check_values` and on loading bytes: `|pos| <= 2^50` m
//!   (about 1.1e15 m: chunks hold the far range); a root's `|chunk| <= 2^61`; scale components non-zero
//!   with magnitude in [2^-40, 2^40]; the rotation's squared norm within 2% of 1; `_pad` zero.
//! * Composition: `world.pos = frame.pos + rotate(frame.rot, frame.scale * local.pos)` in `f64`, then
//!   renormalized into its chunk; `world.rot = normalize(frame.rot * local.rot)` and `world.scale =
//!   frame.scale * local.scale` (clamped to [2^-100, 2^100] in magnitude) in `f32`. Non-uniform scale under
//!   rotation does not shear children; it multiplies per axis, as in most engines. Every step is plain IEEE
//!   arithmetic in a fixed order and never panics, so [`SimStore::world_transform`] and
//!   [`SimStore::world_transforms`] give the same bits everywhere.

use bytemuck::{Pod, Zeroable};
use core::mem::offset_of;
use pe_core::coords::WorldPos;
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
    pub fn at(p: WorldPos) -> Transform {
        Transform { chunk: p.chunk, pos: p.local, ..Transform::IDENTITY }
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

    /// The same root placement with `pos` moved inside its chunk (exact).
    pub fn rebased(&self) -> Transform {
        let p = WorldPos::new(self.chunk, self.pos);
        Transform { chunk: p.chunk, pos: p.local, ..*self }
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

/// A world transform: derived from the hierarchy and the local transforms, never stored or hashed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldTransform {
    pub pos: WorldPos,
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

/// Rotates `v` by the unit quaternion `q`, in `f64` (the quaternion converts exactly):
/// `v + 2w (u x v) + 2 u x (u x v)`.
pub fn rotate(q: [f32; 4], v: [f64; 3]) -> [f64; 3] {
    let (ux, uy, uz, w) = (q[0] as f64, q[1] as f64, q[2] as f64, q[3] as f64);
    let cross = |a: [f64; 3], b: [f64; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let u = [ux, uy, uz];
    let t = cross(u, v);
    let t2 = cross(u, t);
    [
        v[0] + 2.0 * (w * t[0] + t2[0]),
        v[1] + 2.0 * (w * t[1] + t2[1]),
        v[2] + 2.0 * (w * t[2] + t2[2]),
    ]
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
    /// The world transform of an object without a frame.
    pub fn root(t: &Transform) -> WorldTransform {
        WorldTransform {
            pos: WorldPos::new(t.chunk, t.pos),
            rot: quat_normalize(t.rot),
            scale: t.scale.map(clamp_scale),
        }
    }

    /// The world transform of an object with local transform `t` in this frame.
    pub fn compose(&self, t: &Transform) -> WorldTransform {
        let scaled = [
            self.scale[0] as f64 * t.pos[0],
            self.scale[1] as f64 * t.pos[1],
            self.scale[2] as f64 * t.pos[2],
        ];
        WorldTransform {
            pos: self.pos.offset(rotate(self.rot, scaled)),
            rot: quat_normalize(quat_mul(self.rot, t.rot)),
            scale: [
                clamp_scale(self.scale[0] * t.scale[0]),
                clamp_scale(self.scale[1] * t.scale[1]),
                clamp_scale(self.scale[2] * t.scale[2]),
            ],
        }
    }

    /// The local transform that places an object at `self` in the frame `frame` (the inverse of
    /// [`WorldTransform::compose`], up to rounding), or as a root if `frame` is `None`.
    pub fn local_in(&self, frame: Option<&WorldTransform>) -> Transform {
        let Some(f) = frame else {
            return Transform { chunk: self.pos.chunk, pos: self.pos.local, rot: self.rot, scale: self.scale, _pad: 0 };
        };
        let inv = quat_conj(f.rot);
        let d = rotate(inv, self.pos.sub(&f.pos));
        Transform {
            chunk: [0; 3],
            pos: [d[0] / f.scale[0] as f64, d[1] / f.scale[1] as f64, d[2] / f.scale[2] as f64],
            rot: quat_normalize(quat_mul(inv, self.rot)),
            scale: [self.scale[0] / f.scale[0], self.scale[1] / f.scale[1], self.scale[2] / f.scale[2]],
            _pad: 0,
        }
    }
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
    /// reached without passing another transformed object. In depth-first order.
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

    /// The world transform of one object, composed from the top down. `None` if it has no `Transform`.
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
        let mut w = WorldTransform::root(chain.pop().unwrap());
        while let Some(t) = chain.pop() {
            w = w.compose(t);
        }
        Some(w)
    }

    /// Every object's world transform, in depth-first order (objects without a `Transform` are skipped).
    /// One pass: each frame is computed once.
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

    /// New local transforms that keep `objects` at their current world transforms in the frame `frame`
    /// (an object, or `None` for root placement). Validated; nothing is written.
    pub(crate) fn relocalize(&self, objects: &[ObjectId], frame: Option<ObjectId>) -> Result<Vec<(ObjectId, Transform)>, Error> {
        let fw = frame.map(|f| self.world_transform(f).expect("a frame has a Transform"));
        objects
            .iter()
            .map(|&o| {
                let w = self.world_transform(o).expect("relocalized objects have a Transform");
                let t = w.local_in(fw.as_ref());
                let t = if fw.is_none() { t.rebased() } else { t };
                t.validate(fw.is_some()).map_err(|field| Error::InvalidValue {
                    component: ComponentId::TRANSFORM.0,
                    object: o,
                    field: field.to_string(),
                })?;
                Ok((o, t))
            })
            .collect()
    }

    /// Validates and writes a `Transform` (the path behind `insert::<Transform>` and `insert_raw`). If the
    /// object had none, the transformed objects below it keep their world transforms in the new frame.
    pub(crate) fn insert_transform(&mut self, id: ObjectId, mut t: Transform) -> Result<(), Error> {
        let has_frame = self.frame_of(id).is_some();
        let bad = |field: &str| Error::InvalidValue { component: ComponentId::TRANSFORM.0, object: id, field: field.to_string() };
        t.validate(has_frame).map_err(bad)?;
        if !has_frame {
            t = t.rebased();
        }
        if self.has::<Transform>(id) {
            self.put_row(ComponentId::TRANSFORM, id, bytemuck::bytes_of(&t));
            return Ok(());
        }
        // The frame children's new locals need the new frame's world transform: compose it now.
        let new_frame = match self.frame_of(id).and_then(|f| self.world_transform(f)) {
            Some(f) => f.compose(&t),
            None => WorldTransform::root(&t),
        };
        let kids = self.frame_children(id);
        let mut rewritten = Vec::with_capacity(kids.len());
        for k in kids {
            let w = self.world_transform(k).expect("frame children have a Transform");
            let lt = w.local_in(Some(&new_frame));
            lt.validate(true).map_err(|field| Error::InvalidValue {
                component: ComponentId::TRANSFORM.0,
                object: k,
                field: field.to_string(),
            })?;
            rewritten.push((k, lt));
        }
        self.put_row(ComponentId::TRANSFORM, id, bytemuck::bytes_of(&t));
        for (k, lt) in rewritten {
            self.put_row(ComponentId::TRANSFORM, k, bytemuck::bytes_of(&lt));
        }
        Ok(())
    }

    /// Removes a `Transform`; the transformed objects below keep their world transforms in the frame above.
    pub(crate) fn remove_transform(&mut self, id: ObjectId) -> Result<bool, Error> {
        if !self.has::<Transform>(id) {
            return Ok(false);
        }
        let kids = self.frame_children(id);
        let rewritten = self.relocalize(&kids, self.frame_of(id))?;
        self.drop_row(ComponentId::TRANSFORM, id);
        for (k, lt) in rewritten {
            self.put_row(ComponentId::TRANSFORM, k, bytemuck::bytes_of(&lt));
        }
        Ok(true)
    }

    /// Moves every root transform's position inside its chunk (exact; review P21-7).
    pub fn rebase_roots(&mut self) {
        let ids: Vec<ObjectId> = self.column::<Transform>().0.to_vec();
        for id in ids {
            if self.frame_of(id).is_none() {
                let t = self.get::<Transform>(id).unwrap().rebased();
                if t != *self.get::<Transform>(id).unwrap() {
                    self.put_row(ComponentId::TRANSFORM, id, bytemuck::bytes_of(&t));
                }
            }
        }
    }
}

/// [`SimStore::rebase_roots`] as a system, for schedules whose systems move roots through `get_mut`.
pub fn rebase_roots(store: &mut SimStore, _ctx: &TickCtx) {
    store.rebase_roots();
}

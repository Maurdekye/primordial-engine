//! The built-in [`Transform`] component and world-transform propagation (design 16.2, 25.2).
//!
//! * Position is `f64`, rotation and scale `f32` (25.2: the same split as box3d's double-precision mode).
//! * A root is placed by its level-0 chunk (`i64`) plus an `f64` position from that chunk's corner. A child's
//!   position is relative to its parent, and its `chunk` field is ignored (keep it zero).
//! * An object's parent frame is its nearest ancestor that has a `Transform`. An object with none is
//!   placed as a root.
//! * Composition: `world.pos = parent.pos + rotate(parent.rot, parent.scale * local.pos)` in `f64`, then
//!   renormalized into its chunk; `world.rot = parent.rot * local.rot` and `world.scale = parent.scale *
//!   local.scale` in `f32`. Non-uniform scale under rotation does not shear children; the scale simply
//!   multiplies per axis, as in most engines. Every step is plain IEEE arithmetic in a fixed order, so the
//!   result is bit-identical everywhere, and [`SimStore::world_transform`] and
//!   [`SimStore::world_transforms`] give the same bits.

use bytemuck::{Pod, Zeroable};
use core::mem::offset_of;
use pe_core::coords::WorldPos;
use pe_core::ObjectId;

use crate::registry::{Component, ComponentId, FieldDesc, FieldKind};
use crate::store::SimStore;

/// A local transform. 80 bytes, no implicit padding.
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct Transform {
    /// Roots only: the level-0 chunk the position is measured from.
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

    /// A child transform at `pos` relative to its parent.
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

impl WorldTransform {
    /// A root's world transform.
    pub fn root(t: &Transform) -> WorldTransform {
        WorldTransform { pos: WorldPos::new(t.chunk, t.pos), rot: t.rot, scale: t.scale }
    }

    /// The world transform of a child with local transform `t` under this one.
    pub fn compose(&self, t: &Transform) -> WorldTransform {
        let scaled = [
            self.scale[0] as f64 * t.pos[0],
            self.scale[1] as f64 * t.pos[1],
            self.scale[2] as f64 * t.pos[2],
        ];
        WorldTransform {
            pos: self.pos.offset(rotate(self.rot, scaled)),
            rot: quat_mul(self.rot, t.rot),
            scale: [self.scale[0] * t.scale[0], self.scale[1] * t.scale[1], self.scale[2] * t.scale[2]],
        }
    }
}

impl SimStore {
    /// The world transform of one object, composed from its root down. `None` if it has no `Transform`.
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
    /// One pass: each parent frame is computed once.
    pub fn world_transforms(&self) -> Vec<(ObjectId, WorldTransform)> {
        let mut out = Vec::with_capacity(self.count(ComponentId::TRANSFORM));
        // Depth-first with a stack of (object, frame of its nearest transformed ancestor).
        let roots: Vec<ObjectId> = self.roots().collect();
        let mut stack: Vec<(ObjectId, Option<WorldTransform>)> = roots.into_iter().rev().map(|r| (r, None)).collect();
        while let Some((id, parent_frame)) = stack.pop() {
            let frame = match self.get::<Transform>(id) {
                Some(t) => {
                    let w = match &parent_frame {
                        Some(p) => p.compose(t),
                        None => WorldTransform::root(t),
                    };
                    out.push((id, w));
                    Some(w)
                }
                None => parent_frame,
            };
            let kids: Vec<ObjectId> = self.children(id).collect();
            stack.extend(kids.into_iter().rev().map(|k| (k, frame)));
        }
        out
    }
}

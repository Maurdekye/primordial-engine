//! Engine components a script may bind (docs/spec/logic.md 3.2): byte-for-byte copies of the engine's own, named
//! `pe.<Name>` in the manifest so they can never collide with a script's own components (whose names are
//! identifiers). The host binds each to the engine's id only if the world lists it for scripts, refuses a `write` the
//! world does not allow, and runs the world's edit checks on every row a script writes (world-model.md 7.9).
//! pe-logic's tests check these layouts against pe-world's descriptions.
//!
//! Positions keep the wall of [`crate::space`] (logic.md 8): [`Transform`] has no public position fields, and
//! [`Pose`], which the host computes, hands out a [`Pos`] and conversions between spaces, never plain numbers of a
//! place.

use crate::component::{Component, FieldDesc, LinearRgb};
use crate::layout::FieldKind;
use crate::space::{Offset, OutOfRange, Own, Parent, Pos, Space, World};

macro_rules! engine_component {
    ($(#[$m:meta])* $name:ident = $manifest:literal { $($(#[$fm:meta])* $fv:vis $f:ident : $t:ty),* $(,)? }) => {
        $(#[$m])*
        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct $name { $($(#[$fm])* $fv $f: $t),* }

        impl Component for $name {
            const NAME: &'static str = $manifest;
            const FIELDS: &'static [FieldDesc] = &[
                $(FieldDesc {
                    name: stringify!($f),
                    kind: <$t as crate::component::Field>::KIND,
                    count: <$t as crate::component::Field>::COUNT,
                    offset: ::core::mem::offset_of!($name, $f) as u32,
                    color: <$t as crate::component::Field>::COLOR,
                }),*
            ];
        }
    };
}

engine_component! {
    /// A light source (pe-world `Light`). `kind`: 0 point, 1 spot, 2 directional. A spot light's cone is
    /// `0 <= inner_angle <= outer_angle <= pi/2` radians; both are 0 for other kinds. A row outside these faults the
    /// call that wrote it.
    Light = "pe.Light" {
        pub kind: u32,
        pub intensity: f32,
        /// Metres.
        pub range: f32,
        /// Linear light, sRGB primaries (the engine's `Light.color` is marked a colour, world-model.md 3.6).
        pub color: LinearRgb,
        pub inner_angle: f32,
        pub outer_angle: f32,
    }
}

engine_component! {
    /// An object's own transform, relative to its parent (pe-sim `Transform`, 80 bytes), as its inspector shows it.
    ///
    /// Its position is read through [`Transform::place`]: a root's is a [`Pos`], a child's an [`Offset<Parent>`]. The
    /// fields are private, so a root's place never comes out as plain numbers (logic.md 8). Rotation and scale are
    /// plain numbers: a unit quaternion `[x, y, z, w]` and a per-axis scale, relative to the parent.
    Transform = "pe.Transform" {
        /// Roots only: the level-0 chunk the position is measured from; zero for a child.
        chunk: [i64; 3],
        /// From the chunk's corner for a root; the offset in the parent's space for a child.
        pos: [f64; 3],
        rot: [f32; 4],
        scale: [f32; 3],
        _pad: u32,
    }
}

engine_component! {
    /// The bound object's pose in the world, which the host computes for each bound row from the store's composed
    /// transforms (`SimStore::world_transform`, ancestors included). Read-only, never stored or hashed. Binding it
    /// needs the object to have a [`Transform`]; a system binds it to convert between spaces or to save where it is.
    Pose = "pe.Pose" {
        pos: Pos,
        rot: [f32; 4],
        scale: [f32; 3],
        /// 1 if the object has no frame (no ancestor with a `Transform`), so its `Transform` holds a `Pos`.
        root: u32,
    }
}

/// Where an object is, relative to its parent (as in Resonite, file 17 7.3): a root's place is a position, a child's
/// an offset in its parent's space.
#[derive(Clone, Copy, Debug)]
pub enum Place {
    Root(Pos),
    Child(Offset<Parent>),
}

/// Why [`Transform::set_place`] or [`Transform::translate`] wrote nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceError {
    /// The position is past the range the store keeps.
    OutOfRange,
    /// A `Root` place for a child, or a `Child` place for a root.
    Mismatch,
}

impl From<OutOfRange> for PlaceError {
    fn from(_: OutOfRange) -> PlaceError {
        PlaceError::OutOfRange
    }
}

/// The largest local position component the store keeps (pe-sim `POS_LIMIT`, 2^50 m).
const POS_LIMIT: f64 = 1_125_899_906_842_624.0;

impl Transform {
    /// The identity: no offset, no rotation, unit scale.
    pub const IDENTITY: Transform =
        Transform { chunk: [0; 3], pos: [0.0; 3], rot: [0.0, 0.0, 0.0, 1.0], scale: [1.0; 3], _pad: 0 };

    /// The object's place relative to its parent. Which kind it is comes from the object's [`Pose`]: the row alone
    /// can't tell a root at chunk 0 from a child.
    pub fn place(&self, pose: &Pose) -> Place {
        if pose.is_root() {
            // The store keeps a root's row in range, so this holds; a row that broke it would be refused first.
            match Pos::from_root_row(self.chunk, self.pos) {
                Ok(p) => Place::Root(p),
                Err(_) => panic!("a root transform outside the range"),
            }
        } else {
            Place::Child(Offset::new(self.pos[0] as f32, self.pos[1] as f32, self.pos[2] as f32))
        }
    }

    /// Sets the object's place; the kind must match the object's ([`Pose::is_root`]).
    pub fn set_place(&mut self, pose: &Pose, place: Place) -> Result<(), PlaceError> {
        match (place, pose.is_root()) {
            (Place::Root(p), true) => {
                let (chunk, local) = p.to_root_row();
                self.chunk = chunk;
                self.pos = local;
                Ok(())
            }
            (Place::Child(o), false) => {
                self.chunk = [0; 3];
                self.pos = [o.x as f64, o.y as f64, o.z as f64];
                Ok(())
            }
            _ => Err(PlaceError::Mismatch),
        }
    }

    /// Moves the object by `d` in its parent's space (world axes for a root). The same for a root and a child: both
    /// keep a position from their reference, so no pose is needed. Past the store's limit nothing is written.
    pub fn translate(&mut self, d: Offset<Parent>) -> Result<(), PlaceError> {
        let mut p = self.pos;
        for (v, a) in p.iter_mut().zip(d.to_array()) {
            *v += a as f64;
            if !(*v).is_finite() || *v > POS_LIMIT || *v < -POS_LIMIT {
                return Err(PlaceError::OutOfRange);
            }
            if *v == 0.0 {
                *v = 0.0;
            }
        }
        self.pos = p;
        Ok(())
    }

    /// The rotation relative to the parent, a unit quaternion `[x, y, z, w]`.
    pub fn rot(&self) -> [f32; 4] {
        self.rot
    }

    pub fn set_rot(&mut self, q: [f32; 4]) {
        self.rot = q;
    }

    /// The scale relative to the parent, per axis.
    pub fn scale(&self) -> [f32; 3] {
        self.scale
    }

    pub fn set_scale(&mut self, s: [f32; 3]) {
        self.scale = s;
    }

    /// An offset in the object's own space, in its parent's: scaled, then rotated.
    pub fn to_parent(&self, v: Offset<Own>) -> Offset<Parent> {
        forward(self.rot, self.scale, v)
    }

    /// An offset in the parent's space, in the object's own: rotated back, then unscaled.
    pub fn to_own(&self, v: Offset<Parent>) -> Offset<Own> {
        back(self.rot, self.scale, v)
    }
}

impl Pose {
    /// Where the object is.
    pub fn pos(&self) -> Pos {
        self.pos
    }

    /// The world rotation, a unit quaternion `[x, y, z, w]`.
    pub fn rot(&self) -> [f32; 4] {
        self.rot
    }

    /// The world scale, per axis.
    pub fn scale(&self) -> [f32; 3] {
        self.scale
    }

    /// True if the object has no frame, so its [`Transform`]'s place is a [`Place::Root`].
    pub fn is_root(&self) -> bool {
        self.root != 0
    }

    /// An offset in the object's own space, along the world's axes.
    pub fn to_world(&self, v: Offset<Own>) -> Offset<World> {
        forward(self.rot, self.scale, v)
    }

    /// An offset along the world's axes, in the object's own space.
    pub fn to_own(&self, v: Offset<World>) -> Offset<Own> {
        back(self.rot, self.scale, v)
    }

    /// The place of the point at `v` in the object's own space.
    pub fn at(&self, v: Offset<Own>) -> Result<Pos, OutOfRange> {
        self.pos.checked_add(self.to_world(v))
    }
}

/// `rotate(q, s * v)`, in a fixed order of `f32` operations.
fn forward<A: Space, B: Space>(q: [f32; 4], s: [f32; 3], v: Offset<A>) -> Offset<B> {
    let r = rotate(q, [v.x * s[0], v.y * s[1], v.z * s[2]]);
    Offset::new(r[0], r[1], r[2])
}

/// `rotate(conj(q), v) / s`, in a fixed order of `f32` operations.
fn back<A: Space, B: Space>(q: [f32; 4], s: [f32; 3], v: Offset<A>) -> Offset<B> {
    let r = rotate([-q[0], -q[1], -q[2], q[3]], v.to_array());
    Offset::new(r[0] / s[0], r[1] / s[1], r[2] / s[2])
}

/// `v` rotated by the unit quaternion `q`: `v + 2w (u × v) + 2 u × (u × v)`, with `u` the vector part.
fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let [x, y, z, w] = q;
    let cross =
        |a: [f32; 3], b: [f32; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let u = [x, y, z];
    let t = cross(u, v);
    let t = [2.0 * t[0], 2.0 * t[1], 2.0 * t[2]];
    let c = cross(u, t);
    [v[0] + w * t[0] + c[0], v[1] + w * t[1] + c[1], v[2] + w * t[2] + c[2]]
}

// The field kinds the three use, so a change to any is a compile error here as well as a test failure in pe-logic.
const _: () = {
    assert!(matches!(<f32 as crate::component::Field>::KIND, FieldKind::F32));
    assert!(core::mem::size_of::<Light>() == 32);
    assert!(core::mem::size_of::<Transform>() == 80);
    assert!(core::mem::size_of::<Pose>() == 80);
    assert!(core::mem::offset_of!(Pose, rot) == 48);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::ORIGIN;

    fn pose(pos: Pos, rot: [f32; 4], scale: [f32; 3], root: bool) -> Pose {
        Pose { pos, rot, scale, root: root as u32 }
    }

    #[test]
    fn a_root_place_is_a_pos_and_a_child_place_an_offset() {
        let mut t = Transform::IDENTITY;
        let root = pose(ORIGIN, [0.0, 0.0, 0.0, 1.0], [1.0; 3], true);
        let p = ORIGIN + Offset::new(3000.0, -5.0, 0.5);
        t.set_place(&root, Place::Root(p)).unwrap();
        assert_eq!(t.chunk, [2, -1, 0]);
        match t.place(&root) {
            Place::Root(q) => assert_eq!(q, p),
            Place::Child(_) => panic!("a root"),
        }
        let child = pose(ORIGIN, [0.0, 0.0, 0.0, 1.0], [1.0; 3], false);
        assert_eq!(t.set_place(&child, Place::Root(p)), Err(PlaceError::Mismatch));
        t.set_place(&child, Place::Child(Offset::new(1.0, 2.0, 3.0))).unwrap();
        assert_eq!((t.chunk, t.pos), ([0; 3], [1.0, 2.0, 3.0]));
        t.translate(Offset::new(-1.0, 0.0, 0.0)).unwrap();
        assert_eq!(t.pos[0].to_bits(), 0.0f64.to_bits());
        assert_eq!(t.translate(Offset::new(1e15, 0.0, 0.0)), Ok(()));
        let before = t;
        assert_eq!(t.translate(Offset::new(0.0, 0.0, 1.0)), Ok(()));
        assert_eq!(t.translate(Offset::new(1e15, 0.0, 0.0)), Err(PlaceError::OutOfRange), "past 2^50 m");
        assert_eq!(t.pos, [before.pos[0], before.pos[1], 4.0], "a refused move writes nothing");
    }

    #[test]
    fn conversions_go_there_and_back() {
        // A quarter turn about z, scale 2: own +x is world +y, twice as long.
        let h = core::f32::consts::FRAC_1_SQRT_2;
        let p = pose(ORIGIN, [0.0, 0.0, h, h], [2.0; 3], true);
        let w = p.to_world(Offset::new(1.0, 0.0, 0.0));
        assert!((w.x).abs() < 1e-6 && (w.y - 2.0).abs() < 1e-6 && w.z.abs() < 1e-6, "{w:?}");
        let back = p.to_own(w);
        assert!((back.x - 1.0).abs() < 1e-6 && back.y.abs() < 1e-6, "{back:?}");
        assert!((p.at(Offset::new(1.0, 0.0, 0.0)).unwrap() - ORIGIN).within(2.0 + 1e-5));
    }
}

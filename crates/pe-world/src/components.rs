//! The engine's components for M1 (world-model.md 3.2), beside pe-sim's `Node` and `Transform`.
//!
//! Each is a fixed-size row of plain data, little-endian, with a runtime description that covers it exactly. The
//! descriptions are also the schema WorldApi publishes, so field names are part of the interface: fields whose name
//! starts with `_` are padding, always zero, and readers skip them.

use bytemuck::{Pod, Zeroable};
use core::mem::offset_of;
use pe_core::MemberKey;
use pe_sim::{Component, ComponentId, FieldDesc, FieldKind, Registry};

/// A reference to a content-addressed blob (3.4): its BLAKE3 hash and its length. All zero is "none". The same
/// layout serves `BlobRef` (display) and `SimBlobRef` (simulation) fields; the field's kind says which.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(C)]
pub struct BlobRef {
    pub hash: [u8; 32],
    pub len: u64,
}

impl BlobRef {
    pub const NONE: BlobRef = BlobRef { hash: [0; 32], len: 0 };

    /// The reference to `bytes`: `BLAKE3(bytes)` (plain, a content address, 6.1) and the length.
    pub fn of(bytes: &[u8]) -> BlobRef {
        BlobRef { hash: pe_core::Hash::plain(bytes).0, len: bytes.len() as u64 }
    }

    pub fn is_none(&self) -> bool {
        *self == BlobRef::NONE
    }
}

/// Reserved component ids of pe-world's engine components (below [`ComponentId::FIRST_USER`]).
pub mod ids {
    use pe_sim::ComponentId;
    pub const NAME: ComponentId = ComponentId(3);
    pub const SHAPE: ComponentId = ComponentId(4);
    pub const BODY: ComponentId = ComponentId(5);
    pub const BODY_STATE: ComponentId = ComponentId(6);
    pub const APPEARANCE: ComponentId = ComponentId(7);
    pub const LIGHT: ComponentId = ComponentId(8);
    pub const HAND: ComponentId = ComponentId(9);
    pub const SIM_SIZE: ComponentId = ComponentId(10);
    pub const PROTECTION: ComponentId = ComponentId(11);
}

macro_rules! fields {
    ($t:ty; $($name:literal: $field:ident, $kind:ident, $count:expr;)*) => {
        vec![$(FieldDesc::new($name, FieldKind::$kind, offset_of!($t, $field), $count)),*]
    };
}

/// An object's name: UTF-8 bytes in a display blob.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct Name {
    pub text: BlobRef,
}

impl Component for Name {
    const ID: ComponentId = ids::NAME;
    const NAME: &'static str = "Name";
    fn fields() -> Vec<FieldDesc> {
        fields![Name; "text": text, BlobRef, 1;]
    }
}

/// The kinds of [`Shape`].
pub mod shape_kind {
    /// `size` is the half extents. A plane is a large thin box.
    pub const BOX: u32 = 1;
    /// `size[0]` is the radius.
    pub const SPHERE: u32 = 2;
    /// `size[0]` is the radius, `size[1]` the half length of the segment along the local y axis.
    pub const CAPSULE: u32 = 3;
    /// `mesh` is the triangle mesh, for static bodies only (3.2); `size` is the half extents of its bounding box about
    /// the shape's origin (8.1).
    pub const MESH: u32 = 4;
    /// `mesh` is a convex hull; `size` is the half extents of its bounding box about the shape's origin (8.1).
    pub const HULL: u32 = 5;
}

/// A collision shape, at the object's own transform (3.2). Mass comes from `density` (kg per cubic metre).
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct Shape {
    pub kind: u32,
    pub density: f32,
    pub size: [f32; 3],
    pub _pad: u32,
    pub mesh: BlobRef,
}

impl Component for Shape {
    const ID: ComponentId = ids::SHAPE;
    const NAME: &'static str = "Shape";
    fn fields() -> Vec<FieldDesc> {
        fields![Shape;
            "kind": kind, U32, 1;
            "density": density, F32, 1;
            "size": size, F32, 3;
            "_pad": _pad, U32, 1;
            "mesh": mesh, SimBlobRef, 1;
        ]
    }
}

/// The kinds of [`Body`].
pub mod body_kind {
    pub const STATIC: u32 = 0;
    pub const KINEMATIC: u32 = 1;
    pub const DYNAMIC: u32 = 2;
}

/// A rigid body (3.2): its shapes are its own `Shape` plus those of its descendants without a `Body`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct Body {
    pub kind: u32,
    pub friction: f32,
    pub restitution: f32,
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub rolling_resistance: f32,
    pub gravity_scale: f32,
    pub bullet: u8,
    pub may_sleep: u8,
    pub grabbable: u8,
    pub _pad: u8,
}

impl Component for Body {
    const ID: ComponentId = ids::BODY;
    const NAME: &'static str = "Body";
    fn fields() -> Vec<FieldDesc> {
        fields![Body;
            "kind": kind, U32, 1;
            "friction": friction, F32, 1;
            "restitution": restitution, F32, 1;
            "linear_damping": linear_damping, F32, 1;
            "angular_damping": angular_damping, F32, 1;
            "rolling_resistance": rolling_resistance, F32, 1;
            "gravity_scale": gravity_scale, F32, 1;
            "bullet": bullet, Bool, 1;
            "may_sleep": may_sleep, Bool, 1;
            "grabbable": grabbable, Bool, 1;
            "_pad": _pad, U8, 1;
        ]
    }
}

/// Derived and hash-only (3.2): the simulated state of a bound body, written by pe-replica at cut frames. No reader
/// treats it as physics truth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct BodyState {
    pub linear: [f32; 3],
    pub angular: [f32; 3],
    pub sleep_time: f32,
    pub awake: u8,
    pub _pad: [u8; 3],
    /// The body's physics digest (6.3).
    pub digest: [u8; 32],
}

impl Component for BodyState {
    const ID: ComponentId = ids::BODY_STATE;
    const NAME: &'static str = "BodyState";
    fn fields() -> Vec<FieldDesc> {
        fields![BodyState;
            "linear": linear, F32, 3;
            "angular": angular, F32, 3;
            "sleep_time": sleep_time, F32, 1;
            "awake": awake, Bool, 1;
            "_pad": _pad, U8, 3;
            "digest": digest, Hash, 1;
        ]
    }
}

/// How an object is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct Appearance {
    /// Linear RGBA.
    pub base: [f32; 4],
    pub emissive: [f32; 3],
    pub roughness: f32,
    pub metalness: f32,
    pub _pad: u32,
    pub material: BlobRef,
}

impl Component for Appearance {
    const ID: ComponentId = ids::APPEARANCE;
    const NAME: &'static str = "Appearance";
    fn fields() -> Vec<FieldDesc> {
        fields![Appearance;
            "base": base, F32, 4;
            "emissive": emissive, F32, 3;
            "roughness": roughness, F32, 1;
            "metalness": metalness, F32, 1;
            "_pad": _pad, U32, 1;
            "material": material, BlobRef, 1;
        ]
    }
}

/// The kinds of [`Light`].
pub mod light_kind {
    pub const POINT: u32 = 0;
    pub const SPOT: u32 = 1;
    pub const DIRECTIONAL: u32 = 2;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct Light {
    pub kind: u32,
    pub intensity: f32,
    pub range: f32,
    pub color: [f32; 3],
}

impl Component for Light {
    const ID: ComponentId = ids::LIGHT;
    const NAME: &'static str = "Light";
    fn fields() -> Vec<FieldDesc> {
        fields![Light;
            "kind": kind, U32, 1;
            "intensity": intensity, F32, 1;
            "range": range, F32, 1;
            "color": color, F32, 3;
        ]
    }
}

/// A member's hand (section 8): created by world logic at admission, then moved only by its owner's inputs. The
/// hand's target pose is its object's `Transform`; this row holds the rest of the newest input and the stream state
/// pe-replica's rule needs (replica.md 3.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct Hand {
    pub owner: MemberKey,
    /// The frame the newest input is stamped at, and the stamp of the input before it.
    pub stamp: u64,
    pub prev_stamp: u64,
    /// How many frames the newest input covers, and the one before it.
    pub span: u32,
    pub prev_span: u32,
    /// 0 left, 1 right.
    pub side: u32,
    pub grip: f32,
    pub engaged: u8,
    pub has_prev: u8,
    pub _pad: [u8; 6],
}

impl Component for Hand {
    const ID: ComponentId = ids::HAND;
    const NAME: &'static str = "Hand";
    fn fields() -> Vec<FieldDesc> {
        fields![Hand;
            "owner": owner, MemberKey, 1;
            "stamp": stamp, U64, 1;
            "prev_stamp": prev_stamp, U64, 1;
            "span": span, U32, 1;
            "prev_span": prev_span, U32, 1;
            "side": side, U32, 1;
            "grip": grip, F32, 1;
            "engaged": engaged, Bool, 1;
            "has_prev": has_prev, Bool, 1;
            "_pad": _pad, U8, 6;
        ]
    }
}

/// The simulation size (3.5), in metres: positive, finite, at least `MIN_EXTENT`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct SimSize {
    pub metres: f64,
}

impl Component for SimSize {
    const ID: ComponentId = ids::SIM_SIZE;
    const NAME: &'static str = "SimSize";
    fn fields() -> Vec<FieldDesc> {
        fields![SimSize; "metres": metres, F64, 1;]
    }
}

/// Protects the object and its subtree (7.8): only `owner` may edit them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct Protection {
    pub owner: MemberKey,
}

impl Component for Protection {
    const ID: ComponentId = ids::PROTECTION;
    const NAME: &'static str = "Protection";
    fn fields() -> Vec<FieldDesc> {
        fields![Protection; "owner": owner, MemberKey, 1;]
    }
}

/// The registry of a line's first branch from the origin: pe-sim's `Node` and `Transform`, and every component above
/// (3.2).
pub fn engine_registry() -> Registry {
    let mut r = Registry::new();
    r.register_type::<Name>().expect("engine component");
    r.register_type::<Shape>().expect("engine component");
    r.register_type::<Body>().expect("engine component");
    r.register_type::<BodyState>().expect("engine component");
    r.register_type::<Appearance>().expect("engine component");
    r.register_type::<Light>().expect("engine component");
    r.register_type::<Hand>().expect("engine component");
    r.register_type::<SimSize>().expect("engine component");
    r.register_type::<Protection>().expect("engine component");
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_cover_rows() {
        // `register_type` checks each description against its struct; this pins the sizes.
        let r = engine_registry();
        let sizes: Vec<(&str, usize)> = r.iter().map(|d| (d.name(), d.size())).collect();
        assert_eq!(
            sizes,
            [
                ("Node", 192),
                ("Transform", 80),
                ("Name", 40),
                ("Shape", 64),
                ("Body", 32),
                ("BodyState", 64),
                ("Appearance", 80),
                ("Light", 24),
                ("Hand", 72),
                ("SimSize", 8),
                ("Protection", 32),
            ]
        );
    }

    #[test]
    fn blob_ref_is_a_content_address() {
        let empty = BlobRef::of(b"");
        // BLAKE3 of no bytes, from the reference implementation's test vectors.
        let want = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
        let got: String = empty.hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(got, want);
        assert_eq!(BlobRef::of(b"ball").len, 4);
        assert!(BlobRef::NONE.is_none() && !BlobRef::of(b"ball").is_none());
    }
}

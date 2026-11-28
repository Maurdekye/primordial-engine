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
    /// Registered only in worlds whose preset gives members a body ([`register_player`]).
    pub const PLAYER: ComponentId = ComponentId(12);
    /// Registered only in worlds whose preset sets the world's limits as a row ([`register_limits`]).
    pub const LIMITS: ComponentId = ComponentId(13);
    /// Registered only in worlds whose preset holds folders ([`register_folder`]; the home domain).
    pub const FOLDER: ComponentId = ComponentId(14);
    /// Registered only in worlds whose preset holds the starter set ([`register_folder`] registers it too).
    pub const STARTER: ComponentId = ComponentId(15);
}

/// `"name": field, Kind, count;` or, with a unit (world-model.md 3.6), `"name": field, Kind, count, "unit";`.
macro_rules! fields {
    ($t:ty; $($name:literal: $field:ident, $kind:ident, $count:expr $(, $unit:literal)?;)*) => {
        vec![$(FieldDesc::new($name, FieldKind::$kind, offset_of!($t, $field), $count)$(.with_unit($unit))?),*]
    };
}

/// Marks the named fields as colours in linear sRGB (3.6; @coordinator-opus).
fn colours(mut fields: Vec<FieldDesc>, names: &[&str]) -> Vec<FieldDesc> {
    for f in fields.iter_mut().filter(|f| names.contains(&f.name.as_str())) {
        f.color = Some(pe_sim::ColorSpace::LinearSrgb);
    }
    fields
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
            "density": density, F32, 1, "kg/m^3";
            "size": size, F32, 3, "m";
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
            "linear_damping": linear_damping, F32, 1, "1/s";
            "angular_damping": angular_damping, F32, 1, "1/s";
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
            "linear": linear, F32, 3, "m/s";
            "angular": angular, F32, 3, "rad/s";
            "sleep_time": sleep_time, F32, 1, "s";
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
        let f = fields![Appearance;
            "base": base, F32, 4;
            "emissive": emissive, F32, 3;
            "roughness": roughness, F32, 1;
            "metalness": metalness, F32, 1;
            "_pad": _pad, U32, 1;
            "material": material, BlobRef, 1;
        ];
        colours(f, &["base", "emissive"])
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
    /// A spot light's cone (radians from its axis, the object's local -z, as in glTF): full intensity inside
    /// `inner_angle`, none beyond `outer_angle`, with 0 <= inner <= outer <= pi/2. Both are 0 for other kinds, so an
    /// unused angle never makes two equal lights hash differently. An edit outside these is refused
    /// ([`Light::invalid_angle`]).
    pub inner_angle: f32,
    pub outer_angle: f32,
}

impl Light {
    /// The field index (4 `inner_angle`, 5 `outer_angle`) of the first angle an edit may not write: for a spot light,
    /// outside 0 <= inner <= outer <= pi/2 (or not finite); for any other kind, not 0.
    pub fn invalid_angle(&self) -> Option<u32> {
        let (i, o) = (self.inner_angle, self.outer_angle);
        if self.kind == light_kind::SPOT {
            if !(0.0..=std::f32::consts::FRAC_PI_2).contains(&i) {
                return Some(4);
            }
            if !(i..=std::f32::consts::FRAC_PI_2).contains(&o) {
                return Some(5);
            }
            None
        } else if i.to_bits() != 0 {
            Some(4)
        } else if o.to_bits() != 0 {
            Some(5)
        } else {
            None
        }
    }
}

impl Component for Light {
    const ID: ComponentId = ids::LIGHT;
    const NAME: &'static str = "Light";
    fn fields() -> Vec<FieldDesc> {
        let f = fields![Light;
            "kind": kind, U32, 1;
            "intensity": intensity, F32, 1;
            "range": range, F32, 1, "m";
            "color": color, F32, 3;
            "inner_angle": inner_angle, F32, 1, "rad";
            "outer_angle": outer_angle, F32, 1, "rad";
        ];
        colours(f, &["color"])
    }
}

/// A member's hand (section 8): created by world logic at admission, then moved only by its owner's inputs. The
/// hand's target pose is its object's `Transform`; this row holds the rest of the newest input and the stream state
/// pe-replica's rule needs (replica.md 3.3), and its player's strength.
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
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
    /// The most force, in newtons, the hand's hold applies to what it holds (8.1): its player's strength. Set by world
    /// logic at admission ([`DEFAULT_STRENGTH`]); not an input. Its later home is the character controller.
    pub strength: f32,
    pub engaged: u8,
    pub has_prev: u8,
    /// How the hand takes part in physics ([`hand_mode`]), its owner's choice (8.1): set only by the owner's
    /// `HandMode` input.
    pub mode: u8,
    pub _pad: [u8; 1],
}

/// A player's strength until the character controller holds it, N (8.1): provisional, the user's soft value of
/// 2026-11-19 ("~.5-1kg probably": a default player moves about 0.5-1 kg with no lag). 110 N holds 1 kg
/// tight up to a 100 m/s^2 flick against gravity; the user is still choosing between 110 and 60 N.
pub const DEFAULT_STRENGTH: f32 = 110.0;

impl Default for Hand {
    /// A hand of [`DEFAULT_STRENGTH`], every other field zero. `Zeroable::zeroed` gives a strength of 0 instead, a hand
    /// that can hold nothing up: nothing calls it for `Hand`.
    fn default() -> Hand {
        Hand { strength: DEFAULT_STRENGTH, ..Zeroable::zeroed() }
    }
}

/// A hand's mode (8.1, D18b; @coordinator-opus): a personal comfort choice, per member, on the hand.
pub mod hand_mode {
    /// "Closed hand": physical only while the hand is closed (its input's `engaged`, the grasp). The default.
    pub const CLOSED_HAND: u8 = 0;
    /// "Always within reach": always physical within reach.
    pub const WITHIN_REACH: u8 = 1;
    /// The largest valid mode.
    pub const MAX: u8 = WITHIN_REACH;
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
            "strength": strength, F32, 1, "N";
            "engaged": engaged, Bool, 1;
            "has_prev": has_prev, Bool, 1;
            "mode": mode, U8, 1;
            "_pad": _pad, U8, 1;
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
        fields![SimSize; "metres": metres, F64, 1, "m";]
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

/// The world's own limits (7.1): world state, so an ordinary edit changes them and every member switches at that
/// edit's frame (the friend test 11-27, @coordinator-opus). The plane carries one on its `World
/// settings` root. Only an admin member changes it, and never to 0 or past [`SHAPES_CEILING`] (7.1).
/// The first row in id order counts ([`World::shape_limit`](crate::World::shape_limit)); without one, the build's
/// [`MAX_SHAPES`](crate::edit::MAX_SHAPES).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct Limits {
    /// The most collision shapes an edit may leave in the world, at most [`SHAPES_CEILING`].
    pub max_shapes: u32,
}

/// The highest `Limits.max_shapes` counts as: Box3D's own shape index has 22 bits (box3d-fork `MAX_SHAPES`), and a
/// world takes a few shapes outside the rows (the player bodies). A larger value is read as this.
pub const SHAPES_CEILING: u32 = (1 << 22) - 4096;

impl Component for Limits {
    const ID: ComponentId = ids::LIMITS;
    const NAME: &'static str = "Limits";
    fn fields() -> Vec<FieldDesc> {
        fields![Limits; "max_shapes": max_shapes, U32, 1;]
    }
}

/// A folder (design domain-eng d1, C5; @coordinator-opus): everything under an object with this row is
/// stored, not played. Its descendants are inert as a template folder's are ([`crate::templates`]): no body, no
/// collisions, no grab. The folder itself stays a live object (a crate you can pick up and carry its contents in).
/// My Domain renders the top level and keeps the rest in folders (the user's ruling, 11-27).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct Folder {
    /// Reserved: always 0, so a flag given a meaning later finds no old world holding junk in it. An edit writing
    /// anything else is refused.
    pub flags: u32,
}

impl Component for Folder {
    const ID: ComponentId = ids::FOLDER;
    const NAME: &'static str = "Folder";
    fn fields() -> Vec<FieldDesc> {
        fields![Folder; "flags": flags, U32, 1;]
    }
}

/// Which starter object an object came from (design domain-eng d1, C7: "Restore starter set" adds back the starter
/// objects a home no longer has). Provenance only: nothing in play reads it, and a copy keeps it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct Starter {
    /// [`starter_key`] of the starter object's name.
    pub key: u64,
}

impl Component for Starter {
    const ID: ComponentId = ids::STARTER;
    const NAME: &'static str = "Starter";
    fn fields() -> Vec<FieldDesc> {
        fields![Starter; "key": key, U64, 1;]
    }
}

/// The key of the starter object named `name`: the first 8 bytes of `BLAKE3("pe.starter" ‖ name)`, little-endian.
pub fn starter_key(name: &str) -> u64 {
    let mut h = pe_core::commit::Hasher::new("pe.starter");
    h.write(name.as_bytes());
    u64::from_le_bytes(h.finish().0[..8].try_into().expect("8 bytes"))
}

/// Registers [`Folder`] and [`Starter`] in `store`'s registry: a preset whose world holds folders and the starter set
/// (the home) calls it at genesis, so every other world keeps the registry, and the root, it always had.
pub fn register_folder(store: &mut pe_sim::SimStore) {
    store.register_engine(Folder::desc()).expect("the engine's Folder description");
    store.register_engine(Starter::desc()).expect("the engine's Starter description");
}

impl crate::World {
    /// Registers [`Folder`] in this world, as a preset with folders does at genesis. Not an edit: for a preset, or a
    /// test.
    pub fn register_folders(&mut self) {
        register_folder(&mut self.store);
    }
}

/// Registers [`Limits`] in `store`'s registry: a preset that sets the world's limits as a row calls it at genesis.
pub fn register_limits(store: &mut pe_sim::SimStore) {
    store.register_engine(Limits::desc()).expect("the engine's Limits description");
}

impl crate::World {
    /// Gives this world its limits as a row: [`Limits`] registered, on `id` (an existing object), as world logic does
    /// at genesis. Not an edit: for a preset, or a test.
    pub fn set_limits(&mut self, id: pe_core::ObjectId, limits: Limits) -> Result<(), crate::WorldError> {
        register_limits(&mut self.store);
        self.insert(id, limits)
    }
}

/// A member's player body (world-model.md 8.2): world logic makes it at admission in a preset with bodies, then
/// only its owner's `Move` inputs change this row, and physics moves the object. The object's `Transform` is at the
/// feet; the hands are its children. The row holds the newest Move sample the step reads, and what the step used
/// last frame, so the mover can take the room-scale step as a difference. pe-physics builds the player's shapes
/// from its own tuning, not from a `Shape` row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct Player {
    pub owner: MemberKey,
    /// The frame the newest Move is stamped at, and how many frames its pack covers (samples plus `hold`); 0 before
    /// any Move.
    pub stamp: u64,
    pub span: u32,
    /// The cumulative heading the player asked for, a wrapping angle (2^32 is one turn, positive turning left, 0
    /// facing -Z): the local side integrates it, and the step only reads it.
    pub heading: u32,
    /// The yaw the stick is relative to, in the rig frame, in the same units (the head's, for head-relative
    /// movement).
    pub stick_yaw: u32,
    /// Jump presses so far, wrapping.
    pub jumps: u32,
    /// The heading the step used last frame, for the room-scale step.
    pub prev_heading: u32,
    /// The movement stick after its dead zone, x right and y forward, in steps of 1/127; zero at rest and once the
    /// newest pack has run out.
    pub stick: [i8; 2],
    pub tracked: u8,
    /// 1 on the frame `jumps` rose (a press this frame), else 0: the mover's one-shot jump.
    pub jump: u8,
    /// The head's pose in the rig frame (metres; a unit quaternion `[x, y, z, w]`).
    pub head_pos: [f32; 3],
    pub head_rot: [f32; 4],
    /// The head position the step used last frame, in the rig frame.
    pub prev_head: [f32; 3],
    /// How far the feet moved over the last frame's step, in metres: the extra reach slack a grab by this body's
    /// hands gets, since it is judged before the step moves the body (8.2's known looseness, |v|·dt).
    pub moved: f32,
    pub _pad: u32,
    /// Where the feet were at the end of the last frame, as an offset from the world origin (`pos - ORIGIN`, 4.2).
    pub prev_feet: [f64; 3],
}

impl Component for Player {
    const ID: ComponentId = ids::PLAYER;
    const NAME: &'static str = "Player";
    fn fields() -> Vec<FieldDesc> {
        fields![Player;
            "owner": owner, MemberKey, 1;
            "stamp": stamp, U64, 1;
            "span": span, U32, 1;
            "heading": heading, U32, 1;
            "stick_yaw": stick_yaw, U32, 1;
            "jumps": jumps, U32, 1;
            "prev_heading": prev_heading, U32, 1;
            "stick": stick, I8, 2;
            "tracked": tracked, Bool, 1;
            "jump": jump, U8, 1;
            "head_pos": head_pos, F32, 3, "m";
            "head_rot": head_rot, F32, 4;
            "prev_head": prev_head, F32, 3, "m";
            "moved": moved, F32, 1, "m";
            "_pad": _pad, U32, 1;
            "prev_feet": prev_feet, F64, 3, "m";
        ]
    }
}

/// Registers [`Player`] in `store`'s registry: a preset with player bodies calls it at genesis. A world that never
/// does keeps the engine registry, and the root, it always had.
pub fn register_player(store: &mut pe_sim::SimStore) {
    store.register_engine(Player::desc()).expect("the engine's Player description");
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
                ("Light", 32),
                ("Hand", 72),
                ("SimSize", 8),
                ("Protection", 32),
            ]
        );
    }

    /// The engine's colour fields (3.6; @coordinator-opus): `Appearance.base` and `emissive` and
    /// `Light.color` are linear sRGB, and no other engine field is a colour.
    #[test]
    fn the_engine_colours() {
        let r = engine_registry();
        let colours: Vec<(&str, &str)> = r
            .iter()
            .flat_map(|d| d.fields().iter().filter(|f| f.color.is_some()).map(move |f| (d.name(), f.name.as_str())))
            .collect();
        assert_eq!(colours, [("Appearance", "base"), ("Appearance", "emissive"), ("Light", "color")]);
        assert!(r.iter().flat_map(|d| d.fields()).all(|f| f.color.is_none_or(|c| c == pe_sim::ColorSpace::LinearSrgb)));
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

//! Components a script declares (docs/spec/logic.md 3.2). [`component!`](crate::component!) makes a `#[repr(C)]`
//! struct whose layout the store checks against its own rules, and its description for the manifest.

use crate::layout::{ColorSpace, FieldKind};
use crate::space::{OffsetFrom, Pos, Saved};

/// A type a component field may have. Arrays of a scalar are one field with a count. `Offset` is deliberately not a
/// field type: a saved offset must name its reference, so it is an [`OffsetFrom<Own>`](crate::space::OffsetFrom) or
/// an `OffsetFrom<Parent>` (logic.md 8).
pub trait Field: Copy {
    const KIND: FieldKind;
    const COUNT: u32;
    /// The colour space, for a type that holds a colour ([`LinearRgb`], [`LinearRgba`]).
    const COLOR: Option<ColorSpace> = None;
}

pub use crate::color::{LinearRgb, LinearRgba};

impl Field for LinearRgb {
    const KIND: FieldKind = FieldKind::F32;
    const COUNT: u32 = 3;
    const COLOR: Option<ColorSpace> = Some(ColorSpace::LinearSrgb);
}

impl Field for LinearRgba {
    const KIND: FieldKind = FieldKind::F32;
    const COUNT: u32 = 4;
    const COLOR: Option<ColorSpace> = Some(ColorSpace::LinearSrgb);
}

/// Three `f32` in the store. The space it is relative to is in the type, so a script reads it back as the
/// `Offset<Own>` or `Offset<Parent>` it saved.
impl<S: Saved> Field for OffsetFrom<S> {
    const KIND: FieldKind = FieldKind::F32;
    const COUNT: u32 = 3;
}

/// An object id, as the store keeps it (16 bytes, 8-aligned like the store's 8-byte kinds).
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ObjectId(pub [u8; 16]);

macro_rules! scalar {
    ($($t:ty => $k:ident),* $(,)?) => {
        $(
            impl Field for $t {
                const KIND: FieldKind = FieldKind::$k;
                const COUNT: u32 = 1;
            }
            impl<const N: usize> Field for [$t; N] {
                const KIND: FieldKind = FieldKind::$k;
                const COUNT: u32 = N as u32;
            }
        )*
    };
}

scalar!(
    u8 => U8, u16 => U16, u32 => U32, u64 => U64,
    i8 => I8, i16 => I16, i32 => I32, i64 => I64,
    f32 => F32, f64 => F64, bool => Bool, ObjectId => ObjectId, Pos => Pos,
);

/// One field of a component, for the manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldDesc {
    pub name: &'static str,
    pub kind: FieldKind,
    pub count: u32,
    pub offset: u32,
    /// Set by the field's type: [`LinearRgb`] and [`LinearRgba`] are colours.
    pub color: Option<ColorSpace>,
}

/// A component type.
pub trait Component: Copy + 'static {
    const NAME: &'static str;
    const FIELDS: &'static [FieldDesc];
    const SIZE: u32 = core::mem::size_of::<Self>() as u32;
    const ALIGN: u32 = core::mem::align_of::<Self>() as u32;
}

/// A component's description, as a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComponentInfo {
    pub name: &'static str,
    pub size: u32,
    pub align: u32,
    pub fields: &'static [FieldDesc],
}

pub const fn info<C: Component>() -> ComponentInfo {
    ComponentInfo { name: C::NAME, size: C::SIZE, align: C::ALIGN, fields: C::FIELDS }
}

/// Declares a component: a `#[repr(C)]` struct of [`Field`] types. A field of type [`LinearRgb`] or [`LinearRgba`] is
/// marked a colour.
///
/// ```
/// pe_sdk::component! {
///     /// How a lamp blinks.
///     pub struct Blink { period: f32, phase: f32, near: f32 }
/// }
/// use pe_sdk::component::Component;
/// assert_eq!(Blink::NAME, "Blink");
/// assert_eq!(Blink::FIELDS[1].offset, 4);
///
/// pe_sdk::component! {
///     /// A glow, in linear light.
///     pub struct Glow { color: pe_sdk::component::LinearRgb, intensity: f32 }
/// }
/// assert_eq!(Glow::FIELDS[0].color, Some(pe_sdk::layout::ColorSpace::LinearSrgb));
/// assert_eq!(Glow::FIELDS[1].color, None);
/// ```
#[macro_export]
macro_rules! component {
    ($(#[$m:meta])* $vis:vis struct $name:ident { $($(#[$fm:meta])* $f:ident : $t:ty),* $(,)? }) => {
        $(#[$m])*
        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq)]
        $vis struct $name { $($(#[$fm])* pub $f: $t),* }

        impl $crate::component::Component for $name {
            const NAME: &'static str = stringify!($name);
            const FIELDS: &'static [$crate::component::FieldDesc] = &[
                $($crate::component::FieldDesc {
                    name: stringify!($f),
                    kind: <$t as $crate::component::Field>::KIND,
                    count: <$t as $crate::component::Field>::COUNT,
                    offset: ::core::mem::offset_of!($name, $f) as u32,
                    color: <$t as $crate::component::Field>::COLOR,
                }),*
            ];
        }
    };
}

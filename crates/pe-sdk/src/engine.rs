//! Engine components a script may bind (docs/spec/logic.md 3.2): byte-for-byte copies of the engine's own, named
//! `pe.<Name>` in the manifest so they can never collide with a script's own components (whose names are
//! identifiers). The host binds each to the engine's id only if the world lists it for scripts, refuses a `write` the
//! world does not allow, and runs the world's edit checks on every row a script writes (world-model.md 7.9).
//! pe-logic's tests check these layouts against pe-world's descriptions.

use crate::component::{Component, FieldDesc};
use crate::layout::FieldKind;

macro_rules! engine_component {
    ($(#[$m:meta])* $name:ident = $manifest:literal { $($(#[$fm:meta])* $f:ident : $t:ty),* $(,)? }) => {
        $(#[$m])*
        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct $name { $($(#[$fm])* pub $f: $t),* }

        impl Component for $name {
            const NAME: &'static str = $manifest;
            const FIELDS: &'static [FieldDesc] = &[
                $(FieldDesc {
                    name: stringify!($f),
                    kind: <$t as crate::component::Field>::KIND,
                    count: <$t as crate::component::Field>::COUNT,
                    offset: ::core::mem::offset_of!($name, $f) as u32,
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
        kind: u32,
        intensity: f32,
        /// Metres.
        range: f32,
        color: [f32; 3],
        inner_angle: f32,
        outer_angle: f32,
    }
}

engine_component! {
    /// A local transform (pe-sim `Transform`), 80 bytes: the level-0 chunk (objects without a frame only), the
    /// position within it, a unit quaternion `[x, y, z, w]`, the scale, and a pad that must be zero.
    Transform = "pe.Transform" {
        chunk: [i64; 3],
        pos: [f64; 3],
        rot: [f32; 4],
        scale: [f32; 3],
        _pad: u32,
    }
}

// The field kinds the two use, so a change to either is a compile error here as well as a test failure in pe-logic.
const _: () = {
    assert!(matches!(<f32 as crate::component::Field>::KIND, FieldKind::F32));
    assert!(core::mem::size_of::<Light>() == 32);
    assert!(core::mem::size_of::<Transform>() == 80);
};

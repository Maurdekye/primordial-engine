//! Scale-dependent contact-recycling radius ("van der Waals simulation").
//!
//! Primordial Engine fork addition; not part of upstream Box3D or box3d-rust.
//!
//! # Background
//!
//! Box3D's contact recycling (`collide_task`, C `b3CollideTask`) skips the narrow phase for a pair
//! and re-uses last frame's manifold while the pair's relative pose has drifted less than a
//! tolerance since the manifold was last built. Upstream that tolerance is one world-wide length,
//! `World::contact_recycle_distance` (default `10 * linear_slop` = 0.05 m), whatever the size of
//! the two bodies. A 1 cm pebble may slide five times its own size on stale contact points, while
//! a 20 m slab re-runs the narrow phase every time it moves 5 cm.
//!
//! # Formula
//!
//! With scaling enabled, each pair gets its own translational tolerance:
//!
//! ```text
//! L_i   = BodySim::min_extent of body i     (inner radius: smallest shape extent)
//! L     = combine(L_a, L_b)                 (Min by default; a static side takes the other's L)
//! r     = clamp(length_fraction * L, min_distance, max_distance)
//! touching pair:      tolerance = r
//! non-touching pair:  tolerance = min(r, speculative_distance)   (as upstream)
//! ```
//!
//! The rotational gates are unchanged: the fixed angular gate and the arc test, which already
//! scales with `max_extent`, now spend their slack out of `r`.
//!
//! The defaults make a 1 m cube (inner radius 0.5 m) get exactly the upstream 0.05 m.
//!
//! # Determinism
//!
//! Only `*`, `+`, `/`, `min`, `max` and compares on `f32` are used, all IEEE-754 exact-rounded
//! and identical in debug, release and on every x86-64 / wasm32 target. With
//! `enabled == false` the collide loop takes the exact upstream code path.
//!
//! # Override point (R15)
//!
//! [`ContactRecycleScaling`] is the parameterized override; [`ContactRecycleRadiusFn`] replaces
//! the formula entirely. The function must be a pure, deterministic function of its inputs
//! (no globals, no clocks, no transcendental `libm` calls whose results differ by platform).

use crate::core::get_length_units_per_meter;
use crate::math_functions::{max_float, min_float};

/// How the two bodies' characteristic lengths combine into one per-pair length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecycleLengthCombine {
    /// `min(L_a, L_b)`. The smaller body bounds how far stale contact points may slide. Default.
    Min = 0,
    /// `2 L_a L_b / (L_a + L_b)`, between `min` and `2 * min`. Slightly looser for unequal pairs.
    HarmonicMean = 1,
}

impl RecycleLengthCombine {
    /// Decode the snapshot byte. Unknown values fall back to [`RecycleLengthCombine::Min`].
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => RecycleLengthCombine::HarmonicMean,
            _ => RecycleLengthCombine::Min,
        }
    }
}

/// Parameters of the scale-dependent recycling radius. Part of [`crate::types::WorldDef`] and
/// of the world snapshot (only when `enabled`, so disabled worlds keep upstream image bytes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContactRecycleScaling {
    /// Master toggle. `false` = upstream fixed `contact_recycle_distance`.
    pub enabled: bool,
    /// Tolerance as a fraction of the pair's characteristic length. Dimensionless.
    pub length_fraction: f32,
    /// Lower clamp, length units.
    pub min_distance: f32,
    /// Upper clamp, length units.
    pub max_distance: f32,
    /// How the two characteristic lengths combine.
    pub combine: RecycleLengthCombine,
}

/// Default fraction: 0.1 * 0.5 m (a 1 m cube's inner radius) = 0.05 m = the upstream distance.
pub const DEFAULT_RECYCLE_LENGTH_FRACTION: f32 = 0.1;

/// Default lower clamp: one linear slop (0.005 m). Below the slop Box3D cannot resolve contact
/// geometry anyway, so a smaller tolerance only costs narrow-phase time.
pub fn default_recycle_min_distance() -> f32 {
    0.005 * get_length_units_per_meter()
}

/// Default upper clamp: 0.5 m, ten times the upstream distance. Bounds anchor drift on huge
/// bodies where `max_extent`-scaled arc slack stops being the binding test.
pub fn default_recycle_max_distance() -> f32 {
    0.5 * get_length_units_per_meter()
}

impl ContactRecycleScaling {
    /// Defaults with the toggle off (upstream behavior).
    pub fn disabled() -> Self {
        ContactRecycleScaling {
            enabled: false,
            length_fraction: DEFAULT_RECYCLE_LENGTH_FRACTION,
            min_distance: default_recycle_min_distance(),
            max_distance: default_recycle_max_distance(),
            combine: RecycleLengthCombine::Min,
        }
    }

    /// Defaults with the toggle on.
    pub fn enabled() -> Self {
        ContactRecycleScaling {
            enabled: true,
            ..Self::disabled()
        }
    }
}

impl Default for ContactRecycleScaling {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Per-pair input handed to the radius function.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecyclePairInput {
    /// `BodySim::min_extent` of body A (meaningless when `static_a`).
    pub length_a: f32,
    /// `BodySim::min_extent` of body B (meaningless when `static_b`).
    pub length_b: f32,
    /// Body A is static (infinite size for this purpose).
    pub static_a: bool,
    /// Body B is static.
    pub static_b: bool,
    /// The world's upstream `contact_recycle_distance` (already known to be > 0).
    pub base_distance: f32,
}

/// Replaceable radius function. Returns the touching-pair tolerance in length units; values
/// `<= 0` disable recycling for the pair. Must be pure and deterministic.
pub type ContactRecycleRadiusFn = fn(&RecyclePairInput, &ContactRecycleScaling) -> f32;

/// Per-pair characteristic length from the two body lengths.
#[inline]
pub fn pair_characteristic_length(input: &RecyclePairInput, combine: RecycleLengthCombine) -> f32 {
    if input.static_a {
        return input.length_b;
    }
    if input.static_b {
        return input.length_a;
    }
    let a = input.length_a;
    let b = input.length_b;
    match combine {
        RecycleLengthCombine::Min => min_float(a, b),
        RecycleLengthCombine::HarmonicMean => {
            let s = a + b;
            if s > 0.0 {
                2.0 * a * b / s
            } else {
                0.0
            }
        }
    }
}

/// The built-in formula: `clamp(length_fraction * L, min_distance, max_distance)`.
#[inline]
pub fn default_recycle_radius(input: &RecyclePairInput, s: &ContactRecycleScaling) -> f32 {
    let l = pair_characteristic_length(input, s.combine);
    let r = s.length_fraction * l;
    // Clamp written as max-then-min so a mis-ordered (min > max) pair resolves to max_distance.
    min_float(max_float(r, s.min_distance), s.max_distance)
}

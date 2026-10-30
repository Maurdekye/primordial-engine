//! Canonical world coordinates (design 25.2, R16): a level-0 chunk index (`i64` per axis, 1,024 m
//! chunks) plus an `f64` offset inside the chunk, and the hierarchical cell keys derived from them.
//! Ported from the R16 spike (`spikes/chunks/src/coords.rs`).
//!
//! * Level `L` cells are `CHUNK * 2^L` metres wide and corner-aligned, so they nest exactly:
//!   `cell(L + 1) == cell(L) >> 1` on every axis, for negative coordinates too.
//! * Every conversion uses only `+ - * /` by powers of two, `floor` and integer shifts, so it is
//!   exact or IEEE-rounded identically on every machine (design 4.3 rule 3). No `log2`.
//! * Range: +-2^63 chunks * 1024 m = +-2^73 m (about 1 million light-years). Resolution inside a
//!   chunk: one ulp of `f64` just below 1024, 2^-43 m (about 1.1e-13 m), anywhere in that range.

#![allow(clippy::needless_range_loop)] // per-axis loops read clearest with an index

/// log2 of the level-0 chunk width in metres.
pub const CHUNK_EXP: i32 = 10;
/// Level-0 chunk width in metres (exactly representable).
pub const CHUNK: f64 = 1024.0;
/// Chunk indices saturate at +-2^62 (review P21-2): positions never overflow, and the sim crates refuse
/// transforms that come near this limit.
pub const CHUNK_LIMIT: i64 = 1 << 62;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WorldPos {
    /// Level-0 chunk index.
    pub chunk: [i64; 3],
    /// Offset from the chunk's minimum corner, in `[0, CHUNK)` on every axis.
    pub local: [f64; 3],
}

/// A cell of the hierarchy. Coordinates are `i128` so sub-chunk levels down to -60 keep the full
/// +-2^63-chunk range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellKey {
    pub level: i32,
    pub c: [i128; 3],
}

/// Exact `2^e` for |e| < 1023.
pub fn pow2(e: i32) -> f64 {
    assert!(e > -1022 && e < 1023);
    f64::from_bits(((e + 1023) as u64) << 52)
}

/// Width of a level-`level` cell in metres.
pub fn cell_size(level: i32) -> f64 {
    pow2(CHUNK_EXP + level)
}

/// `ceil(log2(x))` for a positive normal `x`, read from the bit pattern (no transcendental).
pub fn ceil_log2(x: f64) -> i32 {
    assert!(x.is_normal() && x > 0.0, "ceil_log2 of {x}");
    let bits = x.to_bits();
    let e = ((bits >> 52) & 0x7ff) as i32 - 1023;
    if bits & ((1u64 << 52) - 1) == 0 { e } else { e + 1 }
}

/// Smallest level whose cell is at least `extent` metres wide.
pub fn level_for_extent(extent: f64) -> i32 {
    ceil_log2(extent) - CHUNK_EXP
}

impl WorldPos {
    pub fn new(chunk: [i64; 3], local: [f64; 3]) -> Self {
        let mut p = WorldPos { chunk, local };
        p.normalize();
        p
    }

    /// Build from absolute metres (for tests and scene setup only; loses precision past 2^53 m).
    pub fn from_abs(x: [f64; 3]) -> Self {
        WorldPos::new([0; 3], x)
    }

    /// Bring `local` back into `[0, CHUNK)`, carrying whole chunks into `chunk`. Exact for every finite
    /// offset: `local - 1024 * floor(local / 1024)` is `local mod 1024`, which is representable. The one
    /// inexact case is a negative offset smaller than half an ulp of 1024, which becomes 0 of the next
    /// chunk.
    ///
    /// Never panics (review P21-2): a chunk index beyond [`CHUNK_LIMIT`] saturates there, with the offset
    /// clamped to that chunk's edge. A non-finite offset is treated as 0. Both are deterministic, and the
    /// sim crates' input validation keeps real positions far from either case.
    pub fn normalize(&mut self) {
        for a in 0..3 {
            if !self.local[a].is_finite() {
                self.local[a] = 0.0;
            }
            let k = (self.local[a] / CHUNK).floor();
            let mut c = self.chunk[a] as i128;
            if k != 0.0 {
                self.local[a] -= k * CHUNK;
                // |k| < 2^1014; clamping it to +-2^63 first keeps the sum exact in i128.
                c += k.clamp(-9.223_372_036_854_775_808e18, 9.223_372_036_854_775_808e18) as i128;
            }
            if self.local[a] >= CHUNK {
                self.local[a] -= CHUNK;
                c += 1;
            }
            let lim = CHUNK_LIMIT as i128;
            if c > lim {
                c = lim;
                self.local[a] = f64::from_bits(CHUNK.to_bits() - 1); // the largest offset below 1024
            } else if c < -lim {
                c = -lim;
                self.local[a] = 0.0;
            }
            self.chunk[a] = c as i64;
        }
    }

    pub fn offset(mut self, d: [f64; 3]) -> Self {
        for a in 0..3 {
            self.local[a] += d[a];
        }
        self.normalize();
        self
    }

    /// `self - other` in metres. Exact in the integer part; the result is rounded once to `f64`.
    pub fn sub(&self, other: &WorldPos) -> [f64; 3] {
        let mut d = [0.0; 3];
        for a in 0..3 {
            let dc = self.chunk[a] as i128 - other.chunk[a] as i128;
            d[a] = (dc as f64) * CHUNK + (self.local[a] - other.local[a]);
        }
        d
    }

    pub fn dist2(&self, other: &WorldPos) -> f64 {
        let d = self.sub(other);
        d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
    }

    /// The cell containing this position at `level`.
    pub fn cell(&self, level: i32) -> CellKey {
        let mut c = [0i128; 3];
        for a in 0..3 {
            c[a] = if level >= 0 {
                (self.chunk[a] as i128) >> level
            } else {
                let k = -level;
                assert!(k <= 60, "level {level} below the supported range");
                let sub = (self.local[a] / cell_size(level)).floor() as i128; // exact: pow2 divide
                ((self.chunk[a] as i128) << k) + sub
            };
        }
        CellKey { level, c }
    }

    /// Position in a region frame: metres from the centre of chunk `origin`, divided by the
    /// region's length unit `2^unit_exp` m. Rounds at most once, to the ulp of the region-frame
    /// value (2^-43 m within 1 km of the origin at unit 1 m); scaling by the unit is exact.
    pub fn to_region(&self, origin: [i64; 3], unit_exp: i32) -> [f64; 3] {
        let inv = pow2(-unit_exp);
        let mut v = [0.0; 3];
        for a in 0..3 {
            let dc = self.chunk[a] as i128 - origin[a] as i128;
            v[a] = ((dc as f64) * CHUNK + (self.local[a] - CHUNK * 0.5)) * inv;
        }
        v
    }

    pub fn from_region(origin: [i64; 3], unit_exp: i32, v: [f64; 3]) -> Self {
        let u = pow2(unit_exp);
        let mut local = [0.0; 3];
        for a in 0..3 {
            local[a] = v[a] * u + CHUNK * 0.5;
        }
        WorldPos::new(origin, local)
    }
}

impl CellKey {
    pub fn size(&self) -> f64 {
        cell_size(self.level)
    }

    pub fn parent(&self) -> CellKey {
        CellKey { level: self.level + 1, c: [self.c[0] >> 1, self.c[1] >> 1, self.c[2] >> 1] }
    }

    /// Minimum corner as a `WorldPos` (exact for levels >= -42).
    pub fn min_corner(&self) -> WorldPos {
        let mut chunk = [0i64; 3];
        let mut local = [0.0; 3];
        for a in 0..3 {
            if self.level >= 0 {
                chunk[a] = (self.c[a] << self.level) as i64;
            } else {
                let k = -self.level;
                chunk[a] = (self.c[a] >> k) as i64;
                let rem = self.c[a] - ((chunk[a] as i128) << k);
                local[a] = rem as f64 * cell_size(self.level);
            }
        }
        WorldPos { chunk, local }
    }

    /// Squared distance from `p` to this cell's box expanded by `loose` cell widths on every side.
    pub fn dist2_to(&self, p: &WorldPos, loose: f64) -> f64 {
        let s = self.size();
        let d = p.sub(&self.min_corner());
        let mut acc = 0.0;
        for a in 0..3 {
            let lo = -loose * s;
            let hi = s + loose * s;
            let e = if d[a] < lo { lo - d[a] } else if d[a] > hi { d[a] - hi } else { 0.0 };
            acc += e * e;
        }
        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_sub() {
        let p = WorldPos::new([0, 0, 0], [-0.25, 1024.0, 2048.5]);
        assert_eq!(p.chunk, [-1, 1, 2]);
        assert_eq!(p.local, [1023.75, 0.0, 0.5]);
        let q = WorldPos::new([5_000_000_000, 0, 0], [3.0, 0.0, 0.0]);
        let r = q.offset([-1024.0 * 3.0, 0.0, 0.0]);
        assert_eq!(r.chunk[0], 4_999_999_997);
        assert_eq!(r.sub(&q), [-3072.0, 0.0, 0.0]);
    }

    /// The OWO chunk-assignment battery (OWO doc, Appendix A): probes at +0.3, -0.7 and +0.9 of a
    /// layer-cell width land in cells 0, -1 and 0 under corner alignment (OWO is centre-aligned,
    /// where +0.9 rounds to +1; with corner-aligned cells floor is the whole rule).
    #[test]
    fn owo_battery_corner_aligned() {
        for level in [-5, 0, 5, 10, 20] {
            let s = cell_size(level);
            for (f, want) in [(0.3, 0i128), (-0.7, -1), (0.9, 0), (-0.0000001, -1), (1.0, 1)] {
                let p = WorldPos::from_abs([f * s, 0.0, 0.0]);
                assert_eq!(p.cell(level).c[0], want, "level {level} f {f}");
            }
        }
    }

    #[test]
    fn cells_nest() {
        let ps = [
            WorldPos::new([-3, 7, i64::MAX / 2], [1.5, 1023.999, 0.0]),
            WorldPos::new([i64::MIN / 2, -1, 0], [0.000001, 512.0, 999.0]),
        ];
        for p in ps {
            for level in -40..62 {
                assert_eq!(p.cell(level).parent(), p.cell(level + 1), "level {level}");
            }
        }
    }

    #[test]
    fn region_round_trip_exact() {
        // Offsets on the region frame's ulp grid survive the round trip bit for bit.
        let p = WorldPos::new([123_456_789, -5, 3], [17.125, 1000.0, 0.5]);
        for unit in [-20, -10, 0, 10] {
            let v = p.to_region([123_456_788, -5, 3], unit);
            let q = WorldPos::from_region([123_456_788, -5, 3], unit, v);
            assert_eq!(p, q, "unit {unit}");
        }
        // Finer offsets round once, by less than one ulp of the region-frame value.
        let p = WorldPos::new([7, 0, 0], [0.001, 0.0, 0.0]);
        let q = WorldPos::from_region([7, 0, 0], 0, p.to_region([7, 0, 0], 0));
        assert!((q.local[0] - p.local[0]).abs() <= 512f64 * f64::EPSILON);
    }

    #[test]
    fn normalize_never_panics() {
        // Review P21-2's repro: a finite but absurd offset saturates instead of panicking.
        let p = WorldPos::new([1, 0, 0], [1e300, -1e300, f64::NAN]);
        assert_eq!(p.chunk, [CHUNK_LIMIT, -CHUNK_LIMIT, 0]);
        assert!(p.local.iter().all(|v| (0.0..CHUNK).contains(v)));
        let q = WorldPos::new([i64::MAX, i64::MIN, 0], [5.0, 5.0, 2048.0]);
        assert_eq!(q.chunk, [CHUNK_LIMIT, -CHUNK_LIMIT, 2]);
    }

    #[test]
    fn ceil_log2_matches() {
        for (x, e) in [(1.0, 0), (1.5, 1), (2.0, 1), (0.75, 0), (0.5, -1), (1024.0, 10), (1025.0, 11)] {
            assert_eq!(ceil_log2(x), e, "{x}");
        }
    }
}

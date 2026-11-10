//! World coordinates behind a type wall (world-model.md section 4; the user's ruling m06d2643578d7).
//!
//! * [`Pos`] is a place in the world: a level-0 chunk index (1,024 m chunks) per axis plus an offset inside the
//!   chunk. Its fields are private. There is no constructor from numbers and no way to read numbers out of it: the
//!   only way to plain numbers is `pos - Pos::ORIGIN`, an [`Offset`], and the only way back is
//!   `Pos::ORIGIN.checked_add(offset)`. The engine obeys the same wall: physics, rendering and regions get numbers by
//!   subtracting a `Pos` they chose as their frame's origin.
//! * **No operator can panic** (4.2). `Pos - Pos` is total; moving a `Pos` is [`Pos::checked_add`] /
//!   [`Pos::checked_sub`], which return [`OutOfRange`]. There is no `Pos + Offset` operator.
//! * **No saturation** (4.3, the user's m7e85960c864a): a result past the `i64` chunk range, or onto the reserved
//!   `i64::MIN`, is an error, never a clamped value. From M1 to M3 that range is a staged limitation; the canonical
//!   encoding ([`Pos::encode`]) is already a varint, so M4's promotion changes no stored bytes.
//! * Exact integer cells ([`Pos::cell`]) stay, because integers cannot lose precision (coordinator m707c519c0bcf).
//! * Every conversion uses only `+ - * /` by powers of two, `floor` and integer arithmetic in a fixed order, so it is
//!   exact or IEEE-rounded identically on every machine (design 4.3 rule 3).

#![allow(clippy::needless_range_loop)] // per-axis loops read clearest with an index

use bytemuck::{Pod, Zeroable};

/// log2 of the level-0 chunk width in metres.
pub const CHUNK_EXP: i32 = 10;
/// Level-0 chunk width in metres (exactly representable).
pub const CHUNK: f64 = 1024.0;
/// The reserved chunk index (4.3): never valid, kept as M4's overflow marker.
pub const RESERVED_CHUNK: i64 = i64::MIN;
/// The finest supported cell level: cells of 2^-50 m. Sizes below [`MIN_EXTENT`] have no level.
pub const MIN_LEVEL: i32 = -60;
/// The coarsest supported cell level: one level-64 cell covers every valid chunk index.
pub const MAX_LEVEL: i32 = 64;

/// A position left the supported range, or was not finite (4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfRange;

impl core::fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "position out of range")
    }
}

/// A displacement in metres, along the world axes (4.2). Ordinary floating point.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Offset(pub [f64; 3]);

impl Offset {
    pub const ZERO: Offset = Offset([0.0; 3]);

    pub fn length2(&self) -> f64 {
        let d = self.0;
        d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
    }

    pub fn length(&self) -> f64 {
        self.length2().sqrt()
    }
}

impl core::ops::Add for Offset {
    type Output = Offset;
    fn add(self, o: Offset) -> Offset {
        Offset([self.0[0] + o.0[0], self.0[1] + o.0[1], self.0[2] + o.0[2]])
    }
}

impl core::ops::Sub for Offset {
    type Output = Offset;
    fn sub(self, o: Offset) -> Offset {
        Offset([self.0[0] - o.0[0], self.0[1] - o.0[1], self.0[2] - o.0[2]])
    }
}

impl core::ops::Neg for Offset {
    type Output = Offset;
    fn neg(self) -> Offset {
        Offset([-self.0[0], -self.0[1], -self.0[2]])
    }
}

impl core::ops::Mul<f64> for Offset {
    type Output = Offset;
    fn mul(self, k: f64) -> Offset {
        Offset([self.0[0] * k, self.0[1] * k, self.0[2] * k])
    }
}

/// A place in the world (4.2). Exact everywhere; fields private.
///
/// Invariants: every chunk index is in `(i64::MIN, i64::MAX]`; every offset is finite, in `[0, 1024)`, and never
/// `-0.0`. So equal positions have equal bits and one canonical encoding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pos {
    chunk: [i64; 3],
    local: [f64; 3],
}

/// A cell of the hierarchy. Coordinates are `i128` so sub-chunk levels down to -60 keep the full chunk range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellKey {
    pub level: i32,
    pub c: [i128; 3],
}

/// The smallest extent that has a level (2^-50 m, about 1e-15 m): `level_for_extent` is at least [`MIN_LEVEL`].
/// world-model.md 3.5 bounds `SimSize` and extents by it, so no data can reach a level outside the range.
pub const MIN_EXTENT: f64 = f64::from_bits((1023 - 50) << 52);

/// Exact `2^e`, for normal results (-1022 < e < 1023); `None` otherwise. Never panics (world-model.md 5.3).
pub fn pow2(e: i32) -> Option<f64> {
    (e > -1022 && e < 1023).then(|| f64::from_bits(((e + 1023) as u64) << 52))
}

/// `2^e` for an exponent the code fixes (a constant in range).
fn p2(e: i32) -> f64 {
    pow2(e).expect("a constant exponent in range")
}

/// Width of a level-`level` cell in metres; `None` outside [`MIN_LEVEL`]..=[`MAX_LEVEL`].
pub fn cell_size(level: i32) -> Option<f64> {
    if !(MIN_LEVEL..=MAX_LEVEL).contains(&level) {
        return None;
    }
    pow2(CHUNK_EXP + level)
}

/// `ceil(log2(x))` for a positive normal `x`, read from the bit pattern (no transcendental); `None` otherwise.
pub fn ceil_log2(x: f64) -> Option<i32> {
    if !(x.is_normal() && x > 0.0) {
        return None;
    }
    let bits = x.to_bits();
    let e = ((bits >> 52) & 0x7ff) as i32 - 1023;
    Some(if bits & ((1u64 << 52) - 1) == 0 { e } else { e + 1 })
}

/// Smallest level whose cell is at least `extent` metres wide; `None` if `extent` is not a positive normal number
/// or its level is outside [`MIN_LEVEL`]..=[`MAX_LEVEL`] (an extent below [`MIN_EXTENT`]).
pub fn level_for_extent(extent: f64) -> Option<i32> {
    let l = ceil_log2(extent)? - CHUNK_EXP;
    (MIN_LEVEL..=MAX_LEVEL).contains(&l).then_some(l)
}

fn valid_chunk(c: i128) -> Option<i64> {
    let v = i64::try_from(c).ok()?;
    (v != RESERVED_CHUNK).then_some(v)
}

/// `+0.0` for `-0.0`, otherwise `x`.
fn canon(x: f64) -> f64 {
    if x == 0.0 { 0.0 } else { x }
}

impl Pos {
    /// The world origin constant (the user's m06d2643578d7): chunk 0, offset 0.
    pub const ORIGIN: Pos = Pos { chunk: [0; 3], local: [0.0; 3] };

    /// The minimum corner of a level-0 chunk. Exact: an integer index, like [`Pos::cell`] (4.2).
    pub fn at_chunk(chunk: [i64; 3]) -> Result<Pos, OutOfRange> {
        if chunk.contains(&RESERVED_CHUNK) {
            return Err(OutOfRange);
        }
        Ok(Pos { chunk, local: [0.0; 3] })
    }

    /// `self` moved by `d`. Exact in the chunk part. Each axis's offset is rounded in `local + d`, and may round once
    /// more when it is carried into the chunk (`v - 1024 * floor(v / 1024)` is exact except for a small negative
    /// `v`, where adding 1024 rounds); every step is a fixed IEEE operation, so the result is the same bits on every
    /// machine. A non-finite `d` or a result past the range is [`OutOfRange`].
    pub fn checked_add(self, d: Offset) -> Result<Pos, OutOfRange> {
        let mut out = self;
        for a in 0..3 {
            let v = self.local[a] + d.0[a];
            if !v.is_finite() {
                return Err(OutOfRange);
            }
            // local mod 1024, exact: `v - 1024 * floor(v / 1024)` is representable for every finite v.
            let k = (v / CHUNK).floor();
            let mut l = v - k * CHUNK;
            // |k| >= 2^64 chunks is out of range whatever the start; below that the cast is exact.
            let mut kc: i128 = if k.abs() < p2(64) { k as i128 } else { return Err(OutOfRange) };
            if l >= CHUNK {
                // A negative v smaller than half an ulp of 1024 rounds to 1024: that is 0 of the next chunk.
                l -= CHUNK;
                kc += 1;
            }
            out.chunk[a] = valid_chunk(self.chunk[a] as i128 + kc).ok_or(OutOfRange)?;
            out.local[a] = canon(l);
        }
        Ok(out)
    }

    /// `self` moved by `-d`.
    pub fn checked_sub(self, d: Offset) -> Result<Pos, OutOfRange> {
        self.checked_add(-d)
    }

    /// Moved by a whole number of chunks per axis (2.3, merge placement): exact, changes no offset.
    pub fn checked_shift_chunks(self, by: [i64; 3]) -> Result<Pos, OutOfRange> {
        let mut out = self;
        for a in 0..3 {
            out.chunk[a] = valid_chunk(self.chunk[a] as i128 + by[a] as i128).ok_or(OutOfRange)?;
        }
        Ok(out)
    }

    /// Squared distance in metres, from `self - other`.
    pub fn dist2(&self, other: &Pos) -> f64 {
        (*self - *other).length2()
    }

    /// The cell containing this position at `level`; `None` outside [`MIN_LEVEL`]..=[`MAX_LEVEL`].
    pub fn cell(&self, level: i32) -> Option<CellKey> {
        let size = cell_size(level)?;
        let mut c = [0i128; 3];
        for a in 0..3 {
            c[a] = if level >= 0 {
                (self.chunk[a] as i128) >> level
            } else {
                let sub = (self.local[a] / size).floor() as i128; // exact: pow2 divide
                ((self.chunk[a] as i128) << -level) + sub
            };
        }
        Some(CellKey { level, c })
    }

    /// The level-0 chunk index (exact, an integer like every cell).
    pub fn chunk_index(&self) -> [i64; 3] {
        self.chunk
    }

    /// The canonical encoding (4.5): per axis, the chunk index as a zigzag LEB128 varint, then the offset's 8
    /// little-endian bytes. One encoding per position.
    pub fn encode(&self, out: &mut Vec<u8>) {
        for a in 0..3 {
            let c = self.chunk[a] as i128;
            let mut z = ((c << 1) ^ (c >> 127)) as u128; // zigzag
            loop {
                let b = (z & 0x7f) as u8;
                z >>= 7;
                if z == 0 {
                    out.push(b);
                    break;
                }
                out.push(b | 0x80);
            }
            out.extend_from_slice(&self.local[a].to_le_bytes());
        }
    }

    /// Decodes [`Pos::encode`]'s bytes, refusing anything non-canonical: a non-minimal varint, an index outside the
    /// M1 range (a valid encoding of a larger index is reported as [`PosDecodeError::OutOfRange`], never as another
    /// position), an offset outside `[0, 1024)` or `-0.0`. Returns the position and the bytes read.
    pub fn decode(bytes: &[u8]) -> Result<(Pos, usize), PosDecodeError> {
        let mut at = 0usize;
        let mut chunk = [0i64; 3];
        let mut local = [0f64; 3];
        for a in 0..3 {
            let mut z: u128 = 0;
            let mut shift = 0u32;
            loop {
                let b = *bytes.get(at).ok_or(PosDecodeError::Truncated)?;
                at += 1;
                if shift >= 70 {
                    // More than ten groups: beyond any i64 index (zigzag fits 64 bits in ten groups).
                    return Err(PosDecodeError::OutOfRange);
                }
                z |= ((b & 0x7f) as u128) << shift;
                if b & 0x80 == 0 {
                    if b == 0 && shift > 0 {
                        return Err(PosDecodeError::NonCanonical); // a trailing zero group: not minimal
                    }
                    break;
                }
                shift += 7;
            }
            let c = ((z >> 1) as i128) ^ -((z & 1) as i128);
            chunk[a] = valid_chunk(c).ok_or(PosDecodeError::OutOfRange)?;
            let raw = bytes.get(at..at + 8).ok_or(PosDecodeError::Truncated)?;
            at += 8;
            let l = f64::from_le_bytes(raw.try_into().unwrap());
            if !(0.0..CHUNK).contains(&l) || l.to_bits() == (-0.0f64).to_bits() {
                return Err(PosDecodeError::NonCanonical);
            }
            local[a] = l;
        }
        Ok((Pos { chunk, local }, at))
    }

    /// The stored form, for component rows ([`PosRow`]).
    pub fn to_row(self) -> PosRow {
        PosRow { chunk: self.chunk, local: self.local }
    }

    /// Validates a stored row back into a `Pos`: the same invariants as [`Pos::decode`].
    pub fn from_row(r: PosRow) -> Result<Pos, OutOfRange> {
        for a in 0..3 {
            let l = r.local[a];
            if r.chunk[a] == RESERVED_CHUNK || !(0.0..CHUNK).contains(&l) || l.to_bits() == (-0.0f64).to_bits() {
                return Err(OutOfRange);
            }
        }
        Ok(Pos { chunk: r.chunk, local: r.local })
    }
}

/// `self - other` in metres, total (4.2, pinned as part of the determinism contract), per axis:
///
/// ```text
/// dc = (a.chunk as i128) - (b.chunk as i128)   // exact; |dc| < 2^64
/// dl = a.local - b.local                        // one IEEE f64 subtraction
/// d  = (dc as f64) * 1024.0 + dl                // the cast rounds only if |dc| > 2^53; * 1024 is exact
/// ```
impl core::ops::Sub for Pos {
    type Output = Offset;
    fn sub(self, other: Pos) -> Offset {
        let mut d = [0.0; 3];
        for a in 0..3 {
            let dc = self.chunk[a] as i128 - other.chunk[a] as i128;
            let dl = self.local[a] - other.local[a];
            d[a] = canon((dc as f64) * CHUNK + dl);
        }
        Offset(d)
    }
}

/// Why [`Pos::decode`] refused its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PosDecodeError {
    Truncated,
    NonCanonical,
    /// A valid encoding of a chunk index beyond the M1-M3 range (4.3), or the reserved index.
    OutOfRange,
}

/// A `Pos` as stored inside a component row: 48 bytes, 8-byte aligned. Its fields are private, so it gives no
/// numbers; [`Pos::from_row`] validates it (a row from another machine is untrusted).
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub struct PosRow {
    chunk: [i64; 3],
    local: [f64; 3],
}

impl Default for PosRow {
    fn default() -> Self {
        Pos::ORIGIN.to_row()
    }
}

impl CellKey {
    pub fn size(&self) -> Option<f64> {
        cell_size(self.level)
    }

    pub fn parent(&self) -> CellKey {
        CellKey { level: self.level + 1, c: [self.c[0] >> 1, self.c[1] >> 1, self.c[2] >> 1] }
    }

    /// The minimum corner, for levels 0..=[`MAX_LEVEL`] (whole chunks, exact). `None` if it is out of range, including
    /// a hand-made key whose shift would lose bits.
    pub fn min_corner(&self) -> Option<Pos> {
        if !(0..=MAX_LEVEL).contains(&self.level) {
            return None;
        }
        let mut chunk = [0i64; 3];
        for a in 0..3 {
            let v = self.c[a].checked_shl(self.level as u32)?;
            if v >> self.level != self.c[a] {
                return None;
            }
            chunk[a] = valid_chunk(v)?;
        }
        Some(Pos { chunk, local: [0.0; 3] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(chunk: [i64; 3], off: [f64; 3]) -> Pos {
        Pos::at_chunk(chunk).unwrap().checked_add(Offset(off)).unwrap()
    }

    #[test]
    fn carry_and_difference() {
        let q = p([0, 0, 0], [-0.25, 1024.0, 2048.5]);
        assert_eq!(q.cell(0).unwrap().c, [-1, 1, 2]);
        assert_eq!(q - p([-1, 1, 2], [0.0; 3]), Offset([1023.75, 0.0, 0.5]));
        let far = p([5_000_000_000, 0, 0], [3.0, 0.0, 0.0]);
        let back = far.checked_sub(Offset([3072.0, 0.0, 0.0])).unwrap();
        assert_eq!(back.cell(0).unwrap().c[0], 4_999_999_997);
        assert_eq!(back - far, Offset([-3072.0, 0.0, 0.0]));
        // The origin route: an explicit choice, coarse far away.
        assert_eq!(q - Pos::ORIGIN, Offset([-0.25, 1024.0, 2048.5]));
        assert_eq!(Pos::ORIGIN.checked_add(Offset([-0.25, 1024.0, 2048.5])).unwrap(), q);
    }

    #[test]
    fn no_saturation() {
        let edge = Pos::at_chunk([i64::MAX, 0, i64::MIN + 1]).unwrap();
        assert_eq!(edge.checked_add(Offset([1024.0, 0.0, 0.0])), Err(OutOfRange));
        assert_eq!(edge.checked_add(Offset([0.0, 0.0, -1.0])), Err(OutOfRange), "onto the reserved index");
        assert_eq!(Pos::at_chunk([RESERVED_CHUNK, 0, 0]), Err(OutOfRange));
        assert_eq!(Pos::ORIGIN.checked_add(Offset([f64::NAN, 0.0, 0.0])), Err(OutOfRange));
        assert_eq!(Pos::ORIGIN.checked_add(Offset([1e300, 0.0, 0.0])), Err(OutOfRange));
        // Pos - Pos is total even across the whole range.
        let lo = Pos::at_chunk([i64::MIN + 1; 3]).unwrap();
        let d = edge - lo;
        assert!(d.0.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn negative_zero_is_canonical() {
        let q = Pos::ORIGIN.checked_add(Offset([-0.0, 0.0, 0.0])).unwrap();
        let mut bytes = Vec::new();
        q.encode(&mut bytes);
        assert_eq!(Pos::decode(&bytes).unwrap().0, q);
        assert_eq!((q - q).0[0].to_bits(), 0.0f64.to_bits());
    }

    #[test]
    fn encoding_round_trips_and_refuses_non_canonical() {
        for q in [Pos::ORIGIN, p([-3, 7, i64::MAX], [1.5, 1023.999, 0.0]), p([i64::MIN + 1, -1, 0], [0.000001, 512.0, 999.0])] {
            let mut b = Vec::new();
            q.encode(&mut b);
            assert_eq!(Pos::decode(&b), Ok((q, b.len())));
            assert_eq!(Pos::from_row(q.to_row()), Ok(q));
        }
        let mut b = Vec::new();
        Pos::ORIGIN.encode(&mut b);
        let mut padded = vec![0x80, 0x00]; // 0 as a two-group varint: not minimal
        padded.extend_from_slice(&b[1..]);
        assert_eq!(Pos::decode(&padded), Err(PosDecodeError::NonCanonical));
        let mut neg0 = b.clone();
        neg0[1..9].copy_from_slice(&(-0.0f64).to_le_bytes());
        assert_eq!(Pos::decode(&neg0), Err(PosDecodeError::NonCanonical));
        assert_eq!(Pos::decode(&b[..5]), Err(PosDecodeError::Truncated));
        // 2^64 as a zigzag varint: a valid encoding beyond the M1 range is OutOfRange, never another position.
        let mut big = Vec::new();
        let mut z: u128 = 1 << 65;
        loop {
            let g = (z & 0x7f) as u8;
            z >>= 7;
            if z == 0 {
                big.push(g);
                break;
            }
            big.push(g | 0x80);
        }
        big.extend_from_slice(&b[1..]);
        assert_eq!(Pos::decode(&big), Err(PosDecodeError::OutOfRange));
    }

    #[test]
    fn whole_chunk_shift_keeps_differences_bit_identical() {
        let a = p([3, 0, -2], [1023.5, 0.25, 7.0]);
        let b = p([4, 1, -2], [0.125, 1000.0, 9.5]);
        let by = [1_000_000_007, -5, 42];
        let (sa, sb) = (a.checked_shift_chunks(by).unwrap(), b.checked_shift_chunks(by).unwrap());
        assert_eq!((sb - sa).0.map(f64::to_bits), (b - a).0.map(f64::to_bits));
        assert_eq!(a.checked_shift_chunks([i64::MAX, 0, 0]), Err(OutOfRange));
    }

    /// The OWO chunk-assignment battery (OWO doc, Appendix A), under corner alignment.
    #[test]
    fn owo_battery_corner_aligned() {
        for level in [-5, 0, 5, 10, 20] {
            let s = cell_size(level).unwrap();
            for (f, want) in [(0.3, 0i128), (-0.7, -1), (0.9, 0), (-0.0000001, -1), (1.0, 1)] {
                let q = Pos::ORIGIN.checked_add(Offset([f * s, 0.0, 0.0])).unwrap();
                assert_eq!(q.cell(level).unwrap().c[0], want, "level {level} f {f}");
            }
        }
    }

    #[test]
    fn cells_nest() {
        for q in [p([-3, 7, i64::MAX / 2], [1.5, 1023.999, 0.0]), p([i64::MIN / 2, -1, 0], [0.000001, 512.0, 999.0])] {
            for level in -40..62 {
                assert_eq!(q.cell(level).unwrap().parent(), q.cell(level + 1).unwrap(), "level {level}");
            }
        }
        let c = p([12, -9, 3], [5.0, 6.0, 7.0]).cell(2).unwrap();
        assert_eq!(c.min_corner().unwrap().cell(2), Some(c));
        // A hand-made key whose shift would lose bits has no corner (no wrap into the range).
        assert_eq!(CellKey { level: 2, c: [i128::MAX / 2, 0, 0] }.min_corner(), None);
    }

    #[test]
    fn ceil_log2_matches() {
        for (x, e) in [(1.0, 0), (1.5, 1), (2.0, 1), (0.75, 0), (0.5, -1), (1024.0, 10), (1025.0, 11)] {
            assert_eq!(ceil_log2(x), Some(e), "{x}");
        }
        // No input panics (world-model.md 5.3): subnormal, zero, negative, NaN and tiny sizes give None.
        for x in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MIN_POSITIVE / 2.0] {
            assert_eq!(ceil_log2(x), None, "{x}");
            assert_eq!(level_for_extent(x), None, "{x}");
        }
        assert_eq!(level_for_extent(1e-300), None);
        assert_eq!(level_for_extent(MIN_EXTENT), Some(MIN_LEVEL));
        assert_eq!(level_for_extent(MIN_EXTENT / 2.0), None);
        assert_eq!(Pos::ORIGIN.cell(MIN_LEVEL - 1), None);
        assert_eq!(Pos::ORIGIN.cell(MAX_LEVEL + 1), None);
        assert_eq!(pow2(-1022), None);
    }
}

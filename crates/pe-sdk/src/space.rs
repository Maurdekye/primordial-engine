//! Positions and offsets for script authors (docs/spec/logic.md 8; world-model.md 4; the user's ruling: "require them to intentionally subtract the origin position ... enforce this via the type system").
//!
//! - [`Pos`] is a place in the world, exact: the store's 1,024 m chunk index per axis plus an offset inside the
//!   chunk. It has no accessor to its numbers and no constructor from numbers. `Pos - Pos` gives an
//!   [`Offset<World>`]; `pos - ORIGIN` is the only way to absolute numbers, and `ORIGIN.checked_add(offset)` the only
//!   way back, so both read as the choice they are.
//! - [`Offset<S>`] is an ordinary `f32` vector tagged with its space: [`Own`] (the bound object's own space),
//!   [`Parent`] (its parent's), [`World`] (world axes, no origin). Two spaces do not mix without a conversion through
//!   a pose (`pe.Pose`, `pe.Transform`). Offsets have no `==`; they offer [`Offset::within`].
//! - A component field may hold a `Pos` or an [`OffsetFrom<Own>`] / [`OffsetFrom<Parent>`], never a bare `Offset`:
//!   a saved offset names what it is relative to.
//!
//! **The same bits as the engine.** pe-core's `Pos` needs `std`, so this is a `no_std` copy written to the same
//! pinned order of operations (world-model.md 4.2): the chunk difference in `i128`, one `f64` subtraction, `* 1024`,
//! the add; `checked_add`'s carry and its correction at 1,024; `-0.0` canonicalized to `+0.0`; the reserved chunk
//! `i64::MIN` refused. pe-logic's tests compare the two on the host and, through a test script, in wasm.
//!
//! **What the wall is not.** It stops accidents, as the ruling asks; it is not a security boundary. `Debug` prints a
//! `Pos` exactly (it reaches only `pe.log`, on the author's machine), and a script can copy a row's bytes through
//! `pe.get`; a determined author can recover numbers either way, as `- ORIGIN` already allows.

#![allow(clippy::needless_range_loop)] // per-axis loops read clearest with an index, as in pe-core

use core::fmt;
use core::marker::PhantomData;
use core::ops::{Add, AddAssign, Mul, Neg, Sub, SubAssign};

/// log2 of the level-0 chunk width (pe-core `CHUNK_EXP`).
pub const CHUNK_EXP: i32 = 10;
/// The level-0 chunk width in metres (pe-core `CHUNK`).
pub const CHUNK: f64 = 1024.0;
/// Never a valid chunk index (pe-core `RESERVED_CHUNK`, world-model.md 4.3).
const RESERVED_CHUNK: i64 = i64::MIN;
/// The finest and coarsest cell levels (pe-core `MIN_LEVEL`, `MAX_LEVEL`).
pub const MIN_LEVEL: i32 = -60;
pub const MAX_LEVEL: i32 = 64;

/// A position left the supported range (about ±9.4 × 10^21 m), or an offset was not finite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfRange;

/// An exact position: 48 bytes, the store's `Pos` field kind (pe-core `PosRow`).
///
/// Invariants, as pe-core's: every chunk index is in `(i64::MIN, i64::MAX]`; every offset is finite, in
/// `[0, 1024)` and never `-0.0`. Every way to make one keeps them, so a `Pos` a script writes passes the store's row
/// check, and equal positions have equal bytes.
#[repr(C)]
#[derive(Clone, Copy, PartialEq)]
pub struct Pos {
    chunk: [i64; 3],
    local: [f64; 3],
}

/// The world's origin: a world constant, chunk 0, offset 0.
pub const ORIGIN: Pos = Pos { chunk: [0; 3], local: [0.0; 3] };

impl Default for Pos {
    fn default() -> Pos {
        ORIGIN
    }
}

/// Exact text: the chunk indices and the offsets in the chunk. Never three absolute floats.
impl fmt::Debug for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pos {{ chunk: {:?}, local: {:?} }}", self.chunk, self.local)
    }
}

/// A space an [`Offset`] is in.
pub trait Space: Copy + 'static {}
/// A space an offset may be saved relative to, in a component field ([`OffsetFrom`]).
pub trait Saved: Space {}

/// The bound object's own space.
#[derive(Clone, Copy, Debug)]
pub struct Own;
/// The bound object's parent's space (the world's axes for a root).
#[derive(Clone, Copy, Debug)]
pub struct Parent;
/// World axes, with no origin.
#[derive(Clone, Copy, Debug)]
pub struct World;

impl Space for Own {}
impl Space for Parent {}
impl Space for World {}
impl Saved for Own {}
impl Saved for Parent {}

/// A vector in space `S`, in metres. Ordinary `f32` maths; no `==` (use [`Offset::within`]).
pub struct Offset<S: Space> {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    space: PhantomData<S>,
}

impl<S: Space> Clone for Offset<S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<S: Space> Copy for Offset<S> {}

impl<S: Space> fmt::Debug for Offset<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Offset({}, {}, {})", self.x, self.y, self.z)
    }
}

impl<S: Space> Default for Offset<S> {
    fn default() -> Self {
        Self::ZERO
    }
}

impl<S: Space> Offset<S> {
    pub const ZERO: Self = Offset::new(0.0, 0.0, 0.0);

    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Offset { x, y, z, space: PhantomData }
    }

    pub const fn from_array(v: [f32; 3]) -> Self {
        Offset::new(v[0], v[1], v[2])
    }

    pub const fn to_array(self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }

    pub fn length_sq(self) -> f32 {
        self.x * self.x + self.y * self.y + self.z * self.z
    }

    /// The length, correctly rounded, so every machine gets the same bits.
    pub fn length(self) -> f32 {
        sqrt_f32(self.length_sq())
    }

    /// True when the offset is no longer than `distance`: the comparison offsets have instead of `==`.
    pub fn within(self, distance: f32) -> bool {
        self.length_sq() <= distance * distance
    }

    pub fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    pub fn cross(self, o: Self) -> Self {
        Offset::new(self.y * o.z - self.z * o.y, self.z * o.x - self.x * o.z, self.x * o.y - self.y * o.x)
    }
}

impl<S: Space> Add for Offset<S> {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Offset::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl<S: Space> Sub for Offset<S> {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Offset::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl<S: Space> AddAssign for Offset<S> {
    fn add_assign(&mut self, o: Self) {
        *self = *self + o;
    }
}

impl<S: Space> SubAssign for Offset<S> {
    fn sub_assign(&mut self, o: Self) {
        *self = *self - o;
    }
}

impl<S: Space> Neg for Offset<S> {
    type Output = Self;
    fn neg(self) -> Self {
        Offset::new(-self.x, -self.y, -self.z)
    }
}

impl<S: Space> Mul<f32> for Offset<S> {
    type Output = Self;
    fn mul(self, k: f32) -> Self {
        Offset::new(self.x * k, self.y * k, self.z * k)
    }
}

/// An offset saved in a component field, relative to the space it names: [`Own`] or [`Parent`]. Three `f32` in the
/// store. A bare [`Offset`] is not a field type, so a saved offset always says what it is relative to.
#[repr(C)]
pub struct OffsetFrom<S: Saved> {
    v: [f32; 3],
    space: PhantomData<S>,
}

impl<S: Saved> Clone for OffsetFrom<S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<S: Saved> Copy for OffsetFrom<S> {}

impl<S: Saved> PartialEq for OffsetFrom<S> {
    /// Equal stored bytes' values: what the store compares, so a component's derived `PartialEq` still works.
    fn eq(&self, o: &Self) -> bool {
        self.v == o.v
    }
}

impl<S: Saved> fmt::Debug for OffsetFrom<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OffsetFrom({}, {}, {})", self.v[0], self.v[1], self.v[2])
    }
}

impl<S: Saved> Default for OffsetFrom<S> {
    fn default() -> Self {
        OffsetFrom { v: [0.0; 3], space: PhantomData }
    }
}

impl<S: Saved> OffsetFrom<S> {
    pub const fn get(self) -> Offset<S> {
        Offset::from_array(self.v)
    }

    pub fn set(&mut self, o: Offset<S>) {
        self.v = o.to_array();
    }
}

impl<S: Saved> From<Offset<S>> for OffsetFrom<S> {
    fn from(o: Offset<S>) -> Self {
        OffsetFrom { v: o.to_array(), space: PhantomData }
    }
}

impl<S: Saved> From<OffsetFrom<S>> for Offset<S> {
    fn from(o: OffsetFrom<S>) -> Self {
        o.get()
    }
}

/// A cell of the hierarchy (pe-core `CellKey`): exact integers, not a position, with no offset arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellKey {
    pub level: i32,
    pub c: [i128; 3],
}

/// `self - o`, total, as pe-core pins it per axis (world-model.md 4.2), then rounded once to `f32`:
///
/// ```text
/// dc = (a.chunk as i128) - (b.chunk as i128)    // exact
/// dl = a.local - b.local                         // one IEEE f64 subtraction
/// d  = (dc as f64) * 1024.0 + dl                 // * 1024 is exact
/// ```
impl Sub for Pos {
    type Output = Offset<World>;
    fn sub(self, o: Pos) -> Offset<World> {
        let d = self.diff(o);
        Offset::new(canon32(d[0] as f32), canon32(d[1] as f32), canon32(d[2] as f32))
    }
}

/// `self` moved by `d`; out of range traps, which faults the call deterministically (logic.md 3.6). Use
/// [`Pos::checked_add`] to handle it.
impl Add<Offset<World>> for Pos {
    type Output = Pos;
    fn add(self, d: Offset<World>) -> Pos {
        match self.checked_add(d) {
            Ok(p) => p,
            Err(_) => panic!("position out of range"),
        }
    }
}

/// `self` moved by `-d`; out of range traps, as `+` does.
impl Sub<Offset<World>> for Pos {
    type Output = Pos;
    fn sub(self, d: Offset<World>) -> Pos {
        match self.checked_sub(d) {
            Ok(p) => p,
            Err(_) => panic!("position out of range"),
        }
    }
}

impl AddAssign<Offset<World>> for Pos {
    fn add_assign(&mut self, d: Offset<World>) {
        *self = *self + d;
    }
}

impl SubAssign<Offset<World>> for Pos {
    fn sub_assign(&mut self, d: Offset<World>) {
        *self = *self - d;
    }
}

impl Pos {
    /// `self - o` per axis in `f64`, before the rounding to `f32` (pe-core's `Pos - Pos`, bit for bit).
    fn diff(self, o: Pos) -> [f64; 3] {
        let mut d = [0.0; 3];
        for (a, out) in d.iter_mut().enumerate() {
            let dc = self.chunk[a] as i128 - o.chunk[a] as i128;
            let dl = self.local[a] - o.local[a];
            *out = canon((dc as f64) * CHUNK + dl);
        }
        d
    }

    /// `self` moved by `d` (pe-core `Pos::checked_add` of `d` widened to `f64`, bit for bit). Exact in the chunk
    /// part; a non-finite `d` or a result past the range is [`OutOfRange`].
    pub fn checked_add(self, d: Offset<World>) -> Result<Pos, OutOfRange> {
        self.add_f64([d.x as f64, d.y as f64, d.z as f64])
    }

    /// `self` moved by `-d`.
    pub fn checked_sub(self, d: Offset<World>) -> Result<Pos, OutOfRange> {
        self.checked_add(-d)
    }

    fn add_f64(self, d: [f64; 3]) -> Result<Pos, OutOfRange> {
        let mut out = self;
        for a in 0..3 {
            let v = self.local[a] + d[a];
            if !v.is_finite() {
                return Err(OutOfRange);
            }
            let k = floor(v / CHUNK);
            let mut l = v - k * CHUNK;
            // |k| >= 2^64 chunks is out of range whatever the start; below that the cast is exact.
            let mut kc: i128 = if abs(k) < TWO_64 { k as i128 } else { return Err(OutOfRange) };
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

    /// The distance in metres, correctly rounded from the exact difference.
    pub fn distance(self, o: Pos) -> f32 {
        let d = self.diff(o);
        sqrt_f64(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]) as f32
    }

    /// The position a fraction `t` of the way to `o`.
    pub fn blend(self, o: Pos, t: f32) -> Result<Pos, OutOfRange> {
        self.checked_add((o - self) * t)
    }

    /// The cell containing this position at `level` (pe-core `Pos::cell`): exact integers. `None` outside
    /// [`MIN_LEVEL`]..=[`MAX_LEVEL`].
    pub fn cell(self, level: i32) -> Option<CellKey> {
        if !(MIN_LEVEL..=MAX_LEVEL).contains(&level) {
            return None;
        }
        let size = f64::from_bits(((CHUNK_EXP + level + 1023) as u64) << 52);
        let mut c = [0i128; 3];
        for (a, out) in c.iter_mut().enumerate() {
            *out = if level >= 0 {
                (self.chunk[a] as i128) >> level
            } else {
                let sub = floor(self.local[a] / size) as i128; // exact: a power-of-two divide
                ((self.chunk[a] as i128) << -level) + sub
            };
        }
        Some(CellKey { level, c })
    }
}

impl Pos {
    /// A root `Transform`'s position: the corner of `chunk` moved by `pos` (pe-world's reading of a root row,
    /// world-model.md 4.4). `OutOfRange` for a chunk or sum the store would refuse.
    pub(crate) fn from_root_row(chunk: [i64; 3], pos: [f64; 3]) -> Result<Pos, OutOfRange> {
        for c in chunk {
            valid_chunk(c as i128).ok_or(OutOfRange)?;
        }
        Pos { chunk, local: [0.0; 3] }.add_f64(pos)
    }

    /// The row form: chunk indices and offsets, for writing a root `Transform`.
    pub(crate) fn to_root_row(self) -> ([i64; 3], [f64; 3]) {
        (self.chunk, self.local)
    }
}

const TWO_64: f64 = 18_446_744_073_709_551_616.0;

fn valid_chunk(c: i128) -> Option<i64> {
    let v = i64::try_from(c).ok()?;
    (v != RESERVED_CHUNK).then_some(v)
}

/// `+0.0` for `-0.0`, otherwise `x`.
fn canon(x: f64) -> f64 {
    if x == 0.0 { 0.0 } else { x }
}

fn canon32(x: f32) -> f32 {
    if x == 0.0 { 0.0 } else { x }
}

fn abs(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & !(1u64 << 63))
}

/// `f64::floor`, bit for bit, for every input (`core` has no float methods on this toolchain). At and above 2^52
/// every finite `f64` is an integer, so it is its own floor; below, the cast to `i64` truncates exactly. `-0.0`,
/// infinities and NaN are returned as they are, as `floor` returns them.
pub(crate) fn floor(x: f64) -> f64 {
    if x.is_nan() || abs(x) >= 4_503_599_627_370_496.0 || x == 0.0 {
        return x; // NaN, integral (|x| >= 2^52, infinities included), or a zero of either sign
    }
    // Exact truncation toward zero. For x in (-1, 0) it is +0.0 > x, so the result is -1.0, as std's.
    let t = x as i64 as f64;
    if t > x { t - 1.0 } else { t }
}

/// The square root of an f32 through the correctly rounded f64 one: f64 has more than 2 * 24 + 2 bits, so rounding
/// twice gives the correctly rounded f32 result.
fn sqrt_f32(x: f32) -> f32 {
    sqrt_f64(x as f64) as f32
}

/// IEEE-754 square root by digit recurrence, correctly rounded, so every machine gets the same bits.
pub(crate) fn sqrt_f64(x: f64) -> f64 {
    if x.is_nan() || x <= 0.0 {
        return if x == 0.0 { x } else { f64::NAN };
    }
    if x == f64::INFINITY {
        return x;
    }
    // Scale into [1, 4) by an even power of two, take the root of the mantissa by bits, scale back.
    let bits = x.to_bits();
    let mut e = ((bits >> 52) & 0x7ff) as i64;
    let mut m = bits & ((1u64 << 52) - 1);
    if e == 0 {
        // Subnormal: normalize.
        let shift = m.leading_zeros() as i64 - 11;
        m <<= shift;
        e = 1 - shift;
        m &= (1u64 << 52) - 1;
    }
    m |= 1u64 << 52;
    let mut exp = e - 1023;
    if exp & 1 != 0 {
        m <<= 1;
        exp -= 1;
    }
    // m is in [2^52, 2^54); the root of m * 2^56 lies in [2^54, 2^55): 53 bits and two rounding bits.
    let v = (m as u128) << 56;
    let mut r: u128 = 0;
    let mut rem: u128 = 0;
    let mut i = 108i32;
    while i >= 0 {
        rem = (rem << 2) | ((v >> i) & 3);
        let t = (r << 2) | 1;
        r <<= 1;
        if rem >= t {
            rem -= t;
            r |= 1;
        }
        i -= 2;
    }
    // Round off the two low bits (nearest, ties to even, with the remainder as the sticky bit).
    let round = (r & 3) as u64;
    let mut q = (r >> 2) as u64;
    let sticky = rem != 0;
    if round > 2 || (round == 2 && (sticky || q & 1 == 1)) {
        q += 1;
    }
    let out_exp = (exp / 2 + 1023) as u64;
    if q >> 53 != 0 {
        q >>= 1;
        return f64::from_bits(((out_exp + 1) << 52) | (q & ((1u64 << 52) - 1)));
    }
    f64::from_bits((out_exp << 52) | (q & ((1u64 << 52) - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqrt_is_correctly_rounded() {
        for x in [1.0f64, 2.0, 3.0, 4.0, 0.5, 1e-300, 1e300, 123456.789, 5e-324, 0.0, 7.0, 1.0 + f64::EPSILON] {
            assert_eq!(sqrt_f64(x).to_bits(), x.sqrt().to_bits(), "{x}");
        }
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..20000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let x = f64::from_bits(s >> 2); // positive finite
            if x.is_finite() && x > 0.0 {
                assert_eq!(sqrt_f64(x).to_bits(), x.sqrt().to_bits(), "{x:e}");
            }
        }
    }

    #[test]
    fn floor_is_std_floor_bit_for_bit() {
        let edges = [
            0.0,
            -0.0,
            0.5,
            -0.5,
            1.0,
            -1.0,
            1.5,
            -1.5,
            2.0f64.powi(52),
            -(2.0f64.powi(52)),
            2.0f64.powi(52) - 0.5,
            -(2.0f64.powi(52) - 0.5),
            2.0f64.powi(63),
            -(2.0f64.powi(63)),
            1e300,
            -1e300,
            5e-324,
            -5e-324,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN,
            -0.999_999_999_999_9,
        ];
        for x in edges {
            assert_eq!(floor(x).to_bits(), x.floor().to_bits(), "{x:e}");
        }
        assert!(floor(f64::NAN).is_nan());
        let mut s = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..200_000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let x = f64::from_bits(s);
            if !x.is_nan() {
                assert_eq!(floor(x).to_bits(), x.floor().to_bits(), "{x:e}");
            }
        }
    }

    #[test]
    fn pos_arithmetic_is_exact_across_chunks() {
        let a = ORIGIN + Offset::<World>::new(100.0, -3.0, 1023.5);
        let b = a + Offset::new(-2000.0, 0.0, 1.0);
        let d = b - a;
        assert_eq!(d.to_array(), [-2000.0, 0.0, 1.0]);
        assert_eq!(a.distance(a), 0.0);
        assert!((a - ORIGIN).within(1100.0));
        assert_eq!((a.blend(b, 0.5).unwrap() - a).to_array(), [-1000.0, 0.0, 0.5]);
        assert_eq!(b.chunk, [-2, -1, 1]);
    }

    #[test]
    fn no_negative_zero_and_no_reserved_chunk() {
        let p = ORIGIN.checked_add(Offset::new(-0.0, 0.0, 0.0)).unwrap();
        assert_eq!(p.local[0].to_bits(), 0.0f64.to_bits());
        let d = p - p;
        assert_eq!(d.x.to_bits(), 0.0f32.to_bits());
        // One chunk above the reserved index: moving down one more chunk is out of range, never i64::MIN.
        let low = Pos { chunk: [i64::MIN + 1, 0, 0], local: [0.0; 3] };
        assert_eq!(low.checked_add(Offset::new(-1.0, 0.0, 0.0)), Err(OutOfRange));
        let high = Pos { chunk: [i64::MAX, 0, 0], local: [1023.0, 0.0, 0.0] };
        assert_eq!(high.checked_add(Offset::new(1.0, 0.0, 0.0)), Err(OutOfRange));
        assert_eq!(ORIGIN.checked_add(Offset::new(f32::INFINITY, 0.0, 0.0)), Err(OutOfRange));
    }

    #[test]
    fn cells_are_exact_integers() {
        let p = ORIGIN + Offset::<World>::new(1536.0, -1.0, 0.25);
        assert_eq!(p.cell(0), Some(CellKey { level: 0, c: [1, -1, 0] }));
        assert_eq!(p.cell(1), Some(CellKey { level: 1, c: [0, -1, 0] }));
        assert_eq!(p.cell(-10), Some(CellKey { level: -10, c: [1536, -1, 0] }));
        assert_eq!(p.cell(MAX_LEVEL + 1), None);
    }
}

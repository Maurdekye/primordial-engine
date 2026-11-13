//! Positions and offsets for script authors (docs/spec/logic.md 8, file 17 section 7).
//!
//! - [`Pos`] is exact: the store's chunk index plus an offset inside the chunk, with no accessor to its numbers.
//!   `Pos - Pos` gives an [`Offset<World>`], `Pos + Offset<World>` a `Pos`. [`ORIGIN`] is a constant `Pos`, and
//!   `pos - ORIGIN` is the only way to absolute numbers, so it reads as the choice it is.
//! - [`Offset<S>`] is an ordinary vector tagged with its space: [`Own`] (the bound object's own space), [`Parent`],
//!   [`World`] (world axes, no origin). Two spaces do not mix without an explicit conversion. Offsets have no `==`;
//!   they offer [`Offset::within`].

use core::marker::PhantomData;
use core::ops::{Add, Mul, Neg, Sub};

/// The edge of a chunk, in metres (pe-core `coords`).
pub const CHUNK_SIZE: f64 = 64.0;

/// An exact position. Three chunk indices and an offset in the chunk, 48 bytes, the store's `Pos` field kind.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pos {
    chunk: [i64; 3],
    off: [f64; 3],
}

/// The world's origin.
pub const ORIGIN: Pos = Pos { chunk: [0; 3], off: [0.0; 3] };

pub trait Space {}

/// The bound object's own space.
#[derive(Clone, Copy, Debug)]
pub struct Own;
/// The bound object's parent's space.
#[derive(Clone, Copy, Debug)]
pub struct Parent;
/// World axes, with no origin.
#[derive(Clone, Copy, Debug)]
pub struct World;

impl Space for Own {}
impl Space for Parent {}
impl Space for World {}

/// A vector in space `S`.
#[derive(Debug)]
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

impl<S: Space> Offset<S> {
    pub const ZERO: Self = Offset::new(0.0, 0.0, 0.0);

    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Offset { x, y, z, space: PhantomData }
    }

    pub fn length_sq(self) -> f32 {
        self.x * self.x + self.y * self.y + self.z * self.z
    }

    /// True when the offset is no longer than `distance`: the comparison offsets have instead of `==`.
    pub fn within(self, distance: f32) -> bool {
        self.length_sq() <= distance * distance
    }

    pub fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
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

impl Sub for Pos {
    type Output = Offset<World>;
    /// Exact on the chunk indices first, then the offsets.
    fn sub(self, o: Pos) -> Offset<World> {
        let d = |i: usize| ((self.chunk[i] - o.chunk[i]) as f64 * CHUNK_SIZE + (self.off[i] - o.off[i])) as f32;
        Offset::new(d(0), d(1), d(2))
    }
}

impl Add<Offset<World>> for Pos {
    type Output = Pos;
    fn add(self, o: Offset<World>) -> Pos {
        let mut p = self;
        for (i, v) in [o.x, o.y, o.z].into_iter().enumerate() {
            let t = p.off[i] + v as f64;
            let k = floor(t / CHUNK_SIZE);
            p.chunk[i] += k as i64;
            p.off[i] = t - k * CHUNK_SIZE;
        }
        p
    }
}

impl Pos {
    /// The distance between two positions.
    pub fn distance(self, o: Pos) -> f32 {
        sqrt((self - o).length_sq())
    }

    /// The position a fraction `t` of the way to `o`.
    pub fn blend(self, o: Pos, t: f32) -> Pos {
        self + (o - self) * t
    }
}

// no_std has no float methods: these use the bit-exact operations wasm and every host CPU agree on.
fn floor(x: f64) -> f64 {
    let t = x as i64 as f64;
    if t > x { t - 1.0 } else { t }
}

/// The square root of an f32 through the correctly rounded f64 one: f64 has more than 2 * 24 + 2 bits, so rounding
/// twice gives the correctly rounded f32 result.
fn sqrt(x: f32) -> f32 {
    libm_sqrt(x as f64) as f32
}

/// IEEE-754 square root by digit recurrence, correctly rounded, so every machine gets the same bits.
fn libm_sqrt(x: f64) -> f64 {
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
            assert_eq!(libm_sqrt(x).to_bits(), x.sqrt().to_bits(), "{x}");
        }
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..20000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let x = f64::from_bits(s >> 2); // positive finite
            if x.is_finite() && x > 0.0 {
                assert_eq!(libm_sqrt(x).to_bits(), x.sqrt().to_bits(), "{x:e}");
            }
        }
    }

    #[test]
    fn pos_arithmetic_is_exact_across_chunks() {
        let a = ORIGIN + Offset::<World>::new(100.0, -3.0, 63.5);
        let b = a + Offset::new(-200.0, 0.0, 1.0);
        let d = b - a;
        assert_eq!((d.x, d.y, d.z), (-200.0, 0.0, 1.0));
        assert_eq!(a.distance(a), 0.0);
        assert!((a - ORIGIN).within(120.0));
        assert_eq!(a.blend(b, 0.5) - a, Offset::<World>::new(-100.0, 0.0, 0.5).into_eq());
    }

    impl<S: Space> Offset<S> {
        fn into_eq(self) -> Offset<S> {
            self
        }
    }

    impl<S: Space> PartialEq for Offset<S> {
        fn eq(&self, o: &Self) -> bool {
            (self.x, self.y, self.z) == (o.x, o.y, o.z)
        }
    }
}

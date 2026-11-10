//! Bit-exact transcendental functions for sim code (design 4.3 rule 3).
//!
//! `std`'s `sin`, `exp` and friends call the platform's libm, whose results differ in the last bit
//! between platforms. These functions use only IEEE `+ - * /`, `sqrt`, comparisons and `f64`/`i64`
//! conversions, which are exact or correctly rounded on every target, so they return the same bits
//! everywhere.
//!
//! The `f32` functions evaluate an `f64` kernel (fdlibm's polynomials) and convert the result to `f32`.
//! Measured against `std` in the tests: within 1 ulp of `f32` over the ranges tested.
//!
//! `sin`/`cos`/`tan` reduce their argument exactly for every finite `f32` (review P21-4): below 2^19 with
//! a two-part pi/2 in `f64`, above it with a 98-bit window of the bits of 2/pi (Payne-Hanek). So they
//! stay in range and accurate up to `f32::MAX`.
//!
//! Special values follow IEEE conventions (`ln(0) = -inf`, `ln(-1) = NaN`, `exp(1000) = inf`). NaN must
//! never enter sim state (4.3 rule 6): callers validate at their boundaries. `asin`/`acos` clamp their
//! input to [-1, 1], so rounding drift in a dot product does not produce NaN.

// The constants are fdlibm's literals, kept digit for digit for traceability.
#![allow(clippy::excessive_precision, clippy::approx_constant)]

/// `round half away from zero` to an integer, without a library call.
#[inline]
fn round_i64(x: f64) -> i64 {
    (if x >= 0.0 { x + 0.5 } else { x - 0.5 }) as i64
}

/// Exact `2^k` for -1022 <= k <= 1023.
#[inline]
fn pow2(k: i64) -> f64 {
    f64::from_bits(((k + 1023) as u64) << 52)
}

const FRAC_2_PI: f64 = 6.36619772367581382433e-01;
// pi/2 split so that k * PIO2_HI is exact for |k| < 2^20 (fdlibm pio2_1, pio2_1t).
const PIO2_HI: f64 = 1.57079632673412561417e+00;
const PIO2_LO: f64 = 6.07710050650619224932e-11;

const S1: f64 = -1.66666666666666324348e-01;
const S2: f64 = 8.33333333332248946124e-03;
const S3: f64 = -1.98412698298579493134e-04;
const S4: f64 = 2.75573137070700676789e-06;
const S5: f64 = -2.50507602534068634195e-08;
const S6: f64 = 1.58969099521155010221e-10;

const C1: f64 = 4.16666666666666019037e-02;
const C2: f64 = -1.38888888888741095749e-03;
const C3: f64 = 2.48015872894767294178e-05;
const C4: f64 = -2.75573143513906633035e-07;
const C5: f64 = 2.08757232129817482790e-09;
const C6: f64 = -1.13596475577881948265e-11;

/// sin on [-pi/4, pi/4].
#[inline]
fn k_sin(x: f64) -> f64 {
    let z = x * x;
    x + x * z * (S1 + z * (S2 + z * (S3 + z * (S4 + z * (S5 + z * S6)))))
}

/// cos on [-pi/4, pi/4].
#[inline]
fn k_cos(x: f64) -> f64 {
    let z = x * x;
    1.0 - 0.5 * z + z * z * (C1 + z * (C2 + z * (C3 + z * (C4 + z * (C5 + z * C6)))))
}

/// 2/pi as a binary fraction: bits 1 to 384 after the point, most significant first. Computed exactly with
/// integer arithmetic (Machin's formula); it begins 0xA2F9836E4E44..., the published expansion.
const TWO_OVER_PI: [u64; 6] = [
    0xA2F9_836E_4E44_1529,
    0xFC27_57D1_F534_DDC0,
    0xDB62_9599_3C43_9041,
    0xFE51_63AB_DEBB_C561,
    0xB724_6E3A_424D_D2E0,
    0x0649_2EEA_09D1_921C,
];

const PIO2: f64 = 1.57079632679489661923e+00;

/// Bit `i` (1-based, weight 2^-i) of 2/pi.
fn two_over_pi_bit(i: i32) -> u128 {
    if !(1..=384).contains(&i) {
        return 0;
    }
    let j = (i - 1) as usize;
    ((TWO_OVER_PI[j / 64] >> (63 - j % 64)) & 1) as u128
}

/// Exact reduction of a finite `f32` with |x| >= 2^19: `x = q * pi/2 + r` with |r| <= pi/4.
///
/// |x| = m * 2^e with m a 24-bit integer. Bits of 2/pi with weight above 2^(e-1) only add multiples of 4 to
/// x * 2/pi, so the 98 bits from 2^-(e-1) to 2^-(e+96) give `x * 2/pi mod 4` with 96 fractional bits. The
/// bits dropped below change r by under 2^-71.
fn reduce_large(x: f32) -> (i64, f64) {
    let bits = x.to_bits();
    let m = ((bits & 0x7f_ffff) | 0x80_0000) as u128;
    let e = ((bits >> 23) & 0xff) as i32 - 150;
    let mut w = 0u128;
    for i in (e - 1)..=(e + 96) {
        w = (w << 1) | two_over_pi_bit(i);
    }
    let v = (m * w) & ((1u128 << 98) - 1);
    let mut q = (v >> 96) as i64;
    let mut f = (v & ((1u128 << 96) - 1)) as i128;
    if f >= 1i128 << 95 {
        f -= 1i128 << 96;
        q += 1;
    }
    let r = f as f64 * pow2(-96) * PIO2;
    if x < 0.0 { (-q, -r) } else { (q, r) }
}

/// `(sin x, cos x)` in `f64`, for a finite `f32` `x`.
fn sin_cos_f64(x: f32) -> (f64, f64) {
    let (k, r) = if x.abs() >= 524_288.0 {
        reduce_large(x)
    } else {
        let xd = x as f64;
        let k = round_i64(xd * FRAC_2_PI);
        let kf = k as f64;
        (k, (xd - kf * PIO2_HI) - kf * PIO2_LO)
    };
    let (s, c) = (k_sin(r), k_cos(r));
    match k & 3 {
        0 => (s, c),
        1 => (c, -s),
        2 => (-s, -c),
        _ => (-c, s),
    }
}

/// `(sin x, cos x)`.
pub fn sin_cos(x: f32) -> (f32, f32) {
    if !x.is_finite() {
        return (f32::NAN, f32::NAN);
    }
    let (s, c) = sin_cos_f64(x);
    (s as f32, c as f32)
}

pub fn sin(x: f32) -> f32 {
    sin_cos(x).0
}

pub fn cos(x: f32) -> f32 {
    sin_cos(x).1
}

pub fn tan(x: f32) -> f32 {
    if !x.is_finite() {
        return f32::NAN;
    }
    let (s, c) = sin_cos_f64(x);
    (s / c) as f32
}

const ATAN_HI: [f64; 4] =
    [4.63647609000806093515e-01, 7.85398163397448278999e-01, 9.82793723247329054082e-01, 1.57079632679489655800e+00];
const ATAN_LO: [f64; 4] =
    [2.26987774529616870924e-17, 3.06161699786838301793e-17, 1.39033110312309984516e-17, 6.12323399573676603587e-17];
const AT: [f64; 11] = [
    3.33333333333329318027e-01,
    -1.99999999998764832476e-01,
    1.42857142725034663711e-01,
    -1.11111104054623557880e-01,
    9.09088713343650656196e-02,
    -7.69187620504482999495e-02,
    6.66107313738753120669e-02,
    -5.83357013379057348645e-02,
    4.97687799461593236017e-02,
    -3.65315727442169155270e-02,
    1.62858201153657823623e-02,
];

/// atan in `f64` (fdlibm's reduction and polynomial).
fn atan_f64(x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    let neg = x < 0.0;
    let mut a = if neg { -x } else { x };
    if a >= 7.378697629483820646e19 {
        // 2^66: atan is pi/2 to f64 precision.
        let r = ATAN_HI[3] + ATAN_LO[3];
        return if neg { -r } else { r };
    }
    let id: i32;
    if a < 0.4375 {
        id = -1;
    } else if a < 1.1875 {
        if a < 0.6875 {
            id = 0;
            a = (2.0 * a - 1.0) / (2.0 + a);
        } else {
            id = 1;
            a = (a - 1.0) / (a + 1.0);
        }
    } else if a < 2.4375 {
        id = 2;
        a = (a - 1.5) / (1.0 + 1.5 * a);
    } else {
        id = 3;
        a = -1.0 / a;
    }
    let z = a * a;
    let w = z * z;
    let s1 = z * (AT[0] + w * (AT[2] + w * (AT[4] + w * (AT[6] + w * (AT[8] + w * AT[10])))));
    let s2 = w * (AT[1] + w * (AT[3] + w * (AT[5] + w * (AT[7] + w * AT[9]))));
    let r = if id < 0 {
        a - a * (s1 + s2)
    } else {
        let i = id as usize;
        ATAN_HI[i] - ((a * (s1 + s2) - ATAN_LO[i]) - a)
    };
    if neg { -r } else { r }
}

const PI: f64 = 3.14159265358979311600e+00;
const PI_LO: f64 = 1.2246467991473531772e-16;

/// atan2 in `f64`, with the IEEE conventions for zeros and infinities of `y` and `x`.
fn atan2_f64(y: f64, x: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    let pi_y = if y.is_sign_negative() { -PI } else { PI };
    if y == 0.0 {
        // atan2(+-0, x >= +0) = +-0; atan2(+-0, x <= -0) = +-pi.
        return if x.is_sign_negative() { pi_y } else { y };
    }
    if x == 0.0 {
        return 0.5 * pi_y;
    }
    if x.is_infinite() {
        if y.is_infinite() {
            let q = if x > 0.0 { 0.25 * pi_y } else { 0.75 * pi_y };
            return q;
        }
        return if x > 0.0 { 0.0f64.copysign(y) } else { pi_y };
    }
    if y.is_infinite() {
        return 0.5 * pi_y;
    }
    let t = atan_f64((y / x).abs());
    match (x > 0.0, y > 0.0) {
        (true, true) => t,
        (true, false) => -t,
        (false, true) => PI - (t - PI_LO),
        (false, false) => (t - PI_LO) - PI,
    }
}

pub fn atan(x: f32) -> f32 {
    atan_f64(x as f64) as f32
}

pub fn atan2(y: f32, x: f32) -> f32 {
    atan2_f64(y as f64, x as f64) as f32
}

/// asin, with the input clamped to [-1, 1].
pub fn asin(x: f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    let x = (x as f64).clamp(-1.0, 1.0);
    atan2_f64(x, ((1.0 - x) * (1.0 + x)).sqrt()) as f32
}

/// acos, with the input clamped to [-1, 1].
pub fn acos(x: f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    let x = (x as f64).clamp(-1.0, 1.0);
    atan2_f64(((1.0 - x) * (1.0 + x)).sqrt(), x) as f32
}

const LN2_HI: f64 = 6.93147180369123816490e-01;
const LN2_LO: f64 = 1.90821492927058770002e-10;
const INV_LN2: f64 = 1.44269504088896338700e+00;

/// e^x in `f64` for |x| < 709: reduce by multiples of ln 2, then a Taylor polynomial on |r| <= 0.347.
fn exp_f64(x: f64) -> f64 {
    let k = round_i64(x * INV_LN2);
    let kf = k as f64;
    let r = (x - kf * LN2_HI) - kf * LN2_LO;
    // Horner form of sum_{n=0..13} r^n / n!; the truncation error is below 2^-60 relative.
    let mut p = 1.0 / 6227020800.0; // 1/13!
    for d in [479001600.0, 39916800.0, 3628800.0, 362880.0, 40320.0, 5040.0, 720.0, 120.0, 24.0, 6.0, 2.0, 1.0, 1.0f64]
    {
        p = p * r + 1.0 / d;
    }
    // 2^k may leave the normal range for |x| near 709; split the scaling in two exact steps.
    let k1 = k / 2;
    p * pow2(k1) * pow2(k - k1)
}

pub fn exp(x: f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    if x > 89.0 {
        return f32::INFINITY;
    }
    if x < -104.0 {
        return 0.0;
    }
    exp_f64(x as f64) as f32
}

/// ln in `f64` for a positive normal `x`: `x = m * 2^e` with m in [sqrt(1/2), sqrt(2)), then
/// `ln m = 2 atanh(s)`, `s = (m - 1) / (m + 1)`, as an odd series in s (|s| < 0.172).
fn ln_f64(x: f64) -> f64 {
    let bits = x.to_bits();
    let mut e = ((bits >> 52) & 0x7ff) as i64 - 1023;
    let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);
    if m > 1.41421356237309504880 {
        m *= 0.5;
        e += 1;
    }
    let s = (m - 1.0) / (m + 1.0);
    let z = s * s;
    // 2 * sum_{n=0..11} s^(2n+1) / (2n+1)
    let mut p = 1.0 / 23.0;
    for d in [21.0, 19.0, 17.0, 15.0, 13.0, 11.0, 9.0, 7.0, 5.0, 3.0, 1.0f64] {
        p = p * z + 1.0 / d;
    }
    let lnm = 2.0 * s * p;
    let ef = e as f64;
    ef * LN2_HI + (ef * LN2_LO + lnm)
}

pub fn ln(x: f32) -> f32 {
    if x.is_nan() || x < 0.0 {
        return f32::NAN;
    }
    if x == 0.0 {
        return f32::NEG_INFINITY;
    }
    if x.is_infinite() {
        return x;
    }
    // f32 subnormals are normal as f64.
    ln_f64(x as f64) as f32
}

pub fn log2(x: f32) -> f32 {
    if x.is_nan() || x <= 0.0 || x.is_infinite() {
        return ln(x);
    }
    (ln_f64(x as f64) * 1.44269504088896338700e+00) as f32
}

/// `x^y` for `x > 0` (and `x = 0`, `y > 0` gives 0). Negative `x` gives NaN; use [`powi`] for integer
/// exponents of negative bases. As in C99, `pow(x, 0)` and `pow(1, y)` are 1 for every `x` and `y`.
pub fn pow(x: f32, y: f32) -> f32 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if x.is_nan() || y.is_nan() || x < 0.0 {
        return f32::NAN;
    }
    if x == 0.0 {
        return if y > 0.0 { 0.0 } else { f32::INFINITY };
    }
    if x.is_infinite() {
        return if y > 0.0 { f32::INFINITY } else { 0.0 };
    }
    let t = y as f64 * ln_f64(x as f64);
    if t > 89.0 {
        return f32::INFINITY;
    }
    if t < -104.0 {
        return 0.0;
    }
    exp_f64(t) as f32
}

/// `x^n` by repeated squaring, in a fixed operation order.
pub fn powi(x: f32, n: i32) -> f32 {
    let mut base = x as f64;
    let mut e = n.unsigned_abs();
    let mut acc = 1.0f64;
    while e > 0 {
        if e & 1 == 1 {
            acc *= base;
        }
        base *= base;
        e >>= 1;
    }
    (if n < 0 { 1.0 / acc } else { acc }) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ulps(a: f32, b: f32) -> u32 {
        if a == b {
            return 0;
        }
        let key = |v: f32| {
            let i = v.to_bits() as i32;
            if i < 0 { i32::MIN.wrapping_sub(i) } else { i }
        };
        key(a).abs_diff(key(b))
    }

    /// Inputs spread over [lo, hi] plus exact special points.
    fn sweep(lo: f32, hi: f32, n: usize) -> Vec<f32> {
        (0..=n).map(|i| lo + (hi - lo) * (i as f32 / n as f32)).collect()
    }

    fn check(name: &str, f: impl Fn(f32) -> f32, g: impl Fn(f32) -> f32, xs: &[f32], max_ulps: u32) {
        let mut worst = (0u32, 0.0f32);
        for &x in xs {
            let (a, b) = (f(x), g(x));
            if b.is_nan() {
                assert!(a.is_nan(), "{name}({x}) = {a}, std gives NaN");
                continue;
            }
            let u = ulps(a, b);
            if u > worst.0 {
                worst = (u, x);
            }
        }
        assert!(worst.0 <= max_ulps, "{name}: {} ulps at x = {}", worst.0, worst.1);
    }

    #[test]
    fn accuracy_against_std() {
        let angles = sweep(-1000.0, 1000.0, 200_000);
        check("sin", sin, f32::sin, &angles, 1);
        check("cos", cos, f32::cos, &angles, 1);
        check("tan", tan, f32::tan, &sweep(-1.5, 1.5, 100_000), 1);
        // Large arguments (review P21-4), against std's f64 functions, which reduce exactly.
        let large: Vec<f32> = (0..20_000)
            .map(|i| f32::from_bits(0x4880_0000 + i * 0xB439))
            .chain([f32::MAX, -f32::MAX, 1.0e38, -3.0e30])
            .collect();
        check("sin large", sin, |x| (x as f64).sin() as f32, &large, 1);
        check("cos large", cos, |x| (x as f64).cos() as f32, &large, 1);
        for &x in &large {
            assert!(sin(x).abs() <= 1.0 && cos(x).abs() <= 1.0 && tan(x).is_finite(), "x = {x}");
        }
        let wide = sweep(-50.0, 50.0, 200_000);
        check("atan", atan, f32::atan, &wide, 1);
        check("asin", asin, f32::asin, &sweep(-1.0, 1.0, 100_000), 1);
        check("acos", acos, f32::acos, &sweep(-1.0, 1.0, 100_000), 1);
        check("exp", exp, f32::exp, &sweep(-100.0, 88.0, 200_000), 1);
        let pos = sweep(1e-6, 1e6, 200_000);
        check("ln", ln, f32::ln, &pos, 1);
        check("ln small", ln, f32::ln, &sweep(1e-38, 1e-30, 10_000), 1);
        check("log2", log2, f32::log2, &pos, 1);
        check("pow", |x| pow(x, 2.5), |x| x.powf(2.5), &sweep(0.0, 100.0, 100_000), 2);
        // Reference in f64: LLVM constant-folds std's f32 powi differently in release builds (2 ulps
        // apart on this sweep), which is one reason sim code uses this module instead.
        check("powi", |x| powi(x, -3), |x| (x as f64).powi(-3) as f32, &sweep(0.1, 10.0, 10_000), 1);
        for &y in &[-3.0f32, -0.5, 0.0, 0.25, 7.0] {
            check("atan2", |x| atan2(y, x), |x| y.atan2(x), &sweep(-20.0, 20.0, 40_000), 1);
        }
    }

    /// Pinned output bits over fixed sweeps: equal in debug and release and on every target (P2-1).
    /// A change here is a determinism bug, not a value to update.
    #[test]
    fn golden_bits() {
        let mut h = crate::hash::Hasher::new(0);
        let mut put = |v: f32| h.write_u32(v.to_bits());
        for x in sweep(-100.0, 100.0, 20_000) {
            let (s, c) = sin_cos(x);
            put(s);
            put(c);
            put(tan(x * 0.01));
            put(atan(x));
            put(atan2(x, 3.0 - x * 0.5));
            put(asin(x * 0.01));
            put(acos(x * 0.01));
            put(exp(x * 0.8));
            put(ln(x.abs() + 1e-3));
            put(log2(x.abs() * 7.0 + 1e-3));
            put(pow(x.abs(), 1.7));
            put(powi(x, 5));
        }
        for i in 0..20_000u32 {
            let x = f32::from_bits(0x4880_0000 + i * 0xB439);
            let (s, c) = sin_cos(x);
            put(s);
            put(c);
            put(tan(-x));
        }
        let got = h.finish();
        println!("RESULT dmath golden={got:#018x}");
        assert_eq!(got, GOLDEN_DMATH);
    }

    const GOLDEN_DMATH: u64 = 0xf17f26f98ad6b88c;

    #[test]
    fn special_values() {
        assert_eq!(atan2(0.0, 0.0).to_bits(), 0.0f32.to_bits());
        assert_eq!(atan2(-0.0, 0.0).to_bits(), (-0.0f32).to_bits());
        assert_eq!(atan2(0.0, -0.0), core::f32::consts::PI);
        assert_eq!(atan2(-0.0, -1.0), -core::f32::consts::PI);
        assert_eq!(atan2(1.0, 0.0), core::f32::consts::FRAC_PI_2);
        assert_eq!(atan2(f32::INFINITY, f32::INFINITY), core::f32::consts::FRAC_PI_4);
        assert_eq!(asin(1.0000001), core::f32::consts::FRAC_PI_2);
        assert_eq!(acos(-1.0000001), core::f32::consts::PI);
        assert_eq!(ln(0.0), f32::NEG_INFINITY);
        assert!(ln(-1.0).is_nan());
        assert_eq!(exp(100.0), f32::INFINITY);
        assert_eq!(exp(-200.0), 0.0);
        assert_eq!(exp(0.0), 1.0);
        assert_eq!(ln(1.0), 0.0);
        assert_eq!(sin(0.0), 0.0);
        assert_eq!(cos(0.0), 1.0);
        assert!(sin(f32::INFINITY).is_nan());
        assert_eq!(pow(0.0, 2.0), 0.0);
        assert_eq!(pow(1.0, f32::INFINITY), 1.0);
        assert_eq!(pow(1.0, f32::NAN), 1.0);
        assert_eq!(pow(f32::NAN, 0.0), 1.0);
        assert_eq!(powi(-2.0, 3), -8.0);
    }
}

//! R16 spike: hierarchical chunks with f64 coordinates, per-region physics worlds, scale bands
//! (per-region length units) and the spatial-index study. See `docs/r16-chunks-results.md`.

pub mod coords;
pub mod phys;
pub mod spatial;

/// Wall-clock milliseconds taken by `f`.
pub fn time_ms<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = std::time::Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64() * 1e3)
}

/// SplitMix64: deterministic scene generation.
pub fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Uniform in [0, 1) from a 64-bit hash (53 bits; exact).
pub fn unit_f64(h: u64) -> f64 {
    (h >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
}

/// Log-uniform in [lo, hi]. Uses `ln`/`exp`, so scene setup only, never sim code.
pub fn log_uniform(h: u64, lo: f64, hi: f64) -> f64 {
    (lo.ln() + unit_f64(h) * (hi.ln() - lo.ln())).exp()
}

pub fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

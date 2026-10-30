//! Counter-based RNG streams (design 4.3 rule 2: randomness comes from the session seed plus the frame
//! plus explicit streams).
//!
//! A stream is a key derived from `(session seed, stream id)`, and its state is one counter: draw `n`
//! is `mix(key + (n + 1) * GAMMA)`, which is SplitMix64. So a stream's whole state is
//! [`Rng::position`], which is what a logic module snapshots (7.4), and any draw can be recomputed
//! without replaying the ones before it.

use bytemuck::{Pod, Zeroable};

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// The SplitMix64 output function: a bijective 64-bit mixer.
#[inline]
pub fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One SplitMix64 step from a seed (the spike's `splitmix64`).
#[inline]
pub fn splitmix64(x: u64) -> u64 {
    mix64(x.wrapping_add(GAMMA))
}

/// A deterministic random stream. 16 bytes, plain data: snapshot it as is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct Rng {
    key: u64,
    pos: u64,
}

impl Rng {
    /// Stream `stream` of the session seeded with `seed`.
    pub fn stream(seed: u64, stream: u64) -> Rng {
        Rng { key: mix64(seed ^ mix64(stream.wrapping_add(GAMMA))), pos: 0 }
    }

    /// A stream used only within one frame: `(seed, stream, frame)`. Systems that draw a fresh stream each
    /// frame need no RNG state in the snapshot at all.
    pub fn for_frame(seed: u64, stream: u64, frame: u64) -> Rng {
        Rng::stream(seed ^ mix64(frame ^ 0xF4A3_5C1B_D6E2_9087), stream)
    }

    /// A child stream, derived deterministically (for example one per object: `rng.fork(object.to_u64())`).
    pub fn fork(&self, tag: u64) -> Rng {
        Rng { key: mix64(self.key ^ mix64(tag ^ 0x5851_F42D_4C95_7F2D)), pos: 0 }
    }

    /// Draws taken so far.
    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn seek(&mut self, pos: u64) {
        self.pos = pos;
    }

    pub fn next_u64(&mut self) -> u64 {
        self.pos += 1;
        mix64(self.key.wrapping_add(self.pos.wrapping_mul(GAMMA)))
    }

    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Uniform in [0, 1), on a grid of 2^-24: exactly representable, so no rounding.
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 * (1.0 / 16_777_216.0)
    }

    /// Uniform in [0, 1), on a grid of 2^-53.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0)
    }

    /// Uniform in [lo, hi). `lo + (hi - lo) * u`, so the same bits everywhere.
    pub fn range_f32(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next_f32()
    }

    /// Uniform integer in [0, n), unbiased (Lemire's multiply-and-reject). `n` must be > 0.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "Rng::below(0)");
        let threshold = n.wrapping_neg() % n;
        loop {
            let m = self.next_u64() as u128 * n as u128;
            if (m as u64) >= threshold {
                return (m >> 64) as u64;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_matches_reference() {
        // SplitMix64 seeded with 0: the first output, from the reference implementation.
        assert_eq!(splitmix64(0), 0xE220_A839_7B1D_CDAF);
        let mut r = Rng { key: 0, pos: 0 };
        assert_eq!(r.next_u64(), 0xE220_A839_7B1D_CDAF);
    }

    #[test]
    fn seek_and_streams() {
        let mut a = Rng::stream(42, 7);
        let draws: Vec<u64> = (0..100).map(|_| a.next_u64()).collect();
        let mut b = Rng::stream(42, 7);
        b.seek(50);
        assert_eq!(b.next_u64(), draws[50]);
        assert_ne!(Rng::stream(42, 8).next_u64(), draws[0]);
        assert_ne!(Rng::for_frame(42, 7, 1).next_u64(), Rng::for_frame(42, 7, 2).next_u64());
        let mut c = Rng::stream(1, 1);
        for _ in 0..10_000 {
            let f = c.next_f32();
            assert!((0.0..1.0).contains(&f));
            assert!(c.below(10) < 10);
        }
    }
}

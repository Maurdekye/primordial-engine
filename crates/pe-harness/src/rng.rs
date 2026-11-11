//! Deterministic draws (DESIGN.md 2 and 3.1).
//!
//! The old harness drew every random number from one stream, in send order, so one extra message moved every later
//! draw. Here a draw is a pure function of (seed, stream, index, purpose). A stream is, for example, one directed
//! link, and an index is the message's number on that link. A change on one link moves nothing on another.
//!
//! Probabilities are integers in parts per million, so no floating point decides an outcome.

/// What a draw is for. Distinct purposes give independent draws for the same message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Purpose {
    Loss = 1,
    Jitter = 2,
    Duplicate = 3,
    DuplicateJitter = 4,
    BurstState = 5,
    Schedule = 6,
}

/// SplitMix64's finaliser: a bijection on u64 with good avalanche.
const fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One draw: a pure function of its four inputs.
pub const fn draw(seed: u64, stream: u64, index: u64, purpose: Purpose) -> u64 {
    mix(mix(mix(mix(seed) ^ stream) ^ index) ^ (purpose as u64))
}

/// A value in `0..n` from a draw, without modulo bias worth caring about (Lemire's multiply-shift). `n = 0` gives 0.
pub const fn below(x: u64, n: u64) -> u64 {
    ((x as u128 * n as u128) >> 64) as u64
}

/// True with probability `ppm` / 1,000,000.
pub const fn chance(x: u64, ppm: u32) -> bool {
    below(x, 1_000_000) < ppm as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_are_pure_and_separate_by_every_input() {
        let a = draw(7, 1, 2, Purpose::Loss);
        assert_eq!(a, draw(7, 1, 2, Purpose::Loss));
        assert_ne!(a, draw(8, 1, 2, Purpose::Loss));
        assert_ne!(a, draw(7, 2, 2, Purpose::Loss));
        assert_ne!(a, draw(7, 1, 3, Purpose::Loss));
        assert_ne!(a, draw(7, 1, 2, Purpose::Jitter));
    }

    #[test]
    fn chance_is_close_to_its_rate() {
        let n = 200_000u64;
        let hits = (0..n).filter(|&i| chance(draw(1, 9, i, Purpose::Loss), 10_000)).count() as u64;
        // 1% of 200,000 = 2,000; the standard deviation is about 44.
        assert!((1_800..=2_200).contains(&hits), "hits {hits}");
        assert!(!chance(u64::MAX, 0));
        assert!(chance(0, 1));
        assert!(chance(u64::MAX, 1_000_000));
    }

    #[test]
    fn below_stays_in_range() {
        for i in 0..10_000 {
            assert!(below(draw(3, 0, i, Purpose::Jitter), 17) < 17);
        }
        assert_eq!(below(u64::MAX, 0), 0);
    }
}

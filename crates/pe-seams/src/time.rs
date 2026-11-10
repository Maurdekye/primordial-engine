//! Time at the seams.
//!
//! - [`HostTime`] is this machine's monotonic clock, in nanoseconds (xr.md 8.1, transport.md 5.1). It is never
//!   adjusted, and it is the only clock a sans-IO core is given: `tick(HostTime)` (S3, S4).
//! - The session frame comes from pe-net's clock model through [`crate::FrameClock`] (transport.md 5.7).
//! - **Every limit is a duration, never a frame count** (the user, md4b42d16d9df): the simulation rate is a world
//!   setting, 60 Hz by default, within a supported range, and it is independent of the display rate (m8ac62c1872ac).
//!   Limits are [`core::time::Duration`]s, which are exact integers. A bound that every machine must decide alike is
//!   turned into frames by [`frames_ceil`] at the rate in effect at the frame where the bound starts, and that rate is
//!   recorded with any stored bound (divergence.md 14).

use core::time::Duration;

/// Monotonic host nanoseconds (QPC on Windows, `CLOCK_MONOTONIC` elsewhere), shared by pe-xr, pe-render, pe-net and
/// the simulation thread. Never adjusted; meaningful on this machine only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostTime(pub i64);

impl HostTime {
    pub const fn from_millis(ms: i64) -> Self {
        HostTime(ms * 1_000_000)
    }

    /// `self - earlier`, in seconds.
    pub fn secs_since(self, earlier: HostTime) -> f64 {
        (self.0 - earlier.0) as f64 * 1e-9
    }

    pub fn plus_nanos(self, ns: i64) -> Self {
        HostTime(self.0 + ns)
    }

    /// `self + d`, saturating at the end of the clock.
    pub fn plus(self, d: Duration) -> Self {
        HostTime(self.0.saturating_add(i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)))
    }
}

/// The number of whole frames that cover `d` at `hz` frames per second, rounded up: the frame count of a bound.
/// Exact integer arithmetic, so every machine gets the same answer. `hz` 0 gives 0.
pub fn frames_ceil(d: Duration, hz: u32) -> u64 {
    let n = d.as_nanos() * hz as u128;
    let f = n.div_ceil(1_000_000_000);
    u64::try_from(f).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_round_up_and_scale_with_the_rate() {
        assert_eq!(frames_ceil(Duration::from_secs(60), 60), 3_600);
        assert_eq!(frames_ceil(Duration::from_secs(60), 90), 5_400);
        assert_eq!(frames_ceil(Duration::from_millis(150), 60), 9);
        assert_eq!(frames_ceil(Duration::from_millis(10), 60), 1);
        assert_eq!(frames_ceil(Duration::ZERO, 60), 0);
        assert_eq!(frames_ceil(Duration::from_secs(1), 0), 0);
    }

    #[test]
    fn host_time_arithmetic() {
        let t = HostTime::from_millis(5);
        assert_eq!(t.plus(Duration::from_millis(2)), HostTime(7_000_000));
        assert!((t.plus_nanos(500_000_000).secs_since(t) - 0.5).abs() < 1e-12);
        assert_eq!(HostTime(i64::MAX - 1).plus(Duration::from_secs(1)), HostTime(i64::MAX));
    }
}

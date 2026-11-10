//! Time at the seams.
//!
//! - [`HostTime`] is this machine's monotonic clock, in nanoseconds (xr.md 8.1, transport.md 5.1). It is never
//!   adjusted, and it is the only clock a sans-IO core is given: `tick(HostTime)` (S3, S4).
//! - The session frame comes from pe-net's clock model through [`crate::FrameClock`] (transport.md 5.7).
//! - **Every limit is a duration, never a frame count** (the user, md4b42d16d9df): the simulation rate is a world
//!   setting, [`SimRate`], 30 to 120 Hz in whole hertz and 60 by default (mc8dc9e5a6ffb), and it is independent of
//!   the display rate (m8ac62c1872ac).
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

/// A world's simulation rate, in whole hertz within the supported range: 30 to 120 Hz, 60 by default (the user,
/// mc8dc9e5a6ffb, after md4b42d16d9df). It is fixed apart from the display rate (m8ac62c1872ac). pe-physics runs
/// `ceil(240 / rate)` sub-steps per frame, so contacts keep their stiffness at every rate (replica.md 13).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SimRate(u32);

impl SimRate {
    pub const MIN_HZ: u32 = 30;
    pub const MAX_HZ: u32 = 120;
    pub const DEFAULT: SimRate = SimRate(60);

    /// `None` outside 30..=120 Hz.
    pub fn new(hz: u32) -> Option<SimRate> {
        (Self::MIN_HZ..=Self::MAX_HZ).contains(&hz).then_some(SimRate(hz))
    }

    pub fn hz(self) -> u32 {
        self.0
    }

    /// The frames that cover `d` at this rate, rounded up ([`frames_ceil`]).
    pub fn frames(self, d: Duration) -> u64 {
        frames_ceil(d, self.0)
    }
}

impl Default for SimRate {
    fn default() -> Self {
        SimRate::DEFAULT
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
    fn the_rate_range_is_the_users() {
        assert_eq!(SimRate::new(29), None);
        assert_eq!(SimRate::new(121), None);
        assert_eq!(SimRate::new(30).map(SimRate::hz), Some(30));
        assert_eq!(SimRate::new(120).map(SimRate::hz), Some(120));
        assert_eq!(SimRate::default().hz(), 60);
        assert_eq!(SimRate::DEFAULT.frames(Duration::from_secs(60)), 3_600);
        assert_eq!(SimRate::new(120).unwrap().frames(Duration::from_secs(60)), 7_200);
    }

    #[test]
    fn host_time_arithmetic() {
        let t = HostTime::from_millis(5);
        assert_eq!(t.plus(Duration::from_millis(2)), HostTime(7_000_000));
        assert!((t.plus_nanos(500_000_000).secs_since(t) - 0.5).abs() < 1e-12);
        assert_eq!(HostTime(i64::MAX - 1).plus(Duration::from_secs(1)), HostTime(i64::MAX));
    }
}

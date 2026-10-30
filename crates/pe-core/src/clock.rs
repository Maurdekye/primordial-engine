//! The fixed-step clock (design 5) and the slewed session clock (design 5, 10.4).
//!
//! * [`TickRate`] is a world's sim rate in whole hertz (D4: 90 Hz target, 60 Hz fallback, per world).
//!   Frame boundaries are computed in integer microseconds, so 90 Hz (an 11,111.1 us frame) has no
//!   rounding drift.
//! * [`FrameClock`] maps session time to the frame the sim should have reached:
//!   `frame = floor((session_us - epoch_us) * hz / 1e6)`.
//! * [`SessionClock`] maps the local monotonic clock to session time through an offset that slews toward
//!   the clock-sync estimate by at most `max_slew_ppm` of elapsed time, and never runs backwards. It is
//!   loop machinery, not sim state.
//!
//! All arithmetic saturates, so extreme inputs behave the same in debug and release builds (review P21-8).

use crate::ids::Frame;

/// A sim tick rate in hertz, 1 to 1,000.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TickRate {
    hz: u32,
}

impl TickRate {
    pub const HZ60: TickRate = TickRate { hz: 60 };
    pub const HZ90: TickRate = TickRate { hz: 90 };

    pub fn new(hz: u32) -> Option<TickRate> {
        (1..=1000).contains(&hz).then_some(TickRate { hz })
    }

    pub fn hz(self) -> u32 {
        self.hz
    }

    /// The step length passed to physics: `1 / hz` rounded once to `f32` (IEEE division, so the same bits
    /// everywhere).
    pub fn dt_f32(self) -> f32 {
        1.0f32 / self.hz as f32
    }

    pub fn dt_f64(self) -> f64 {
        1.0f64 / self.hz as f64
    }

    /// Whole frames elapsed after `us` microseconds (floor). Negative input gives `None`.
    pub fn frames_in(self, us: i64) -> Option<u64> {
        (us >= 0).then(|| ((us as u128 * self.hz as u128) / 1_000_000) as u64)
    }

    /// The first microsecond (from the epoch) at which `frames_in` reaches `f`: `ceil(f * 1e6 / hz)`.
    pub fn start_us(self, f: Frame) -> i64 {
        let n = f.0 as u128 * 1_000_000;
        i64::try_from(n.div_ceil(self.hz as u128)).unwrap_or(i64::MAX)
    }
}

/// Session time to frames for one world: its epoch (session microseconds at frame 0) and tick rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameClock {
    pub epoch_us: i64,
    pub rate: TickRate,
}

impl FrameClock {
    pub fn new(epoch_us: i64, rate: TickRate) -> Self {
        FrameClock { epoch_us, rate }
    }

    /// The present frame at session time `session_us`: the sim has stepped every frame before it. Before
    /// the epoch the present is frame 0.
    pub fn present(&self, session_us: i64) -> Frame {
        Frame(self.rate.frames_in(session_us.saturating_sub(self.epoch_us)).unwrap_or(0))
    }

    /// How many frames a sim whose state is at `state` must step to reach the present (0 if it is ahead).
    pub fn due(&self, state: Frame, session_us: i64) -> u64 {
        self.present(session_us).0.saturating_sub(state.0)
    }

    /// Session time at which frame `f` becomes the present.
    pub fn start_us(&self, f: Frame) -> i64 {
        self.epoch_us.saturating_add(self.rate.start_us(f))
    }
}

/// Local monotonic time to session time, slewed (design 5: at most 1% toward the estimate).
#[derive(Clone, Debug)]
pub struct SessionClock {
    offset_us: i64,
    target_us: i64,
    max_slew_ppm: u32,
    last_local_us: i64,
    last_session_us: i64,
}

impl SessionClock {
    /// 1%, design 5.
    pub const DEFAULT_SLEW_PPM: u32 = 10_000;

    /// Starts at `offset_us` exactly: the first sync before joining is a step, later ones slew.
    pub fn new(local_now_us: i64, offset_us: i64) -> Self {
        SessionClock {
            offset_us,
            target_us: offset_us,
            max_slew_ppm: Self::DEFAULT_SLEW_PPM,
            last_local_us: local_now_us,
            last_session_us: local_now_us.saturating_add(offset_us),
        }
    }

    pub fn with_slew_ppm(mut self, ppm: u32) -> Self {
        self.max_slew_ppm = ppm;
        self
    }

    /// A new estimate of `session - local` from clock sync. The applied offset moves toward it gradually.
    pub fn set_target(&mut self, target_offset_us: i64) {
        self.target_us = target_offset_us;
    }

    pub fn offset_us(&self) -> i64 {
        self.offset_us
    }

    /// Session time at local monotonic time `local_now_us`. Never decreases between calls.
    pub fn now(&mut self, local_now_us: i64) -> i64 {
        let dt = local_now_us.saturating_sub(self.last_local_us).max(0) as i128;
        let max_step = (dt * self.max_slew_ppm as i128 / 1_000_000).min(i64::MAX as i128) as i64;
        let gap = self.target_us.saturating_sub(self.offset_us);
        self.offset_us = self.offset_us.saturating_add(gap.clamp(-max_step, max_step));
        self.last_local_us = self.last_local_us.max(local_now_us);
        let session = local_now_us.saturating_add(self.offset_us).max(self.last_session_us);
        self.last_session_us = session;
        session
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_boundaries_90hz_exact() {
        let r = TickRate::HZ90;
        // 90 frames in exactly one second, 900 in ten: no drift from the 11,111.1 us frame.
        assert_eq!(r.frames_in(1_000_000), Some(90));
        assert_eq!(r.frames_in(999_999), Some(89));
        assert_eq!(r.frames_in(10_000_000), Some(900));
        for f in [0u64, 1, 2, 89, 90, 91, 123_456_789] {
            let s = r.start_us(Frame(f));
            assert_eq!(r.frames_in(s), Some(f));
            if s > 0 {
                assert_eq!(r.frames_in(s - 1), Some(f - 1));
            }
        }
        assert_eq!(r.frames_in(-1), None);
        assert_eq!(TickRate::HZ60.dt_f32().to_bits(), (1.0f32 / 60.0).to_bits());
        assert!(TickRate::new(0).is_none() && TickRate::new(1001).is_none());
    }

    #[test]
    fn frame_clock_due() {
        let c = FrameClock::new(5_000_000, TickRate::HZ60);
        assert_eq!(c.present(4_000_000), Frame(0));
        assert_eq!(c.present(6_000_000), Frame(60));
        assert_eq!(c.due(Frame(58), 6_000_000), 2);
        assert_eq!(c.due(Frame(61), 6_000_000), 0);
        assert_eq!(c.present(c.start_us(Frame(1234))), Frame(1234));
    }

    #[test]
    fn session_clock_slews_and_is_monotonic() {
        let mut c = SessionClock::new(0, 1_000);
        assert_eq!(c.now(0), 1_000);
        // Target 10 ms behind: at 1% the offset can move 10 us per ms of local time.
        c.set_target(-9_000);
        assert_eq!(c.now(1_000), 1_000 + 1_000 - 10);
        let mut last = 0;
        for t in (2_000..2_000_000).step_by(1_000) {
            let s = c.now(t);
            assert!(s >= last, "session time went backwards");
            last = s;
        }
        assert_eq!(c.offset_us(), -9_000, "slew reaches the target");
        // A local clock that repeats a value does not move session time backwards.
        let a = c.now(2_000_000);
        let b = c.now(1_999_000);
        assert!(b >= a);
    }
}

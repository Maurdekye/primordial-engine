//! The host clock, and the frame clock of a world run alone (app.md 2).
//!
//! **The host clock** is monotonic host nanoseconds from the operating system's performance counter: QPC on Windows,
//! `CLOCK_MONOTONIC` elsewhere, the base [`HostTime`] names. pe-app reads it here and hands the same function to
//! pe-render (`DesktopConfig::clock`), pe-net's runner, pe-xr and the simulation thread, so every time is on one base.
//! It is the counter itself, not the time since the process started, because OpenXR converts its `XrTime` to and from
//! exactly these counters: it is the same reading as pe-xr's own clock (xr.md 6.1).

use std::sync::Arc;

use pe_seams::{Frame, FrameClock, HostTime, SimRate};

/// The host clock as every thread takes it.
pub type HostClock = Arc<dyn Fn() -> HostTime + Send + Sync>;

/// The host clock now.
pub fn now() -> HostTime {
    os::now()
}

/// The host clock, to hand to the other threads.
pub fn shared() -> HostClock {
    Arc::new(now)
}

#[cfg(windows)]
mod os {
    use std::sync::OnceLock;

    use pe_seams::HostTime;

    unsafe extern "system" {
        fn QueryPerformanceCounter(count: *mut i64) -> i32;
        fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
    }

    fn frequency() -> i64 {
        static FREQ: OnceLock<i64> = OnceLock::new();
        *FREQ.get_or_init(|| {
            let mut f = 0i64;
            // SAFETY: a valid out pointer. The frequency is fixed at boot, and the call cannot fail on Windows XP or
            // later.
            unsafe { QueryPerformanceFrequency(&mut f) };
            f.max(1)
        })
    }

    pub fn now() -> HostTime {
        let mut c = 0i64;
        // SAFETY: a valid out pointer; QPC cannot fail on Windows XP or later.
        unsafe { QueryPerformanceCounter(&mut c) };
        // Through i128, so no counter a machine can reach overflows: pe-xr's conversion.
        HostTime((i128::from(c) * 1_000_000_000 / i128::from(frequency())) as i64)
    }
}

#[cfg(unix)]
mod os {
    use pe_seams::HostTime;

    pub fn now() -> HostTime {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: a valid out pointer; CLOCK_MONOTONIC is always available.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        HostTime(i64::from(ts.tv_sec) * 1_000_000_000 + i64::from(ts.tv_nsec))
    }
}

/// The frame clock of a world run on this machine alone: frame 0 starts at `epoch`, and the frames follow at the
/// world's rate. In a session the frame clock is pe-net's clock model instead (transport.md 5.7).
#[derive(Clone, Copy, Debug)]
pub struct LocalClock {
    epoch: HostTime,
    rate: SimRate,
}

impl LocalClock {
    pub fn new(epoch: HostTime, rate: SimRate) -> LocalClock {
        LocalClock { epoch, rate }
    }
}

impl FrameClock for LocalClock {
    fn rate_hz(&self) -> u32 {
        self.rate.hz()
    }

    fn frame_at(&self, now: HostTime) -> Option<Frame> {
        let since = i128::from(now.0) - i128::from(self.epoch.0);
        (since >= 0).then(|| Frame((since * i128::from(self.rate.hz()) / 1_000_000_000) as u64))
    }

    /// The first nanosecond of `frame`: the earliest host time at which [`LocalClock::frame_at`] reaches it.
    fn host_time_of(&self, frame: Frame) -> HostTime {
        let hz = i128::from(self.rate.hz());
        let since = (i128::from(frame.0) * 1_000_000_000 + hz - 1) / hz;
        HostTime(i64::try_from(i128::from(self.epoch.0) + since).unwrap_or(i64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_clock_moves_forward() {
        let a = now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = now();
        assert!(b.0 - a.0 >= 1_000_000, "{a:?} -> {b:?}");
    }

    #[test]
    fn a_frame_starts_where_the_clock_reaches_it() {
        for hz in [30, 59, 60, 90, 120] {
            let c = LocalClock::new(HostTime(1_234_567), SimRate::new(hz).unwrap());
            assert_eq!(c.frame_at(HostTime(1_234_566)), None, "{hz} Hz: before frame 0");
            assert_eq!(c.frame_at(HostTime(1_234_567)), Some(Frame(0)));
            for f in 0..2_000u64 {
                let t = c.host_time_of(Frame(f));
                assert_eq!(c.frame_at(t), Some(Frame(f)), "{hz} Hz, frame {f}");
                if f > 0 {
                    assert_eq!(c.frame_at(HostTime(t.0 - 1)), Some(Frame(f - 1)), "{hz} Hz, frame {f}");
                }
            }
        }
    }
}

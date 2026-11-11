//! The host clock (spec 6.1): monotonic nanoseconds from the operating system's performance counter,
//! QPC on Windows and `CLOCK_MONOTONIC` on Linux. OpenXR converts its `XrTime` to and from exactly these
//! counters (`XR_KHR_win32_convert_performance_counter_time`, `XR_KHR_convert_timespec_time`), so the
//! renderer, the simulation and the headset share one time base.

use crate::seam::HostTime;

#[cfg(windows)]
mod os {
    unsafe extern "system" {
        fn QueryPerformanceCounter(count: *mut i64) -> i32;
        fn QueryPerformanceFrequency(freq: *mut i64) -> i32;
    }

    pub fn counter() -> i64 {
        let mut c = 0i64;
        // SAFETY: a valid out pointer; QPC cannot fail on Windows XP and later.
        unsafe { QueryPerformanceCounter(&mut c) };
        c
    }

    pub fn frequency() -> i64 {
        static FREQ: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
        *FREQ.get_or_init(|| {
            let mut f = 0i64;
            // SAFETY: a valid out pointer; the frequency is fixed at boot.
            unsafe { QueryPerformanceFrequency(&mut f) };
            f.max(1)
        })
    }
}

/// QPC ticks to host nanoseconds, without overflow for any counter a machine can reach.
#[cfg(windows)]
pub fn counter_to_host(counter: i64) -> HostTime {
    let f = os::frequency() as i128;
    HostTime(((counter as i128 * 1_000_000_000) / f) as i64)
}

/// Host nanoseconds to QPC ticks (rounded down), for `xrConvertWin32PerformanceCounterToTimeKHR`.
#[cfg(windows)]
pub fn host_to_counter(t: HostTime) -> i64 {
    let f = os::frequency() as i128;
    ((t.0 as i128 * f) / 1_000_000_000) as i64
}

/// The host clock now.
#[cfg(windows)]
pub fn now() -> HostTime {
    counter_to_host(os::counter())
}

/// `CLOCK_MONOTONIC` as a `timespec`, to host nanoseconds.
#[cfg(unix)]
pub fn timespec_to_host(ts: &libc::timespec) -> HostTime {
    HostTime(ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64)
}

/// Host nanoseconds to a `CLOCK_MONOTONIC` `timespec`, for `xrConvertTimespecTimeToTimeKHR`.
#[cfg(unix)]
pub fn host_to_timespec(t: HostTime) -> libc::timespec {
    libc::timespec { tv_sec: t.0.div_euclid(1_000_000_000) as _, tv_nsec: t.0.rem_euclid(1_000_000_000) as _ }
}

#[cfg(unix)]
pub fn now() -> HostTime {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid out pointer; CLOCK_MONOTONIC is always available on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    timespec_to_host(&ts)
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

    #[cfg(windows)]
    #[test]
    fn counter_round_trips_within_one_tick() {
        let c = os::counter();
        let back = host_to_counter(counter_to_host(c));
        assert!((c - back).abs() <= 1, "{c} -> {back}");
    }

    #[cfg(unix)]
    #[test]
    fn timespec_round_trips_exactly() {
        let t = HostTime(12_345_678_901_234);
        assert_eq!(timespec_to_host(&host_to_timespec(t)), t);
    }
}

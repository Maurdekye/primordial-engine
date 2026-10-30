// Primordial Engine fork: wall-clock stage timer that fills `world::Profile`, like C's
// b3GetTicks / b3GetMillisecondsAndReset. Only active with the `stage-profile` feature; without
// it the timer is a zero-sized no-op and every profile field stays 0. Timings never feed back
// into the simulation.
//
// SPDX-License-Identifier: MIT

#[derive(Clone, Copy)]
pub(crate) struct Ticks {
    #[cfg(feature = "stage-profile")]
    start: std::time::Instant,
}

impl Ticks {
    #[inline]
    pub(crate) fn now() -> Ticks {
        Ticks {
            #[cfg(feature = "stage-profile")]
            start: std::time::Instant::now(),
        }
    }

    /// Milliseconds since `now` or the previous `lap`, then restart. (b3GetMillisecondsAndReset)
    #[inline]
    pub(crate) fn lap(&mut self) -> f32 {
        #[cfg(feature = "stage-profile")]
        {
            let now = std::time::Instant::now();
            let ms = (now - self.start).as_secs_f64() * 1e3;
            self.start = now;
            ms as f32
        }
        #[cfg(not(feature = "stage-profile"))]
        {
            0.0
        }
    }
}

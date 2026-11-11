//! The per-principal token bucket (spec 9.3; file 06, the D38 re-run): `tx_per_s` refill with a burst of
//! `burst`. Time is passed in, so tests are deterministic.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate {
    pub per_s: f64,
    pub burst: f64,
}

impl Rate {
    pub const TX_DEFAULT: Rate = Rate { per_s: 20.0, burst: 40.0 };
}

#[derive(Clone, Debug)]
pub struct Bucket {
    rate: Rate,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    pub fn new(rate: Rate, now: Instant) -> Self {
        assert!(rate.per_s > 0.0 && rate.burst >= 1.0);
        Bucket { rate, tokens: rate.burst, at: now }
    }

    fn refill(&mut self, now: Instant) {
        // A clock that goes backwards adds nothing.
        let dt = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + dt * self.rate.per_s).min(self.rate.burst);
        if now > self.at {
            self.at = now;
        }
    }

    /// Takes one token, or says how long until one is free.
    pub fn take(&mut self, now: Instant) -> Result<(), Duration> {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - self.tokens) / self.rate.per_s;
            Err(Duration::from_secs_f64(wait).max(Duration::from_millis(1)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_then_the_rate() {
        let t0 = Instant::now();
        let mut b = Bucket::new(Rate { per_s: 20.0, burst: 40.0 }, t0);
        for _ in 0..40 {
            assert!(b.take(t0).is_ok());
        }
        let wait = b.take(t0).unwrap_err();
        assert!(wait <= Duration::from_millis(51) && wait >= Duration::from_millis(49), "{wait:?}");
        // After 0.5 s, ten more.
        let t1 = t0 + Duration::from_millis(500);
        for _ in 0..10 {
            assert!(b.take(t1).is_ok());
        }
        assert!(b.take(t1).is_err());
    }

    #[test]
    fn refill_is_capped_at_the_burst() {
        let t0 = Instant::now();
        let mut b = Bucket::new(Rate { per_s: 20.0, burst: 4.0 }, t0);
        let t1 = t0 + Duration::from_secs(3600);
        for _ in 0..4 {
            assert!(b.take(t1).is_ok());
        }
        assert!(b.take(t1).is_err());
    }

    #[test]
    fn a_clock_going_backwards_adds_nothing() {
        let t0 = Instant::now() + Duration::from_secs(10);
        let mut b = Bucket::new(Rate { per_s: 1.0, burst: 1.0 }, t0);
        assert!(b.take(t0).is_ok());
        assert!(b.take(t0 - Duration::from_secs(5)).is_err());
        assert!(b.take(t0 + Duration::from_millis(999)).is_err());
        assert!(b.take(t0 + Duration::from_millis(1001)).is_ok());
    }
}

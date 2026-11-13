//! The rate budget v0 of one link (transport.md 4), sans-IO: what the Core may hand quinn, from what a real
//! endpoint observes (quinn's `cwnd` and RTTs, 4.1).
//!
//! - **The budget** is `cwnd / srtt` bytes per second (4.2).
//! - **The delay guard** (4.3): Repair and Bulk send only while `srtt - base_rtt` is under `D_guard`; `base_rtt`
//!   is the least RTT seen over the last `W_base`.
//! - **The ride gate** (4.2): redundant rides are on only while the delay guard holds, with a band (off at
//!   `D_guard`, back on below `D_guard / 2`), and the budget is at least `K_redundant` times the reliable send
//!   rate: the wire bytes of Protocol and Action messages first sent, over a one-second window that slides.

use std::collections::VecDeque;
use std::time::Duration;

use pe_seams::HostTime;

/// transport.md 10.
#[derive(Clone, Debug)]
pub struct BudgetParams {
    /// `D_guard`.
    pub d_guard: Duration,
    /// `W_base`.
    pub w_base: Duration,
    /// `K_redundant`.
    pub k_redundant: u64,
}

impl Default for BudgetParams {
    fn default() -> Self {
        Self { d_guard: Duration::from_millis(25), w_base: Duration::from_secs(30), k_redundant: 4 }
    }
}

const RATE_WINDOW_NS: i64 = 1_000_000_000;
/// `base_rtt` keeps the least sample per bucket of `w_base / BASE_BUCKETS`.
const BASE_BUCKETS: i64 = 30;

#[derive(Debug)]
pub struct Budget {
    p: BudgetParams,
    srtt: Duration,
    cwnd: u64,
    /// (bucket start, least RTT in it), oldest first.
    base: VecDeque<(i64, Duration)>,
    /// First sends of Protocol and Action: (when, wire bytes), oldest first, within the last second.
    first_sends: VecDeque<(i64, u64)>,
    first_sent_bytes: u64,
    rides: bool,
}

impl Budget {
    pub fn new(p: BudgetParams) -> Self {
        Self {
            p,
            srtt: Duration::ZERO,
            cwnd: 0,
            base: VecDeque::new(),
            first_sends: VecDeque::new(),
            first_sent_bytes: 0,
            rides: true,
        }
    }

    /// quinn's path statistics, read at each wake-up (4.1).
    pub fn observe(&mut self, now: HostTime, srtt: Duration, latest_rtt: Duration, cwnd: u64) {
        self.srtt = srtt;
        self.cwnd = cwnd;
        let bucket = (self.p.w_base.as_nanos() as i64 / BASE_BUCKETS).max(1);
        let b = now.0.div_euclid(bucket) * bucket;
        if !latest_rtt.is_zero() {
            match self.base.back_mut() {
                Some((s, m)) if *s == b => *m = (*m).min(latest_rtt),
                _ => self.base.push_back((b, latest_rtt)),
            }
        }
        let horizon = now.0 - self.p.w_base.as_nanos() as i64;
        while self.base.front().is_some_and(|(s, _)| *s + bucket <= horizon) {
            self.base.pop_front();
        }
        self.update_rides();
    }

    /// The budget in bytes per second (4.2): `cwnd / srtt`; 0 before the first RTT.
    pub fn bps(&self) -> u64 {
        // A loopback path can measure an RTT near zero: floor it at 100 us so the budget stays finite.
        let ns = (self.srtt.as_nanos() as u64).max(100_000);
        if self.cwnd == 0 { 0 } else { (u128::from(self.cwnd) * 1_000_000_000 / u128::from(ns)) as u64 }
    }

    /// The least RTT over the last `W_base`.
    pub fn base_rtt(&self) -> Option<Duration> {
        self.base.iter().map(|(_, m)| *m).min()
    }

    /// `srtt - base_rtt`: the queue this link's packets wait in.
    pub fn queueing_delay(&self) -> Duration {
        self.base_rtt().map_or(Duration::ZERO, |b| self.srtt.saturating_sub(b))
    }

    /// The delay guard (4.3): may Repair and Bulk send now?
    pub fn low_lanes_may_send(&self) -> bool {
        self.queueing_delay() < self.p.d_guard
    }

    /// A Protocol or Action message went out for the first time: `wire` bytes, its framing included.
    pub fn on_first_send(&mut self, now: HostTime, wire: u64) {
        self.first_sends.push_back((now.0, wire));
        self.first_sent_bytes += wire;
        self.slide(now);
        self.update_rides();
    }

    fn slide(&mut self, now: HostTime) {
        while let Some(&(t, b)) = self.first_sends.front() {
            if t + RATE_WINDOW_NS > now.0 {
                break;
            }
            self.first_sends.pop_front();
            self.first_sent_bytes -= b;
        }
    }

    /// The reliable send rate: wire bytes of Protocol and Action first sends in the last second.
    pub fn reliable_rate(&mut self, now: HostTime) -> u64 {
        self.slide(now);
        self.first_sent_bytes
    }

    fn update_rides(&mut self) {
        let delay = self.queueing_delay();
        if self.rides && delay >= self.p.d_guard {
            self.rides = false;
        } else if !self.rides && delay < self.p.d_guard / 2 {
            self.rides = true;
        }
    }

    /// The ride gate (4.2): are redundant rides on now?
    pub fn rides_on(&mut self, now: HostTime) -> bool {
        let rate = self.reliable_rate(now);
        self.rides && self.bps() >= self.p.k_redundant.saturating_mul(rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: i64) -> HostTime {
        HostTime(n * 1_000_000)
    }

    fn d(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_budget_is_cwnd_over_srtt() {
        let mut b = Budget::new(BudgetParams::default());
        assert_eq!(b.bps(), 0, "no window yet");
        b.observe(ms(0), d(80), d(80), 12_000);
        assert_eq!(b.bps(), 150_000);
    }

    #[test]
    fn the_delay_guard_pauses_the_low_lanes() {
        let mut b = Budget::new(BudgetParams::default());
        b.observe(ms(0), d(40), d(40), 12_000);
        assert!(b.low_lanes_may_send());
        b.observe(ms(100), d(70), d(70), 12_000);
        assert_eq!(b.queueing_delay(), d(30));
        assert!(!b.low_lanes_may_send());
    }

    #[test]
    fn base_rtt_forgets_after_its_window() {
        // A route change raises the base delay: after W_base the guard opens again (@design-reviewer's Low 3).
        let mut b = Budget::new(BudgetParams::default());
        b.observe(ms(0), d(20), d(20), 12_000);
        for t in (1..=31).map(|s| s * 1000) {
            b.observe(ms(t), d(60), d(60), 12_000);
        }
        assert_eq!(b.base_rtt(), Some(d(60)));
        assert!(b.low_lanes_may_send());
    }

    #[test]
    fn rides_stop_at_d_guard_and_resume_below_half() {
        let mut b = Budget::new(BudgetParams::default());
        b.observe(ms(0), d(40), d(40), 100_000);
        assert!(b.rides_on(ms(0)));
        b.observe(ms(10), d(66), d(66), 100_000); // 26 ms of queue
        assert!(!b.rides_on(ms(10)));
        b.observe(ms(20), d(55), d(55), 100_000); // 15 ms: still off, inside the band
        assert!(!b.rides_on(ms(20)));
        b.observe(ms(30), d(52), d(52), 100_000); // 12 ms: below D_guard / 2
        assert!(b.rides_on(ms(30)));
    }

    #[test]
    fn rides_stop_when_the_budget_is_under_k_times_the_reliable_rate() {
        let mut b = Budget::new(BudgetParams::default());
        b.observe(ms(0), d(100), d(100), 1_000); // 10,000 B/s
        for i in 0..20 {
            b.on_first_send(ms(i * 10), 100); // 2,000 B in the window: 4 x 2,000 < 10,000
        }
        assert!(b.rides_on(ms(200)));
        for i in 20..40 {
            b.on_first_send(ms(i * 10), 100); // 4,000 B: 4 x 4,000 > 10,000
        }
        assert!(!b.rides_on(ms(400)));
        // The window slides: a second later the rate is back to nothing.
        assert!(b.rides_on(ms(1_400)));
        assert_eq!(b.reliable_rate(ms(1_400)), 0);
    }
}

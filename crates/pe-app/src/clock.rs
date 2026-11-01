//! Clock sync (design 10.4): continuous and fault-tolerant among the members.
//!
//! Every member's session time is its monotonic local clock plus an offset. A ping round trip to a
//! peer gives one sample of that peer's session clock against this member's local clock:
//! `r - (t1 + rtt/2)`, where `r` is the peer's session time when it answered. Clocks drift apart (by
//! about 10-100 ppm between PCs, and by 2-9 % between Windows and a WSL2 VM on this project's machine:
//! spikes/out/m1/wsl-clock-skew), so a sync done once at the start is not enough.
//!
//! * Per peer, `PeerClock` fits `offset = a + b * local` by least squares over the last 30 s, taking the
//!   sample with the shortest round trip in each 2 s bucket (as NTP's clock filter does: a queued
//!   round trip is biased by its queue, and the path's delay may change).
//! * The target offset is the **fault-tolerant midpoint** of the predicted offsets to all members, this
//!   member's own included: drop the t lowest and the t highest (t = floor((n - 1) / 3), as in 10.8)
//!   and take the midpoint of the lowest and highest left. The frequency (the rate the offset moves at)
//!   is the same midpoint of the members' fitted rates, its own included. With n >= 3t + 1 every honest
//!   target lies within the honest range and the honest spread at least halves each round (fault-
//!   tolerant averaging); a plain median does not converge under two-faced faults (review CM-1).
//! * The frequency is selected again at every round (design 10.4 item 5). A pong also carries the
//!   replier's monotonic local time and its applied frequency correction `r_j`. The slope of the raw
//!   time against this member's clock is ρ_j, a pure ratio of two oscillators that no correction
//!   moves; (1 + ρ_j)(1 + r_j) - 1 is then peer j's session frequency in this member's raw units,
//!   exactly and without a fit's lag. The member moves its own correction `r` toward the
//!   fault-tolerant midpoint of those values (its own `r` included) by a gain per round, so honest
//!   frequencies converge as the offsets do, two-faced liars included. Fitting slopes of the peers'
//!   corrected clocks instead lags and oscillates (two members swung +-80 ms with a 20 s period).
//! * The offset runs at that frequency and slews toward the target by a capped residual, so session
//!   time never steps, never runs backwards and never skips a frame.
//! * R18's one-way stamps use the **rate-corrected clock** (`rc`): the local clock run at the fitted
//!   frequency without the slewed offset, so neither skew nor a slew reads as queueing (review CM-2).
//!
//! Before the session starts, a member takes its offset from the leader at once (the bootstrap, as
//! spikes/p2p does); from frame 0 nobody jumps, and the fits start afresh.

use std::collections::{BTreeMap, VecDeque};

/// Samples older than this are dropped.
const WINDOW_US: u64 = 30_000_000;
/// A fit takes the shortest round trip in each bucket of this length.
const BUCKET_US: u64 = 2_000_000;
/// A frequency is fitted once the buckets span at least 10 s (Theil-Sen over pairs at least 4 s
/// apart): a change of path delay, or a queue on one direction, shifts the measured offset, and over
/// a short span that reads as a frequency (with 6 s, the 80 kbit/s run's first congestion read as
/// +0.76 % for 2 s and worsened its queue peak by about 90 ms, measured).
const MIN_SPAN_US: u64 = 10_000_000;
const MIN_PAIR_US: u64 = 4_000_000;
/// A round trip counts toward the RTT excess (R18's clamp) while it is this recent.
const RTT_FRESH_US: u64 = 2_000_000;
/// The RTT excess is the least of the last this many round trips (over the best): jitter on a path
/// then reads as no queue, while a standing queue lifts every one of them (review H1).
const RTT_EXCESS_SAMPLES: usize = 4;
/// The offset is predicted from the peer's newest sample within this of its best round trip.
const NEAR_RTT_US: u64 = 2_000;
/// The residual correction beyond the frequency, per µs of local time: 12 % while more than `NEAR_US`
/// off (enough to follow a 9 % drift before a rate is fitted; 2 ms per 60 Hz frame), then 1 %.
const FAST_SLEW: f64 = 0.12;
const SLOW_SLEW: f64 = 0.01;
const NEAR_US: f64 = 5_000.0;
/// The frequency correction never exceeds this (a clock off by more is broken, not drifting).
const MAX_RATE: f64 = 0.15;
/// Each round (a new sample from any member) moves the correction this share of the way to the
/// round's selection: gradual, so neither the session clock nor the rate-corrected clock jumps.
const RATE_GAIN: f64 = 0.25;
/// A weak pull per round toward the raw clocks' fault-tolerant midpoint (the frequency the members'
/// oscillators actually run at): consensus alone fixes only the members' agreement, and the common
/// frequency then wandered by up to 7.5 % on a Windows + WSL run (measured). It trims at least one
/// value from each end when n >= 3 (coordinator, 2026-11-01, review AN-1): at n = 3 the trimmed
/// midpoint would be the midrange, and one VM 9 % slow would pull everyone 4.5 %.
const RATE_ANCHOR: f64 = 0.02;

/// One peer's clocks against this member's local clock.
#[derive(Default)]
pub struct PeerClock {
    /// (local time at the round trip's midpoint, session offset, raw-clock offset, round trip), µs.
    samples: VecDeque<(u64, i64, i64, u64)>,
    /// The fitted frequency of the peer's raw clock against this local clock, minus 1.
    rate: Option<f64>,
    /// Session offsets sampled before this local time are void (members still bootstrapping); raw
    /// offsets stay valid, since no correction moves a raw clock.
    session_from: u64,
    /// The peer's applied frequency correction, as its latest pong reported it.
    applied: f64,
}

impl PeerClock {
    fn sample(&mut self, t1: u64, t4: u64, r: u64, raw: u64, applied: f64) {
        self.applied = if applied.is_finite() { applied.clamp(-MAX_RATE, MAX_RATE) } else { 0.0 };
        let rtt = t4.saturating_sub(t1);
        let mid = t1 + rtt / 2;
        self.samples.push_back((mid, r as i64 - mid as i64, raw as i64 - mid as i64, rtt));
        while self.samples.front().is_some_and(|s| s.0 + WINDOW_US < mid) && self.samples.len() > 1 {
            self.samples.pop_front();
        }
        self.rate = self.fit_rate();
    }

    /// The best (shortest round trip; the newest on a tie) sample of each bucket, oldest first.
    fn best(&self) -> Vec<(u64, i64, i64, u64)> {
        let mut buckets: BTreeMap<u64, (u64, i64, i64, u64)> = BTreeMap::new();
        for &s in &self.samples {
            let b = buckets.entry(s.0 / BUCKET_US).or_insert(s);
            if s.3 <= b.3 {
                *b = s;
            }
        }
        buckets.into_values().collect()
    }

    /// The slope of the raw-clock offset over the window (Theil-Sen: the median of the slopes between
    /// bucket pairs at least 4 s apart, robust to a few buckets displaced by a path change). It is the
    /// ratio of the two oscillators, constant, so the whole window sharpens it.
    fn fit_rate(&self) -> Option<f64> {
        let good = self.best();
        let (first, last) = (good.first()?.0, good.last()?.0);
        if good.len() < 4 || last - first < MIN_SPAN_US {
            return None;
        }
        let mut slopes = Vec::new();
        for (i, a) in good.iter().enumerate() {
            for b in &good[i + 1..] {
                if b.0 - a.0 >= MIN_PAIR_US {
                    slopes.push((b.2 - a.2) as f64 / (b.0 - a.0) as f64);
                }
            }
        }
        (!slopes.is_empty()).then(|| midpoint(slopes, true))
    }

    /// The latest round trip above the window's shortest, if it is recent: queueing on the path, on
    /// one clock, so neither skew nor a frequency error can fake it.
    fn rtt_excess(&self, local: u64) -> Option<u64> {
        let latest = self.samples.back()?;
        if latest.0 + RTT_FRESH_US < local {
            return None;
        }
        let best = self.best_rtt().unwrap_or(latest.3);
        let least = self.samples.iter().rev().take(RTT_EXCESS_SAMPLES).map(|s| s.3).min()?;
        Some(least - best)
    }

    /// The peer's session frequency against this member's raw clock, (1 + ρ)(1 + r_j) - 1, once ρ is
    /// fitted: exact, from the reported correction, so it needs no fit of the peer's corrected clock.
    fn session_rate(&self) -> Option<f64> {
        self.rate.map(|rho| (1.0 + rho) * (1.0 + self.applied) - 1.0)
    }

    /// The peer's session offset predicted at local time `local`: its newest valid sample whose round
    /// trip is near the best (a queued one is biased by the queue), carried forward at the peer's own
    /// session frequency (`rate` until that is known). An older sample would carry whatever the peer's
    /// frequency did since (review M2).
    fn predict(&self, local: u64, rate: f64) -> Option<f64> {
        let rate = self.session_rate().unwrap_or(rate);
        let best = self.samples.iter().filter(|s| s.0 >= self.session_from).map(|s| s.3).min()?;
        let s = self.samples.iter().rev().find(|s| s.0 >= self.session_from && s.3 <= best + NEAR_RTT_US)?;
        Some(s.1 as f64 + rate * (local as f64 - s.0 as f64))
    }

    fn best_rtt(&self) -> Option<u64> {
        self.samples.iter().map(|s| s.3).min()
    }
}

/// The fault-tolerant midpoint (drop the t lowest and t highest, t = floor((n - 1) / 3); the midpoint
/// of the rest's extremes), or with `plain` the median (the midpoint of the middle two for an even
/// count): the contrast design 10.4 asks the evidence to run.
pub fn midpoint(v: Vec<f64>, plain: bool) -> f64 {
    let t = (v.len() - 1) / 3;
    midpoint_t(v, t, plain)
}

/// The fault-tolerant midpoint with `t` given (from the membership); `v` holds at least 2t + 1 values.
fn midpoint_t(mut v: Vec<f64>, t: usize, plain: bool) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if plain {
        return (v[(n - 1) / 2] + v[n / 2]) / 2.0;
    }
    (v[t] + v[n - 1 - t]) / 2.0
}

#[derive(Default)]
pub struct ClockSync {
    peers: BTreeMap<u32, PeerClock>,
    /// The offset in use and its frequency correction (µs of offset per µs of local time).
    off: f64,
    rate: f64,
    /// The rate-corrected clock (R18's stamps) at local time `last`.
    rc: f64,
    last: Option<u64>,
    /// A sample has come since the last frequency round.
    round: bool,
    /// Contrast runs: a plain median instead of the fault-tolerant midpoint.
    pub plain_median: bool,
}

impl ClockSync {
    pub fn new(plain_median: bool) -> ClockSync {
        ClockSync { plain_median, ..Default::default() }
    }

    /// A pong from `peer`: the ping left at local `t1` and came back at local `t4`; the peer's session
    /// clock read `r` and its raw local clock `raw` when it answered, and its applied frequency
    /// correction was `applied`. Each sample is a frequency round.
    pub fn sample(&mut self, peer: u32, t1: u64, t4: u64, r: u64, raw: u64, applied: f64) {
        self.peers.entry(peer).or_default().sample(t1, t4, r, raw, applied);
        self.round = true;
    }

    /// Before the session: take the offset from `leader`'s best round trip at once.
    pub fn bootstrap(&mut self, leader: u32) -> Option<i64> {
        let p = self.peers.get(&leader)?;
        // The best round trip; the newest on a tie (an older sample carries the drift since).
        let s = p.samples.iter().rev().min_by_key(|s| s.3)?;
        self.off = s.1 as f64;
        Some(self.off.round() as i64)
    }

    /// From frame 0 nobody jumps: the session offsets sampled while members were still bootstrapping
    /// are void from `local` on. Their raw-clock samples stay: no bootstrap moves a raw clock, so the
    /// frequencies keep the samples gathered since the start (review M2).
    pub fn restart_fits(&mut self, local: u64) {
        for p in self.peers.values_mut() {
            p.session_from = local;
        }
    }

    /// Advance to local time `local` toward the members' fault-tolerant midpoint (`members` are the
    /// live members other than this one; a peer without samples does not count yet). Returns the
    /// offset to use, µs.
    pub fn tick(&mut self, local: u64, members: &[u32]) -> i64 {
        let dt = self.last.map_or(0.0, |t| (local as f64 - t as f64).max(0.0));
        self.last = Some(local);
        self.rc += (1.0 + self.rate) * dt;
        // t comes from the membership, not from the values present: with fewer than 2t + 1 values a
        // liar could move the midpoint, so nothing moves until enough members are sampled (review L2).
        let t = members.len() / 3;
        let enough = |v: &Vec<f64>| v.len() > 2 * t;
        // The frequency round (item 5): each peer's session frequency in this member's raw units,
        // (1 + ρ_j)(1 + r_j) - 1, and its own correction; a gain toward their midpoint.
        if self.round {
            self.round = false;
            let mut rates = vec![self.rate];
            rates.extend(members.iter().filter_map(|m| self.peers.get(m).and_then(|p| p.session_rate())));
            let mut raw = vec![0.0];
            raw.extend(members.iter().filter_map(|m| self.peers.get(m).and_then(|p| p.rate)));
            if enough(&rates) {
                let target = midpoint_t(rates, t, self.plain_median);
                let ta = if raw.len() >= 3 { t.max(1) } else { t };
                let anchor = if raw.len() > 2 * ta { midpoint_t(raw, ta, self.plain_median) } else { self.rate };
                self.rate = (self.rate + RATE_GAIN * (target - self.rate) + RATE_ANCHOR * (anchor - self.rate)).clamp(-MAX_RATE, MAX_RATE);
            }
        }
        let follow = self.off + self.rate * dt;
        let mut offs = vec![follow];
        offs.extend(members.iter().filter_map(|m| self.peers.get(m).and_then(|p| p.predict(local, self.rate))));
        let target = if enough(&offs) { midpoint_t(offs, t, self.plain_median) } else { follow };
        let err = target - follow;
        let k = if err.abs() > NEAR_US { FAST_SLEW } else { SLOW_SLEW };
        self.off = follow + err.clamp(-k * dt, k * dt);
        self.off.round() as i64
    }

    /// The rate-corrected clock at local time `local` (µs; monotonic, at the fitted frequency).
    pub fn rc(&self, local: u64) -> u64 {
        let dt = self.last.map_or(0.0, |t| local as f64 - t as f64);
        (self.rc + (1.0 + self.rate) * dt).max(0.0) as u64
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// The shortest round trip to `peer` in the window.
    pub fn best_rtt_to(&self, peer: u32) -> Option<u64> {
        self.peers.get(&peer)?.best_rtt()
    }


    /// The queueing on the round trip to `peer` now (see `PeerClock::rtt_excess`).
    pub fn rtt_excess(&self, peer: u32, local: u64) -> Option<u64> {
        self.peers.get(&peer)?.rtt_excess(local)
    }

    /// The shortest round trip to any peer in the window.
    pub fn best_rtt_us(&self) -> u64 {
        self.peers.values().filter_map(|p| p.best_rtt()).min().unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midpoints() {
        // n = 4, t = 1: the midpoint of the middle two, as a plain median.
        assert_eq!(midpoint(vec![0.0, 10.0, 20.0, 1000.0], false), 15.0);
        assert_eq!(midpoint(vec![0.0, 10.0, 20.0, 1000.0], true), 15.0);
        // n = 2, t = 0: split the difference.
        assert_eq!(midpoint(vec![0.0, 10.0], false), 5.0);
        // n = 7, t = 2 (design 10.4's two colluders): a slow member sees the liars far behind, itself
        // and its slow twin at 0, three honest members ahead by d = 100. The median pins it at 0; the
        // fault-tolerant midpoint moves it toward d/2.
        let seen = vec![-1000.0, -1000.0, 0.0, 0.0, 100.0, 100.0, 100.0];
        assert_eq!(midpoint(seen.clone(), true), 0.0);
        assert_eq!(midpoint(seen, false), 50.0);
    }

    /// Two members whose local clocks differ by 5 %, each pinging the other every 500 ms over a
    /// symmetric 1 ms round trip, gated as the node does: before frame 0 (1 s) only b pings and takes
    /// a's clock (the bootstrap), then both restart their fits and sync. Their session clocks meet and
    /// stay together, without steps.
    #[test]
    fn two_members_with_a_5_percent_skew_converge_without_steps() {
        let skew = 0.05;
        let (mut a, mut b) = (ClockSync::default(), ClockSync::default());
        let la = |t: u64| t;
        let lb = |t: u64| (t as f64 * (1.0 - skew)) as u64 + 2_000_000;
        let (mut oa, mut ob) = (0i64, 0i64);
        let (mut last_a, mut worst) = (0i64, 0f64);
        for ms in 0..90_000u64 {
            let t = ms * 1000;
            let synced = ms >= 1000;
            if ms == 1000 {
                a.restart_fits(la(t));
                b.restart_fits(lb(t));
            }
            let mid = t + 500;
            if ms % 500 == 0 || (!synced && ms % 25 == 0) {
                // Each answers with its session and raw clocks at the round trip's midpoint.
                b.sample(1, lb(t), lb(t + 1000), (la(mid) as i64 + oa) as u64, la(mid), a.rate());
            }
            if ms % 500 == 0 {
                a.sample(2, la(t), la(t + 1000), (lb(mid) as i64 + ob) as u64, lb(mid), b.rate());
            }
            if !synced {
                b.bootstrap(1);
            }
            oa = a.tick(la(t), if synced { &[2] } else { &[] });
            ob = b.tick(lb(t), if synced { &[1] } else { &[] });
            let sa = la(t) as i64 + oa;
            assert!(sa >= last_a, "a's session time ran backwards at {ms} ms");
            if ms > 0 {
                assert!(sa - last_a <= 1_200, "a stepped at {ms} ms: {}", sa - last_a);
            }
            last_a = sa;
            if ms > 30_000 {
                let sb = lb(t) as i64 + ob;
                worst = worst.max((sa - sb).abs() as f64);
            }
        }
        assert!(worst < 2_000.0, "session clocks {worst} µs apart after convergence");
    }

    /// Two members, the second's clock running `skew` slow, synced as the node does (pings every 500
    /// ms from the start, a bootstrap burst to member 1, frame 0 at 1 s). The worst gap between their
    /// session clocks in each second (review M2's probe).
    fn gap_by_second(skew: f64) -> Vec<f64> {
        let (mut a, mut b) = (ClockSync::default(), ClockSync::default());
        let la = |t: u64| t;
        let lb = |t: u64| (t as f64 * (1.0 - skew)) as u64 + 2_000_000;
        let (mut oa, mut ob) = (0i64, 0i64);
        let mut worst = vec![0f64; 40];
        for ms in 0..40_000u64 {
            let t = ms * 1000;
            let synced = ms >= 1000;
            if ms == 1000 {
                a.restart_fits(la(t));
                b.restart_fits(lb(t));
            }
            let mid = t + 500;
            if ms % 500 == 0 || (!synced && ms % 25 == 0) {
                b.sample(1, lb(t), lb(t + 1000), (la(mid) as i64 + oa) as u64, la(mid), a.rate());
            }
            if ms % 500 == 0 {
                a.sample(2, la(t), la(t + 1000), (lb(mid) as i64 + ob) as u64, lb(mid), b.rate());
            }
            if !synced {
                b.bootstrap(1);
            }
            oa = a.tick(la(t), if synced { &[2] } else { &[] });
            ob = b.tick(lb(t), if synced { &[1] } else { &[] });
            let gap = ((la(t) as i64 + oa) - (lb(t) as i64 + ob)).abs() as f64;
            let w = &mut worst[(ms / 1000) as usize];
            *w = w.max(gap);
        }
        worst
    }

    #[test]
    fn the_start_up_gap_stays_small_at_up_to_9_percent_skew() {
        for skew in [0.023, 0.05, 0.09] {
            let w = gap_by_second(skew);
            if std::env::var("CLOCK_PROBE").is_ok() {
                eprintln!("skew {skew}: {:?}", w.iter().map(|g| (g / 1000.0).round() as i64).collect::<Vec<_>>());
            }
            let peak = w[1..].iter().cloned().fold(0.0, f64::max);
            let settled = w.iter().rposition(|&g| g >= 5_000.0).map_or(0, |i| i + 1);
            // Measured: peaks 11 / 24 / 43 ms (skew x the newest sample's age, up to a ping interval,
            // until a frequency is fitted at 10 s), then under 5 ms within a few seconds.
            assert!(peak < 60_000.0, "skew {skew}: peak gap {peak} µs");
            assert!(settled <= 18, "skew {skew}: under 5 ms only from second {settled}");
        }
    }

    #[test]
    fn the_rate_corrected_clock_never_jumps_while_the_offset_slews() {
        let mut c = ClockSync::default();
        // A peer 300 ms ahead: the offset slews toward it, the rc clock runs at ~1x without the slew.
        for ms in 0..5_000u64 {
            let t = ms * 1000;
            if ms % 500 == 0 {
                c.sample(2, t, t + 1000, t + 500 + 300_000, t + 500, 0.0);
            }
            let before = c.rc(t);
            c.tick(t, &[2]);
            let after = c.rc(t + 1000);
            assert!(after >= before && after - before <= 1_100, "rc moved {} µs in 1 ms", after - before);
        }
    }

    #[test]
    fn a_path_delay_change_does_not_read_as_a_frequency() {
        let mut p = PeerClock::default();
        // The peer's raw clock runs 2 % fast; at 15 s the return path gets 40 ms slower (asymmetric:
        // the measured offset shifts by 20 ms). Theil-Sen keeps the frequency within 0.1 %.
        for i in 0..60u64 {
            let t1 = i * 500_000;
            let rtt = if i >= 30 { 41_000 } else { 1_000 };
            let mid_true = t1 + 500;
            let raw = (mid_true as f64 * 1.02) as u64 + 5_000_000;
            p.sample(t1, t1 + rtt, raw, raw, 0.0);
        }
        let r = p.rate.unwrap();
        assert!((r - 0.02).abs() < 0.001, "rate {r}");
    }

    #[test]
    fn queued_round_trips_do_not_bias_a_peer() {
        let mut p = PeerClock::default();
        // Half the pongs wait 30 ms in a queue (asymmetric): only the best round trips count.
        for i in 0..40u64 {
            let t1 = i * 500_000;
            let rtt = if i % 2 == 1 { 31_000 } else { 1_000 };
            p.sample(t1, t1 + rtt, t1 + 500 + 7_000_000, t1 + 500, 0.0);
        }
        let o = p.predict(20_000_000, 0.0).unwrap();
        assert!((o - 7_000_000.0).abs() < 200.0, "offset {o}");
    }
}

//! The session clock (transport.md 5): sans-IO, driven by `HostTime` and samples.
//!
//! - Session time is `HostTime + offset`. The offset is the only state, and it moves only as 5.4 and 5.5 allow.
//! - Samples are NTP's four timestamps; per peer, the minimum-delay sample per bucket is kept (5.2).
//! - The target is the fault-tolerant midpoint of the members' offsets and our own (5.3).
//! - Before frame 0 the offset jumps to the target until it is steady and a quorum of members has been heard
//!   (5.4); after, it slews toward it at most `S_MAX` (5.5). No frequency loop.
//! - A clock more than 0.1 % from the midpoint is a fault (5.6).
//! - [`SharedClock`] publishes the model for the simulation thread without a lock (5.7). It implements
//!   `pe_seams::FrameClock`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use pe_seams::{Frame, FrameClock, HostTime, PeerId, SimRate};

/// transport.md 10.
#[derive(Clone, Debug)]
pub struct ClockParams {
    /// `W_clock`: samples older than this are dropped.
    pub window: Duration,
    /// `B_clock`: one sample (the least delay) kept per bucket.
    pub bucket: Duration,
    /// `E_ready`: converged when the target moves less than this for three rounds.
    pub e_ready: Duration,
    /// `T_ready`: not ready by then is a fault.
    pub t_ready: Duration,
    /// `T_settle`: the steering time constant.
    pub t_settle: Duration,
    /// `S_max`, in parts per billion: the steering rate limit (0.2 % = 2,000,000 ppb).
    pub s_max_ppb: i64,
    /// `T_pinned`: steering held at `S_max` this long is a fault.
    pub t_pinned: Duration,
    /// The 0.1 % fault limit on a clock's rate against ours, in ppb.
    pub fault_rate_ppb: i64,
}

impl Default for ClockParams {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(30),
            bucket: Duration::from_secs(2),
            e_ready: Duration::from_millis(1),
            t_ready: Duration::from_secs(5),
            t_settle: Duration::from_secs(4),
            s_max_ppb: 2_000_000,
            t_pinned: Duration::from_secs(10),
            fault_rate_ppb: 1_000_000,
        }
    }
}

/// One exchange (transport.md 5.2): `t1` my send and `t4` my receive (host time), `t2` and `t3` the peer's
/// receive and send (its session time, ns).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub t1: HostTime,
    pub t2: i64,
    pub t3: i64,
    pub t4: HostTime,
}

impl Sample {
    /// The peer's session time minus my host time, ns.
    pub fn offset(&self) -> i64 {
        ((self.t2 - self.t1.0) + (self.t3 - self.t4.0)) / 2
    }

    /// The round trip without the peer's own delay, ns. `None` when the timestamps don't make sense.
    pub fn delay(&self) -> Option<i64> {
        let d = (self.t4.0 - self.t1.0) - (self.t3 - self.t2);
        (d >= 0 && self.t4 >= self.t1 && self.t3 >= self.t2).then_some(d)
    }
}

#[derive(Default, Debug)]
struct PeerClock {
    /// (bucket index, offset, delay, received at), oldest first: the least-delay sample of each bucket.
    buckets: VecDeque<(i64, i64, i64, HostTime)>,
}

impl PeerClock {
    fn add(&mut self, s: &Sample, p: &ClockParams) {
        let Some(delay) = s.delay() else { return };
        let b = s.t4.0 / p.bucket.as_nanos() as i64;
        match self.buckets.back_mut() {
            Some(last) if last.0 == b => {
                if delay < last.2 {
                    *last = (b, s.offset(), delay, s.t4);
                }
            }
            Some(last) if last.0 > b => {} // older than what we hold: ignore
            _ => self.buckets.push_back((b, s.offset(), delay, s.t4)),
        }
        let oldest = (s.t4.0 - p.window.as_nanos() as i64) / p.bucket.as_nanos() as i64;
        while self.buckets.front().is_some_and(|f| f.0 < oldest) {
            self.buckets.pop_front();
        }
    }

    /// The offset of the least-delay sample in the newest three buckets (5.2).
    fn estimate(&self) -> Option<i64> {
        self.buckets.iter().rev().take(3).min_by_key(|x| x.2).map(|x| x.1)
    }

    /// The peer's rate against ours over the window, in ppb: the least-squares slope of the bucket offsets.
    fn rate_ppb(&self, p: &ClockParams) -> Option<i64> {
        if self.buckets.len() < 4 {
            return None;
        }
        let span = self.buckets.back()?.3.0 - self.buckets.front()?.3.0;
        if span < p.window.as_nanos() as i64 / 3 {
            return None;
        }
        let n = self.buckets.len() as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
        let x0 = self.buckets.front()?.3.0;
        for &(_, off, _, at) in &self.buckets {
            let x = (at.0 - x0) as f64;
            let y = off as f64;
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let den = n * sxx - sx * sx;
        (den > 0.0).then(|| ((n * sxy - sx * sy) / den * 1e9) as i64)
    }
}

/// The fault-tolerant midpoint (5.3): drop the t lowest and t highest of n values, t = floor((n - 1) / 3), and
/// take the midpoint of the lowest and highest left. `None` for no values.
pub fn ft_midpoint(values: &[i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let t = (v.len() - 1) / 3;
    let kept = &v[t..v.len() - t];
    let (lo, hi) = (kept[0] as i128, kept[kept.len() - 1] as i128);
    Some(((lo + hi).div_euclid(2)) as i64)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClockEvent {
    /// Converged before frame 0 (5.4): pe-agree's join may proceed.
    Ready,
    /// Not ready within `T_ready` (5.4), steering pinned at `S_max` (5.6), or a peer more than 0.1 % off.
    Fault(ClockFault),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClockFault {
    NotReady {
        heard: usize,
        needed: usize,
    },
    /// My steering has been at its limit for `T_pinned`: my clock is the odd one.
    Pinned,
    /// A peer's clock runs this many ppb from mine (with two members, nobody can tell whose clock is wrong).
    PeerRate {
        peer: PeerId,
        ppb: i64,
    },
}

/// The clock's state machine. Feed it samples and ticks; read the model through [`SharedClock`].
#[derive(Debug)]
pub struct Clock {
    p: ClockParams,
    peers: BTreeMap<PeerId, PeerClock>,
    /// The members on the start cut's list, besides me (5.4): who must be heard before ready.
    members: Vec<PeerId>,
    /// Offset model: `offset(h) = offset_ref + (h - host_ref) * slew_ppb / 1e9`.
    host_ref: HostTime,
    offset_ref: i64,
    slew_ppb: i64,
    /// Before frame 0 (converging) or after (steering).
    running: bool,
    ready: bool,
    started: Option<HostTime>,
    last_targets: VecDeque<i64>,
    pinned_since: Option<HostTime>,
    faulted_peers: BTreeMap<PeerId, bool>,
    pinned_reported: bool,
    not_ready_reported: bool,
    epoch_ns: Option<i64>,
    rate: SimRate,
    shared: std::sync::Arc<SharedClock>,
    events: Vec<ClockEvent>,
}

impl Clock {
    /// A clock for a world's first member: offset 0, ready at once, frame 0 at `epoch_ns` of session time.
    pub fn first_member(p: ClockParams, now: HostTime, epoch_ns: i64, rate: SimRate) -> Self {
        let mut c = Self::joining(p, now, Vec::new());
        c.ready = true;
        c.events.clear();
        c.start(now, epoch_ns, rate);
        c
    }

    /// A clock that must converge on `members` before frame 0 (5.4).
    pub fn joining(p: ClockParams, now: HostTime, members: Vec<PeerId>) -> Self {
        Self {
            p,
            peers: BTreeMap::new(),
            members,
            host_ref: now,
            offset_ref: 0,
            slew_ppb: 0,
            running: false,
            ready: false,
            started: Some(now),
            last_targets: VecDeque::new(),
            pinned_since: None,
            faulted_peers: BTreeMap::new(),
            pinned_reported: false,
            not_ready_reported: false,
            epoch_ns: None,
            rate: SimRate::DEFAULT,
            shared: std::sync::Arc::new(SharedClock::default()),
            events: Vec::new(),
        }
    }

    /// The app's member list replaces the welcome's (5.4, 6.4): before the clock starts, readiness is re-checked
    /// against it; once it runs, the list doesn't make it unready.
    pub fn set_members(&mut self, members: Vec<PeerId>) {
        self.members = members;
        if !self.running {
            self.ready = false; // re-checked against the authoritative list (5.4)
        }
    }

    /// A clock with no role yet: not ready, no `T_ready` running. The Core holds one from the start so the
    /// simulation thread's [`SharedClock`] exists before the session does.
    pub fn idle(p: ClockParams) -> Self {
        let mut c = Self::joining(p, HostTime(0), Vec::new());
        c.started = None;
        c
    }

    /// Become a world's first member (5.4): offset 0, ready, frame 0 at `epoch_ns`. Keeps the published model.
    pub fn begin_first_member(&mut self, now: HostTime, epoch_ns: i64, rate: SimRate) {
        self.ready = true;
        self.started = Some(now);
        self.start(now, epoch_ns, rate);
    }

    /// Begin converging on `members` (5.4), `T_ready` from now. Keeps the published model.
    pub fn begin_joining(&mut self, now: HostTime, members: Vec<PeerId>) {
        self.members = members;
        self.started = Some(now);
        self.ready = false;
        self.host_ref = now;
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Frame 0's session time and the world's rate, from the welcome (5.1). From here on the offset only slews.
    pub fn start(&mut self, now: HostTime, epoch_ns: i64, rate: SimRate) {
        self.rebase(now);
        self.epoch_ns = Some(epoch_ns);
        self.rate = rate;
        self.running = true;
        self.publish();
    }

    pub fn shared(&self) -> std::sync::Arc<SharedClock> {
        self.shared.clone()
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// My session time at `now`, ns.
    pub fn session_ns(&self, now: HostTime) -> i64 {
        now.0 + self.offset_at(now)
    }

    fn offset_at(&self, now: HostTime) -> i64 {
        self.offset_ref + ((now.0 - self.host_ref.0) as i128 * self.slew_ppb as i128 / 1_000_000_000) as i64
    }

    fn rebase(&mut self, now: HostTime) {
        self.offset_ref = self.offset_at(now);
        self.host_ref = now;
    }

    /// A sample from a peer (5.2).
    pub fn on_sample(&mut self, peer: PeerId, s: Sample) {
        self.peers.entry(peer).or_default().add(&s, &self.p);
    }

    /// A peer that left: its samples go.
    pub fn forget(&mut self, peer: &PeerId) {
        self.peers.remove(peer);
    }

    fn heard(&self) -> usize {
        self.members.iter().filter(|m| self.peers.get(*m).and_then(|c| c.estimate()).is_some()).count()
    }

    fn needed(&self) -> usize {
        (2 * self.members.len()).div_ceil(3)
    }

    /// One round (call at each sample interval): recompute the target and steer, converge or report (5.3-5.6).
    pub fn tick(&mut self, now: HostTime) {
        let own = self.offset_at(now);
        // Only the members on my list count, before frame 0 and after (5.3, 5.4).
        let mut values: Vec<i64> = self
            .peers
            .iter()
            // Only members count (5.3): a joiner's unconverged clock must not pull a member's.
            .filter(|(k, _)| self.members.contains(k))
            .filter_map(|(_, c)| c.estimate())
            .collect();
        // Until my clock has started (a first member at once, a joiner at its first Ready) my offset is not a
        // time yet, so the target is the members' alone (5.3, 5.4); counting it, a joiner only halved its
        // distance each round (days away: about 27 rounds, past `T_ready`).
        if self.running {
            values.push(own);
        }
        let target = ft_midpoint(&values).unwrap_or(own);

        if !self.running {
            // Converge: jump (5.4).
            self.offset_ref = target;
            self.host_ref = now;
            self.slew_ppb = 0;
            self.last_targets.push_back(target);
            while self.last_targets.len() > 3 {
                self.last_targets.pop_front();
            }
            let steady = self.last_targets.len() == 3
                && self.last_targets.iter().max().copied().unwrap_or(0)
                    - self.last_targets.iter().min().copied().unwrap_or(0)
                    < self.p.e_ready.as_nanos() as i64;
            // A clock with no role yet (`idle`, before a welcome) has no members to need: it is never ready.
            if !self.ready && self.started.is_some() && steady && self.heard() >= self.needed() {
                self.ready = true;
                self.events.push(ClockEvent::Ready);
            }
            if !self.ready
                && !self.not_ready_reported
                && self.started.is_some_and(|s| now.0 - s.0 >= self.p.t_ready.as_nanos() as i64)
            {
                self.not_ready_reported = true;
                self.events
                    .push(ClockEvent::Fault(ClockFault::NotReady { heard: self.heard(), needed: self.needed() }));
            }
        } else {
            // Steer: slew, never jump (5.5).
            self.rebase(now);
            let err = target - self.offset_ref;
            let want = (err as i128 * 1_000_000_000 / self.p.t_settle.as_nanos() as i128) as i64;
            self.slew_ppb = want.clamp(-self.p.s_max_ppb, self.p.s_max_ppb);
            if self.slew_ppb.abs() == self.p.s_max_ppb {
                let since = *self.pinned_since.get_or_insert(now);
                if !self.pinned_reported && now.0 - since.0 >= self.p.t_pinned.as_nanos() as i64 {
                    self.pinned_reported = true;
                    self.events.push(ClockEvent::Fault(ClockFault::Pinned));
                }
            } else {
                self.pinned_since = None;
                self.pinned_reported = false;
            }
        }

        // A peer whose clock runs more than the limit from mine (5.6), reported once until it recovers.
        for (k, c) in &self.peers {
            if let Some(r) = c.rate_ppb(&self.p) {
                let bad = r.abs() > self.p.fault_rate_ppb;
                let was = self.faulted_peers.insert(*k, bad).unwrap_or(false);
                if bad && !was {
                    self.events.push(ClockEvent::Fault(ClockFault::PeerRate { peer: *k, ppb: r }));
                }
            }
        }
        self.publish();
    }

    pub fn events(&mut self) -> Vec<ClockEvent> {
        std::mem::take(&mut self.events)
    }

    fn publish(&self) {
        self.shared.store(Model {
            host_ref: self.host_ref.0,
            offset_ref: self.offset_ref,
            slew_ppb: self.slew_ppb,
            epoch_ns: if self.running { self.epoch_ns } else { None },
            rate_hz: self.rate.hz(),
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Model {
    host_ref: i64,
    offset_ref: i64,
    slew_ppb: i64,
    epoch_ns: Option<i64>,
    rate_hz: u32,
}

impl Model {
    fn session_ns(&self, now: i64) -> i64 {
        now + self.offset_ref + ((now - self.host_ref) as i128 * self.slew_ppb as i128 / 1_000_000_000) as i64
    }
}

/// The published clock model (5.7): written by the network thread, read by any thread without a lock (a
/// sequence counter around plain atomics; a reader retries if a write overlapped).
#[derive(Debug, Default)]
pub struct SharedClock {
    seq: AtomicU64,
    host_ref: AtomicI64,
    offset_ref: AtomicI64,
    slew_ppb: AtomicI64,
    /// `i64::MIN` for "not started".
    epoch_ns: AtomicI64,
    rate_hz: AtomicU64,
}

impl SharedClock {
    fn store(&self, m: Model) {
        self.seq.fetch_add(1, Ordering::AcqRel); // odd: writing
        self.host_ref.store(m.host_ref, Ordering::Release);
        self.offset_ref.store(m.offset_ref, Ordering::Release);
        self.slew_ppb.store(m.slew_ppb, Ordering::Release);
        self.epoch_ns.store(m.epoch_ns.unwrap_or(i64::MIN), Ordering::Release);
        self.rate_hz.store(u64::from(m.rate_hz), Ordering::Release);
        self.seq.fetch_add(1, Ordering::AcqRel); // even: done
    }

    fn load(&self) -> Model {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let m = Model {
                host_ref: self.host_ref.load(Ordering::Acquire),
                offset_ref: self.offset_ref.load(Ordering::Acquire),
                slew_ppb: self.slew_ppb.load(Ordering::Acquire),
                epoch_ns: Some(self.epoch_ns.load(Ordering::Acquire)).filter(|e| *e != i64::MIN),
                rate_hz: u32::try_from(self.rate_hz.load(Ordering::Acquire)).unwrap_or(0),
            };
            if self.seq.load(Ordering::Acquire) == s1 {
                return m;
            }
        }
    }

    /// My session time at `now`, ns.
    pub fn session_ns(&self, now: HostTime) -> i64 {
        self.load().session_ns(now.0)
    }
}

impl FrameClock for SharedClock {
    fn rate_hz(&self) -> u32 {
        self.load().rate_hz
    }

    fn frame_at(&self, now: HostTime) -> Option<Frame> {
        let m = self.load();
        let epoch = m.epoch_ns?;
        let since = m.session_ns(now.0) - epoch;
        if since < 0 || m.rate_hz == 0 {
            return None;
        }
        Some(Frame((since as u128 * u128::from(m.rate_hz) / 1_000_000_000) as u64))
    }

    fn host_time_of(&self, frame: Frame) -> HostTime {
        let m = self.load();
        let Some(epoch) = m.epoch_ns else { return HostTime(i64::MAX) };
        if m.rate_hz == 0 {
            return HostTime(i64::MAX);
        }
        // Session time of the frame's start (rounded up so frame_at(host_time_of(f)) == f), then invert
        // session = h + offset_ref + (h - host_ref) * slew / 1e9.
        let s = epoch as i128 + (u128::from(frame.0) * 1_000_000_000).div_ceil(u128::from(m.rate_hz)) as i128;
        let k = 1_000_000_000i128 + m.slew_ppb as i128;
        let num = (s - m.offset_ref as i128) * 1_000_000_000 + m.host_ref as i128 * m.slew_ppb as i128;
        let mut h = num.div_euclid(k) as i64;
        // Integer rounding either way: settle on the first host time whose frame is `frame`.
        let start = (s - epoch as i128) as i64;
        while m.session_ns(h) - epoch < start {
            h += 1;
        }
        while m.session_ns(h - 1) - epoch >= start {
            h -= 1;
        }
        HostTime(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;
    fn key(n: u8) -> PeerId {
        pe_seams::MemberKey([n; 32])
    }

    /// A sample between me and a peer whose session clock is `off` ns ahead of my host clock, with one-way
    /// delays `d_out` and `d_back`.
    fn sample(at: i64, off: i64, d_out: i64, d_back: i64) -> Sample {
        let t1 = HostTime(at);
        let t2 = at + d_out + off;
        let t3 = t2 + 50_000;
        let t4 = HostTime(t3 - off + d_back);
        Sample { t1, t2, t3, t4 }
    }

    #[test]
    fn sample_offset_and_delay() {
        let s = sample(10 * S, 3_000_000, 2_000_000, 2_000_000);
        assert_eq!(s.offset(), 3_000_000);
        assert_eq!(s.delay(), Some(4_000_000));
        // Asymmetric paths bias the offset by half the difference: the reason for the least-delay filter.
        assert_eq!(sample(10 * S, 0, 6_000_000, 2_000_000).offset(), 2_000_000);
    }

    #[test]
    fn midpoint_tolerates_t_liars() {
        assert_eq!(ft_midpoint(&[]), None);
        assert_eq!(ft_midpoint(&[10, 20]), Some(15)); // n = 2: t = 0
        assert_eq!(ft_midpoint(&[0, 10, 1_000_000]), Some(500_000)); // n = 3: t = 0, one liar moves it
        assert_eq!(ft_midpoint(&[0, 10, 20, 1_000_000]), Some(15)); // n = 4: t = 1, the liar is dropped
        assert_eq!(ft_midpoint(&[-1_000_000, 0, 10, 20]), Some(5));
        assert_eq!(ft_midpoint(&[i64::MAX, i64::MAX]), Some(i64::MAX));
    }

    #[test]
    fn least_delay_sample_wins_in_a_bucket() {
        let p = ClockParams::default();
        let mut c = PeerClock::default();
        c.add(&sample(10 * S, 5_000_000, 20_000_000, 1_000_000), &p); // queued on the way out: biased
        c.add(&sample(10 * S + 100_000_000, 5_000_000, 1_000_000, 1_000_000), &p);
        assert_eq!(c.estimate(), Some(5_000_000));
    }

    #[test]
    fn a_clock_with_no_role_yet_is_never_ready() {
        // B's clock before its welcome: no members, so a quorum of none would be "heard" at once. It must wait for
        // begin_joining, or pe-app reads a frame clock with no epoch (seen as frame 0 "9e9 s ahead").
        let mut c = Clock::idle(ClockParams::default());
        for r in 0..20 {
            c.on_sample(key(1), sample(r * S, 7_000_000, 1_000_000, 1_000_000));
            c.tick(HostTime(r * S));
        }
        assert!(!c.is_ready());
        assert_eq!(c.events(), vec![]);
        c.begin_joining(HostTime(20 * S), vec![key(1)]);
        for r in 20..25 {
            c.on_sample(key(1), sample(r * S, 7_000_000, 1_000_000, 1_000_000));
            c.tick(HostTime(r * S));
        }
        assert_eq!(c.events(), vec![ClockEvent::Ready]);
    }

    #[test]
    fn a_joiner_converges_on_a_quorum_and_then_only_slews() {
        let p = ClockParams::default();
        let members = vec![key(1), key(2), key(3)];
        let mut c = Clock::joining(p.clone(), HostTime(0), members.clone());
        // Peers are 7 ms ahead of my host clock; only two of three heard: that's the quorum (ceil(2*3/3) = 2).
        let mut t = 0;
        for r in 0..5 {
            t = r * S / 5;
            c.on_sample(key(1), sample(t, 7_000_000, 1_000_000, 1_000_000));
            c.on_sample(key(2), sample(t, 7_000_000, 1_000_000, 1_000_000));
            c.tick(HostTime(t));
        }
        assert!(c.is_ready(), "converged on a quorum");
        assert_eq!(c.events(), vec![ClockEvent::Ready]);
        assert!((c.session_ns(HostTime(t)) - (t + 7_000_000)).abs() < 1_000_000);
        // Frame 0 now; a peer then jumps 100 ms: I must not jump, only slew at most 0.2 %.
        c.start(HostTime(t), t + 7_000_000, SimRate::DEFAULT);
        let before = c.session_ns(HostTime(t));
        for k in 1..=10 {
            let now = t + k * S;
            for m in &members {
                c.on_sample(*m, sample(now, 107_000_000, 1_000_000, 1_000_000));
            }
            c.tick(HostTime(now));
            let s = c.session_ns(HostTime(now));
            let real = now - t;
            assert!(s - before >= real - real / 500 - 1 && s - before <= real + real / 500 + 1, "slew within 0.2 %");
        }
    }

    #[test]
    fn not_ready_without_a_quorum_is_a_fault() {
        let p = ClockParams::default();
        let mut c = Clock::joining(p, HostTime(0), vec![key(1), key(2), key(3)]);
        for r in 0..30 {
            let t = r * S / 5;
            c.on_sample(key(1), sample(t, 0, 1_000_000, 1_000_000)); // only one of three
            c.tick(HostTime(t));
        }
        assert!(!c.is_ready());
        assert_eq!(c.events(), vec![ClockEvent::Fault(ClockFault::NotReady { heard: 1, needed: 2 })]);
    }

    #[test]
    fn a_drifting_peer_is_reported() {
        let p = ClockParams::default();
        let mut c = Clock::first_member(p, HostTime(0), 0, SimRate::DEFAULT);
        // Peer runs 0.15 % fast: its offset grows 1.5 ms per second.
        for k in 0..40 {
            let t = k * S;
            c.on_sample(key(9), sample(t, k * 1_500_000, 1_000_000, 1_000_000));
            c.tick(HostTime(t));
        }
        let ev = c.events();
        assert!(
            ev.iter().any(|e| matches!(e, ClockEvent::Fault(ClockFault::PeerRate { peer, ppb }) if *peer == key(9) && (*ppb - 1_500_000).abs() < 100_000)),
            "{ev:?}"
        );
    }

    #[test]
    fn frames_follow_the_published_model() {
        let p = ClockParams::default();
        let c = Clock::first_member(p, HostTime(5 * S), 5 * S, SimRate::new(60).unwrap());
        let sh = c.shared();
        assert_eq!(sh.rate_hz(), 60);
        assert_eq!(sh.frame_at(HostTime(5 * S - 1)), None);
        assert_eq!(sh.frame_at(HostTime(5 * S)), Some(Frame(0)));
        assert_eq!(sh.frame_at(HostTime(6 * S)), Some(Frame(60)));
        for f in [0u64, 1, 59, 60, 61, 3_600, 123_457] {
            let h = sh.host_time_of(Frame(f));
            assert_eq!(sh.frame_at(h), Some(Frame(f)), "frame {f}");
            assert_eq!(sh.frame_at(HostTime(h.0 - 1)), if f == 0 { None } else { Some(Frame(f - 1)) }, "frame {f}");
        }
    }

    #[test]
    fn host_time_of_inverts_a_slewing_model() {
        let p = ClockParams::default();
        let mut c = Clock::first_member(p, HostTime(0), 0, SimRate::new(90).unwrap());
        c.slew_ppb = 2_000_000;
        c.offset_ref = 3_000_000;
        c.publish();
        let sh = c.shared();
        for f in [0u64, 1, 89, 900, 81_000] {
            let h = sh.host_time_of(Frame(f));
            assert_eq!(sh.frame_at(h), Some(Frame(f)), "frame {f}");
            assert_ne!(sh.frame_at(HostTime(h.0 - 1)), Some(Frame(f)), "frame {f} starts exactly at h");
        }
    }
}

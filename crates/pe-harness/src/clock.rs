//! Virtual time (DESIGN.md 3.1).
//!
//! Global time is nanoseconds since the start of a run. Each node sees its own monotonic `HostTime` (S3's input to
//! `tick`), which is global time with an offset and a drift (file 14, P9). Frames are not derived here: pe-net's
//! sans-IO session clock maps HostTime to frames (engine-architect, m03f00fa4bc71).

use std::collections::BTreeMap;

/// Global virtual time, in nanoseconds since the run started.
pub type Nanos = u64;

pub const MS: Nanos = 1_000_000;
pub const SEC: Nanos = 1_000_000_000;

/// A node's local clock: an offset and a drift in parts per million against global time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalClock {
    pub offset_ns: i64,
    pub drift_ppm: i32,
}

impl LocalClock {
    /// The node's HostTime (local monotonic nanoseconds) at a global time.
    pub fn host_time(&self, global: Nanos) -> i64 {
        let g = global as i128;
        let drift = g * self.drift_ppm as i128 / 1_000_000;
        (g + drift + self.offset_ns as i128) as i64
    }
}

/// A priority queue ordered by (time, insertion order). Equal times pop in the order they were pushed, so a run is
/// deterministic without any ordering on the items themselves.
#[derive(Debug)]
pub struct EventQueue<T> {
    items: BTreeMap<(Nanos, u64), T>,
    next_seq: u64,
}

impl<T> Default for EventQueue<T> {
    fn default() -> Self {
        Self { items: BTreeMap::new(), next_seq: 0 }
    }
}

impl<T> EventQueue<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, at: Nanos, item: T) {
        self.items.insert((at, self.next_seq), item);
        self.next_seq += 1;
    }

    /// The time of the earliest event, if any.
    pub fn peek_time(&self) -> Option<Nanos> {
        self.items.keys().next().map(|&(t, _)| t)
    }

    /// Removes and returns the earliest event if it is due at or before `now`.
    pub fn pop_due(&mut self, now: Nanos) -> Option<(Nanos, T)> {
        match self.peek_time() {
            Some(t) if t <= now => self.items.pop_first().map(|((t, _), v)| (t, v)),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_clock_frames_round_trip_at_every_supported_rate() {
        use pe_seams::{FrameClock, HostTime, SimRate};
        for hz in [30, 60, 90, 120, 47] {
            let c = SessionClock { rate: SimRate::new(hz).unwrap(), start: HostTime(5_000) };
            assert_eq!(c.frame_at(HostTime(4_999)), None, "before frame 0");
            assert_eq!(c.frame_at(HostTime(5_000)), Some(pe_seams::Frame(0)));
            for f in [1u64, 2, 59, 60, 3_599, 3_600, 1_000_003] {
                let t = c.host_time_of(pe_seams::Frame(f));
                assert_eq!(c.frame_at(t), Some(pe_seams::Frame(f)), "{hz} Hz, frame {f} starts at its own time");
                assert_eq!(c.frame_at(t.plus_nanos(-1)), Some(pe_seams::Frame(f - 1)), "{hz} Hz, just before {f}");
            }
        }
        let c = SessionClock { rate: SimRate::DEFAULT, start: HostTime(0) };
        assert_eq!(c.frame_at(HostTime::from_millis(60_000)), Some(pe_seams::Frame(3_600)), "one minute at 60 Hz");
    }

    #[test]
    fn equal_times_pop_in_push_order() {
        let mut q = EventQueue::new();
        q.push(5, "b");
        q.push(3, "a");
        q.push(5, "c");
        assert_eq!(q.pop_due(2), None);
        assert_eq!(q.pop_due(10), Some((3, "a")));
        assert_eq!(q.pop_due(10), Some((5, "b")));
        assert_eq!(q.pop_due(10), Some((5, "c")));
        assert!(q.is_empty());
    }

    #[test]
    fn local_clock_offset_and_drift() {
        let c = LocalClock { offset_ns: -2 * MS as i64, drift_ppm: 100 };
        assert_eq!(c.host_time(0), -2_000_000);
        // 100 ppm over 10 s is 1 ms.
        assert_eq!(c.host_time(10 * SEC), (10 * SEC) as i64 + 1_000_000 - 2_000_000);
        let back = LocalClock { offset_ns: 0, drift_ppm: -50 };
        assert_eq!(back.host_time(20 * SEC), (20 * SEC) as i64 - 1_000_000);
        assert_eq!(LocalClock::default().host_time(123), 123);
    }
}

/// A session clock for one node: S4's [`pe_seams::FrameClock`] over its HostTime, at the world's [`SimRate`].
///
/// Frame 0 starts at the node's HostTime `start`. A node whose local clock is offset or drifting (`LocalClock`) sees
/// its frames start early or late against global time, as file 14's P9 describes. pe-net's real clock model, which
/// converges and steers gradually, replaces this in network scenarios once it exists (transport.md 5.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionClock {
    pub rate: pe_seams::SimRate,
    pub start: pe_seams::HostTime,
}

impl pe_seams::FrameClock for SessionClock {
    fn rate_hz(&self) -> u32 {
        self.rate.hz()
    }

    fn frame_at(&self, now: pe_seams::HostTime) -> Option<pe_seams::Frame> {
        let since = i128::from(now.0) - i128::from(self.start.0);
        (since >= 0).then(|| pe_seams::Frame((since * i128::from(self.rate.hz()) / 1_000_000_000) as u64))
    }

    fn host_time_of(&self, frame: pe_seams::Frame) -> pe_seams::HostTime {
        let ns = (u128::from(frame.0) * 1_000_000_000).div_ceil(u128::from(self.rate.hz()));
        pe_seams::HostTime(self.start.0 + ns as i64)
    }
}

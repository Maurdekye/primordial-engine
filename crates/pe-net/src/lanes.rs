//! The lanes of one link (transport.md 3), sans-IO: what goes into each datagram and stream, what is delivered.
//!
//! - **Protocol and Action:** numbered per lane; sent once in a datagram, a small one riding in the next `r - 1`
//!   datagrams too; retransmitted after an RTO with backoff; acknowledged by QUIC-style ranges; delivered once,
//!   in any order (3.3). A message over half a datagram goes on its own stream instead.
//! - **Pose:** one slot; a new pose replaces an unsent one; never repeated; an older arrival is dropped (3.4).
//! - **Repair and Bulk:** one stream per message, in that priority (3.5).
//! - **Control records** ride in datagrams: the clock's samples (5.2).
//!
//! Nothing a peer sends can panic this module: a bad datagram is a [`FrameError`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use bytes::Bytes;
use pe_seams::{HostTime, Lane};

/// transport.md 10.
#[derive(Clone, Debug)]
pub struct LaneParams {
    /// `T_ack`: longest wait before an acknowledgement-only datagram.
    pub t_ack: Duration,
    /// `N_ack_ranges`: acknowledgement ranges per lane in one datagram.
    pub n_ack_ranges: usize,
    /// `r`: datagrams a small message rides in, the first send included.
    pub r: u8,
    /// `L_redundant`: the largest message that gets redundancy.
    pub l_redundant: usize,
    /// `T_rto_min`: floor of the retransmission timeout's variance term.
    pub t_rto_min: Duration,
    /// `L_msg`: the largest Protocol or Action message.
    pub l_msg: usize,
    /// `L_bulk`: the largest Repair or Bulk message.
    pub l_bulk: usize,
}

impl Default for LaneParams {
    fn default() -> Self {
        Self {
            t_ack: Duration::from_millis(10),
            n_ack_ranges: 32,
            r: 2,
            l_redundant: 128,
            t_rto_min: Duration::from_millis(20),
            l_msg: 1 << 20,
            l_bulk: 64 << 20,
        }
    }
}

const VERSION: u8 = 1;
/// Item kinds in a datagram: the lanes by their number, then control records.
const K_PROTOCOL: u8 = 0;
const K_ACTION: u8 = 1;
const K_POSE: u8 = 2;
const K_CLOCK_PING: u8 = 0x10;
const K_CLOCK_PONG: u8 = 0x11;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    Truncated,
    BadVersion(u8),
    BadKind(u8),
    TooLong,
    BadAck,
    /// A varint in a longer form than its value needs: every message has one encoding (found by fuzzing).
    NonMinimal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendError {
    /// Larger than `L_msg` (Protocol, Action, Pose) or `L_bulk` (Repair, Bulk).
    TooLarge { lane: Lane, len: usize },
}

/// A clock exchange (transport.md 5.2): `Ping { t1 }` out, `Pong { t1, t2, t3 }` back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    ClockPing { t1: i64 },
    ClockPong { t1: i64, t2: i64, t3: i64 },
}

/// What one datagram delivered.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Incoming {
    pub messages: Vec<(Lane, Bytes)>,
    pub control: Vec<Control>,
}

// QUIC variable-length integers (RFC 9000 16), up to 2^62 - 1.
pub(crate) fn put_varint(out: &mut Vec<u8>, v: u64) {
    if v < 1 << 6 {
        out.push(v as u8);
    } else if v < 1 << 14 {
        out.extend_from_slice(&((v as u16) | 0x4000).to_be_bytes());
    } else if v < 1 << 30 {
        out.extend_from_slice(&((v as u32) | 0x8000_0000).to_be_bytes());
    } else {
        out.extend_from_slice(&((v & 0x3fff_ffff_ffff_ffff) | 0xc000_0000_0000_0000).to_be_bytes());
    }
}

fn varint_len(v: u64) -> usize {
    match v {
        0..0x40 => 1,
        0x40..0x4000 => 2,
        0x4000..0x4000_0000 => 4,
        _ => 8,
    }
}

pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

impl Reader<'_> {
    pub(crate) fn u8(&mut self) -> Result<u8, FrameError> {
        let (&b, rest) = self.0.split_first().ok_or(FrameError::Truncated)?;
        self.0 = rest;
        Ok(b)
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&[u8], FrameError> {
        if self.0.len() < n {
            return Err(FrameError::Truncated);
        }
        let (a, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(a)
    }

    pub(crate) fn varint(&mut self) -> Result<u64, FrameError> {
        let first = self.u8()?;
        let len = 1usize << (first >> 6);
        let mut v = u64::from(first & 0x3f);
        for &b in self.bytes(len - 1)? {
            v = (v << 8) | u64::from(b);
        }
        if varint_len(v) != len {
            return Err(FrameError::NonMinimal);
        }
        Ok(v)
    }

    pub(crate) fn i64(&mut self) -> Result<i64, FrameError> {
        let b: [u8; 8] = self.bytes(8)?.try_into().map_err(|_| FrameError::Truncated)?;
        Ok(i64::from_le_bytes(b))
    }
}

/// Which sequence numbers arrived: everything below `next`, plus a bounded set above it.
#[derive(Debug, Default)]
struct RecvSet {
    next: u64,
    above: BTreeSet<u64>,
}

impl RecvSet {
    /// `true` if `seq` is new.
    fn insert(&mut self, seq: u64, cap: usize) -> bool {
        if seq < self.next || self.above.contains(&seq) {
            return false;
        }
        if seq > self.next + cap as u64 * 64 {
            return false; // implausibly far ahead: drop it, the sender retransmits
        }
        self.above.insert(seq);
        while self.above.remove(&self.next) {
            self.next += 1;
        }
        true
    }

    /// Up to `n` ranges `[start, end)` above `next`, newest first.
    fn ranges(&self, n: usize) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = Vec::new();
        for &s in &self.above {
            match out.last_mut() {
                Some(r) if r.1 == s => r.1 = s + 1,
                _ => out.push((s, s + 1)),
            }
        }
        out.reverse();
        out.truncate(n);
        out
    }
}

#[derive(Debug)]
struct Pending {
    bytes: Bytes,
    sent_at: Option<HostTime>,
    first_sent: Option<HostTime>,
    rides_left: u8,
    backoff: u32,
}

/// The lanes of one link.
#[derive(Debug)]
pub struct Link {
    p: LaneParams,
    next_seq: [u64; 5],
    /// Protocol, Action: unacknowledged messages by seq.
    pending: [BTreeMap<u64, Pending>; 2],
    /// Messages for the stream path, per lane, oldest first.
    streams: [VecDeque<(u64, Bytes)>; 5],
    pose: Option<(u64, Bytes)>,
    recv: [RecvSet; 2],
    last_pose: Option<u64>,
    ack_due: Option<HostTime>,
    control_out: VecDeque<Control>,
    srtt: Duration,
    rttvar: Duration,
    /// Lane traffic may go out (`send`) and be taken in (`recv`). Before the welcome (transport.md 6.4) a link
    /// carries only control records; the Core opens each side as the welcome exchange goes.
    open_send: bool,
    open_recv: bool,
    /// Bytes put on the wire in datagrams, and the message bytes among them (for the 1.5 test, 3.3).
    pub wire_bytes: u64,
    pub payload_bytes: u64,
    /// Wire bytes of Protocol and Action first sends, item framing included: the reliable send rate's input
    /// (transport.md 4.2).
    pub first_sent_wire: u64,
    /// Lane items that arrived while the receive side was closed (the Core treats any as a fault, 6.4).
    pub closed_drops: u64,
}

fn rel(lane: Lane) -> Option<usize> {
    match lane {
        Lane::Protocol => Some(0),
        Lane::Action => Some(1),
        _ => None,
    }
}

impl Link {
    pub fn new(p: LaneParams) -> Self {
        Self {
            p,
            next_seq: [0; 5],
            pending: Default::default(),
            streams: Default::default(),
            pose: None,
            recv: Default::default(),
            last_pose: None,
            ack_due: None,
            control_out: VecDeque::new(),
            srtt: Duration::from_millis(100),
            rttvar: Duration::from_millis(50),
            open_send: true,
            open_recv: true,
            wire_bytes: 0,
            payload_bytes: 0,
            first_sent_wire: 0,
            closed_drops: 0,
        }
    }

    /// Open or close lane traffic each way (6.4). A closed send side keeps what is queued; a closed receive side
    /// drops lane items unacknowledged (the sender repeats them) and keeps only control records.
    pub fn set_open(&mut self, send: bool, recv: bool) {
        self.open_send = send;
        self.open_recv = recv;
    }

    /// The path's round-trip estimate, from QUIC's statistics (transport.md 4.1).
    pub fn set_rtt(&mut self, srtt: Duration, rttvar: Duration) {
        self.srtt = srtt;
        self.rttvar = rttvar;
    }

    fn rto(&self, backoff: u32) -> Duration {
        let base = self.srtt + (self.rttvar * 4).max(self.p.t_rto_min);
        base * 2u32.saturating_pow(backoff.min(6))
    }

    /// Queue a message (seam S4's `send`). `max_dgram` is QUIC's current datagram payload size: a Protocol or
    /// Action message over half of it takes the stream path.
    pub fn send(&mut self, lane: Lane, bytes: Bytes, max_dgram: usize) -> Result<(), SendError> {
        let limit = if matches!(lane, Lane::Repair | Lane::Bulk) { self.p.l_bulk } else { self.p.l_msg };
        if bytes.len() > limit {
            return Err(SendError::TooLarge { lane, len: bytes.len() });
        }
        let li = lane as usize;
        let seq = self.next_seq[li];
        self.next_seq[li] += 1;
        match (lane, rel(lane)) {
            (_, Some(_)) if bytes.len() > max_dgram / 2 => self.streams[li].push_back((seq, bytes)),
            (_, Some(i)) => {
                let rides = if bytes.len() <= self.p.l_redundant { self.p.r } else { 1 };
                self.pending[i]
                    .insert(seq, Pending { bytes, sent_at: None, first_sent: None, rides_left: rides, backoff: 0 });
            }
            (Lane::Pose, _) => {
                if bytes.len() > max_dgram.saturating_sub(16) {
                    return Err(SendError::TooLarge { lane, len: bytes.len() });
                }
                self.pose = Some((seq, bytes));
            }
            _ => self.streams[li].push_back((seq, bytes)),
        }
        Ok(())
    }

    /// Queue a control record (it rides in the next datagram, ahead of every lane).
    pub fn send_control(&mut self, c: Control) {
        self.control_out.push_back(c);
    }

    /// The next message for the stream path, in lane priority; `None` when there is none. Repair and Bulk only if
    /// `low` (the delay guard, 4.3, holds).
    pub fn poll_stream(&mut self, low: bool) -> Option<(Lane, u64, Bytes)> {
        if !self.open_send {
            return None;
        }
        for lane in Lane::ALL.into_iter().filter(|l| low || !matches!(l, Lane::Repair | Lane::Bulk)) {
            if let Some((seq, b)) = self.streams[lane as usize].pop_front() {
                return Some((lane, seq, b));
            }
        }
        None
    }

    /// When this link next needs a datagram even with nothing new: an acknowledgement or a retransmission.
    pub fn next_timeout(&self) -> Option<HostTime> {
        let rto = self
            .pending
            .iter()
            .flat_map(|m| m.values())
            .filter_map(|p| p.sent_at.map(|s| s.plus(self.rto(p.backoff))))
            .min();
        match (self.ack_due, rto) {
            (Some(a), Some(r)) => Some(a.min(r)),
            (a, r) => a.or(r),
        }
    }

    fn write_acks(&self, out: &mut Vec<u8>) {
        for r in &self.recv {
            put_varint(out, r.next);
            let ranges = r.ranges(self.p.n_ack_ranges);
            put_varint(out, ranges.len() as u64);
            for (s, e) in ranges {
                put_varint(out, s);
                put_varint(out, e - s);
            }
        }
    }

    /// Build the next datagram of at most `max` bytes, or `None` when nothing is due. Order: control records,
    /// then Protocol and Action messages never sent or past their RTO (oldest first), then the pose, then, if
    /// `redundancy` and the datagram is going out anyway, rides of small messages already sent (3.3). The caller
    /// allows redundancy only while the link's budget is well above its reliable send rate (4.2), so rides never
    /// take a thin link's capacity.
    pub fn poll_datagram(&mut self, now: HostTime, max: usize, redundancy: bool) -> Option<Vec<u8>> {
        let mut out = vec![VERSION];
        self.write_acks(&mut out);
        let header = out.len();
        if header + 1 > max {
            return None;
        }
        let mut payload = 0usize;
        while let Some(c) = self.control_out.front() {
            let mut b = Vec::new();
            match *c {
                Control::ClockPing { t1 } => {
                    b.push(K_CLOCK_PING);
                    b.extend_from_slice(&t1.to_le_bytes());
                }
                Control::ClockPong { t1, t2, t3 } => {
                    b.push(K_CLOCK_PONG);
                    for v in [t1, t2, t3] {
                        b.extend_from_slice(&v.to_le_bytes());
                    }
                }
            }
            if out.len() + b.len() > max {
                break;
            }
            out.extend_from_slice(&b);
            self.control_out.pop_front();
        }
        // Pass 1: never sent, or past the RTO.
        for i in (0..2).filter(|_| self.open_send) {
            let kind = i as u8;
            let rto: Vec<(u64, Duration)> = self.pending[i].iter().map(|(s, p)| (*s, self.rto(p.backoff))).collect();
            for (seq, rto) in rto {
                let Some(p) = self.pending[i].get_mut(&seq) else { continue };
                let due = match p.sent_at {
                    None => true,
                    Some(at) => now >= at.plus(rto),
                };
                if !due {
                    continue;
                }
                let need = 1 + varint_len(seq) + varint_len(p.bytes.len() as u64) + p.bytes.len();
                if out.len() + need > max {
                    continue; // a smaller one may still fit
                }
                out.push(kind);
                put_varint(&mut out, seq);
                put_varint(&mut out, p.bytes.len() as u64);
                out.extend_from_slice(&p.bytes);
                payload += p.bytes.len();
                if p.sent_at.is_some() {
                    p.backoff += 1;
                } else {
                    p.first_sent = Some(now);
                    self.first_sent_wire += need as u64;
                }
                p.sent_at = Some(now);
                p.rides_left = p.rides_left.saturating_sub(1);
            }
        }
        if let Some((seq, b)) = self.pose.as_ref().filter(|_| self.open_send) {
            let need = 1 + varint_len(*seq) + varint_len(b.len() as u64) + b.len();
            if out.len() + need <= max {
                out.push(K_POSE);
                put_varint(&mut out, *seq);
                put_varint(&mut out, b.len() as u64);
                out.extend_from_slice(b);
                payload += b.len();
                self.pose = None;
            }
        }
        // Rides: redundancy for small messages already sent once (3.3), only into a datagram that goes out
        // anyway, and only into space that is left.
        let going = out.len() > header || self.ack_due.is_some_and(|t| now >= t);
        for i in (0..2).filter(|_| redundancy && going && self.open_send) {
            let kind = i as u8;
            for (seq, p) in self.pending[i].iter_mut() {
                if p.sent_at.is_none() || p.sent_at == Some(now) || p.rides_left == 0 {
                    continue;
                }
                let need = 1 + varint_len(*seq) + varint_len(p.bytes.len() as u64) + p.bytes.len();
                if out.len() + need > max {
                    continue;
                }
                out.push(kind);
                put_varint(&mut out, *seq);
                put_varint(&mut out, p.bytes.len() as u64);
                out.extend_from_slice(&p.bytes);
                p.rides_left -= 1;
            }
        }
        let ack_due = self.ack_due.is_some_and(|t| now >= t);
        if out.len() == header && !ack_due {
            return None;
        }
        self.ack_due = None;
        self.wire_bytes += out.len() as u64;
        self.payload_bytes += payload as u64;
        Some(out)
    }

    /// Only a message that has been transmitted can be acknowledged: an ack (replayed, or from a peer acking
    /// ahead) never drops one still queued (found by the sequence fuzz target, @design-reviewer's ask).
    fn apply_acks(&mut self, i: usize, next: u64, ranges: &[(u64, u64)]) {
        let pend = &mut self.pending[i];
        let acked: Vec<u64> = pend
            .iter()
            .filter(|(s, p)| {
                p.first_sent.is_some() && (**s < next || ranges.iter().any(|(a, b)| (*a..*b).contains(*s)))
            })
            .map(|(s, _)| *s)
            .collect();
        for s in acked {
            pend.remove(&s);
        }
    }

    /// A datagram from the peer.
    pub fn on_datagram(&mut self, now: HostTime, data: &[u8]) -> Result<Incoming, FrameError> {
        let mut r = Reader(data);
        let v = r.u8()?;
        if v != VERSION {
            return Err(FrameError::BadVersion(v));
        }
        let mut acks = Vec::new();
        for _ in 0..2 {
            let next = r.varint()?;
            let n = r.varint()?;
            if n as usize > self.p.n_ack_ranges {
                return Err(FrameError::BadAck);
            }
            let mut ranges = Vec::new();
            for _ in 0..n {
                let s = r.varint()?;
                let len = r.varint()?;
                ranges.push((s, s.checked_add(len).ok_or(FrameError::BadAck)?));
            }
            acks.push((next, ranges));
        }
        let mut inc = Incoming::default();
        let mut wants_ack = false;
        while !r.0.is_empty() {
            match r.u8()? {
                k @ (K_PROTOCOL | K_ACTION | K_POSE) => {
                    let seq = r.varint()?;
                    let len = usize::try_from(r.varint()?).map_err(|_| FrameError::TooLong)?;
                    if len > self.p.l_msg {
                        return Err(FrameError::TooLong);
                    }
                    let b = Bytes::copy_from_slice(r.bytes(len)?);
                    if !self.open_recv {
                        self.closed_drops += 1;
                        continue; // before the welcome: dropped unacknowledged
                    }
                    if k == K_POSE {
                        if self.last_pose.is_none_or(|l| seq > l) {
                            self.last_pose = Some(seq);
                            inc.messages.push((Lane::Pose, b));
                        }
                    } else {
                        wants_ack = true;
                        if self.recv[k as usize].insert(seq, self.p.n_ack_ranges) {
                            inc.messages.push((if k == K_PROTOCOL { Lane::Protocol } else { Lane::Action }, b));
                        }
                    }
                }
                K_CLOCK_PING => inc.control.push(Control::ClockPing { t1: r.i64()? }),
                K_CLOCK_PONG => inc.control.push(Control::ClockPong { t1: r.i64()?, t2: r.i64()?, t3: r.i64()? }),
                other => return Err(FrameError::BadKind(other)),
            }
        }
        // Only now, with the whole datagram valid, act on its acknowledgements.
        for (i, (next, ranges)) in acks.into_iter().enumerate() {
            self.apply_acks(i, next, &ranges);
        }
        if wants_ack && self.ack_due.is_none() {
            self.ack_due = Some(now.plus(self.p.t_ack));
        }
        Ok(inc)
    }

    /// A message that arrived on a stream: delivered once (Protocol and Action are deduplicated by seq).
    pub fn on_stream(&mut self, lane: Lane, seq: u64, bytes: Bytes) -> Option<(Lane, Bytes)> {
        if !self.open_recv {
            return None;
        }
        match rel(lane) {
            Some(i) => self.recv[i].insert(seq, self.p.n_ack_ranges).then_some((lane, bytes)),
            None => Some((lane, bytes)),
        }
    }

    /// Bytes queued per lane, and the age of the oldest unacknowledged Protocol and Action messages (4.4).
    pub fn queued(&self, now: HostTime) -> ([u64; 5], [Duration; 2]) {
        let mut q = [0u64; 5];
        for (i, m) in self.pending.iter().enumerate() {
            q[i] += m.values().map(|p| p.bytes.len() as u64).sum::<u64>();
        }
        for (i, s) in self.streams.iter().enumerate() {
            q[i] += s.iter().map(|(_, b)| b.len() as u64).sum::<u64>();
        }
        if let Some((_, b)) = &self.pose {
            q[2] += b.len() as u64;
        }
        let age = |m: &BTreeMap<u64, Pending>| {
            m.values()
                .filter_map(|p| p.first_sent)
                .min()
                .map_or(Duration::ZERO, |t| Duration::from_nanos(u64::try_from(now.0 - t.0).unwrap_or(0)))
        };
        (q, [age(&self.pending[0]), age(&self.pending[1])])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: usize = 1162;

    fn ms(n: i64) -> HostTime {
        HostTime::from_millis(n)
    }

    fn deliver(from: &mut Link, to: &mut Link, now: HostTime, drop: bool) -> Vec<(Lane, Bytes)> {
        let mut got = Vec::new();
        while let Some(d) = from.poll_datagram(now, MAX, true) {
            if !drop {
                got.extend(to.on_datagram(now, &d).unwrap().messages);
            }
        }
        got
    }

    #[test]
    fn varints_round_trip() {
        for v in [0u64, 1, 63, 64, 16_383, 16_384, (1 << 30) - 1, 1 << 30, (1 << 62) - 1] {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            assert_eq!(b.len(), varint_len(v));
            assert_eq!(Reader(&b).varint().unwrap(), v);
        }
    }

    #[test]
    fn a_varint_in_a_longer_form_than_needed_is_refused() {
        // Found by the long fuzz run: a hello whose token length 40 was written 0x40 0x28 was accepted.
        for b in [&[0x40u8, 0x28][..], &[0x40, 0x00], &[0x80, 0, 0, 0x3f], &[0xc0, 0, 0, 0, 0x3f, 0xff, 0xff, 0xff]] {
            assert_eq!(Reader(b).varint(), Err(FrameError::NonMinimal), "{b:02x?}");
        }
        assert_eq!(Reader(&[0x80, 0, 0x40, 0]).varint(), Ok(0x4000));
    }

    #[test]
    fn messages_arrive_once_each() {
        let (mut a, mut b) = (Link::new(LaneParams::default()), Link::new(LaneParams::default()));
        for i in 0..20u8 {
            a.send(if i % 3 == 0 { Lane::Protocol } else { Lane::Action }, Bytes::from(vec![i; 40]), MAX).unwrap();
        }
        let got = deliver(&mut a, &mut b, ms(0), false);
        assert_eq!(got.len(), 20);
        // b's acknowledgement clears a's queue.
        let _ = deliver(&mut b, &mut a, ms(20), false);
        assert_eq!(a.queued(ms(20)).0[..2], [0, 0]);
        // Replaying a datagram delivers nothing twice.
        a.send(Lane::Action, Bytes::from_static(b"once"), MAX).unwrap();
        let d = a.poll_datagram(ms(30), MAX, true).unwrap();
        assert_eq!(b.on_datagram(ms(30), &d).unwrap().messages.len(), 1);
        assert_eq!(b.on_datagram(ms(31), &d).unwrap().messages.len(), 0);
    }

    #[test]
    fn an_ack_never_drops_a_message_not_yet_transmitted() {
        // Found by the sequence fuzz target: an ack covering seqs this side has queued but not sent (replayed,
        // or from a peer acking ahead) used to drop them unsent.
        let (mut a, mut b) = (Link::new(LaneParams::default()), Link::new(LaneParams::default()));
        for i in 0..4u8 {
            a.send(Lane::Protocol, Bytes::from(vec![i; 40]), MAX).unwrap();
        }
        assert_eq!(deliver(&mut a, &mut b, ms(0), false).len(), 4);
        let ack = b.poll_datagram(ms(20), MAX, true).expect("b acknowledges seqs 0..4");
        let mut c = Link::new(LaneParams::default());
        c.set_open(false, true); // before the welcome: c queues, sends nothing
        c.send(Lane::Protocol, Bytes::from_static(b"queued"), MAX).unwrap();
        c.on_datagram(ms(21), &ack).unwrap();
        assert_eq!(c.queued(ms(21)).0[0], 6, "the queued message survives an ack of its seq");
        // The same ack still clears a's transmitted messages.
        a.on_datagram(ms(21), &ack).unwrap();
        assert_eq!(a.queued(ms(21)).0[0], 0);
    }

    #[test]
    fn a_lost_small_message_rides_in_the_next_datagram() {
        let (mut a, mut b) = (Link::new(LaneParams::default()), Link::new(LaneParams::default()));
        a.send(Lane::Action, Bytes::from_static(b"first"), MAX).unwrap();
        assert!(deliver(&mut a, &mut b, ms(0), true).is_empty()); // lost
        a.send(Lane::Action, Bytes::from_static(b"second"), MAX).unwrap();
        let got = deliver(&mut a, &mut b, ms(16), false);
        let got: Vec<&[u8]> = got.iter().map(|(_, x)| &x[..]).collect();
        assert_eq!(got, vec![&b"second"[..], &b"first"[..]], "no retransmission round trip");
    }

    #[test]
    fn a_lost_large_message_is_retransmitted_after_the_rto() {
        let mut a = Link::new(LaneParams::default());
        let mut b = Link::new(LaneParams::default());
        a.set_rtt(Duration::from_millis(40), Duration::from_millis(5));
        a.send(Lane::Action, Bytes::from(vec![7u8; 300]), MAX).unwrap(); // over L_redundant: no ride
        assert!(deliver(&mut a, &mut b, ms(0), true).is_empty());
        assert!(a.poll_datagram(ms(10), MAX, true).is_none(), "nothing before the RTO");
        let rto = a.next_timeout().unwrap();
        assert_eq!(rto, ms(60)); // srtt 40 + max(4 * 5, 20)
        assert_eq!(deliver(&mut a, &mut b, rto, false).len(), 1);
    }

    /// Runs `frames` at 60 Hz: one 30-byte action every `every` frames, 1 % loss both ways, a token-bucket budget
    /// of `bps` bytes per second (or none). Returns (delivered, sent, wire/payload).
    fn thin_link(frames: i64, every: i64, bps: Option<u64>) -> (usize, usize, f64) {
        let (mut a, mut b) = (Link::new(LaneParams::default()), Link::new(LaneParams::default()));
        a.set_rtt(Duration::from_millis(80), Duration::from_millis(10));
        let mut seed = 12345u64;
        let mut lost = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33).is_multiple_of(100)
        };
        let (mut delivered, mut sent, mut credit) = (0, 0, 0u64);
        for f in 0..frames {
            let now = HostTime(f * 16_666_667);
            if f % every == 0 {
                a.send(Lane::Action, Bytes::from(vec![f as u8; 30]), MAX).unwrap();
                sent += 1;
            }
            if let Some(bps) = bps {
                credit = (credit + bps / 60).min(MAX as u64);
            }
            loop {
                let max = bps.map_or(MAX, |_| credit as usize);
                // As pe-net's queue decides (4.2): redundancy only while the budget is at least four times the
                // reliable send rate (30 B per message, 60 / every per second).
                let rate = 30 * 60 / every as u64;
                let redundancy = bps.is_none_or(|b| b >= 4 * rate);
                let Some(d) = a.poll_datagram(now, max, redundancy) else { break };
                credit = credit.saturating_sub(d.len() as u64);
                if !lost() {
                    delivered += b.on_datagram(now, &d).unwrap().messages.len();
                }
            }
            while let Some(d) = b.poll_datagram(now, MAX, true) {
                if !lost() {
                    let _ = a.on_datagram(now, &d).unwrap();
                }
            }
        }
        (delivered, sent, a.wire_bytes as f64 / a.payload_bytes as f64)
    }

    #[test]
    fn wire_over_payload_stays_under_1_5_on_a_thin_link() {
        // transport.md 9: 16 kbit/s = 2,000 B/s, 1 % loss. Redundancy only fills spare budget, so on a thin
        // link the wire stays near the payload plus framing and loss.
        let (delivered, sent, ratio) = thin_link(6000, 2, Some(2_000));
        assert!(delivered >= sent - 2, "{delivered} of {sent}");
        assert!(ratio < 1.5, "wire/payload {ratio:.2}");
        // With no budget limit, small messages ride twice (r = 2): about double, by design.
        let (_, _, free) = thin_link(6000, 2, None);
        assert!(free < 2.6, "{free:.2}");
        println!("thin link wire/payload {ratio:.2}; unlimited {free:.2}");
    }

    #[test]
    fn a_closed_link_carries_only_control_records() {
        let (mut a, mut b) = (Link::new(LaneParams::default()), Link::new(LaneParams::default()));
        a.set_open(false, false);
        b.set_open(true, false);
        a.send(Lane::Action, Bytes::from_static(b"act"), MAX).unwrap();
        a.send(Lane::Bulk, Bytes::from_static(b"blob"), MAX).unwrap();
        a.send_control(Control::ClockPing { t1: 7 });
        assert!(a.poll_stream(true).is_none());
        let d = a.poll_datagram(ms(0), MAX, true).unwrap();
        let inc = b.on_datagram(ms(0), &d).unwrap();
        assert_eq!(inc.control, vec![Control::ClockPing { t1: 7 }]);
        assert!(inc.messages.is_empty());
        // b sends before a takes anything in: a drops it unacknowledged, and b repeats it after the RTO.
        b.send(Lane::Protocol, Bytes::from_static(b"early"), MAX).unwrap();
        let d = b.poll_datagram(ms(1), MAX, false).unwrap();
        assert!(a.on_datagram(ms(1), &d).unwrap().messages.is_empty());
        a.set_open(true, true);
        let d = b.poll_datagram(ms(1_000), MAX, false).unwrap();
        assert_eq!(
            a.on_datagram(ms(1_000), &d).unwrap().messages,
            vec![(Lane::Protocol, Bytes::from_static(b"early"))]
        );
        // Once open, a's queued action and blob go out.
        assert!(a.poll_stream(true).is_some());
        b.set_open(true, true);
        let d = a.poll_datagram(ms(1_001), MAX, true).unwrap();
        assert_eq!(b.on_datagram(ms(1_001), &d).unwrap().messages, vec![(Lane::Action, Bytes::from_static(b"act"))]);
    }

    #[test]
    fn a_pose_is_superseded_and_late_ones_dropped() {
        let (mut a, mut b) = (Link::new(LaneParams::default()), Link::new(LaneParams::default()));
        a.send(Lane::Pose, Bytes::from_static(b"p1"), MAX).unwrap();
        a.send(Lane::Pose, Bytes::from_static(b"p2"), MAX).unwrap();
        let d2 = a.poll_datagram(ms(0), MAX, true).unwrap();
        assert!(a.poll_datagram(ms(0), MAX, true).is_none(), "p1 was replaced, never queued");
        a.send(Lane::Pose, Bytes::from_static(b"p3"), MAX).unwrap();
        let d3 = a.poll_datagram(ms(1), MAX, true).unwrap();
        assert_eq!(b.on_datagram(ms(2), &d3).unwrap().messages, vec![(Lane::Pose, Bytes::from_static(b"p3"))]);
        assert!(b.on_datagram(ms(3), &d2).unwrap().messages.is_empty(), "an older pose arriving late is dropped");
    }

    #[test]
    fn protocol_goes_first_when_space_is_short() {
        let mut a = Link::new(LaneParams::default());
        a.send(Lane::Action, Bytes::from(vec![1u8; 200]), MAX).unwrap();
        a.send(Lane::Protocol, Bytes::from(vec![2u8; 200]), MAX).unwrap();
        let d = a.poll_datagram(ms(0), 260, true).unwrap();
        let mut b = Link::new(LaneParams::default());
        assert_eq!(b.on_datagram(ms(0), &d).unwrap().messages[0].0, Lane::Protocol);
    }

    #[test]
    fn large_and_bulk_messages_take_streams_in_priority() {
        let mut a = Link::new(LaneParams::default());
        a.send(Lane::Bulk, Bytes::from(vec![0u8; 10]), MAX).unwrap();
        a.send(Lane::Repair, Bytes::from(vec![0u8; 10]), MAX).unwrap();
        a.send(Lane::Action, Bytes::from(vec![0u8; 900]), MAX).unwrap(); // over half a datagram
        let order: Vec<Lane> = std::iter::from_fn(|| a.poll_stream(true).map(|x| x.0)).collect();
        assert_eq!(order, vec![Lane::Action, Lane::Repair, Lane::Bulk]);
        let mut b = Link::new(LaneParams::default());
        assert!(b.on_stream(Lane::Action, 0, Bytes::new()).is_some());
        assert!(b.on_stream(Lane::Action, 0, Bytes::new()).is_none(), "once");
        assert_eq!(
            a.send(Lane::Action, Bytes::from(vec![0u8; (1 << 20) + 1]), MAX),
            Err(SendError::TooLarge { lane: Lane::Action, len: (1 << 20) + 1 })
        );
    }

    #[test]
    fn clock_records_ride_ahead() {
        let (mut a, mut b) = (Link::new(LaneParams::default()), Link::new(LaneParams::default()));
        a.send_control(Control::ClockPing { t1: -5 });
        a.send_control(Control::ClockPong { t1: 1, t2: i64::MIN, t3: i64::MAX });
        let d = a.poll_datagram(ms(0), MAX, true).unwrap();
        assert_eq!(
            b.on_datagram(ms(0), &d).unwrap().control,
            vec![Control::ClockPing { t1: -5 }, Control::ClockPong { t1: 1, t2: i64::MIN, t3: i64::MAX }]
        );
    }

    #[test]
    fn malformed_datagrams_are_errors_never_panics() {
        let mut a = Link::new(LaneParams::default());
        for i in 0..10u8 {
            a.send(Lane::Action, Bytes::from(vec![i; 30]), MAX).unwrap();
        }
        a.send(Lane::Pose, Bytes::from_static(b"pose"), MAX).unwrap();
        a.send_control(Control::ClockPing { t1: 9 });
        let d = a.poll_datagram(ms(0), MAX, true).unwrap();
        let mut b = Link::new(LaneParams::default());
        for n in 0..d.len() {
            let _ = b.on_datagram(ms(0), &d[..n]);
        }
        for pos in 0..d.len() {
            for v in [0u8, 1, 0x3f, 0x40, 0x7f, 0x80, 0xbf, 0xc0, 0xff] {
                let mut x = d.clone();
                x[pos] = v;
                let _ = Link::new(LaneParams::default()).on_datagram(ms(0), &x);
            }
        }
        assert_eq!(b.on_datagram(ms(0), &[2]), Err(FrameError::BadVersion(2)));
        assert_eq!(b.on_datagram(ms(0), &[]), Err(FrameError::Truncated));
    }
}

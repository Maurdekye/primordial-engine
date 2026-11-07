//! In-process session simulator: N peers, M worlds, a virtual clock and virtual links with delay,
//! jitter, loss and scripted faults. Every message is encoded to bytes, so byte counts are real.
//! The same peer logic drives the UDP spike (spikes/p2p) with a real clock and sockets.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use pe_sim_core::session::*;
use pe_sim_core::ownership::{Change, Key};
use pe_sim_core::splitmix64;

pub type WorldId = u32;

#[derive(Clone, Copy, Debug)]
pub struct Link {
    pub delay_us: u64,
    pub jitter_us: u64,
    pub loss_pct: u32,
}

/// Wire overhead per datagram (IP/UDP/AEAD/session), as in the ball-pit wire estimate.
pub const DATAGRAM_OVERHEAD: u64 = 52;
/// The per-datagram overhead in use: DATAGRAM_OVERHEAD (phase 3, Noise), or PE_DGRAM_OVERHEAD for a run (today's
/// signed datagrams: 117).
pub fn dgram_overhead() -> u64 {
    static V: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("PE_DGRAM_OVERHEAD").ok().and_then(|v| v.parse().ok()).unwrap_or(DATAGRAM_OVERHEAD))
}
/// (spike: PE_DROP_PAUSED_KICKS=1, the harness drops a planned kick while its admin is paused, as before mdffbecdc9b48)
fn drop_paused_kicks() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("PE_DROP_PAUSED_KICKS").is_ok())
}

/// (spike: PE_FED_MIN_LEVEL=L replaces `fed_level_floor` for a run, for measuring; 0 is no floor)
fn fed_min_level_override() -> Option<usize> {
    static V: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("PE_FED_MIN_LEVEL").ok().and_then(|v| v.parse().ok()))
}
/// Largest datagram payload a batch may fill (review R3-9).
pub const MTU: usize = 1200;
/// A member answers a fetch only while its uplink to the requester holds less than this; the
/// requester asks again later, with backoff (review R3-9).
pub const REPAIR_MAX_BACKLOG_US: u64 = 100_000;

/// (spike measurement: PE_SHARED_DOWN=<member>:<kbit/s>) that member's downlink as one shared bottleneck: every datagram into
/// it goes through one token bucket and queue of that rate (4 kB burst, 500 ms queue), as a real access link does, instead
/// of a capped queue per sender. For the withholding check (@design-reviewer mb4caa9f500a3): with a cap per link, a
/// withholding holder halves the honest one's spare; on a shared downlink the honest one can use all of it.
fn shared_down() -> Option<(MemberId, Cap)> {
    static V: std::sync::OnceLock<Option<(MemberId, Cap)>> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let v = std::env::var("PE_SHARED_DOWN").ok()?;
        let (m, k) = v.split_once(':')?;
        Some((m.parse().ok()?, Cap { rate_bps: k.parse::<u64>().ok()? * 1000, burst_bytes: 4000, max_queue_us: 500_000 }))
    })
}

/// Bandwidth cap for one directed link (ported unchanged from @ballpit-viz, 2996d39): a token
/// bucket feeding a FIFO queue. Tokens refill at `rate_bps` bits/s up to `burst_bytes`; a datagram
/// leaves the queue when the bucket holds its wire size. A datagram that would wait longer than
/// `max_queue_us` is dropped (tail drop), as a router with a full buffer would.
#[derive(Clone, Copy, Debug)]
pub struct Cap {
    pub rate_bps: u64,
    pub burst_bytes: u64,
    pub max_queue_us: u64,
}

/// Token-bucket state of one directed link.
#[derive(Clone, Copy, Debug, Default)]
pub struct Shaper {
    /// Bucket level (bytes) at `t_ref_us`, the departure time of the last queued datagram.
    tokens: f64,
    t_ref_us: u64,
    started: bool,
}

impl Shaper {
    /// Departure time from the bottleneck for a datagram of `wire` bytes offered at `now_us`, or
    /// None if the queue is full. FIFO: never departs before the previous datagram.
    pub fn offer(&mut self, cap: &Cap, now_us: u64, wire: u64) -> Option<u64> {
        let rate = cap.rate_bps as f64 / 8.0 / 1e6; // bytes per microsecond
        if !self.started {
            self.started = true;
            self.tokens = cap.burst_bytes as f64;
            self.t_ref_us = now_us;
        }
        let t0 = now_us.max(self.t_ref_us);
        let have = (self.tokens + (t0 - self.t_ref_us) as f64 * rate).min(cap.burst_bytes as f64);
        let (depart, left) = if have >= wire as f64 {
            (t0, have - wire as f64)
        } else {
            (t0 + ((wire as f64 - have) / rate).ceil() as u64, 0.0)
        };
        if depart - now_us > cap.max_queue_us {
            return None;
        }
        self.tokens = left;
        self.t_ref_us = depart;
        Some(depart)
    }

    /// How long a datagram offered now would wait (queue backlog), in microseconds.
    pub fn backlog_us(&self, now_us: u64) -> u64 {
        self.t_ref_us.saturating_sub(now_us)
    }
}

/// One send attempt, for per-peer traffic metrics.
#[derive(Clone, Copy, Debug)]
pub struct SendRec {
    pub at_us: u64,
    pub from: MemberId,
    pub to: MemberId,
    /// Payload plus [`DATAGRAM_OVERHEAD`].
    pub wire: u64,
    /// 0 sent, 1 dropped by a full bandwidth queue, 2 lost on the link or by a filter.
    pub fate: u8,
    /// Arrival time at the receiver (fate 0 only).
    pub arrive_us: u64,
}

/// What a scripted fault does to one message.
pub enum Fate {
    Deliver,
    Drop,
    Delay(u64),
}

pub type Filter = Box<dyn FnMut(u64, WorldId, MemberId, MemberId, &Msg) -> Fate>;

pub struct Net {
    /// Thin links Part B, rule 5: the receivers on the state feed, per world. No entries are sent to them.
    pub fed: BTreeSet<(WorldId, MemberId)>,
    /// Bytes offered per directed link, with when, over the last second (the feed controller's leave test).
    recent: BTreeMap<(MemberId, MemberId), std::collections::VecDeque<(u64, u64)>>,
    pub now_us: u64,
    seq: u64,
    queue: BinaryHeap<Reverse<(u64, u64, MemberId, WorldId, MemberId, Vec<u8>)>>,
    pub links: BTreeMap<(MemberId, MemberId), Link>,
    pub default_link: Link,
    rng: u64,
    pub filter: Option<Filter>,
    /// (world, kind) -> (messages, payload bytes)
    pub sent: BTreeMap<(WorldId, &'static str), (u64, u64)>,
    pub dropped: u64,
    /// Count only from this virtual time (to measure a window).
    pub count_from_us: u64,
    pub count_to_us: u64,
    /// Bandwidth caps per directed link (none = unlimited). See [`Cap`].
    pub caps: BTreeMap<(MemberId, MemberId), Cap>,
    pub default_cap: Option<Cap>,
    pub shapers: BTreeMap<(MemberId, MemberId), Shaper>,
    /// Datagrams dropped by a full bandwidth queue (also counted in `dropped`).
    pub cap_dropped: u64,
    /// Wire bytes (payload + overhead) offered per sender, in the counting window.
    pub wire_from: BTreeMap<MemberId, u64>,
    /// The same bytes by message kind (inside batches too), per sender: (messages, bytes). Batch
    /// framing and the per-datagram overhead are their own kinds, so the kinds sum to `wire_from`.
    pub kinds_from: BTreeMap<(MemberId, &'static str), (u64, u64)>,
    /// Measurement only (`--by-kind`): each sender's entries by kind, split by part ([`entry_parts`]).
    pub parts_from: BTreeMap<(MemberId, &'static str), [u64; 6]>,
    /// The rest of each sender's bytes: message and batch framing, and messages other than entries.
    pub framing_from: BTreeMap<MemberId, u64>,
    /// R18 (viz-4): merge everything one member sends one peer in one tick into one datagram.
    pub batching: bool,
    outbox: BTreeMap<(WorldId, MemberId, MemberId), Vec<Msg>>,
    /// Rev 22, ack priority: a fed member's client queue per directed link, (protocol, own inputs), each (world, msg).
    client_q: BTreeMap<(MemberId, MemberId), (std::collections::VecDeque<(WorldId, Msg)>, std::collections::VecDeque<(WorldId, Msg)>)>,
    /// The feed exit's background catch-up (@coordinator-opus m5eeeaff3c105): per directed link to a fed member, the
    /// entries of fetch answers waiting at the lowest priority, below feed and echo. A datagram of them leaves only while
    /// the link's router queue and client queues are empty, and holds at most what the link carries in 100 ms (one entry
    /// at least), so a feed packet or a Ckpts waits behind at most that.
    catchup_q: BTreeMap<(MemberId, MemberId), std::collections::VecDeque<(WorldId, Msg)>>,
    /// Entries sent through `catchup_q`, and their wire bytes.
    pub catchup_sent: (u64, u64),
    /// Links whose catch-up is held this tick: its source's next checkpoint (and the Ckpts announcing it) is due within one
    /// catch-up datagram, so the echo doesn't wait behind one (set by the harness before each flush).
    pub catchup_closed: BTreeSet<(MemberId, MemberId)>,
    /// The feed exit (b): an echoing member's own catch-up, the answers to fetches it sent within the ranges it asked, sent
    /// at once past R3-9's yield in datagrams of their own (`send_catchup`). Per link, each datagram's departure from the
    /// bottleneck and its serialization time, and its bytes with when, so the enter test can leave that pull out (margin
    /// rule 1, @coordinator-opus m360a9ee1d5bb) and test only the live stream (rule 2).
    catchup_inflight: BTreeMap<(MemberId, MemberId), std::collections::VecDeque<(u64, u64)>>,
    catchup_recent: BTreeMap<(MemberId, MemberId), std::collections::VecDeque<(u64, u64)>>,
    /// The feed exit: members in their echo phase this tick (set by the harness). Like fed members, they get explicit
    /// checkpoints, never live ones: behind the hole an echoing member catches up on, live checkpoints waited (capped per
    /// author, the farthest evicted), and each evicted one then held its chain until fetched again, so the catch-up walked
    /// to the live head in 16-entry answers (merged down-k12-m3: still frozen at 90 s; 128 of 293 live checkpoints held).
    pub echoing: BTreeSet<(WorldId, MemberId)>,
    /// Every send attempt, when `tracing` is on.
    pub tracing: bool,
    /// (spike diagnostics, with `tracing`) Every entry posted: (now, from, to, author, seq, kind, to a fed member).
    pub post_log: Vec<(u64, MemberId, MemberId, MemberId, u32, &'static str, bool)>,
    pub trace: Vec<SendRec>,
}

/// Measurement only (`--by-kind`): the entries of `m` by part, as (entry kind, [entries, header
/// (member, seq, stamp), prev id, ack head ids, payload, signatures]) in bytes on the wire (the spike
/// signs nothing and its ids are 8 bytes); returns the rest of `m`'s bytes (message and batch framing;
/// all of a message that carries no entry).
pub fn entry_parts(m: &Msg, out: &mut Vec<(&'static str, [u64; 6])>) -> u64 {
    let total = encode_msg(m).len() as u64;
    match m {
        Msg::Entry(e) | Msg::Live(e) => {
            let nh = match &e.payload {
                Payload::Ack { heads, .. } => heads.len() as u64,
                _ => 0,
            };
            let full = total - 1;
            out.push((kind(m), [1, 16, 8, 8 * nh, full - 16 - 8 - 8 * nh, 0]));
            1
        }
        Msg::Batch(v) => {
            let mut inner = 0;
            for x in v {
                let before = out.len();
                entry_parts(x, out);
                inner += out[before..].iter().map(|(_, p)| p[1..].iter().sum::<u64>()).sum::<u64>();
            }
            total - inner
        }
        _ => total,
    }
}

pub fn kind(m: &Msg) -> &'static str {
    match m {
        Msg::Entry(e) | Msg::Live(e) => match e.payload {
            Payload::Touch { .. } => "entry/touch",
            Payload::Hand { .. } => "entry/hand",
            Payload::Ack { .. } => "ack",
            Payload::Vote { .. } | Payload::Propose { .. } | Payload::Prevote { .. } | Payload::Precommit { .. } => "vote",
            Payload::Kick { .. } | Payload::SetRole { .. } | Payload::DefineRole { .. } | Payload::Ownership { .. } => "admin",
            Payload::Attest { .. } => "attest",
            Payload::Provenance { .. } => "provenance",
            _ => "entry/transfer",
        },
        Msg::Fetch { .. } | Msg::FetchValue { .. } | Msg::FetchEntry { .. } => "fetch",
        Msg::Join { .. } => "join",
        Msg::Batch(_) => "batch",
        Msg::Feed { .. } => "feed",
        Msg::Ckpts { .. } => "ckpts",
        Msg::CertReq { .. } => "certreq",
        Msg::Certs { .. } => "certs",
    }
}

impl Net {
    pub fn new(default_link: Link, seed: u64) -> Self {
        Net {
            fed: BTreeSet::new(),
            recent: BTreeMap::new(),
            now_us: 0,
            seq: 0,
            queue: BinaryHeap::new(),
            links: BTreeMap::new(),
            default_link,
            rng: seed,
            filter: None,
            sent: BTreeMap::new(),
            dropped: 0,
            count_from_us: 0,
            count_to_us: u64::MAX,
            caps: BTreeMap::new(),
            default_cap: None,
            shapers: BTreeMap::new(),
            cap_dropped: 0,
            wire_from: BTreeMap::new(),
            kinds_from: BTreeMap::new(),
            parts_from: BTreeMap::new(),
            framing_from: BTreeMap::new(),
            batching: false,
            outbox: BTreeMap::new(),
            client_q: BTreeMap::new(),
            catchup_q: BTreeMap::new(),
            catchup_sent: (0, 0),
            catchup_closed: BTreeSet::new(),
            catchup_inflight: BTreeMap::new(),
            catchup_recent: BTreeMap::new(),
            echoing: BTreeSet::new(),
            tracing: false,
            post_log: Vec::new(),
            trace: Vec::new(),
        }
    }

    fn rand(&mut self) -> u64 {
        self.rng = splitmix64(self.rng);
        self.rng
    }

    /// Bits per second offered on the link from `from` to `to` over the last second.
    pub fn recent_bps(&self, from: MemberId, to: MemberId) -> u64 {
        let since = self.now_us.saturating_sub(1_000_000);
        self.recent.get(&(from, to)).map_or(0, |q| q.iter().filter(|(t, _)| *t >= since).map(|(_, b)| b * 8).sum())
    }

    pub fn send(&mut self, world: WorldId, from: MemberId, to: MemberId, m: &Msg) {
        let bytes = encode_msg(m);
        // (spike, measurement only: PE_WHATIF_LIVE=1 with PE_NO_LIVE=1 charges every link elision would leave out as free)
        let charged = bytes.len() as u64 - whatif_live_saving(from, m);
        {
            let now = self.now_us;
            let q = self.recent.entry((from, to)).or_default();
            q.push_back((now, charged + dgram_overhead()));
            while q.front().is_some_and(|&(t, _)| t + 1_000_000 < now) {
                q.pop_front();
            }
        }
        if self.now_us >= self.count_from_us && self.now_us < self.count_to_us {
            let c = self.sent.entry((world, kind(m))).or_default();
            c.0 += 1;
            c.1 += bytes.len() as u64;
        }
        let link = self.links.get(&(from, to)).copied().unwrap_or(self.default_link);
        let mut extra = 0;
        if let Some(f) = self.filter.as_mut() {
            // c6e564f's rule (as in the crate harness for option 2's runs): a datagram of two or more messages meets
            // the filter message by message, as each would alone. A filter written for entries otherwise let every
            // batched datagram past it (partbrand's Split and Rotate faults). One datagram: dropped if any part is,
            // else delayed by the longest delay.
            let fate = match m {
                Msg::Batch(v) if v.len() > 1 => {
                    let mut fate = Fate::Deliver;
                    for x in v {
                        match (f(self.now_us, world, from, to, &explicit_view(x)), &fate) {
                            (Fate::Drop, _) => fate = Fate::Drop,
                            (Fate::Delay(d), Fate::Deliver) => fate = Fate::Delay(d),
                            (Fate::Delay(d), Fate::Delay(e)) if d > *e => fate = Fate::Delay(d),
                            _ => {}
                        }
                    }
                    fate
                }
                _ => f(self.now_us, world, from, to, &explicit_view(m)),
            };
            match fate {
                Fate::Deliver => {}
                Fate::Drop => {
                    self.dropped += 1;
                    self.record(from, to, bytes.len() as u64, 2, 0);
                    return;
                }
                Fate::Delay(d) => extra = d,
            }
        }
        if self.now_us >= self.count_from_us && self.now_us < self.count_to_us {
            *self.wire_from.entry(from).or_default() += bytes.len() as u64 + dgram_overhead();
            let mut count = |k: &'static str, n: u64| {
                let c = self.kinds_from.entry((from, k)).or_default();
                c.0 += 1;
                c.1 += n;
            };
            count("(datagram overhead)", dgram_overhead());
            match m {
                Msg::Batch(v) => {
                    let mut inner = 0;
                    for x in v {
                        let n = encode_msg(x).len() as u64;
                        inner += n;
                        count(kind(x), n);
                    }
                    count("(batch framing)", bytes.len() as u64 - inner);
                }
                _ => count(kind(m), bytes.len() as u64),
            }
            let mut v = Vec::new();
            let rest = entry_parts(m, &mut v);
            *self.framing_from.entry(from).or_default() += rest;
            for (k, p) in v {
                let c = self.parts_from.entry((from, k)).or_default();
                for i in 0..6 {
                    c[i] += p[i];
                }
            }
        }
        // Bandwidth cap: the datagram leaves the bottleneck queue at `depart` (or is tail-dropped).
        let depart = match self.shape(from, to, charged) {
            Some(d) => d,
            None => {
                self.dropped += 1;
                self.cap_dropped += 1;
                self.record(from, to, bytes.len() as u64, 1, 0);
                return;
            }
        };
        if link.loss_pct > 0 && (self.rand() % 100) < link.loss_pct as u64 {
            self.dropped += 1;
            self.record(from, to, bytes.len() as u64, 2, 0);
            return;
        }
        let jitter = if link.jitter_us > 0 { self.rand() % (link.jitter_us + 1) } else { 0 };
        let at = depart + link.delay_us + jitter + extra;
        self.record(from, to, bytes.len() as u64, 0, at);
        self.seq += 1;
        self.queue.push(Reverse((at, self.seq, to, world, from, bytes)));
    }

    /// Revision 22, D-5: a leave request goes first, ahead of the sender's queued datagrams (its client puts it at the
    /// head of its send queue): it leaves after its own serialization time and is never tail-dropped. Not batched.
    pub fn send_urgent(&mut self, world: WorldId, from: MemberId, to: MemberId, m: &Msg) {
        let bytes = encode_msg(m);
        let link = self.links.get(&(from, to)).copied().unwrap_or(self.default_link);
        let ser = match self.caps.get(&(from, to)).copied().or(self.default_cap) {
            Some(cap) => ((bytes.len() as u64 + dgram_overhead()) as f64 * 8.0 * 1e6 / cap.rate_bps as f64).ceil() as u64,
            None => 0,
        };
        let jitter = if link.jitter_us > 0 { self.rand() % (link.jitter_us + 1) } else { 0 };
        let at = self.now_us + ser + link.delay_us + jitter;
        self.record(from, to, bytes.len() as u64, 0, at);
        self.seq += 1;
        self.queue.push(Reverse((at, self.seq, to, world, from, bytes)));
    }

    /// Departure time through the link's bandwidth cap for a payload of `payload` bytes (now, if
    /// the link is uncapped), or None if its queue is full.
    pub fn shape(&mut self, from: MemberId, to: MemberId, payload: u64) -> Option<u64> {
        if let Some((m, cap)) = shared_down().filter(|&(m, _)| m == to) {
            return self.shapers.entry((0, m)).or_default().offer(&cap, self.now_us, payload + dgram_overhead());
        }
        let Some(cap) = self.caps.get(&(from, to)).copied().or(self.default_cap) else { return Some(self.now_us) };
        self.shapers.entry((from, to)).or_default().offer(&cap, self.now_us, payload + dgram_overhead())
    }

    /// Current queue backlog of a capped link, in microseconds (0 if uncapped): the router queue plus, for a fed
    /// member, its client queue (rev 22 ack priority), so the R18 controller still climbs when its inputs back up.
    pub fn backlog_us(&self, from: MemberId, to: MemberId) -> u64 {
        let router = self.router_backlog_us(from, to);
        let client = match (self.client_q.get(&(from, to)), self.caps.get(&(from, to)).copied().or(self.default_cap)) {
            (Some((hi, lo)), Some(cap)) if !hi.is_empty() || !lo.is_empty() => {
                let bytes: u64 = hi.iter().chain(lo.iter()).map(|(_, m)| encode_msg(m).len() as u64).sum();
                let datagrams = bytes / MTU as u64 + 1;
                ((bytes + datagrams * dgram_overhead()) as f64 * 8.0 * 1e6 / cap.rate_bps as f64) as u64
            }
            _ => 0,
        };
        router + client
    }

    /// The router queue alone (what a datagram offered now would wait at the bottleneck).
    fn router_backlog_us(&self, from: MemberId, to: MemberId) -> u64 {
        let key = if shared_down().is_some_and(|(m, _)| m == to) { (0, to) } else { (from, to) };
        self.shapers.get(&key).map_or(0, |s| s.backlog_us(self.now_us))
    }

    /// Rev 22 ack priority: a fed member's client sends its protocol messages ahead of its own inputs. Each tick it
    /// releases datagrams, protocol first, while the router queue is empty (the harness stand-in for pacing at its
    /// estimated rate; production: R18's delay-gradient capacity estimate). Inputs wait in the client queue and run late; rule 1
    /// voids what arrives past its deadline ("a spectator in effect"), and the echo stays timely.
    fn release_client_queues(&mut self) {
        let keys: Vec<(MemberId, MemberId)> = self.client_q.iter().filter(|(_, (hi, lo))| !hi.is_empty() || !lo.is_empty()).map(|(k, _)| *k).collect();
        for (from, to) in keys {
            if self.caps.get(&(from, to)).copied().or(self.default_cap).is_none() {
                continue;
            }
            // Pacing at the link rate: the next datagram goes only once the router queue is empty (the bucket's burst
            // still passes at once). One full datagram's worth of queue is over a second at 8 kbit/s.
            loop {
                if self.router_backlog_us(from, to) > 0 {
                    break;
                }
                let (hi, lo) = self.client_q.get_mut(&(from, to)).unwrap();
                let Some(w) = hi.front().or(lo.front()).map(|x| x.0) else { break };
                let mut cur: Vec<Msg> = Vec::new();
                let mut size = 0;
                for q in [&mut *hi, &mut *lo] {
                    while let Some((w2, m)) = q.front() {
                        let n = encode_msg(m).len();
                        if *w2 != w || (!cur.is_empty() && size + n > MTU) {
                            break;
                        }
                        size += n;
                        cur.push(q.pop_front().unwrap().1);
                    }
                }
                if cur.is_empty() {
                    break;
                }
                self.send_batch(w, from, to, cur);
            }
        }
        // The feed exit's background catch-up: the lowest priority, one datagram at a time while the link is idle.
        let keys: Vec<(MemberId, MemberId)> = self.catchup_q.iter().filter(|(_, q)| !q.is_empty()).map(|(k, _)| *k).collect();
        for (from, to) in keys {
            let busy = self.router_backlog_us(from, to) > 0
                || self.client_q.get(&(from, to)).is_some_and(|(h, l)| !h.is_empty() || !l.is_empty())
                || self.catchup_closed.contains(&(from, to));
            if busy {
                continue;
            }
            let budget = match self.caps.get(&(from, to)).copied().or(self.default_cap) {
                Some(cap) => ((cap.rate_bps / 8 / 10) as usize).saturating_sub(dgram_overhead() as usize).min(MTU),
                None => MTU,
            };
            let q = self.catchup_q.get_mut(&(from, to)).unwrap();
            let w = q.front().unwrap().0;
            let mut cur: Vec<Msg> = Vec::new();
            let mut size = 0;
            while let Some((w2, m)) = q.front() {
                let n = encode_msg(m).len();
                if *w2 != w || (!cur.is_empty() && size + n > budget) {
                    break;
                }
                size += n;
                cur.push(q.pop_front().unwrap().1);
            }
            self.catchup_sent.0 += cur.len() as u64;
            self.catchup_sent.1 += size as u64 + dgram_overhead();
            self.send_batch(w, from, to, cur);
        }
    }

    /// The feed exit's background catch-up: queue `m` (an entry answering a fed member's fetch) at the lowest priority on
    /// the link from `from` to `to`, past the rule-5 filter (the fed member asked for it).
    pub fn post_catchup(&mut self, world: WorldId, from: MemberId, to: MemberId, m: &Msg) {
        self.catchup_q.entry((from, to)).or_default().push_back((world, m.clone()));
    }

    /// Whether the link from `from` to `to` still has catch-up entries waiting.
    pub fn catchup_pending(&self, from: MemberId, to: MemberId) -> bool {
        self.catchup_q.get(&(from, to)).is_some_and(|q| !q.is_empty())
    }

    /// The feed exit (b): an echoing member's own catch-up answer, sent at once (it doesn't yield to the queue) in MTU
    /// datagrams of its own, each recorded so the enter test can leave it out.
    pub fn send_catchup(&mut self, world: WorldId, from: MemberId, to: MemberId, msgs: Vec<Msg>) {
        let mut cur: Vec<Msg> = Vec::new();
        let mut size = 0;
        let mut batches: Vec<Vec<Msg>> = Vec::new();
        // (spike: PE_CATCHUP_DGRAM_MS caps each datagram at that much of the link; 0, the default, sends MTU datagrams. A
        // 200 ms cap was slower at every rate measured, with more drops: down-k12-m3 120 against 50.)
        static DG: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let dg_ms = *DG.get_or_init(|| std::env::var("PE_CATCHUP_DGRAM_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0));
        let budget = match self.caps.get(&(from, to)).copied().or(self.default_cap) {
            Some(cap) if dg_ms > 0 => ((cap.rate_bps / 8 * dg_ms / 1000) as usize).saturating_sub(dgram_overhead() as usize).min(MTU),
            _ => MTU,
        };
        for m in msgs {
            let n = encode_msg(&m).len();
            if !cur.is_empty() && size + n > budget {
                batches.push(std::mem::take(&mut cur));
                size = 0;
            }
            size += n;
            cur.push(m);
        }
        if !cur.is_empty() {
            batches.push(cur);
        }
        let rate = self.caps.get(&(from, to)).copied().or(self.default_cap).map(|c| c.rate_bps);
        for v in batches {
            let m = if v.len() == 1 { v.into_iter().next().unwrap() } else { Msg::Batch(v) };
            let wire = encode_msg(&m).len() as u64 + dgram_overhead();
            let before = self.shapers.get(&(from, to)).map(|s| s.t_ref_us);
            self.send(world, from, to, &m);
            let now = self.now_us;
            let r = self.catchup_recent.entry((from, to)).or_default();
            r.push_back((now, wire));
            while r.front().is_some_and(|&(t, _)| t + 1_000_000 < now) {
                r.pop_front();
            }
            if let (Some(rate), Some(after)) = (rate, self.shapers.get(&(from, to)).map(|s| s.t_ref_us)) {
                if Some(after) != before {
                    self.catchup_inflight.entry((from, to)).or_default().push_back((after, (wire as f64 * 8.0 * 1e6 / rate as f64) as u64));
                }
            }
        }
    }

    /// Margin rule (1): the backlog on a link less what an echoing member's own catch-up still holds in it.
    pub fn live_backlog_us(&self, from: MemberId, to: MemberId) -> u64 {
        let pull: u64 = self.catchup_inflight.get(&(from, to)).map_or(0, |q| q.iter().filter(|&&(d, _)| d > self.now_us).map(|&(_, s)| s).sum());
        self.backlog_us(from, to).saturating_sub(pull)
    }

    /// Margin rule (2): the bits per second a sender offered on a link over the last second, less its catch-up answers.
    pub fn live_bps(&self, from: MemberId, to: MemberId) -> u64 {
        let since = self.now_us.saturating_sub(1_000_000);
        let pull: u64 = self.catchup_recent.get(&(from, to)).map_or(0, |q| q.iter().filter(|(t, _)| *t >= since).map(|(_, b)| b * 8).sum());
        self.recent_bps(from, to).saturating_sub(pull)
    }

    /// The links with catch-up entries waiting: (from, to, the world of the next one).
    pub fn catchup_links(&self) -> Vec<(MemberId, MemberId, WorldId)> {
        self.catchup_q.iter().filter_map(|(&(f, t), q)| q.front().map(|(w, _)| (f, t, *w))).collect()
    }

    /// Send now, or queue for this tick's batch to that peer.
    pub fn post(&mut self, world: WorldId, from: MemberId, to: MemberId, m: &Msg) {
        if self.tracing {
            if let Msg::Entry(e) | Msg::Live(e) = m {
                let filtered = self.fed.contains(&(world, to));
                self.post_log.push((self.now_us, from, to, e.member, e.seq, kind(m), filtered));
            }
        }
        // Thin links Part B, rule 5: no entries to a member on the state feed, but the view change's (revision 21:
        // a fed member votes and takes part in the decision; they are few and small), and every admin entry: a
        // decided change is applied only once each admin entry it lists is held (VCR-8), so without them a fed member
        // never applied a change listing a SetRole and stayed in that epoch for good (e1-same-promote under
        // carry-over: no ack of it declared a later epoch, so the n = 3 epoch after it could vouch nothing).
        if let Msg::Entry(e) | Msg::Live(e) = m {
            let view_change = matches!(
                e.payload,
                Payload::Vote { .. }
                    | Payload::Propose { .. }
                    | Payload::Prevote { .. }
                    | Payload::Precommit { .. }
                    | Payload::Kick { .. }
                    | Payload::SetRole { .. }
                    | Payload::DefineRole { .. }
                    | Payload::Ownership { .. }
            );
            if self.fed.contains(&(world, to)) && !view_change {
                return;
            }
        }
        if self.batching {
            self.outbox.entry((world, from, to)).or_default().push(m.clone());
        } else {
            self.send(world, from, to, m);
        }
    }

    /// End of tick: everything one member sends one peer, in datagrams of at most `MTU` bytes.
    /// Review R3-9: unsplit, a tick's repairs became one multi-kilobyte datagram, which a full
    /// router queue dropped whole.
    pub fn flush(&mut self) {
        // (spike: PE_NO_ACK_PRIORITY switches rev 22's ack priority off, for comparison)
        let prio = std::env::var("PE_NO_ACK_PRIORITY").is_err();
        for ((w, from, to), v) in std::mem::take(&mut self.outbox) {
            // A fed member's datagrams go through its client queue; so do a member's just off the feed while the queue
            // still holds older ones (in order).
            let queued = self.client_q.get(&(from, to)).is_some_and(|(h, l)| !h.is_empty() || !l.is_empty());
            if prio && (self.fed.contains(&(w, from)) || queued) && self.caps.get(&(from, to)).or(self.default_cap.as_ref()).is_some() {
                let (hi, lo) = self.client_q.entry((from, to)).or_default();
                for m in v {
                    let protocol = match &m {
                        Msg::Entry(e) | Msg::Live(e) => is_protocol(&e.payload),
                        _ => true,
                    };
                    if protocol { hi.push_back((w, m)) } else { lo.push_back((w, m)) }
                }
                continue;
            }
            let mut cur: Vec<Msg> = Vec::new();
            let mut size = 0;
            for m in v {
                let n = encode_msg(&m).len();
                if !cur.is_empty() && size + n > MTU {
                    self.send_batch(w, from, to, std::mem::take(&mut cur));
                    size = 0;
                }
                size += n;
                cur.push(m);
            }
            if !cur.is_empty() {
                self.send_batch(w, from, to, cur);
            }
        }
        self.release_client_queues();
    }

    fn send_batch(&mut self, w: WorldId, from: MemberId, to: MemberId, mut v: Vec<Msg>) {
        let m = if v.len() == 1 { v.pop().unwrap() } else { Msg::Batch(v) };
        self.send(w, from, to, &m);
    }

    fn record(&mut self, from: MemberId, to: MemberId, payload: u64, fate: u8, arrive_us: u64) {
        if self.tracing {
            self.trace.push(SendRec { at_us: self.now_us, from, to, wire: payload + dgram_overhead(), fate, arrive_us });
        }
    }

    /// Messages due now: (to, world, from, msg).
    pub fn due(&mut self) -> Vec<(MemberId, WorldId, MemberId, Msg)> {
        let mut out = Vec::new();
        while let Some(Reverse((at, ..))) = self.queue.peek() {
            if *at > self.now_us {
                break;
            }
            let Reverse((_, _, to, w, from, b)) = self.queue.pop().unwrap();
            out.push((to, w, from, decode_msg(&b).expect("decode")));
        }
        out
    }

    pub fn bytes(&self, world: WorldId, prefix: &str) -> (u64, u64) {
        self.sent
            .iter()
            .filter(|((w, k), _)| *w == world && k.starts_with(prefix))
            .fold((0, 0), |a, (_, v)| (a.0 + v.0, a.1 + v.1))
    }
}

/// A planned player action at a frame of one world.
#[derive(Clone, Debug)]
pub enum Action {
    Touch { obj: u32, impulse: [f32; 3], grab: bool },
    Hand { obj: u32, target_mm: [i32; 3], active: bool, ahead: u64 },
    Portal { open: bool },
    /// Start a cross-world transfer: depart `obj` from this world towards `dest`.
    Carry { transfer: u64, obj: u32, dest: WorldId },
    Backdated { obj: u32, impulse: [f32; 3], back: u64 },
    /// D80: kick a member, give a member a role, define a role (admin entries).
    Kick { target: MemberId },
    /// Revision 22: ask to leave (a vote on oneself without a kick).
    Leave,
    SetRole { member: MemberId, role: RoleId },
    DefineRole { role: RoleId, perms: u64 },
    /// World ownership: publish a certificate for `change` on this replica's ownership head,
    /// signed by `keys`.
    Owner { change: Change, keys: Vec<Key> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobPhase {
    WaitDepart,
    WaitArrive,
    WaitVerdict,
    /// P2-5: the transfer committed; relay the source's commit certificate (the hop) into the
    /// destination, so the object can leave it again.
    WaitCommit,
    Done,
}

/// Carrier state machine for one R14 transfer (settled handoff, design 22.6).
#[derive(Clone, Debug)]
pub struct Job {
    pub transfer: u64,
    pub src: WorldId,
    pub dst: WorldId,
    pub depart: EntryId,
    pub arrive: Option<EntryId>,
    pub verdict: Option<EntryId>,
    pub phase: JobPhase,
    pub accepted: Option<bool>,
    /// The departed record (P2-5: its global id and `via` name the hop to relay).
    pub record: Option<ObjRecord>,
    pub log: Vec<String>,
}

pub struct Replica {
    pub sim: WorldSim,
    pub scene: SceneKind,
    pub epoch_us: u64,
    pub dt_us: u64,
    /// Background mode: advance only every `period_us`, in one burst (design 22.3).
    pub throttle_us: Option<u64>,
    pub throttle_from_us: u64,
    last_burst_us: u64,
    pub plan: BTreeMap<u64, Vec<Action>>,
    next_ack_us: u64,
    pub ack_every_us: u64,
    /// 7.6 reduction 2: whether the member was idle at its last ack decision.
    was_idle: bool,
    /// Test fixture (review VCR-12 under 7.6 reduction 2): ack only on the member's own interval, never
    /// at once on leaving idle, so a member set to ack rarely keeps a stale horizon.
    pub ack_grid_only: bool,
    last_frame_seen: u64,
    /// R18: adapt the hand-entry rate and ack interval to this member's uplink queue.
    pub adapt: bool,
    pub level: usize,
    level_t: u64,
    /// Thin links (Part A): the uplink queue delay (us) when the level last changed, to tell a queue still
    /// growing at the coarsest packed level from one draining there.
    level_b: u64,
    calm_since: u64,
    /// The controller's last tick (us) at which the member was idle (7.6 reduction 2): calm before it does
    /// not count toward lowering into D56's remembered level.
    idle_at: u64,
    /// D56: the level at which congestion began (the highest such level while the memory lasts), and
    /// when the uplink was last congested. The controller lowers into that level or any finer one only
    /// after a longer calm (`CONGESTED_REENTRY_US`), and forgets it after `CONGESTED_MEMORY_US` of
    /// non-idle time with no congestion (`quiet_us`), or `CONGESTED_CEILING_US` after the last congestion.
    congested: Option<(usize, u64)>,
    /// Non-idle time with no congestion since the uplink was last congested (us): idle time does not age
    /// D56's memory (@coordinator-opus, 2026-11-01).
    quiet_us: u64,
    /// Whether the uplink queue was above 40 ms at the last tick, to find where congestion begins.
    over: bool,
    /// Thin links (Part A): the drain level's quick return to packed level 5 was used in this D56 episode.
    /// Until then D56 remembers at most packed level 5. After that, leaving the drain level follows D56 like
    /// any other level, so a link the packed levels cannot carry does not cycle.
    drained: bool,
    hand_last: BTreeMap<u32, u64>,
    /// D73(a): per hand, the poses waiting for their pack while R18 thins the stream.
    packs: BTreeMap<u32, Pack>,
    /// Time spent at each congestion level (ms), and the highest level reached.
    pub level_ms: [u64; 9],
    /// Remote state-carrying entries awaiting the acceptance check: (entry, author, obj).
    pub pending_accept: Vec<(EntryId, MemberId, u32)>,
}

pub struct Peer {
    pub id: MemberId,
    pub skew_us: i64,
    pub worlds: BTreeMap<WorldId, Replica>,
    pub jobs: Vec<Job>,
    /// R14 genesis config of each world this peer joined: the worlds it trades objects with.
    /// Every member of a world holds the same list (reviews R3-4 to R3-6).
    pub routes: BTreeMap<WorldId, BTreeSet<WorldId>>,
    /// A crashed peer neither receives, ticks nor sends (reviews R3-2, R3-7).
    pub crashed: bool,
}

/// Result of one acceptance check (user ruling 09:31Z): after the receiver re-simulated to its
/// present frame, does its copy of the object equal the author's copy at that frame?
#[derive(Clone, Copy, Debug)]
pub struct Acceptance {
    pub world: WorldId,
    pub receiver: MemberId,
    pub author: MemberId,
    pub obj: u32,
    pub frame: u64,
    /// Equal to the author's state for that frame as the author held it at that instant.
    pub equal: bool,
    /// The receiver's object hash, to compare later with the author's final history.
    pub receiver_hash: u64,
}

/// (spike) The feed controller's knobs, read once:
/// * PE_ENTER_US, PE_ENTER_HOLD_US, PE_ENTER_NOW_US: the enter test's thresholds (40 ms, 1 s, 400 ms).
/// * The enter test reads the (t + 1)-th worst qualifying sender's backlog, not the worst, so one liar sender alone can't
///   put a member on the feed at n >= 4 (t = 0 at n = 3: no change there; adopted, @coordinator-opus mcad9ec6a4a45).
///   PE_ENTER_WORST=1 reads the worst, as before.
/// * PE_ENTER_ANY_SENDER=1: every level-0 sender's backlog counts, as before the member-2 fix.
/// * PE_ENTER_ANY_LEVEL=1: senders at any R18 level count (the member-2 test tells a sender's own uplink queue from t's
///   downlink queue; the level doesn't: R18 thins a sender for a thin receiver's queue too).
/// * PE_CLEAR_US: the member-2 test's "clear" threshold for a sender's link to another member (40 ms).
/// * PE_EXIT_BOUND_MS: an exit bound T in ms. None by default: the user's ruling (b) (m79deb050976c, "go with b for now")
///   has the member leave on the live test and catch up while its echo covers it; PE_EXIT_BOUND_MS=2000 is option (a).
/// * PE_NO_PROGRESS_MS: the no-progress guard (5000): an echoing member whose held chains don't advance for this long goes
///   back on the feed, and each guard re-entry doubles its next leave test's hold (reset when an echo phase ends normally).
/// * (b) applies at 3 members only (@liaison-opus m81715c78c902, via @coordinator-opus mc2af3d524792): at 4 or more the
///   exit keeps option (a), the 2 s catch-up bound, until the user rules on the widening. PE_EXIT_B_ALL=1 applies (b) at
///   every size, for measuring.
/// * PE_REENTRY_HOLDOFF_MS (6000): no re-entry this long after leaving the feed (margin rule (2)) or after the end of the
///   echo phase (the enter test). 4 s was the smallest with no flip-back in down-k12-m3 (3 s flipped back by rule (2) at
///   17.9 s); 6 s is @coordinator-opus's ruling (m89c5dcd4adb0) for margin, which costs only the member itself, since (D)
///   covers the others while it echoes. PE_REENTRY_FACTOR (1.3): rule (2)'s
///   factor (1.0: re-enter only once the live stream alone exceeds the cap, a hysteresis band below the leave test's 1.3).
/// * PE_NO_CATCHUP=1: no background catch-up while fed, as before the feed-exit fix.
struct FeedKnobs {
    enter_us: u64,
    hold_us: u64,
    now_us: u64,
    rank: bool,
    any_sender: bool,
    any_level: bool,
    clear_us: u64,
    exit_bound_us: Option<u64>,
    catchup: bool,
    no_progress_us: u64,
    reentry_holdoff_us: u64,
    reentry_factor: f64,
    b_all: bool,
}

fn feed_knobs() -> &'static FeedKnobs {
    static K: std::sync::OnceLock<FeedKnobs> = std::sync::OnceLock::new();
    K.get_or_init(|| {
        let num = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(d);
        let on = |k: &str| std::env::var(k).is_ok();
        FeedKnobs {
            enter_us: num("PE_ENTER_US", 40_000),
            hold_us: num("PE_ENTER_HOLD_US", 1_000_000),
            now_us: num("PE_ENTER_NOW_US", 400_000),
            rank: !on("PE_ENTER_WORST"),
            any_sender: on("PE_ENTER_ANY_SENDER"),
            any_level: on("PE_ENTER_ANY_LEVEL"),
            clear_us: num("PE_CLEAR_US", 40_000),
            exit_bound_us: std::env::var("PE_EXIT_BOUND_MS").ok().and_then(|v| v.parse::<u64>().ok()).map(|ms| ms * 1000),
            catchup: !on("PE_NO_CATCHUP"),
            no_progress_us: num("PE_NO_PROGRESS_MS", 5000) * 1000,
            reentry_holdoff_us: num("PE_REENTRY_HOLDOFF_MS", 6000) * 1000,
            b_all: on("PE_EXIT_B_ALL"),
            reentry_factor: std::env::var("PE_REENTRY_FACTOR").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(1.3),
        }
    })
}

/// Thin links Part B, rule 5: one receiver's feed controller.
#[derive(Clone, Debug, Default)]
struct FeedCtl {
    fed: bool,
    feeder: MemberId,
    feeder_since_us: u64,
    leave_ok_since_us: Option<u64>,
    next_send_us: u64,
    rr: usize,
    /// When it last came off the feed: no re-entry for LEAVE_US, while it catches up on the log.
    left_us: Option<u64>,
    /// Since when the level-0 backlog into it has stayed over ENTER_US.
    over_since_us: Option<u64>,
    /// The frame it came off the feed at: no re-entry before its confirmed frame passes it (it has caught up).
    left_frame: u64,
    /// The exit bound: its catch-up time in seconds as last computed, and when (us); computed at most every 100 ms.
    catchup_s: f64,
    catchup_at: Option<u64>,
    /// Margin rule (2): since when, while it echoes, some sender's live rate into it has failed the 1.3x test.
    rate_fail_since_us: Option<u64>,
    /// The no-progress guard: its held chains' total as last seen while echoing, and since when it stood there.
    progress: Option<(u64, u64)>,
    /// Times it left the feed and went back on (flip-backs: re-entries after a leave).
    entries: u32,
    /// Whether it was echoing at the last tick, and when its last echo phase ended (the re-entry hold-off).
    was_echoing: bool,
    echo_end_us: Option<u64>,
    /// Consecutive guard re-entries: each doubles the next leave test's hold (reset when an echo phase ends normally).
    guard_streak: u32,
    /// The catch-up's holders: per holder, its requests that drew nothing (charged when the next attempt rose); per chain,
    /// the holder its last request went to and that request's attempt.
    holder_fail: BTreeMap<MemberId, u32>,
    last_req: BTreeMap<MemberId, (MemberId, u32)>,
}

pub struct Sim {
    /// Thin links Part B, rule 5: run the per-receiver feed controller (sessions where Part B applies).
    pub feed: bool,
    feeds: BTreeMap<(WorldId, MemberId), FeedCtl>,
    pub net: Net,
    pub peers: Vec<Peer>,
    pub acceptance: Vec<Acceptance>,
    pub end_us: u64,
    /// Process peers in reverse order (order-independence check).
    pub reverse_order: bool,
    /// R14: a carrier's deadline, in frames of the destination after its present (review R3-6).
    pub carry_deadline_frames: u64,
    /// Thin links Part B (plan section 6): feeders that lie, sending every body 0.2 m higher than it is.
    pub lying_feeders: BTreeSet<MemberId>,
    /// Fixture: the feed controller's room (how many members it may feed) instead of n − q, for cells beyond the
    /// scope (two fed at n = 4).
    pub feed_room: Option<usize>,
    /// Acks each member authored (traces).
    pub ack_count: BTreeMap<(WorldId, MemberId), u64>,
}

impl Peer {
    pub fn new(id: MemberId, skew_us: i64) -> Self {
        Peer { id, skew_us, worlds: BTreeMap::new(), jobs: Vec::new(), routes: BTreeMap::new(), crashed: false }
    }

    /// R14: world `w` trades objects with the worlds `to` (its genesis route list).
    pub fn route(&mut self, w: WorldId, to: &[WorldId]) {
        if self.worlds.contains_key(&w) {
            self.routes.insert(w, to.iter().copied().collect());
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn join(&mut self, world: WorldId, members: &[MemberId], scene: SceneKind, cfg: SimConfig, epoch_us: u64, plan: BTreeMap<u64, Vec<Action>>) {
        let dt_us = (cfg.dt as f64 * 1e6).round() as u64;
        self.worlds.insert(
            world,
            Replica {
                sim: WorldSim::new(world, self.id, members, scene, cfg),
                scene,
                epoch_us,
                dt_us,
                throttle_us: None,
                throttle_from_us: 0,
                last_burst_us: 0,
                plan,
                next_ack_us: 0,
                // Saturating: a scenario with hz = 0 (dt = inf, so dt_us = u64::MAX) must reach the
                // tool's validator, which refuses it, instead of overflowing here in a debug build.
                ack_every_us: dt_us.saturating_mul(2),
                was_idle: false,
                ack_grid_only: false,
                last_frame_seen: u64::MAX,
                adapt: false,
                level: 0,
                level_t: 0,
                calm_since: 0,
                idle_at: 0,
                congested: None,
                quiet_us: 0,
                over: false,
                drained: false,
                level_b: 0,
                hand_last: BTreeMap::new(),
                packs: BTreeMap::new(),
                level_ms: [0; 9],
                pending_accept: Vec::new(),
            },
        );
    }
}

fn broadcast(net: &mut Net, w: WorldId, from: MemberId, members: &[MemberId], m: &Msg) {
    let live = match m {
        Msg::Entry(e) if goes_live(from, e) => Some(Msg::Live(e.clone())),
        _ => None,
    };
    for &to in members {
        if to != from {
            // A fed member gets no live checkpoint (it holds no state after prev); `post` filters it as the entry. Nor does
            // a member in its echo phase (`Net::echoing`): it gets the explicit one.
            static ECHO_LIVE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let explicit = net.echoing.contains(&(w, to)) && !*ECHO_LIVE.get_or_init(|| std::env::var("PE_ECHO_LIVE").is_ok());
            net.post(w, from, to, if explicit { m } else { live.as_ref().unwrap_or(m) });
        }
    }
}

/// Link elision (the R18 controller-cliff item): whether `from` sends its own entry `e` live, without its checkpoint
/// link ([`Msg::Live`]). Only a checkpoint with a link, only its author's broadcast (never a fetch reply, relay or proof),
/// and not the entries a receiver takes by value at arrival (the view change's and admin entries) or a carried state
/// (the transfer accept reads the id at arrival). (spike: PE_NO_LIVE=1 sends every link, for A/B.)
pub fn goes_live(from: MemberId, e: &Entry) -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    e.member == from
        && e.link != 0
        && e.carried.is_none()
        && !matches!(
            e.payload,
            Payload::Vote { .. }
                | Payload::Propose { .. }
                | Payload::Prevote { .. }
                | Payload::Precommit { .. }
                | Payload::Kick { .. }
                | Payload::SetRole { .. }
                | Payload::DefineRole { .. }
                | Payload::Ownership { .. }
        )
        && !*OFF.get_or_init(|| std::env::var("PE_NO_LIVE").is_ok_and(|v| v == "1"))
}

/// Measurement only (PE_WHATIF_LIVE=1, with PE_NO_LIVE=1): the bytes link elision would leave out of `m` from `from`, so
/// the shaper and the controllers see elision's bytes on the explicit wire, with no live entries held behind holes.
pub fn whatif_live_saving(from: MemberId, m: &Msg) -> u64 {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var("PE_WHATIF_LIVE").is_ok_and(|v| v == "1")) {
        return 0;
    }
    fn walk(from: MemberId, m: &Msg) -> u64 {
        match m {
            Msg::Entry(e) if e.member == from && e.link != 0 && e.carried.is_none() && !matches!(
                e.payload,
                Payload::Vote { .. } | Payload::Propose { .. } | Payload::Prevote { .. } | Payload::Precommit { .. }
                    | Payload::Kick { .. } | Payload::SetRole { .. } | Payload::DefineRole { .. } | Payload::Ownership { .. }
            ) => 8,
            Msg::Batch(v) => v.iter().map(|x| walk(from, x)).sum(),
            _ => 0,
        }
    }
    walk(from, m)
}

/// `m` with every live entry as the explicit one (its link is held in memory on the sending side), for fault filters
/// written for `Msg::Entry`.
pub fn explicit_view(m: &Msg) -> std::borrow::Cow<'_, Msg> {
    fn has_live(m: &Msg) -> bool {
        match m {
            Msg::Live(_) => true,
            Msg::Batch(v) => v.iter().any(has_live),
            _ => false,
        }
    }
    fn conv(m: &Msg) -> Msg {
        match m {
            Msg::Live(e) => Msg::Entry(e.clone()),
            Msg::Batch(v) => Msg::Batch(v.iter().map(conv).collect()),
            x => x.clone(),
        }
    }
    if has_live(m) { std::borrow::Cow::Owned(conv(m)) } else { std::borrow::Cow::Borrowed(m) }
}

fn flatten(m: Msg, out: &mut Vec<Msg>) {
    match m {
        Msg::Batch(v) => v.into_iter().for_each(|x| flatten(x, out)),
        x => out.push(x),
    }
}

/// R18: hand-entry stride and ack interval (frames) at each congestion level.
/// Congestion levels (R18): frames between hand entries, and between acks. Review R3-9 added
/// levels 4 and 5 for links of about 8-16 kbit/s, and aligns both to the same frame ticks, so a
/// hand entry and an ack share one datagram.
/// Thin links (Part A): level 6 drains a backlog at stride 8 without packs (D73 option B), and levels 7
/// and 8 send a single pose and an ack every 16 and 32 frames, for links the packed levels cannot carry.
/// Above `PACKED_LEVELS` the author's hand is not exact: it reaches each pose over the stride, as
/// before D73.
pub const HAND_STRIDE: [u64; 9] = [1, 2, 3, 4, 6, 8, 8, 16, 32];
/// The coarsest level that sends packs (D73(a)); above it, single poses.
pub const PACKED_LEVELS: usize = 5;
/// The level that drains a backlog without packs and returns to packed level 5 after 300 ms of calm, once
/// per D56 episode.
pub const DRAIN_LEVEL: usize = 6;
/// From the coarsest packed level up, the controller climbs only while its queue is at least this deep (us), or
/// still growing (by 10 ms since the last level change).
pub const DEEP_QUEUE_US: u64 = 300_000;

/// D73(a): a hand's poses not yet sent, for consecutive frames from `p0`, at stride `k`.
struct Pack {
    p0: u64,
    k: u64,
    targets: Vec<[i32; 3]>,
}

/// D73(a): send hand `obj`'s pending pack, if any: one entry stamped at its first frame holding a
/// target per frame. Its poses stop being local-only; the pack replaces them (its insertion rolls this
/// replica back to the first frame, which re-simulates with the same targets).
fn flush_pack(r: &mut Replica, obj: u32) -> Option<Entry> {
    let p = r.packs.remove(&obj)?;
    for j in 0..p.targets.len() as u64 {
        r.sim.own_pending.remove(&(p.p0 + j, obj));
    }
    r.sim.hold_horizon_below = r.packs.values().map(|p| p.p0).min();
    let mut ts = p.targets.into_iter();
    let target_mm = ts.next().expect("a pack holds a pose");
    Some(r.sim.author_stamped(Payload::Hand { obj, target_mm, active: true, span: 1, more: ts.collect() }, p.p0))
}
pub const ACK_FRAMES: [u64; 9] = [2, 2, 3, 4, 6, 8, 8, 16, 32];
/// 7.6 reduction 2: an idle member acks every this many frames, on multiples of it, at any level.
pub const IDLE_ACK_FRAMES: u64 = 8;
/// Part B, rule 5: a fed member acks every P frames (P <= 32; the spike uses 16, or its level's interval if longer), from the moment it is fed, whatever
/// its level. By level alone it acked every 2 frames at level 0 until its controller stepped down, flooding a thin
/// uplink (r18 capped n = 4, 8 kbit/s: about 62 acks a second offered in the first gesture's window).
pub const FED_ACK_FRAMES: u64 = 16;
/// Revision 22 (@coordinator-opus m26541fc023df): P = 32 for a fed member at n >= 4, 16 at n = 3. At n = 4 the lag
/// didn't change (53.7 / 53.8 frames at both) and the fed member's uplink dropped 29.3 -> 22.6 kbit/s; at n = 3 the fed
/// echo is always needed and 32 cost 13 frames (43.0 -> 56.4).
pub const FED_ACK_FRAMES_N4: u64 = 32;
/// Revision 22: whether `n` members with `f` of them fed are in scope (n − f >= q). Outside it at n >= 4 is the two-fed
/// region, where rule 3's latency floor applies and P = 16.
pub fn fed_in_scope(n: usize, f: usize) -> bool {
    n.saturating_sub(f) >= pe_sim_core::session::quorum(pe_sim_core::session::QuorumRule::Bft, n).1
}
/// Revision 22, the start transient (@coordinator-opus mbe5278d26bae): the lowest R18 level a fed member may be at, matching
/// its ack period P. Level 8 (hand stride and acks 32) while n − f >= q, level 7 (16, as P = 16) in the two-fed region,
/// and no floor at n = 3 (today's levels). Measured at 64433b2, r18 capped with 8 members and member 1 fed at 8 kbit/s and
/// sweeping: without a floor its gesture starts at stride 1, offers 93.3 kbit/s of 56 and the others stall 233 frames; at
/// level 8 they stall 0 (7 changed little there).
pub fn fed_level_floor(n: usize, f: usize) -> Option<usize> {
    (n >= 4).then(|| if fed_in_scope(n, f) { 8 } else { 7 })
}
/// D56 (@coordinator-opus, 2026-10-31): after congestion begins at level L, the controller lowers into L
/// or any finer level only after this much calm instead of 300 ms, and forgets L after
/// `CONGESTED_MEMORY_US` with no congestion [proposal: 5 s and 30 s]. Without it a stream at twice the
/// cap cycled about every 3 s.
pub const CONGESTED_REENTRY_US: u64 = 5_000_000;
pub const CONGESTED_MEMORY_US: u64 = 30_000_000;
/// The memory's ceiling: forgotten this long after the last congestion, idle or not, so a session of
/// short gestures re-probes finer levels eventually (@coordinator-opus, 2026-11-01) [proposal: 5 min].
pub const CONGESTED_CEILING_US: u64 = 300_000_000;

impl Sim {
    pub fn new(net: Net, mut peers: Vec<Peer>, end_us: u64) -> Self {
        // R14 trust config (D46): each world knows the member lists of the worlds on its own route
        // list, and of no other (reviews R3-4 to R3-6).
        let mut worlds: BTreeMap<WorldId, Vec<MemberId>> = BTreeMap::new();
        for p in &peers {
            for (w, r) in &p.worlds {
                worlds.entry(*w).or_insert_with(|| r.sim.members.iter().copied().collect());
            }
        }
        for p in &mut peers {
            for (w, r) in p.worlds.iter_mut() {
                let routes = p.routes.get(w).cloned().unwrap_or_default();
                r.sim.peers_of = worlds.iter().filter(|(x, _)| routes.contains(x)).map(|(x, m)| (*x, m.clone())).collect();
            }
        }
        // P2-5: epoch certificates (static stub of P2-4's interface) of every world, standing in for
        // self-certifying world ids: a chain may pass through worlds off this world's route list.
        let mut epochs = EpochBook::default();
        for p in &peers {
            for (w, r) in &p.worlds {
                epochs.rule = r.sim.cfg.quorum;
                if !epochs.worlds.contains_key(w) {
                    epochs.genesis(*w, &worlds[w], r.sim.genesis_objects);
                }
            }
        }
        for p in &mut peers {
            for r in p.worlds.values_mut() {
                r.sim.epochs = epochs.clone();
            }
        }
        Sim { feed: false, feeds: BTreeMap::new(), net, peers, acceptance: Vec::new(), end_us, reverse_order: false, carry_deadline_frames: 600, lying_feeders: BTreeSet::new(), feed_room: None, ack_count: BTreeMap::new() }
    }

    fn peer_index(&self, id: MemberId) -> usize {
        self.peers.iter().position(|p| p.id == id).expect("peer")
    }

    /// Thin links Part B, rule 5: each member's feed controller, and the feeders' packets.
    /// * Enter when the backlog on the links into the member from senders at level 0 is over 40 ms: the full
    ///   stream doesn't fit its downlink. A thinned sender's backlog is its own uplink's, not the receiver's.
    ///   At most n − q members are fed (the plan's scope; beyond it, P1 (a)'s fed C_max would be needed).
    /// * Leave once every sender's link into it carries 1.3x the stream that sender offers a member not on the
    ///   feed, for 5 s; then it fetches the log again.
    /// * The feeder is its best-linked peer (smallest backlog toward it), rotated every 10 s; it sends the
    ///   bodies' states every 50 ms, within 70% of its link to the member.
    fn run_feeds(&mut self, now: u64) {
        let knobs = feed_knobs();
        let enter_us = knobs.enter_us;
        const LEAVE_US: u64 = 5_000_000;
        const ROTATE_US: u64 = 10_000_000;
        const PERIOD_US: u64 = 50_000;
        // Thin links Part B: the full stream "doesn't fit" only if the backlog stays over ENTER_US this long. One
        // 16-entry fetch reply is about 50 ms on a 256 kbit/s link, and put clean members on the feed; at 300 ms a
        // slow sender's catch-up burst after GST still did (partbrand seed 11). A thin link's backlog grows for good.
        let enter_hold_us = knobs.hold_us;
        // ... unless it is clearly thin: a backlog this deep enters at once (an 8-16 kbit/s downlink under the full
        // stream gets there in tens of ms; the catch-up bursts seen on fat links peaked at 43-212 ms).
        let enter_now_us = knobs.now_us;
        // The feed exit (D): members in their echo phase count as fed for the room (S-4) and never feed (their view
        // waits on the catch-up).
        let echoing: BTreeSet<(WorldId, MemberId)> =
            self.peers.iter().filter(|p| !p.crashed).flat_map(|p| p.worlds.iter().filter(|(_, r)| r.sim.in_echo_phase()).map(move |(w, _)| (*w, p.id))).collect();
        let mut todo: Vec<(WorldId, MemberId, Vec<MemberId>)> = Vec::new();
        // Each member's R18 level and quorum, per world.
        let mut level: BTreeMap<(WorldId, MemberId), usize> = BTreeMap::new();
        let mut room: BTreeMap<WorldId, usize> = BTreeMap::new();
        let room_fixture = self.feed_room;
        for p in &self.peers {
            if p.crashed {
                continue;
            }
            for (w, r) in &p.worlds {
                level.insert((*w, p.id), r.level);
                if (r.sim.localized() || r.sim.fed_mode_capable()) && r.sim.members.contains(&p.id) {
                    todo.push((*w, p.id, r.sim.members.iter().copied().collect()));
                    // The fixture lifts the room only at n >= 4 (S-4: at most one fed at n = 3).
                    room.entry(*w).or_insert_with(|| {
                        room_fixture
                            .filter(|_| r.sim.members.len() >= 4)
                            .unwrap_or_else(|| r.sim.members.len().saturating_sub(pe_sim_core::session::quorum(pe_sim_core::session::QuorumRule::Bft, r.sim.members.len()).1))
                    });
                }
            }
        }
        // Revision 22: below 3 members a fed member's echo counts for nothing (2 players keep D54), so its feed ends, as a
        // leave would (leave-s4: after the second thin member left by request, the fed one stalled the 2-player session).
        let ended: Vec<(WorldId, MemberId)> = self
            .net
            .fed
            .iter()
            .copied()
            .filter(|&(w, m)| {
                let pi = self.peer_index(m);
                self.peers[pi].worlds.get(&w).is_none_or(|r| !(r.sim.localized() || r.sim.fed_mode_capable()))
            })
            .collect();
        for (w, t) in ended {
            self.net.fed.remove(&(w, t));
            if let Some(ctl) = self.feeds.get_mut(&(w, t)) {
                ctl.fed = false;
                ctl.left_us = Some(now);
            }
            let pi = self.peer_index(t);
            let fetches = match self.peers[pi].worlds.get_mut(&w) {
                Some(r) => {
                    r.sim.set_fed(false);
                    r.sim.recovery_fetches()
                }
                None => Vec::new(),
            };
            for f in fetches {
                if let Msg::Fetch { author, .. } = f {
                    if author != t {
                        self.net.post(w, t, author, &f);
                    }
                }
            }
        }
        let cap_of = |net: &Net, a: MemberId, b: MemberId| net.caps.get(&(a, b)).copied().or(net.default_cap).map_or(u64::MAX, |c| c.rate_bps);
        for (w, t, members) in todo {
            let senders: Vec<MemberId> = members.iter().copied().filter(|&s| s != t).collect();
            if senders.is_empty() {
                continue;
            }
            let mut ctl = self.feeds.get(&(w, t)).cloned().unwrap_or_default();
            if !ctl.fed {
                // The member-2 fix (@coordinator-opus m9c4af4baeb36, provisional): a level-0 sender's backlog into t counts
                // only if its link to some other member not on the feed is clear (under ENTER_US). A thin uplink queues to
                // everyone, a thin downlink only into t (down-k32-m3: member 2, on 256 kbit/s, went on the feed on member
                // 3's own 32 kbit/s uplink queue while member 3's controller sat at level 0). At n = 3 with one member fed
                // no such member exists, so nothing counts, and the room (n − q = 1) keeps a second one off the feed anyway.
                // Margin rule (1) (@coordinator-opus m360a9ee1d5bb, mca39b9f6b726): queue built by an echoing member's own
                // catch-up (answers to fetches it sent, within the ranges asked) doesn't count; everything else does.
                // (b) at 3 members only (PE_EXIT_B_ALL=1: every size); at 4 or more, option (a)'s exit and enter test.
                let b_mode = members.len() == 3 || knobs.b_all;
                let bk = |net: &Net, a: MemberId, b: MemberId| if b_mode { net.live_backlog_us(a, b) } else { net.backlog_us(a, b) };
                let qual: Vec<MemberId> = senders
                    .iter()
                    .copied()
                    .filter(|&s| knobs.any_level || level.get(&(w, s)) == Some(&0))
                    .filter(|&s| knobs.any_sender || members.iter().any(|&x| x != s && x != t && !self.net.fed.contains(&(w, x)) && bk(&self.net, s, x) <= knobs.clear_us))
                    .collect();
                let mut qualifying: Vec<u64> = qual.iter().map(|&s| bk(&self.net, s, t)).collect();
                qualifying.sort_unstable_by(|a, b| b.cmp(a));
                // The (t + 1)-th worst, so one liar sender alone can't put t on the feed at n >= 4 (spike: PE_ENTER_WORST=1, the worst).
                let rank = if knobs.rank { pe_sim_core::session::quorum(pe_sim_core::session::QuorumRule::Bft, members.len()).0 } else { 0 };
                let backlog = qualifying.get(rank).copied().unwrap_or(0);
                // Margin rule (2): while it echoes, it goes back on the feed only once the live stream alone fails the 1.3x
                // test, the senders' offered live rate toward it (not what reaches it), for ENTER_HOLD_US; (t + 1)-th worst.
                let echoing_t = echoing.contains(&(w, t));
                let rate_fails = qual.iter().filter(|&&s| (cap_of(&self.net, s, t) as f64) < knobs.reentry_factor * self.net.live_bps(s, t) as f64).count() > rank
                    && ctl.left_us.is_none_or(|t0| now >= t0 + knobs.reentry_holdoff_us);
                ctl.rate_fail_since_us = if echoing_t && rate_fails { Some(ctl.rate_fail_since_us.unwrap_or(now)) } else { None };
                let rate_held = ctl.rate_fail_since_us.is_some_and(|t0| now >= t0 + enter_hold_us);
                // The no-progress guard (@coordinator-opus mca39b9f6b726): an echoing member whose held chains don't advance
                // for no_progress_us goes back on the feed, as under (a), whatever the rest says (a withholding holder can
                // otherwise stretch its freeze).
                let guard = if echoing_t {
                    let pt = self.peer_index(t);
                    let total: u64 = self.peers[pt].worlds.get(&w).map_or(0, |r| senders.iter().map(|&x| r.sim.held_head(x) as u64).sum());
                    match ctl.progress {
                        Some((v, since)) if v == total => now >= since + knobs.no_progress_us,
                        _ => {
                            ctl.progress = Some((total, now));
                            false
                        }
                    }
                } else {
                    ctl.progress = None;
                    false
                };
                // The feed exit (b): an echoing member's catch-up, one request per chain at a time, each chain from its
                // author first and, unanswered, from the next holder (the fetch fallback); answered at once past R3-9's yield
                // (`Net::send_catchup`).
                if echoing_t && knobs.catchup {
                    let pi = self.peer_index(t);
                    let fetches = self.peers[pi].worlds.get_mut(&w).map(|r| r.sim.catchup_fetches(now / 1000)).unwrap_or_default();
                    for (f, attempt) in fetches {
                        let Msg::Fetch { author, .. } = f else { continue };
                        // A request that drew nothing within 2 s (its attempt rose) charges the holder it went to. The next
                        // goes to the holder charged least, the author first among equals: with two holders, rotating by
                        // attempt alone sent the request after one lost answer back to the withholder (down-k12-m3, member
                        // 1 withholding: 5 s without progress, and the guard).
                        if let Some(&(h, a0)) = ctl.last_req.get(&author) {
                            if attempt > a0 {
                                *ctl.holder_fail.entry(h).or_default() += 1;
                            }
                        }
                        let mut holders: Vec<MemberId> = vec![author];
                        holders.extend(senders.iter().copied().filter(|&s| s != author && !self.net.fed.contains(&(w, s))));
                        let src = *holders.iter().min_by_key(|&&h| (ctl.holder_fail.get(&h).copied().unwrap_or(0), h != author, h)).unwrap();
                        ctl.last_req.insert(author, (src, attempt));
                        if src != t {
                            self.net.post(w, t, src, &f);
                        }
                    }
                }
                // (spike diagnostics: PE_FEED_SIGNALS=1) every 250 ms, the enter test's inputs for each member not fed:
                // per sender (level, backlog into t in ms, bits offered into t over the last second, entries of its chain t
                // lacks), the qualifying backlog, and t's and member 1's confirmation lag in frames.
                if now % 250_000 == 0 && std::env::var("PE_FEED_SIGNALS").is_ok() {
                    let pt = self.peer_index(t);
                    let lag_of = |pi: usize| self.peers[pi].worlds.get(&w).map_or(0, |r| r.sim.frame.saturating_sub(r.sim.conf));
                    let per: Vec<String> = senders
                        .iter()
                        .map(|&s| {
                            let ps = self.peer_index(s);
                            let lacks = match (self.peers[ps].worlds.get(&w), self.peers[pt].worlds.get(&w)) {
                                (Some(rs), Some(rt)) => rs.sim.held_head(s).saturating_sub(rt.sim.held_head(s)),
                                _ => 0,
                            };
                            format!("m{s}:L{} {}ms {}b/s -{}", level.get(&(w, s)).copied().unwrap_or(0), self.net.backlog_us(s, t) / 1000, self.net.recent_bps(s, t), lacks)
                        })
                        .collect();
                    let p1 = self.peer_index(members[0]);
                    eprintln!("SIG t={} m{t}: [{}] qualifying {} ms; lag m{t} {} m{} {}", now / 1000, per.join(", "), backlog / 1000, lag_of(pt), members[0], lag_of(p1));
                }
                let fed_now = members.iter().filter(|&&m| m != t && (self.net.fed.contains(&(w, m)) || echoing.contains(&(w, m)))).count();
                let feeders: Vec<MemberId> = senders.iter().copied().filter(|&s| !self.net.fed.contains(&(w, s)) && !echoing.contains(&(w, s))).collect();
                let caught_up = {
                    let pi = self.peer_index(t);
                    self.peers[pi].worlds.get(&w).is_none_or(|r| r.sim.conf >= ctl.left_frame)
                };
                // The re-entry hold-off (PE_REENTRY_HOLDOFF_MS): none within it of leaving (rule (2), above) or of the end of
                // its echo phase (the enter test: the tail of its catch-up goes out as ordinary repairs, which count).
                if ctl.was_echoing && !echoing_t {
                    // Its echo phase ended normally (a re-entry would have put it on the feed): the back-off resets.
                    ctl.echo_end_us = Some(now);
                    ctl.guard_streak = 0;
                }
                ctl.was_echoing = echoing_t;
                let held_off = b_mode && ctl.echo_end_us.is_some_and(|t0| now < t0 + knobs.reentry_holdoff_us);
                let settled = ctl.left_us.is_none_or(|t0| now >= t0 + LEAVE_US) && caught_up && !held_off;
                ctl.over_since_us = if backlog > enter_us { Some(ctl.over_since_us.unwrap_or(now)) } else { None };
                let held = backlog > enter_now_us || ctl.over_since_us.is_some_and(|t0| now >= t0 + enter_hold_us);
                // Echoing, its echo still covers it, so it may go back on the feed before it has caught up: by rule (2), or
                // by the guard. Otherwise as before.
                let enter = if echoing_t && b_mode { (held && rate_held) || guard } else { held && settled };
                if enter && fed_now < room.get(&w).copied().unwrap_or(0) && !feeders.is_empty() {
                    let feeder = *feeders.iter().min_by_key(|&&s| (self.net.backlog_us(s, t), s)).unwrap();
                    if std::env::var("PE_FEED_LOG").is_ok() {
                        let worst = senders.iter().filter(|&&s| level.get(&(w, s)) == Some(&0)).max_by_key(|&&s| self.net.backlog_us(s, t)).copied();
                        let why = if guard { "the no-progress guard" } else if echoing_t { "rule (2) while echoing" } else { "the enter test" };
                        eprintln!("FEED t={} ms: {t} goes on the feed ({why}), feeder {feeder}; worst level-0 sender {worst:?}; qualifying live backlog {} us", now / 1000, backlog);
                    }
                    ctl = FeedCtl {
                        fed: true,
                        feeder,
                        feeder_since_us: now,
                        leave_ok_since_us: None,
                        next_send_us: now,
                        rr: ctl.rr,
                        left_us: ctl.left_us,
                        over_since_us: None,
                        left_frame: ctl.left_frame,
                        catchup_s: 0.0,
                        catchup_at: None,
                        rate_fail_since_us: None,
                        progress: None,
                        entries: ctl.entries + 1,
                        was_echoing: false,
                        echo_end_us: ctl.echo_end_us,
                        guard_streak: ctl.guard_streak + u32::from(guard),
                        holder_fail: ctl.holder_fail.clone(),
                        last_req: ctl.last_req.clone(),
                    };
                    self.net.fed.insert((w, t));
                    let pi = self.peer_index(t);
                    if let Some(r) = self.peers[pi].worlds.get_mut(&w) {
                        r.sim.set_fed(true);
                    }
                }
            } else {
                // A feeder that went on the feed itself, or isn't a member any more, can't feed: re-pick at once (the
                // n = 4 two-fed cell: member 4 was picked as member 3's feeder, then went on the feed and kept feeding).
                if self.net.fed.contains(&(w, ctl.feeder)) || echoing.contains(&(w, ctl.feeder)) || !senders.contains(&ctl.feeder) {
                    let feeders: Vec<MemberId> = senders.iter().copied().filter(|&s| !self.net.fed.contains(&(w, s)) && !echoing.contains(&(w, s))).collect();
                    if let Some(&f) = feeders.iter().min_by_key(|&&s| (self.net.backlog_us(s, t), s)) {
                        if std::env::var("PE_FEED_LOG").is_ok() {
                            eprintln!("FEED t={} ms: {t}'s feeder {} re-picked: {f}", now / 1000, ctl.feeder);
                        }
                        ctl.feeder = f;
                        ctl.feeder_since_us = now;
                    }
                }
                // The feed exit's background catch-up (@coordinator-opus m5eeeaff3c105): the fed member fetches every other
                // chain from what it holds, from an idle link (a member neither fed, echoing nor feeding, the least queued
                // toward it; the feeder only if there is none). The answers go at the lowest priority (`Net::post_catchup`).
                if knobs.catchup {
                    let pi = self.peer_index(t);
                    let fetches = self.peers[pi].worlds.get_mut(&w).map(|r| r.sim.catchup_fetches(now / 1000)).unwrap_or_default();
                    if !fetches.is_empty() {
                        let mut idle: Vec<MemberId> = senders.iter().copied().filter(|&s| s != ctl.feeder && !self.net.fed.contains(&(w, s)) && !echoing.contains(&(w, s))).collect();
                        idle.sort_by_key(|&s| (self.net.backlog_us(s, t), s));
                        idle.push(ctl.feeder);
                        // An unanswered request moves on to the next holder (the fetch fallback).
                        for (f, attempt) in fetches {
                            let src = idle[attempt as usize % idle.len()];
                            self.net.post(w, t, src, &f);
                        }
                    }
                }
                let ok = senders.iter().all(|&s| {
                    let reference = members.iter().copied().find(|&x| x != s && x != t && !self.net.fed.contains(&(w, x)));
                    let stream = reference.map_or(0, |x| self.net.recent_bps(s, x));
                    cap_of(&self.net, s, t) as f64 >= 1.3 * stream as f64
                });
                // The exit bound (@coordinator-opus m9c4af4baeb36, T = 2 s): and its catch-up fits in T. Over the authors,
                // the wire bytes of each one's chain it lacks over the spare rate of that author's link into it (cap − the
                // stream that author offers a member not on the feed); the links run in parallel. Read at most every 100 ms.
                let bound_now = if members.len() == 3 || knobs.b_all { knobs.exit_bound_us } else { Some(2_000_000) };
                let ok = match bound_now {
                    Some(bound) if ok => {
                        if ctl.catchup_at.is_none_or(|t0| now >= t0 + 100_000) {
                            ctl.catchup_s = self.catchup_secs(w, t, &members);
                            ctl.catchup_at = Some(now);
                        }
                        ctl.catchup_s * 1e6 <= bound as f64
                    }
                    _ => ok,
                };
                if std::env::var("PE_FEED_LOG").is_ok() && now % 2_000_000 == 0 {
                    let v: Vec<(MemberId, u64, u64)> = senders
                        .iter()
                        .map(|&s| {
                            let reference = members.iter().copied().find(|&x| x != s && x != t && !self.net.fed.contains(&(w, x)));
                            (s, cap_of(&self.net, s, t), reference.map_or(0, |x| self.net.recent_bps(s, x)))
                        })
                        .collect();
                    eprintln!("FEED t={} ms: {t} fed; leave test (sender, cap, stream) {v:?} catch-up {:.2} s, ok {ok}", now / 1000, ctl.catchup_s);
                }
                ctl.leave_ok_since_us = if ok { Some(ctl.leave_ok_since_us.unwrap_or(now)) } else { None };
                // The back-off: each guard re-entry doubles the leave test's hold (up to 64x), so a withholder can't cycle
                // the member through freeze stints (@design-reviewer, ruled m89c5dcd4adb0).
                let leave_hold = LEAVE_US.saturating_mul(1u64 << ctl.guard_streak.min(6));
                if ctl.leave_ok_since_us.is_some_and(|t0| now >= t0 + leave_hold) {
                    if std::env::var("PE_FEED_LOG").is_ok() {
                        eprintln!("FEED t={} ms: {t} leaves the feed; catch-up {:.2} s", now / 1000, ctl.catchup_s);
                    }
                    ctl.fed = false;
                    ctl.left_us = Some(now);
                    ctl.left_frame = {
                        let pi = self.peer_index(t);
                        self.peers[pi].worlds.get(&w).map_or(0, |r| r.sim.frame)
                    };
                    self.net.fed.remove(&(w, t));
                    let pi = self.peer_index(t);
                    let fetches = match self.peers[pi].worlds.get_mut(&w) {
                        Some(r) => {
                            r.sim.set_fed(false);
                            // The feed exit (b): in its echo phase the paced catch-up takes over (one request per chain).
                            if r.sim.in_echo_phase() && knobs.catchup {
                                Vec::new()
                            } else {
                                r.sim.recovery_fetches()
                            }
                        }
                        None => Vec::new(),
                    };
                    // Each chain from its own author, so the catch-up spreads over the links.
                    for f in fetches {
                        let to = match f {
                            Msg::Fetch { author, .. } => author,
                            _ => ctl.feeder,
                        };
                        self.net.post(w, t, to, &f);
                    }
                } else {
                    if now >= ctl.feeder_since_us + ROTATE_US {
                        let feeders: Vec<MemberId> = senders.iter().copied().filter(|&s| !self.net.fed.contains(&(w, s))).collect();
                        if !feeders.is_empty() {
                            let i = feeders.iter().position(|&s| s == ctl.feeder).map_or(0, |i| i + 1);
                            ctl.feeder = feeders[i % feeders.len()];
                        }
                        ctl.feeder_since_us = now;
                    }
                    if now >= ctl.next_send_us {
                        // Within 70% of the feeder's link to the member, datagram overhead included: packets of at
                        // least 4 bodies, as often as that rate allows, at most every 50 ms.
                        let rate = cap_of(&self.net, ctl.feeder, t) as f64 * 0.7 / 8.0;
                        let header = dgram_overhead() as f64 + 13.0;
                        let size = (rate * PERIOD_US as f64 / 1e6).clamp(header + 4.0 * 30.0, 1200.0);
                        ctl.next_send_us = now + ((size / rate * 1e6) as u64).max(PERIOD_US);
                        let budget = (size - dgram_overhead() as f64) as usize;
                        let pf = self.peer_index(ctl.feeder);
                        let mut rr = ctl.rr;
                        let pkt = if self.peers[pf].crashed { None } else { self.peers[pf].worlds.get(&w).and_then(|r| r.sim.feed_packet(budget, &mut rr)) };
                        ctl.rr = rr;
                        let pkt = match pkt {
                            Some(Msg::Feed { frame, fed, mut bodies }) if self.lying_feeders.contains(&ctl.feeder) => {
                                for b in &mut bodies {
                                    b.pos[1] += 2000;
                                }
                                Some(Msg::Feed { frame, fed, bodies })
                            }
                            p => p,
                        };
                        if let Some(m) = pkt {
                            self.net.post(w, ctl.feeder, t, &m);
                        }
                        // Gap 1: and the epoch certificates the fed member's acks show it lacks.
                        let push = if self.peers[pf].crashed { None } else { self.peers[pf].worlds.get_mut(&w).and_then(|r| r.sim.cert_push(t)) };
                        if let Some(m) = push {
                            self.net.post(w, ctl.feeder, t, &m);
                        }
                    }
                }
            }
            self.feeds.insert((w, t), ctl);
        }
    }

    /// The exit bound: fed member `t`'s catch-up time in seconds if it left the feed now. For each other member a: the wire
    /// bytes of a's chain from what t holds to what a holds (16-entry answers in MTU datagrams), over the spare rate of
    /// a's link into t (its cap less the stream a offers a member not on the feed). The links run in parallel: the worst.
    fn catchup_secs(&self, w: WorldId, t: MemberId, members: &[MemberId]) -> f64 {
        let pt = self.peer_index(t);
        let Some(rt) = self.peers[pt].worlds.get(&w) else { return 0.0 };
        let mut worst: f64 = 0.0;
        for &a in members.iter().filter(|&&a| a != t) {
            let pa = self.peer_index(a);
            let Some(ra) = self.peers[pa].worlds.get(&w) else { continue };
            let (from, to) = (rt.sim.held_head(a) + 1, ra.sim.held_head(a));
            if to < from {
                continue;
            }
            let (bytes, _) = ra.sim.chain_bytes(a, from, to);
            let wire = bytes + (bytes / MTU as u64 + 1) * dgram_overhead();
            let reference = members.iter().copied().find(|&x| x != a && x != t && !self.net.fed.contains(&(w, x)));
            let stream = reference.map_or(0, |x| self.net.recent_bps(a, x));
            let cap = self.net.caps.get(&(a, t)).copied().or(self.net.default_cap).map_or(u64::MAX, |c| c.rate_bps);
            let spare = cap as f64 - stream as f64;
            worst = worst.max(if spare <= 0.0 { f64::INFINITY } else { wire as f64 * 8.0 / spare });
        }
        worst
    }

    /// Thin links Part B (TLP-23): every member sends each member its log shows fed its checkpoints since that
    /// member's echo of its chain.
    fn send_ckpts(&mut self) {
        let mut out = Vec::new();
        for p in &mut self.peers {
            if p.crashed {
                continue;
            }
            for (w, r) in p.worlds.iter_mut() {
                for (to, m) in r.sim.ckpt_msgs() {
                    out.push((*w, p.id, to, m));
                }
            }
        }
        for (w, from, to, m) in out {
            self.net.post(w, from, to, &m);
        }
    }

    /// Thin links Part B, rule 5: whether `m` is on the state feed in world `w` now.
    pub fn is_fed(&self, w: WorldId, m: MemberId) -> bool {
        self.net.fed.contains(&(w, m))
    }

    /// Run until `end_us` in 1 ms steps.
    pub fn run(&mut self) {
        while self.net.now_us <= self.end_us {
            self.step_ms();
            self.net.now_us += 1000;
        }
    }

    pub fn step_ms(&mut self) {
        let now = self.net.now_us;
        // 1. deliveries (a batch is its messages, in order)
        let mut due = Vec::new();
        for (to, w, from, m) in self.net.due() {
            // Rule 4 (D): each datagram's size, as it arrives directly from its sender.
            let bytes = encode_msg(&m).len() as u64 + dgram_overhead() - whatif_live_saving(from, &m);
            let pi = self.peer_index(to);
            if !self.peers[pi].crashed {
                if let Some(r) = self.peers[pi].worlds.get_mut(&w) {
                    r.sim.note_rx(from, bytes);
                }
            }
            let mut v = Vec::new();
            flatten(m, &mut v);
            due.extend(v.into_iter().map(|x| (to, w, from, x)));
        }
        // The feed exit: members in their echo phase, whose own catch-up doesn't yield (below) and who get explicit
        // checkpoints (`broadcast`).
        let echoing: BTreeSet<(WorldId, MemberId)> =
            self.peers.iter().filter(|p| !p.crashed).flat_map(|p| p.worlds.iter().filter(|(_, r)| r.sim.in_echo_phase()).map(move |(w, _)| (*w, p.id))).collect();
        self.net.echoing = echoing.clone();
        for (to, w, from, m) in due {
            let pi = self.peer_index(to);
            let peer = &mut self.peers[pi];
            if peer.crashed {
                continue;
            }
            let Some(r) = peer.worlds.get_mut(&w) else { continue };
            let members: Vec<MemberId> = r.sim.audience();
            match m {
                Msg::Entry(e) => {
                    let carried = e.carried.map(|(o, _)| o);
                    let author = e.member;
                    let id = e.id();
                    // Proofs of equivocation come back with this member's vote: broadcast them.
                    let out = r.sim.insert(e);
                    if let Some(obj) = carried {
                        if author != to {
                            r.pending_accept.push((id, author, obj));
                        }
                    }
                    for v in out {
                        broadcast(&mut self.net, w, to, &members, &Msg::Entry(v));
                    }
                    for (dest, f) in r.sim.take_fetches(now / 1000) {
                        if dest != to {
                            self.net.post(w, to, dest, &f);
                        }
                    }
                }
                Msg::Live(e) => {
                    let out = r.sim.insert_live(from, e);
                    for v in out {
                        broadcast(&mut self.net, w, to, &members, &Msg::Entry(v));
                    }
                    for (dest, f) in r.sim.take_fetches(now / 1000) {
                        if dest != to {
                            self.net.post(w, to, dest, &f);
                        }
                    }
                }
                Msg::Fetch { author, from: a, to: b } => {
                    // Review R3-9: repairs yield to the queue. Answering into a full uplink only
                    // makes the queue, and the requester's next round trip, longer.
                    // (spike diagnostics: PE_REPAIR_TRACE=<requester>) each fetch from that member, answered or skipped.
                    let trace = std::env::var("PE_REPAIR_TRACE").ok().and_then(|v| v.parse::<MemberId>().ok()) == Some(from);
                    // The feed exit's background catch-up: a fed member's fetch is answered at the lowest priority (below
                    // feed and echo), one answer at a time per link (a repeated request while one waits is dropped).
                    if self.net.fed.contains(&(w, from)) && feed_knobs().catchup {
                        if !self.net.catchup_pending(to, from) {
                            for e in r.sim.on_fetch(author, a, b) {
                                self.net.post_catchup(w, to, from, &e);
                            }
                        }
                        continue;
                    }
                    // Scoped (B): at 2 members, my own chain's repair while I'm paused at my window doesn't yield. And
                    // (@coordinator-opus mc7179a848c6e, extended to the echo phase): a member in its echo phase catching up
                    // on its own hole doesn't wait behind the live stream either (nobound down-k12-m3: 42 of its 55 fetches
                    // skipped). (spike: PE_NO_ECHO_REPAIR_PRIORITY=1 switches the second off)
                    let echo_catchup = echoing.contains(&(w, from)) && std::env::var("PE_NO_ECHO_REPAIR_PRIORITY").is_err();
                    // (spike: PE_WITHHOLD_CATCHUP=<member>) that member never answers a member in its echo phase: the
                    // withholding cell (@coordinator-opus mca39b9f6b726).
                    if echoing.contains(&(w, from)) && std::env::var("PE_WITHHOLD_CATCHUP").ok().and_then(|v| v.parse::<MemberId>().ok()) == Some(to) {
                        continue;
                    }
                    if echo_catchup {
                        // Its own pull: at once, in datagrams of its own, which the enter test leaves out (margin rule 1).
                        let reply = r.sim.on_fetch(author, a, b);
                        if trace {
                            eprintln!("REPAIR t={} ms m{to} answers m{from}'s fetch of m{author} s{a}..={b} with {} entries (echo phase, own catch-up)", now / 1000, reply.len());
                        }
                        self.net.send_catchup(w, to, from, reply);
                        continue;
                    }
                    if self.net.backlog_us(to, from) > REPAIR_MAX_BACKLOG_US && !r.sim.repair_holds_conf(from, author) && !echo_catchup {
                        if trace {
                            eprintln!("REPAIR t={} ms m{to} skips m{from}'s fetch of m{author} s{a}..={b}: backlog {} us", now / 1000, self.net.backlog_us(to, from));
                        }
                        continue;
                    }
                    if trace && self.net.backlog_us(to, from) > REPAIR_MAX_BACKLOG_US {
                        eprintln!("REPAIR t={} ms m{to} answers m{from}'s fetch of m{author} s{a}..={b} past its backlog {} us (scoped B)", now / 1000, self.net.backlog_us(to, from));
                    }
                    let reply = r.sim.on_fetch(author, a, b);
                    if trace {
                        eprintln!("REPAIR t={} ms m{to} answers m{from}'s fetch of m{author} s{a}..={b} with {} entries", now / 1000, reply.len());
                    }
                    for e in reply {
                        self.net.post(w, to, from, &e);
                    }
                }
                Msg::FetchValue { value } => {
                    for e in r.sim.on_fetch_value(value) {
                        self.net.post(w, to, from, &e);
                    }
                }
                Msg::Join { member, attests, sig, expires } => r.sim.on_join(member, attests, sig, expires),
                Msg::FetchEntry { id, back } => {
                    for e in r.sim.on_fetch_entry(id, back) {
                        self.net.post(w, to, from, &e);
                    }
                }
                Msg::Batch(_) => {}
                Msg::Feed { frame, fed, bodies } => r.sim.on_feed(frame, fed, bodies),
                Msg::Ckpts { member, after, list, fed, echo } => r.sim.on_ckpts(from, member, after, list, fed, echo),
                Msg::CertReq { epoch } => {
                    if let Some(c) = r.sim.on_cert_req(epoch) {
                        self.net.post(w, to, from, &c);
                    }
                }
                Msg::Certs { packs } => {
                    for v in r.sim.on_certs(packs) {
                        broadcast(&mut self.net, w, to, &members, &Msg::Entry(v));
                    }
                    for (dest, f) in r.sim.take_fetches(now / 1000) {
                        if dest != to {
                            self.net.post(w, to, dest, &f);
                        }
                    }
                }
            }
        }
        // 2. peers tick
        let order: Vec<usize> = if self.reverse_order { (0..self.peers.len()).rev().collect() } else { (0..self.peers.len()).collect() };
        for pi in order {
            self.tick_peer(pi, now);
        }
        if self.feed {
            self.send_ckpts();
            self.run_feeds(now);
        }
        // 3. acceptance checks whose receiver is caught up and whose author has the frame
        self.resolve_acceptance();
        // The feed exit's background catch-up, below echo: a source holds its catch-up while its next checkpoint block
        // starts within 7 frames (one catch-up datagram, at most 100 ms of its link), so the Ckpts announcing the new
        // checkpoint don't wait behind one (down-k8-m3: the catch-up raised the others' fed-mode lag 36.3 -> 42.1 frames).
        let closed: BTreeSet<(MemberId, MemberId)> = self
            .net
            .catchup_links()
            .into_iter()
            .filter(|&(f, _, w)| {
                let pi = self.peer_index(f);
                self.peers[pi].worlds.get(&w).is_some_and(|r| r.sim.frame % CKPT_FRAMES >= CKPT_FRAMES - 7)
            })
            .map(|(f, t, _)| (f, t))
            .collect();
        self.net.catchup_closed = closed;
        self.net.flush();
    }

    fn tick_peer(&mut self, pi: usize, now: u64) {
        if self.peers[pi].crashed {
            return;
        }
        let pid = self.peers[pi].id;
        let skew = self.peers[pi].skew_us;
        let wids: Vec<WorldId> = self.peers[pi].worlds.keys().copied().collect();
        let mut new_jobs = Vec::new();
        // Each world's present for this peer (R14 deadlines are frames of the destination).
        let presents: BTreeMap<WorldId, u64> = self.peers[pi]
            .worlds
            .iter()
            .map(|(w, r)| (*w, ((now as i64 + skew).max(0) as u64).saturating_sub(r.epoch_us) / r.dt_us))
            .collect();
        let deadline_frames = self.carry_deadline_frames;
        for w in wids {
            let r = self.peers[pi].worlds.get_mut(&w).unwrap();
            let clock = (now as i64 + skew).max(0) as u64;
            if clock < r.epoch_us {
                continue;
            }
            let clock_frame = (clock - r.epoch_us) / r.dt_us;
            r.sim.now = clock_frame;
            let members: Vec<MemberId> = r.sim.audience();
            if r.adapt {
                // R18 congestion control. Signal: this member's own uplink queue delay (the spike
                // reads the bottleneck directly; production estimates a LEDBAT-style one-way queueing
                // delay, 10.9). Up fast, down slowly, and slower still into the level where congestion
                // began or a finer one.
                let b = if self.feed && (r.sim.localized() || r.sim.fed_mode_capable()) {
                    // Thin links Part B, rule 5: the smallest backlog over receivers not on the feed. Congestion on
                    // this member's own uplink shows toward every receiver; a backlog toward one receiver is that
                    // receiver's downlink, which its feed controller takes care of.
                    members.iter().filter(|&&m| m != pid && !self.net.fed.contains(&(w, m))).map(|&m| self.net.backlog_us(pid, m)).min().unwrap_or(0)
                } else {
                    members.iter().filter(|&&m| m != pid).map(|&m| self.net.backlog_us(pid, m)).max().unwrap_or(0)
                };
                // 7.6 reduction 2's idle state: idle calm says nothing about whether the link can carry a
                // gesture (@coordinator-opus, 2026-11-01).
                let idle_now = r.sim.idle();
                if b > 40_000 {
                    // D56: remember the level at which congestion began, not the levels climbed through
                    // while the queue drains. Any congestion keeps the memory, raise or not (review
                    // D56-1): it is forgotten only after 30 s of non-idle time with none.
                    let onset = !r.over;
                    r.over = true;
                    let l = match r.congested {
                        Some((l, _)) if onset => l.max(r.level),
                        Some((l, _)) => l,
                        None => r.level,
                    };
                    // Thin links (Part A): until the drain level's quick return has been used in this episode, D56
                    // remembers at most the coarsest packed level. The first climb through the drain and floor levels
                    // clears a passing backlog; remembered, an onset reached on the way held a 32 kbit/s sweeper at
                    // stride 16 for the rest of its run (26.5 s at level 7, its hand 213 mm behind) and started its
                    // next gesture there. Congestion again after the return means the link needs the floor levels,
                    // and from then on they are remembered: capping them always kept a two-member 8 kbit/s sweeper at
                    // the drain level, which that link cannot carry (363 stalled frames for the other, against 209).
                    let l = if r.drained { l } else { l.min(PACKED_LEVELS) };
                    r.congested = Some((l, now));
                    r.quiet_us = 0;
                    // Thin links (Part A): from the coarsest packed level up, climb only while the queue is deep
                    // (`DEEP_QUEUE_US`) or still growing (by 10 ms) since the last level change. A backlog that level 5
                    // drains with the author's hand exact stays there: on main, every cell that stalled had reached
                    // the 500 ms router queue, and climbing on any 40 ms queue cost the sweeper's hand 86-437 mm at
                    // 24-64 kbit/s for nothing. Climbing only past 300 ms waited too long where level 5 cannot carry
                    // the stream: at 12 kbit/s with four members the others stalled 154 frames, against 116 with the
                    // growth test (main: 155). Below level 5, climbs stay at 100 ms per level.
                    let deep_enough = r.level < PACKED_LEVELS || b > DEEP_QUEUE_US || b > r.level_b + 10_000;
                    if deep_enough && now >= r.level_t + 100_000 && r.level < HAND_STRIDE.len() - 1 {
                        r.level += 1;
                        r.level_t = now;
                        r.level_b = b;
                    }
                } else {
                    r.over = false;
                    if !idle_now {
                        r.quiet_us += 1000;
                    }
                }
                if r.congested.is_some_and(|(_, t)| r.quiet_us >= CONGESTED_MEMORY_US || now >= t + CONGESTED_CEILING_US) {
                    r.congested = None;
                    r.drained = false;
                }
                if b < 10_000 {
                    if r.calm_since == 0 {
                        r.calm_since = now;
                    }
                } else {
                    r.calm_since = 0;
                }
                if idle_now {
                    r.idle_at = now;
                }
                // Lowering into the remembered level or a finer one takes 5 s of calm with the member
                // active throughout: calm while idle never lowers into L (idle calm stops at L + 1), and
                // does not count once the member is active again, not even at the next gesture's first
                // frame. Lowering above that is unchanged.
                // Thin links (review PA-2): above the coarsest packed level, an idle member steps down on calm
                // at the normal 300 ms pace, and an active one after 5 s of calm, idle or not (review PA2-1,
                // @coordinator-opus, 2026-11-01 13:12Z: real gestures can be seconds apart, and idle calm says
                // nothing about the link at levels 6-8 either). So an idle member is back at level 5 within
                // about a second and its next gesture starts packed; the climb rule moves it up within 100-300 ms
                // if level 5 congests. Held at levels 6-8 after a second congestion instead, a third gesture
                // started there without packs: the author's hand trailed 217-757 mm, and a fine-link sweeper's
                // too after its thin watcher was demoted (@coordinator-opus, 2026-11-01). With 5 s per level
                // while idle, a gesture 15 s after the one before still started at level 6 (@design-reviewer).
                // At level 5 and below the 5 s still count only active time.
                // Thin links (Part A): the drain level returns to packed level 5 after 300 ms of calm, whatever
                // D56 remembers and whether or not the member is idle, the first time in a D56 episode. That
                // drains a first congestion's backlog of packs and gives the author its exact hand back soon.
                let quick_drain = r.level == DRAIN_LEVEL && !r.drained;
                let (calm_needed, calm_from) = if quick_drain {
                    (300_000, r.calm_since)
                } else if r.level > 0 && r.congested.is_some_and(|(l, _)| r.level - 1 <= l) {
                    if r.level <= PACKED_LEVELS {
                        (CONGESTED_REENTRY_US, r.calm_since.max(r.idle_at))
                    } else if idle_now {
                        (300_000, r.calm_since)
                    } else {
                        (CONGESTED_REENTRY_US, r.calm_since)
                    }
                } else {
                    (300_000, r.calm_since)
                };
                if r.level > 0 && r.calm_since > 0 && now >= calm_from + calm_needed && now >= r.level_t + 300_000 {
                    if quick_drain {
                        r.drained = true;
                    }
                    r.level -= 1;
                    r.level_t = now;
                    r.level_b = b;
                    r.calm_since = now;
                }
                // Revision 22, the start transient: a fed member never goes below `fed_level_floor`, so a gesture that
                // starts on a thin fed uplink starts coarse. (spike: PE_FED_MIN_LEVEL=L overrides the floor for measuring;
                // 0 turns it off)
                if self.net.fed.contains(&(w, pid)) {
                    let n = r.sim.members.len();
                    let f = r.sim.members.iter().filter(|&&m| self.net.fed.contains(&(w, m))).count();
                    let floor = fed_min_level_override().or_else(|| fed_level_floor(n, f));
                    if let Some(l) = floor {
                        r.level = r.level.max(l.min(HAND_STRIDE.len() - 1));
                    }
                }
                r.ack_every_us = ACK_FRAMES[r.level] * r.dt_us;
                r.level_ms[r.level] += 1;
            }
            // Simulate once per new clock frame (coalesces all late arrivals of a frame into one
            // rollback: option (c)); background worlds advance in periodic bursts.
            // A background world catches up before its member authors anything that carries state
            // (design 22.3 rule 2): a due action forces a burst.
            let due = r.plan.get(&clock_frame).is_some_and(|v| v.iter().any(|a| !matches!(a, Action::Hand { .. })));
            let throttled = !due && r.throttle_us.is_some_and(|p| now >= r.throttle_from_us && now < r.last_burst_us + p);
            let mut new_frame = false;
            if !throttled && (clock_frame != r.last_frame_seen || !r.sim.caught_up()) {
                new_frame = clock_frame != r.last_frame_seen;
                r.last_frame_seen = clock_frame;
                if r.throttle_us.is_some() && now >= r.throttle_from_us {
                    r.last_burst_us = now;
                }
                r.sim.advance(clock_frame);
                // Planned actions for this frame (authored at the start of the frame). Hand poses
                // stream even while the sim is behind (stamped at the present, R18); anything else
                // needs a caught-up sim and is dropped otherwise (as in 3d0f98a). A kick too is
                // authored while its admin is paused (@coordinator-opus mdffbecdc9b48): it is judged on
                // its signature, epoch and the previous decided ACL ((e1)), not on the sim's state, and
                // a pause beyond Part B's tolerance ends only by a kick or a recovered link (the user,
                // 2026-11-01 17:26Z), so dropping it kept such a pause going (cutfix-shrink-late).
                let ready = r.sim.caught_up() && r.sim.frame == clock_frame;
                {
                    let actions = r.plan.remove(&clock_frame).unwrap_or_default();
                    {
                        for a in actions {
                            // (spike: PE_DROP_PAUSED_KICKS=1 drops them again, to compare a cell with 3d0f98a's rule)
                            if !ready && !(matches!(a, Action::Hand { .. }) || matches!(a, Action::Kick { .. }) && !drop_paused_kicks()) {
                                continue;
                            }
                            if let Action::Hand { obj, target_mm, active, ahead } = a {
                                let k = if r.adapt { HAND_STRIDE[r.level] } else { 1 };
                                // The frame this pose is for: the stamp an entry authored now would get.
                                let s = r.sim.frame.max(r.sim.now) + ahead;
                                let mut out = Vec::new();
                                if !active {
                                    out.extend(flush_pack(r, obj));
                                    out.push(r.sim.author_ahead(Payload::Hand { obj, target_mm, active, span: 1, more: Vec::new() }, ahead));
                                } else {
                                    if r.hand_last.get(&obj).is_some_and(|&l| s <= l) {
                                        continue;
                                    }
                                    // Thin links (Part A): above the packed levels, the pre-D73 thinning: one
                                    // pose on frame ticks that are multiples of k, reached over k frames.
                                    if k > 1 && r.level > PACKED_LEVELS {
                                        if clock_frame % k != 0 {
                                            continue;
                                        }
                                        r.hand_last.insert(obj, s);
                                        out.extend(flush_pack(r, obj));
                                        out.push(r.sim.author_ahead(Payload::Hand { obj, target_mm, active, span: k as u8, more: Vec::new() }, ahead));
                                        for e in out {
                                            broadcast(&mut self.net, w, pid, &members, &Msg::Entry(e));
                                        }
                                        continue;
                                    }
                                    r.hand_last.insert(obj, s);
                                    if k == 1 {
                                        out.extend(flush_pack(r, obj));
                                        out.push(r.sim.author_ahead(Payload::Hand { obj, target_mm, active, span: 1, more: Vec::new() }, ahead));
                                    } else {
                                        // D73(a): under R18 thinning every frame keeps its own target. The
                                        // pose moves this member's hand at once (`own_pending`) and goes out
                                        // in a pack of the frames of one stride window, [m k, m k + k - 1],
                                        // stamped at its first frame: one entry every k frames, the
                                        // author's hand exact. A stride change or a gap sends it early.
                                        if r.packs.get(&obj).is_some_and(|p| p.k != k || p.p0 + p.targets.len() as u64 != s) {
                                            out.extend(flush_pack(r, obj));
                                        }
                                        let p = r.packs.entry(obj).or_insert(Pack { p0: s, k, targets: Vec::new() });
                                        p.targets.push(target_mm);
                                        let full = (s + 1) % k == 0 || p.targets.len() as u64 >= k;
                                        r.sim.own_pending.insert((s, obj), target_mm);
                                        r.sim.hold_horizon_below = r.packs.values().map(|p| p.p0).min();
                                        if full {
                                            out.extend(flush_pack(r, obj));
                                        }
                                    }
                                }
                                for e in out {
                                    broadcast(&mut self.net, w, pid, &members, &Msg::Entry(e));
                                }
                                continue;
                            }
                            // Revision 22: a member that has asked to leave acts no more (its entries past the head its
                            // request froze are cut at the decision; counted meanwhile, their withdrawal would roll back
                            // confirmed frames).
                            if r.sim.leaving() && !matches!(a, Action::Leave) {
                                continue;
                            }
                            // Revision 22, D-5: a leave request is sent first, ahead of queued entries.
                            if matches!(a, Action::Leave) {
                                if let Some(e) = r.sim.request_leave() {
                                    for &to in &members {
                                        if to != pid {
                                            self.net.send_urgent(w, pid, to, &Msg::Entry(e.clone()));
                                        }
                                    }
                                }
                                continue;
                            }
                            let e = match a {
                                Action::Touch { obj, impulse, grab } => {
                                    r.sim.author(Payload::Touch { obj, impulse, grab }, Some(obj))
                                }
                                Action::Hand { .. } => unreachable!("hand poses are handled above"),
                                Action::Portal { open } => r.sim.author(Payload::Portal { open }, None),
                                Action::Owner { change, keys } => {
                                    let Some(cert) = r.sim.owner_cert(change, &keys) else { continue };
                                    r.sim.publish_ownership(cert)
                                }
                                Action::Backdated { obj, impulse, back } => {
                                    r.sim.author_backdated(Payload::Touch { obj, impulse, grab: false }, back)
                                }
                                Action::Kick { target } => r.sim.kick(target),
                                Action::Leave => match r.sim.request_leave() {
                                    Some(e) => e,
                                    None => continue,
                                },
                                Action::SetRole { member, role } => r.sim.set_role(member, role),
                                Action::DefineRole { role, perms } => r.sim.define_role(role, perms),
                                Action::Carry { obj, dest, .. } => {
                                    let deadline = presents.get(&dest).map_or(u64::MAX, |p| p + deadline_frames);
                                    let hop = r.sim.pending_hop(obj);
                                    let e = r.sim.author(Payload::Depart { obj, dest, deadline, hop }, None);
                                    let id = e.id();
                                    let transfer = transfer_id(w, id);
                                    new_jobs.push(Job {
                                        transfer,
                                        src: w,
                                        dst: dest,
                                        depart: id,
                                        arrive: None,
                                        verdict: None,
                                        phase: JobPhase::WaitDepart,
                                        accepted: None,
                                        record: None,
                                        log: vec![format!("t={}ms depart authored in world {w} at frame {}", now / 1000, e.stamp)],
                                    });
                                    broadcast(&mut self.net, w, pid, &members, &Msg::Entry(e));
                                    continue;
                                }
                            };
                            broadcast(&mut self.net, w, pid, &members, &Msg::Entry(e));
                            // What recording it produced (my vote on the target of my own kick) goes right after it, in
                            // chain order. Sent at the tick's end, behind my ack of the same tick, it reached everyone
                            // behind a hole, and each asked every member for it, the fed ones too, which answered on their
                            // thin uplinks (kickliar-fed7: 59 B ahead of member 7's round-1 prevote, 8 frames later).
                            for v in r.sim.take_self_out() {
                                broadcast(&mut self.net, w, pid, &members, &Msg::Entry(v));
                            }
                        }
                    }
                }
            }
            let r = self.peers[pi].worlds.get_mut(&w).unwrap();
            // With batching, acks ride in the same datagram as the frame's entries (R18, viz-4). An idle
            // member (7.6 reduction 2) acks every IDLE_ACK_FRAMES frames at any level, the longer of the
            // two intervals; the ack that leaves idle goes at once, off the grid.
            let idle = r.sim.idle();
            let leaving = r.was_idle && !idle && !r.ack_grid_only;
            r.was_idle = idle;
            let base = (r.ack_every_us / r.dt_us).max(1);
            let fed = self.net.fed.contains(&(w, pid));
            let every = if fed {
                // (spike: PE_FED_ACK_FRAMES overrides P, for measuring)
                // Revision 22 (@coordinator-opus mfdac6071ad92): 32 only while n − f >= q; in the two-fed region (n − f < q),
                // where the latency floor applies and E_T's P95 is about P, 16 until settled.
                let n = r.sim.members.len();
                let f = r.sim.members.iter().filter(|&&m| self.net.fed.contains(&(w, m))).count();
                let in_scope = fed_in_scope(n, f);
                let p =std::env::var("PE_FED_ACK_FRAMES").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(if n >= 4 && in_scope { FED_ACK_FRAMES_N4 } else { FED_ACK_FRAMES });
                base.max(p)
            } else if idle {
                base.max(IDLE_ACK_FRAMES)
            } else {
                base
            };
            // (spike: PE_FED_ACK_PHASE=1 offsets a fed member's ack ticks by half a period from the 32-frame checkpoint
            // blocks, so the ack after a block's end isn't always ahead of that block's checkpoint; measuring E_T)
            let phase = if fed && std::env::var("PE_FED_ACK_PHASE").is_ok() { every / 2 } else { 0 };
            // (spike measurement, option 2 of m9d40b0d9a9d7: PE_ECHO_ACK=B, a fed member acks within B frames of a Ckpts
            // that advanced its echo, besides its tick)
            let echo_ack = fed && new_frame && r.sim.echo_ack_at.is_some_and(|t| r.sim.now >= t);
            let ack_due = leaving
                || echo_ack
                || if self.net.batching {
                    new_frame && (clock_frame + phase) % every == 0
                } else {
                    now >= r.next_ack_us
                };
            // Carry-over, hole 2: a member that has asked to leave authors no ack from its request on.
            if ack_due && !(r.sim.quiet_until_member && !r.sim.members.contains(&pid)) && !r.sim.acks_held() {
                r.next_ack_us = now + every * r.dt_us;
                *self.ack_count.entry((w, pid)).or_default() += 1;
                r.sim.echo_ack_at = None;
                let a = r.sim.author_ack();
                broadcast(&mut self.net, w, pid, &members, &Msg::Entry(a));
            }
            for e in r.sim.attest_due() {
                broadcast(&mut self.net, w, pid, &members, &Msg::Entry(e));
            }
            // Revision 22, S-4 on the transition: a removal that left n = 3 with two fed sends one fed member out by
            // request, first, ahead of queued datagrams (D-5).
            // Revision 22, the latency floor's leave: a fed member over it in the two-fed region, with removal votes from
            // t + 1 distinct full members, leaves by request, sent first too (D-5).
            if let Some(e) = r.sim.s4_leave_due().or_else(|| r.sim.latency_leave_due()) {
                for &to in &members {
                    if to != pid {
                        self.net.send_urgent(w, pid, to, &Msg::Entry(e.clone()));
                    }
                }
            }
            // Review R3-2: vote out members that have gone silent; P2-4: decide removals.
            for e in r.sim.suspects_due() {
                broadcast(&mut self.net, w, pid, &members, &Msg::Entry(e));
            }
            for e in r.sim.view_change_due() {
                broadcast(&mut self.net, w, pid, &members, &Msg::Entry(e));
            }
            // What recording my own entries produced this tick (my vote on the target of my own kick).
            for e in r.sim.take_self_out() {
                broadcast(&mut self.net, w, pid, &members, &Msg::Entry(e));
            }
            for (dest, f) in r.sim.take_fetches(now / 1000) {
                if dest != pid {
                    self.net.post(w, pid, dest, &f);
                }
            }
        }
        // Reviews NEW-6, R3-7: certificates are self-certifying, so every member of both worlds
        // relays a pending transfer it is not relaying yet; the carrier may have crashed. A second
        // relay of the same transfer is refused as a duplicate, so this changes no outcome.
        let p = &self.peers[pi];
        let mut adopt = Vec::new();
        for (src, rs) in &p.worlds {
            for (t, esc) in &rs.sim.t.escrow {
                let known = p.jobs.iter().chain(new_jobs.iter()).any(|j| j.transfer == *t);
                if esc.status == EscrowStatus::Pending && p.worlds.contains_key(&esc.dest) && !known {
                    adopt.push(Job {
                        transfer: *t,
                        src: *src,
                        dst: esc.dest,
                        depart: esc.depart,
                        arrive: None,
                        verdict: None,
                        phase: JobPhase::WaitDepart,
                        accepted: None,
                        record: None,
                        log: vec![format!("t={}ms relaying transfer {t} from world {src}, as a member of both worlds", now / 1000)],
                    });
                }
            }
        }
        new_jobs.extend(adopt);
        self.peers[pi].jobs.extend(new_jobs);
        self.run_jobs(pi, now);
    }

    /// Settled handoff (design 22.6): relay only confirmed facts between worlds.
    fn run_jobs(&mut self, pi: usize, now: u64) {
        let pid = self.peers[pi].id;
        let mut jobs = std::mem::take(&mut self.peers[pi].jobs);
        for j in jobs.iter_mut() {
            match j.phase {
                JobPhase::WaitDepart => {
                    let src = &self.peers[pi].worlds[&j.src].sim;
                    let Some(f) = src.effective_frame(j.depart) else { continue };
                    if !(src.is_confirmed(j.depart) && src.frame > f && src.caught_up()) {
                        continue;
                    }
                    let esc = src.t.escrow.get(&j.transfer).copied();
                    match esc {
                        Some(Escrow { status: EscrowStatus::Pending, record: Some(rec), deadline, .. }) => {
                            // Relay only with A's certificate (review NEW-4), which covers the
                            // object's provenance chain (P2-5).
                            let chain = src.chain_of(rec.global);
                            let chash = chain_hash(&chain);
                            let Some((proof, cert)) = src.vouch(rec.src_frame, |e| fact_departed(j.src, j.depart, &rec, j.dst, deadline, chash, e)) else { continue };
                            let dst = self.peers[pi].worlds.get_mut(&j.dst).unwrap();
                            if !dst.sim.caught_up() || dst.sim.frame < dst.sim.now {
                                continue;
                            }
                            j.record = Some(rec);
                            let e = dst.sim.author(Payload::Arrive { transfer: j.transfer, record: rec, depart: j.depart, deadline, cert, chain, proof }, None);
                            j.log.push(format!(
                                "t={}ms depart confirmed in world {} (frame {f}); arrive authored in world {} at frame {}",
                                now / 1000, j.src, j.dst, e.stamp
                            ));
                            j.arrive = Some(e.id());
                            j.phase = JobPhase::WaitArrive;
                            let members: Vec<MemberId> = dst.sim.members.iter().copied().collect();
                            broadcast(&mut self.net, j.dst, pid, &members, &Msg::Entry(e));
                        }
                        other => {
                            j.log.push(format!("t={}ms depart confirmed but not pending ({other:?}); nothing relayed", now / 1000));
                            j.phase = JobPhase::Done;
                        }
                    }
                }
                JobPhase::WaitArrive => {
                    let id = j.arrive.unwrap();
                    let dst = &self.peers[pi].worlds[&j.dst].sim;
                    let Some(f) = dst.effective_frame(id) else { continue };
                    if !(dst.is_confirmed(id) && dst.frame > f && dst.caught_up()) {
                        continue;
                    }
                    let (accepted, frame) = match dst.t.arrivals.get(&j.transfer) {
                        Some(Arrival::Accepted { frame, .. }) => (true, *frame),
                        Some(Arrival::Rejected { frame, .. }) => (false, *frame),
                        None => continue,
                    };
                    // Relay only with B's certificate over the verdict (review NEW-3), with B's
                    // epoch proof at its arrival frame (R3-8b).
                    let Some((proof, cert)) = dst.vouch(frame, |e| fact_verdict(j.dst, j.transfer, accepted, frame, e)) else { continue };
                    let src = self.peers[pi].worlds.get_mut(&j.src).unwrap();
                    // State-free: stamped at the present even if this replica lags (design 22.3).
                    let sim_at = src.sim.frame;
                    let e = src.sim.author(Payload::Verdict { transfer: j.transfer, accepted, cert, frame, proof }, None);
                    j.log.push(format!(
                        "t={}ms arrival confirmed in world {} (frame {f}, accepted={accepted}); verdict authored in world {} at frame {} (its sim there at frame {sim_at})",
                        now / 1000, j.dst, j.src, e.stamp
                    ));
                    j.accepted = Some(accepted);
                    j.verdict = Some(e.id());
                    j.phase = JobPhase::WaitVerdict;
                    let members: Vec<MemberId> = src.sim.members.iter().copied().collect();
                    broadcast(&mut self.net, j.src, pid, &members, &Msg::Entry(e));
                }
                JobPhase::WaitVerdict => {
                    let id = j.verdict.unwrap();
                    let src = &self.peers[pi].worlds[&j.src].sim;
                    if src.is_confirmed(id) && src.effective_frame(id).is_some_and(|f| src.frame > f) {
                        j.log.push(format!("t={}ms verdict confirmed in world {}; transfer {} resolved", now / 1000, j.src, j.transfer));
                        let committed = matches!(src.t.escrow.get(&j.transfer), Some(Escrow { status: EscrowStatus::Committed, .. }));
                        j.phase = if committed { JobPhase::WaitCommit } else { JobPhase::Done };
                    }
                }
                JobPhase::WaitCommit => {
                    // P2-5: once A's majority has attested the commit, copy the hop into B's log.
                    let rec = j.record.expect("relayed record");
                    let src = &self.peers[pi].worlds[&j.src].sim;
                    // P25-5: signed by the epoch active at the commit frame, with A's certificates
                    // up to it, so any world can check the hop from A's genesis; VC-7 and VC-8 in
                    // `own_hop`.
                    let Some(hop) = src.own_hop(j.transfer, rec.global) else { continue };
                    let dst = self.peers[pi].worlds.get_mut(&j.dst).unwrap();
                    let e = dst.sim.author(Payload::Provenance { hop }, None);
                    j.log.push(format!("t={}ms commit certified in world {}; hop relayed into world {} at frame {}", now / 1000, j.src, j.dst, e.stamp));
                    j.phase = JobPhase::Done;
                    let members: Vec<MemberId> = dst.sim.members.iter().copied().collect();
                    broadcast(&mut self.net, j.dst, pid, &members, &Msg::Entry(e));
                }
                JobPhase::Done => {}
            }
        }
        self.peers[pi].jobs = jobs;
    }

    fn resolve_acceptance(&mut self) {
        for pi in 0..self.peers.len() {
            let wids: Vec<WorldId> = self.peers[pi].worlds.keys().copied().collect();
            for w in wids {
                let pending = std::mem::take(&mut self.peers[pi].worlds.get_mut(&w).unwrap().pending_accept);
                let mut keep = Vec::new();
                for (id, author, obj) in pending {
                    let rs = &self.peers[pi].worlds[&w].sim;
                    let ready = rs.caught_up() && rs.frame >= rs.now && rs.effective_frame(id).is_some_and(|f| rs.frame > f);
                    if !ready {
                        keep.push((id, author, obj));
                        continue;
                    }
                    let frame = rs.frame - 1;
                    let mine = rs.obj_hist.get(&(frame, obj)).copied();
                    let ai = self.peer_index(author);
                    let theirs = self.peers[ai].worlds.get(&w).and_then(|r| r.sim.obj_hist.get(&(frame, obj)).copied());
                    match (mine, theirs) {
                        (Some(a), Some(b)) => self.acceptance.push(Acceptance {
                            world: w,
                            receiver: self.peers[pi].id,
                            author,
                            obj,
                            frame,
                            equal: a == b,
                            receiver_hash: a,
                        }),
                        _ => keep.push((id, author, obj)),
                    }
                }
                self.peers[pi].worlds.get_mut(&w).unwrap().pending_accept = keep;
            }
        }
    }

    pub fn replica(&self, peer: MemberId, w: WorldId) -> &Replica {
        &self.peers[self.peer_index(peer)].worlds[&w]
    }
}

/// The ball-pit hand target (millimetres) that `member`'s controller is at in frame `fr`, or None
/// outside the scripted gesture: descend over 0.33 s, sweep for 3 s in a sine across the pit, rise.
pub fn ballpit_target_mm(hz: u64, member: MemberId, fr: u64) -> Option<[i32; 3]> {
    let f = |s: f64| (s * hz as f64).round() as u64; // seconds -> frames
    let (s0, s1, s2, s3) = (f(2.0), f(2.33), f(5.33), f(5.66));
    if fr < s0 || fr > s3 {
        return None;
    }
    let t = fr as f64 / hz as f64;
    let dir = if member == 1 { 1.0 } else { -1.0 };
    let (x, z) = if fr < s1 {
        (-0.9 * dir, -0.6)
    } else if fr <= s2 {
        let u = (fr - s1) as f64 / (s2 - s1) as f64;
        ((-0.9 + 1.8 * u) * dir, 0.6 * (std::f64::consts::TAU * 1.5 * u).sin() - 0.0 * t)
    } else {
        (0.9 * dir, 0.6 * (std::f64::consts::TAU * 1.5).sin())
    };
    let y = if fr < s1 {
        1.2 - 1.0 * (fr - s0) as f64 / (s1 - s0) as f64
    } else if fr <= s2 {
        0.2
    } else {
        0.2 + 1.0 * (fr - s2) as f64 / (s3 - s2) as f64
    };
    Some([(x * 1000.0).round() as i32, (y * 1000.0).round() as i32, (z * 1000.0).round() as i32])
}

/// The ball-pit plan for `member`'s hand (object 500 + member - 1): one Hand action per frame of
/// the gesture, stamped `ahead` frames later (input delay), then park.
pub fn ballpit_sweep(hz: u64, member: MemberId, ahead: u64) -> Vec<(u64, Action)> {
    let obj = 500 + (member - 1);
    let s3 = (5.66 * hz as f64).round() as u64;
    let mut v = Vec::new();
    for fr in 0..=s3 {
        if let Some(target_mm) = ballpit_target_mm(hz, member, fr) {
            v.push((fr, Action::Hand { obj, target_mm, active: true, ahead }));
        }
    }
    v.push((s3 + 1, Action::Hand { obj, target_mm: [0, 1500, 0], active: false, ahead }));
    v
}

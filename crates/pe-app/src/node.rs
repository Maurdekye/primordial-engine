//! The node: one peer process's IO around its pe-session `SessionCore`. It owns the UDP socket, the
//! clock and its sync with the leader, datagram signing, batching, scripted faults, the background
//! plan, and the results
//! written at the end. It runs on pe-mcp's engine thread (`EngineHandle::spawn_ticking`) and
//! implements pe-mcp's `DrivenPeer`, so an agent can drive it.
//!
//! Session start: the leader is the lowest member id. Every other member pings the leader, estimates
//! its clock offset from the lowest-RTT sample (as spikes/p2p does), and reports Ready. Once every
//! member is Ready, the leader picks an epoch 1 s ahead on its own clock and sends Start until each
//! member acks. Session time = local time + offset (0 for the leader).
//!
//! From frame 0, clock sync keeps running (design 10.4; `clock.rs`): every member pings every live
//! member every 500 ms (a pong carries the replier's session time) and slews toward the fault-tolerant
//! midpoint of their clocks, never stepping. R18's stamps use the rate-corrected clock, so drift
//! between machines (2-9 % between Windows and a WSL2 VM here) never reads as queueing.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::mpsc::{self, Receiver, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ed25519_dalek::{SigningKey, VerifyingKey};
use pe_mcp::backend::{Authored, Counters, DrivenPeer, EntryId, EntryStatus, Interaction, ScenarioControl};
use pe_mcp::model::*;
use pe_mcp::sim_view;
use pe_session::peer::{AuthorError, Event, SessionCore};
use pe_session::session::{digest, final_log, replay_sim_like, Payload, SceneKind, WorldSim};
use pe_session::splitmix64;
use serde::{Deserialize, Serialize};

use crate::clock::ClockSync;
use crate::config::SessionFile;
use crate::wire::{self, Item, Stamp, QREP_NONE};

pub const WORLD: u32 = 1;
const PINGS: usize = 12;
/// From frame 0, a member pings every live member this often (design 10.4).
const SYNC_EVERY_US: u64 = 500_000;
const LINGER_US: u64 = 3_000_000;

// R18 link signal (design 10.9): a LEDBAT-style one-way queueing delay (RFC 6817), measured by the
// receiver from each datagram's send time and echoed back in the datagram stamp (`qrep`).
/// The base delay is the minimum one-way delay over 12 buckets of 10 s (2 minutes), expired by time.
/// Not RFC 6817's 10 minutes (coordinator, review AR-1): clock drift of 100 ppm would move the estimate
/// by 60 ms over 10 minutes, past the 40 ms trigger. A queue standing longer than 2 minutes becomes the
/// base (design 10.9's limits; standing queues are the loss rule's job).
const BASE_BUCKET_US: u64 = 10_000_000;
const BASE_BUCKETS: usize = 12;
/// The current delay is the minimum over the last 100 ms or the last 4 samples, whichever spans more.
const CURRENT_US: u64 = 100_000;
const CURRENT_SAMPLES: usize = 4;
/// A report counts at most this much above the reporter's round-trip excess (design 10.4's clamp: a
/// clock-frequency error cannot fake queueing beyond what the round trip, on one clock, shows).
const RTT_CLAMP_MARGIN_US: u64 = 20_000;
/// A member's report counts at its last value for 5 s (fresh within 1 s), then is ignored.
const REPORT_HOLD_US: u64 = 5_000_000;
/// The scripted bandwidth cap's token bucket (the harness's `Cap` model).
const CAP_BURST_BYTES: f64 = 4000.0;
/// Scripted `codel`: CoDel's control law (RFC 8289, simplified to per-datagram decisions): once the
/// sojourn time has stayed above 5 ms for 100 ms, drop, then drop again every 100 ms / sqrt(count).
const CODEL_TARGET_US: u64 = 5_000;
const CODEL_INTERVAL_US: u64 = 100_000;
/// One row of the link series every 250 ms of session time.
const SERIES_US: u64 = 250_000;
/// Datagrams the receive thread may hold for the engine; beyond that it drops, as a full kernel
/// socket buffer would (review AR-2: a flood must not grow memory). The default UDP receive buffers
/// (64 KB on Windows, about 208 KB on Linux) hold a few hundred to about a thousand of our datagrams.
const RX_QUEUE: usize = 1024;
/// The same bound in bytes (review AR-6): a flood of 64 KB datagrams must not hold 64 MB. Counted by
/// length rather than refusing datagrams over `wire::MTU`, because a single oversized item is sent
/// alone in one oversized datagram.
const RX_QUEUE_BYTES: usize = RX_QUEUE * wire::MTU;

/// A scripted change to this node's outgoing links, at a session time.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FaultStep {
    /// Session milliseconds since the epoch; 0 also covers the time before it (clock sync).
    pub at_ms: u64,
    /// One destination member, or every link when absent.
    #[serde(default)]
    pub to: Option<u32>,
    #[serde(default)]
    pub loss_pct: u32,
    #[serde(default)]
    pub delay_ms: u64,
    #[serde(default)]
    pub jitter_ms: u64,
    /// A bandwidth cap on the link, kbit/s (0: none): a token bucket with a 4 kB burst and a
    /// tail-drop queue of `queue_ms`, ahead of the delay. The estimator never reads this queue.
    #[serde(default)]
    pub rate_kbps: u64,
    #[serde(default = "default_queue_ms")]
    pub queue_ms: u64,
    /// CoDel-like active queue management on the capped queue (design 10.9's AQM limit).
    #[serde(default)]
    pub codel: bool,
}

fn default_queue_ms() -> u64 {
    500
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Link {
    loss_pct: u32,
    delay_us: u64,
    jitter_us: u64,
    rate_bps: u64,
    queue_us: u64,
    codel: bool,
}

/// The scripted cap's queue towards one member.
#[derive(Default)]
struct Shaper {
    tokens: f64,
    last_us: u64,
    queue: std::collections::VecDeque<(u64, SocketAddr, Vec<u8>)>,
    bytes: u64,
    first_above: u64,
    dropping: bool,
    count: u64,
    drop_next: u64,
    /// The longest sojourn in the current series row, µs (the true queue, for the evidence only).
    row_max_us: u64,
}

/// R18: what this node measures on one member's datagrams to it.
#[derive(Default)]
struct RxLink {
    /// The sender's clock, unwrapped to 64 bits from the newest value seen.
    last_ts: Option<i64>,
    /// (bucket start, minimum one-way delay in it), oldest first.
    base: std::collections::VecDeque<(u64, i64)>,
    /// (receive time, one-way delay), oldest first.
    recent: std::collections::VecDeque<(u64, i64)>,
    q_us: Option<u64>,
    last_seq: Option<u16>,
    received: u64,
    lost: u64,
}

impl RxLink {
    fn sample(&mut self, st: Stamp, recv_us: u64) {
        let ts = match self.last_ts {
            None => st.ts as i64,
            Some(l) => l + st.ts.wrapping_sub(l as u32) as i32 as i64,
        };
        self.last_ts = Some(self.last_ts.map_or(ts, |l| l.max(ts)));
        // Offset by the unknown clock difference, which is constant, so it cancels in q.
        let owd = recv_us as i64 - ts;
        match self.base.back_mut() {
            Some(b) if recv_us < b.0 + BASE_BUCKET_US => b.1 = b.1.min(owd),
            _ => {
                self.base.push_back((recv_us, owd));
                if self.base.len() > BASE_BUCKETS {
                    self.base.pop_front();
                }
            }
        }
        // Buckets also expire by time (review AR-5): after a long silence the old ones must not stay.
        while self.base.front().is_some_and(|b| b.0 + BASE_BUCKET_US * BASE_BUCKETS as u64 <= recv_us) {
            self.base.pop_front();
        }
        self.recent.push_back((recv_us, owd));
        while self.recent.len() > CURRENT_SAMPLES && self.recent.front().is_some_and(|r| r.0 + CURRENT_US < recv_us) {
            self.recent.pop_front();
        }
        let base = self.base.iter().map(|b| b.1).min().unwrap_or(owd);
        let current = self.recent.iter().map(|r| r.1).min().unwrap_or(owd);
        self.q_us = Some((current - base).max(0) as u64);
        // Loss from sequence gaps; a reordered or duplicated datagram counts as neither.
        self.received += 1;
        match self.last_seq {
            Some(l) => {
                let gap = st.seq.wrapping_sub(l);
                if gap >= 1 && gap < 0x8000 {
                    self.lost += (gap - 1) as u64;
                    self.last_seq = Some(st.seq);
                }
            }
            None => self.last_seq = Some(st.seq),
        }
    }
}

/// R18 evidence: one row of the link series.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LinkRow {
    pub t_ms: u64,
    pub level: usize,
    /// What the controller was fed (or would have been, with the signal off), ms; None before any report.
    pub signal_ms: Option<f64>,
    pub held: bool,
    /// The longest real sojourn in this node's scripted cap queues during the row, ms.
    pub true_queue_ms: f64,
    pub kbit_s: f64,
    /// Clock frame minus confirmed frame at the row's end (a stall shows as a growing lag).
    pub conf_lag: u64,
    /// This node's own estimate of each member's queue towards it, ms.
    pub rx_q_ms: BTreeMap<u32, f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LinkSummary {
    pub signal_fed: bool,
    /// Ticks spent at each congestion level (SessionCore's `level_ms`).
    pub level_ticks: Vec<u64>,
    pub max_level: usize,
    pub max_signal_ms: f64,
    pub max_true_queue_ms: f64,
    pub hand_entries: u64,
    /// Ticks at which the round-trip clamp lowered the fed signal (design 10.4).
    #[serde(default)]
    pub clamped_ticks: u64,
    /// Per member: (datagrams received from it, datagrams lost on its way to me).
    pub rx: BTreeMap<u32, (u64, u64)>,
}

pub struct NodeOpts {
    pub me: u32,
    pub session: SessionFile,
    pub key: SigningKey,
    pub socket: UdpSocket,
    pub faults: Vec<FaultStep>,
    /// The member stops dead at this session time (a scripted drop-out): no goodbye, no flush.
    pub crash_at_ms: Option<u64>,
    pub touches_per_s: u32,
    /// Author this member's hand along a sweep every frame (ball pit only); R18 thins it.
    pub hand_sweep: bool,
    /// Feed the R18 controller the measured link signal (off: today's level-0 behaviour, for comparison).
    pub link_signal: bool,
    /// Test runs (design 10.4's evidence): this member's local clock runs this many ppm fast (a
    /// simulated skewed machine); its pongs to the listed members lie by (µs, ppm of local time: a
    /// constant and a growing lie); and a plain median instead of the fault-tolerant midpoint.
    pub clock_skew_ppm: i64,
    pub clock_lie_us: BTreeMap<u32, (i64, i64)>,
    pub clock_median: bool,
    pub out_dir: PathBuf,
    /// Set once the results are written and the linger time has passed.
    pub finished: Arc<AtomicBool>,
    pub verbose: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Traffic {
    pub datagrams_sent: u64,
    pub datagrams_dropped_by_fault: u64,
    #[serde(default)]
    pub datagrams_dropped_by_cap: u64,
    #[serde(default)]
    pub datagrams_dropped_by_codel: u64,
    /// Dropped by the receive thread because the engine's queue was full (1024 datagrams or
    /// 1024 × MTU bytes).
    #[serde(default)]
    pub datagrams_dropped_rx_full: u64,
    pub datagrams_received: u64,
    pub bytes_sent: u64,
    pub rejected: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    pub session: String,
    pub member: u32,
    pub frames: u64,
    pub conf: u64,
    /// FNV digest of the state hashes of frames 0..frames.
    pub digest: String,
    /// The same over an offline replay of this member's final log.
    pub replay_digest: String,
    pub replay_equal: bool,
    pub members_at_end: Vec<u32>,
    pub evicted: Vec<u32>,
    pub epoch: u32,
    pub rollbacks: u64,
    pub max_rollback_depth: u64,
    pub checks_ok: u64,
    pub checks_bad: u64,
    pub entries_authored: u64,
    pub clock_offset_us: i64,
    pub best_rtt_us: u64,
    /// The frequency correction of this member's session clock against its local clock, ppm (10.4).
    #[serde(default)]
    pub clock_rate_ppm: f64,
    pub traffic: Traffic,
    #[serde(default)]
    pub link: LinkSummary,
    pub events: Vec<String>,
}

pub struct Node {
    core: SessionCore,
    me: u32,
    session: SessionFile,
    key: SigningKey,
    keys: BTreeMap<u32, VerifyingKey>,
    sock: UdpSocket,
    /// Datagrams from the receive thread, each with its arrival time (µs since `origin`).
    net_rx: Receiver<(u64, Vec<u8>)>,
    rx_full: Arc<AtomicU64>,
    /// Bytes waiting in `net_rx`.
    rx_bytes: Arc<AtomicUsize>,
    origin: Instant,
    leader: u32,
    /// Session time = local time + offset.
    offset_us: i64,
    /// Continuous, fault-tolerant clock sync among the members (design 10.4).
    clock: ClockSync,
    pongs: usize,
    ready_from: BTreeSet<u32>,
    acked: BTreeSet<u32>,
    epoch: Option<u64>,
    last_ping: u64,
    /// Pings still unanswered, (member, send time): a pong counts only if it echoes one of them.
    pings_out: BTreeSet<(u32, u64)>,
    /// Session time has reached frame 0: from here the clock slews among the members.
    synced: bool,
    skew: f64,
    clock_lie_us: BTreeMap<u32, (i64, i64)>,
    last_ready: u64,
    last_start: u64,
    control: BTreeMap<u32, Vec<Item>>,
    faults: Vec<FaultStep>,
    links: BTreeMap<u32, Link>,
    default_link: Link,
    delayed: Vec<(u64, SocketAddr, Vec<u8>)>,
    shapers: BTreeMap<u32, Shaper>,
    rx: BTreeMap<u32, RxLink>,
    out_seq: BTreeMap<u32, u16>,
    /// Members' reports of my queue towards them: (ms, when received).
    reports: BTreeMap<u32, (u16, u64)>,
    link_signal: bool,
    signal_us: Option<u64>,
    signal_held: bool,
    max_signal_us: u64,
    clamped_ticks: u64,
    max_true_queue_us: u64,
    hand: Option<u32>,
    hand_frame: u64,
    hand_entries: u64,
    /// An agent's held hand target (pe-mcp, D52 ruling (a)).
    hold: sim_view::HandHold,
    series: Vec<LinkRow>,
    row_start_us: u64,
    row_bytes: u64,
    rng: u64,
    crash_at_ms: Option<u64>,
    plan: BTreeMap<u64, Vec<Payload>>,
    authored: u64,
    out_dir: PathBuf,
    written: Option<u64>,
    finished: Arc<AtomicBool>,
    traffic: Traffic,
    log: Vec<String>,
    verbose: bool,
    last_status: u64,
}

impl Node {
    pub fn new(o: NodeOpts) -> Node {
        let s = &o.session;
        let ids = s.ids();
        let scene = s.scene_kind().expect("validated session");
        // View-change membership (the default), not the static one: M1 must survive a member
        // dropping out, which takes suspicion, votes and a new epoch.
        let core = SessionCore::new(WorldSim::new(WORLD, o.me, &ids, scene, s.sim_config(), s.key_ring(o.key.clone())));
        let keys = ids.iter().map(|&m| (m, s.key_of(m).expect("validated key"))).collect();
        // Background touches: deterministic in (seed, member), on non-hand objects.
        let mut plan: BTreeMap<u64, Vec<Payload>> = BTreeMap::new();
        if o.touches_per_s > 0 {
            let objects = match scene {
                SceneKind::Pile { side, layers } => (side * side * layers) as u64,
                SceneKind::BallPit { balls, .. } => balls as u64,
                _ => 1,
            };
            let period = (60 / o.touches_per_s as u64).max(1);
            let mut r = splitmix64(s.seed ^ (o.me as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut f = 60 + r % period;
            while f + 60 < s.frames {
                r = splitmix64(r);
                let c = |k: u32| ((r >> (16 + 8 * k)) & 0xff) as f32 / 255.0;
                plan.entry(f).or_default().push(Payload::Touch { obj: (r % objects) as u32, impulse: [4.0 * c(0) - 2.0, 4.0 * c(1), 4.0 * c(2) - 2.0], grab: false });
                f += period;
            }
        }
        // A receive thread blocks on the socket and stamps each datagram as it arrives (design 10.9:
        // the receive time is taken at the socket), so the engine's tick time never reads as R18
        // queueing. The engine thread only sends.
        let origin = Instant::now();
        let (net_tx, net_rx) = mpsc::sync_channel(RX_QUEUE);
        let rx_full = Arc::new(AtomicU64::new(0));
        let skew = 1.0 + o.clock_skew_ppm as f64 / 1e6;
        let full = rx_full.clone();
        let rx_bytes = Arc::new(AtomicUsize::new(0));
        let queued = rx_bytes.clone();
        let rsock = o.socket.try_clone().expect("clone the socket");
        rsock.set_read_timeout(Some(Duration::from_millis(100))).expect("read timeout");
        std::thread::Builder::new()
            .name("pe-net-rx".into())
            .spawn(move || {
                let mut buf = vec![0u8; 65536];
                loop {
                    match rsock.recv_from(&mut buf) {
                        Ok((n, _)) => {
                            let t = (origin.elapsed().as_micros() as f64 * skew) as u64;
                            // This thread is the only one adding, so the check cannot overshoot.
                            if queued.load(Ordering::Acquire) + n > RX_QUEUE_BYTES {
                                full.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            queued.fetch_add(n, Ordering::AcqRel);
                            match net_tx.try_send((t, buf[..n].to_vec())) {
                                Ok(()) => {}
                                Err(TrySendError::Full(_)) => {
                                    queued.fetch_sub(n, Ordering::AcqRel);
                                    full.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(TrySendError::Disconnected(_)) => break,
                            }
                        }
                        // A timeout lets nothing through; Windows reports an ICMP port-unreachable
                        // for an earlier send (a member not up yet, or gone) as a reset: skip it.
                        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::ConnectionReset) => {}
                        Err(_) => break,
                    }
                }
            })
            .expect("spawn the receive thread");
        // Each member's hand in the ball pit is object balls + its rank among the sorted ids.
        let hand = match scene {
            SceneKind::BallPit { balls, .. } if o.hand_sweep => ids.iter().position(|&m| m == o.me).map(|k| (balls + k) as u32),
            _ => None,
        };
        Node {
            core,
            me: o.me,
            leader: ids[0],
            keys,
            session: o.session,
            key: o.key,
            sock: o.socket,
            net_rx,
            rx_full,
            rx_bytes,
            origin,
            offset_us: 0,
            clock: ClockSync::new(o.clock_median),
            pongs: 0,
            ready_from: BTreeSet::new(),
            acked: BTreeSet::new(),
            epoch: None,
            last_ping: 0,
            pings_out: BTreeSet::new(),
            synced: false,
            skew: 1.0 + o.clock_skew_ppm as f64 / 1e6,
            clock_lie_us: o.clock_lie_us,
            last_ready: 0,
            last_start: 0,
            control: BTreeMap::new(),
            faults: o.faults,
            links: BTreeMap::new(),
            default_link: Link::default(),
            delayed: Vec::new(),
            shapers: BTreeMap::new(),
            rx: BTreeMap::new(),
            out_seq: BTreeMap::new(),
            reports: BTreeMap::new(),
            link_signal: o.link_signal,
            signal_us: None,
            signal_held: false,
            max_signal_us: 0,
            clamped_ticks: 0,
            max_true_queue_us: 0,
            hand,
            hand_frame: u64::MAX,
            hand_entries: 0,
            hold: Default::default(),
            series: Vec::new(),
            row_start_us: 0,
            row_bytes: 0,
            rng: splitmix64(o.me as u64 ^ 0x5EED),
            crash_at_ms: o.crash_at_ms,
            plan,
            authored: 0,
            out_dir: o.out_dir,
            written: None,
            finished: o.finished,
            traffic: Traffic::default(),
            log: Vec::new(),
            verbose: o.verbose,
            last_status: 0,
        }
    }

    fn local_us(&self) -> u64 {
        (self.origin.elapsed().as_micros() as f64 * self.skew) as u64
    }

    /// Now on the session timeline.
    fn now_us(&self) -> u64 {
        (self.local_us() as i64 + self.offset_us).max(0) as u64
    }

    /// Advance the offset (design 10.4). Before frame 0 a member other than the leader takes the
    /// leader's offset at once (the bootstrap); from frame 0 it slews toward the fault-tolerant
    /// midpoint of the live members' clocks.
    fn apply_clock(&mut self) {
        let local = self.local_us();
        let members: Vec<u32> = if self.synced {
            self.core.sim.membership().members().iter().copied().filter(|&m| m != self.me).collect()
        } else {
            if self.me != self.leader {
                let _ = self.clock.bootstrap(self.leader);
            }
            vec![]
        };
        let o = self.clock.tick(local, &members);
        if o != self.offset_us {
            self.offset_us = o;
            self.core.set_offset(o);
        }
    }

    fn session_ms(&self) -> Option<u64> {
        let e = self.epoch?;
        Some(self.now_us().saturating_sub(e) / 1000)
    }

    fn note(&mut self, s: String) {
        let t = self.session_ms().map_or("-".to_string(), |m| format!("{m}"));
        let line = format!("[m{} t={t}ms f={}] {s}", self.me, self.core.sim.frame);
        if self.verbose {
            eprintln!("{line}");
        }
        self.log.push(line);
    }

    fn rand(&mut self) -> u64 {
        self.rng = splitmix64(self.rng);
        self.rng
    }

    fn push_control(&mut self, to: u32, i: Item) {
        self.control.entry(to).or_default().push(i);
    }

    // ---- network in ---------------------------------------------------------------------------

    fn receive_all(&mut self) {
        while let Ok((recv_us, buf)) = self.net_rx.try_recv() {
            let n = buf.len();
            self.rx_bytes.fetch_sub(n, Ordering::AcqRel);
            self.traffic.datagrams_received += 1;
            let keys = &self.keys;
            let r = wire::unpack(self.session.session, &buf[..n], |m| keys.get(&m).copied());
            match r {
                Ok((sender, st, items)) if sender != self.me => {
                    // R18 measures on the rate-corrected clock, as the sender stamps (design 10.4).
                    let recv_rc = self.clock.rc(recv_us);
                    self.rx.entry(sender).or_default().sample(st, recv_rc);
                    if st.qrep != QREP_NONE {
                        self.reports.insert(sender, (st.qrep, recv_us));
                    }
                    for i in items {
                        self.on_item(sender, i, recv_us);
                    }
                }
                Ok(_) => {}
                Err(e) => *self.traffic.rejected.entry(format!("{e:?}")).or_default() += 1,
            }
        }
    }

    fn on_item(&mut self, sender: u32, i: Item, recv_us: u64) {
        let local = self.local_us();
        match i {
            // The core verifies entry signatures; the datagram already authenticated `sender`.
            Item::Msg(m) => self.core.receive(sender, m, local),
            // A pong carries this member's session time (a test lie added for some members).
            Item::Ping(t) => {
                let (lie, ppm) = self.clock_lie_us.get(&sender).copied().unwrap_or((0, 0));
                let local = self.local_us();
                let lie = lie + (local as f64 * ppm as f64 / 1e6) as i64;
                let (now, raw) = (self.now_us() as i64 + lie, local as i64 + lie);
                self.push_control(sender, Item::Pong(t, now.max(0) as u64, raw.max(0) as u64))
            }
            Item::Pong(t, r, raw) if self.pings_out.remove(&(sender, t)) => {
                // The peer's clocks read r (session) and raw (local) about halfway through the round trip.
                if sender == self.leader {
                    self.pongs += 1;
                }
                self.clock.sample(sender, t, recv_us, r, raw);
                self.apply_clock();
            }
            Item::Pong(..) => {}
            Item::Ready if self.me == self.leader => {
                self.ready_from.insert(sender);
            }
            Item::Start(e) if sender == self.leader && self.pongs >= PINGS / 2 => {
                if self.epoch.is_none() {
                    self.epoch = Some(e);
                    self.core.start(e);
                    let off = self.offset_us;
                    self.note(format!("session start received: epoch {e} (leader clock), my offset {off} us, best rtt {} us", self.clock.best_rtt_us()));
                }
                self.push_control(sender, Item::StartAck);
            }
            Item::StartAck if self.me == self.leader => {
                self.acked.insert(sender);
            }
            _ => {}
        }
    }

    // ---- start ----------------------------------------------------------------------------------

    fn sync_clock(&mut self) {
        let local = self.local_us();
        let others: Vec<u32> = self.session.ids().into_iter().filter(|&m| m != self.me).collect();
        if !self.synced && self.epoch.is_some_and(|e| self.now_us() >= e) {
            // Frame 0: every member has its epoch (sent 1 s ahead), so nobody jumps from here on.
            self.synced = true;
            self.clock.restart_fits();
        }
        let to: Vec<u32> = if self.synced {
            if local >= self.last_ping + SYNC_EVERY_US {
                self.core.sim.membership().members().iter().copied().filter(|&m| m != self.me).collect()
            } else {
                vec![]
            }
        } else if self.me != self.leader && self.pongs < PINGS && local >= self.last_ping + 25_000 {
            vec![self.leader]
        } else {
            vec![]
        };
        if !to.is_empty() {
            self.last_ping = local;
            // A ping lost on the way is forgotten after 5 s.
            self.pings_out.retain(|&(_, t)| t + 5_000_000 > local);
            for m in to {
                self.pings_out.insert((m, local));
                self.push_control(m, Item::Ping(local));
            }
        }
        self.apply_clock();
        if self.me != self.leader {
            if self.pongs >= PINGS / 2 && self.epoch.is_none() && local >= self.last_ready + 200_000 {
                self.last_ready = local;
                let l = self.leader;
                self.push_control(l, Item::Ready);
            }
            return;
        }
        if self.epoch.is_none() && others.iter().all(|m| self.ready_from.contains(m)) {
            let e = self.now_us() + 1_000_000;
            self.epoch = Some(e);
            self.core.start(e);
            self.note(format!("every member ready: session epoch {e}"));
        }
        if let Some(e) = self.epoch {
            if local >= self.last_start + 200_000 {
                self.last_start = local;
                let unacked: Vec<u32> = others.into_iter().filter(|m| !self.acked.contains(m)).collect();
                for m in unacked {
                    self.push_control(m, Item::Start(e));
                }
            }
        }
    }

    // ---- network out ----------------------------------------------------------------------------

    fn apply_faults(&mut self) {
        // Steps at 0 ms hold from the node's start, clock sync included: the path a session starts on.
        let ms = match self.session_ms() {
            Some(ms) => ms,
            None if self.faults.first().is_some_and(|f| f.at_ms == 0) => 0,
            None => return,
        };
        while self.faults.first().is_some_and(|f| f.at_ms <= ms) {
            let f = self.faults.remove(0);
            let l = Link {
                loss_pct: f.loss_pct,
                delay_us: f.delay_ms * 1000,
                jitter_us: f.jitter_ms * 1000,
                rate_bps: f.rate_kbps * 1000,
                queue_us: f.queue_ms * 1000,
                codel: f.codel,
            };
            match f.to {
                Some(m) => {
                    self.links.insert(m, l);
                }
                None => {
                    self.links.clear();
                    self.default_link = l;
                }
            }
            let cap = if f.rate_kbps > 0 { format!(", cap {} kbit/s, {} ms queue{}", f.rate_kbps, f.queue_ms, if f.codel { ", CoDel" } else { "" }) } else { String::new() };
            self.note(format!("fault: links to {} now {}% loss, {} ms +-{} ms{cap}", f.to.map_or("all".into(), |m| format!("member {m}")), f.loss_pct, f.delay_ms, f.jitter_ms));
        }
    }

    fn flush_out(&mut self) {
        let mut per: BTreeMap<u32, Vec<Item>> = std::mem::take(&mut self.control);
        for o in self.core.outbox() {
            // Class Repair may be dropped while the queue to its requester is long (the requester
            // asks again); a LAN has no such queue, so everything goes out.
            let item = Item::Msg(o.msg);
            for to in o.to {
                per.entry(to).or_default().push(item.clone());
            }
        }
        for (to, items) in per {
            let Some(addr) = self.session.addr_of(to) else { continue };
            let link = self.links.get(&to).copied().unwrap_or(self.default_link);
            // The send time is stamped as the datagram is built, just before it is handed to the
            // network (the scripted cap and delay stand for the network). The stamp is on the
            // rate-corrected clock (design 10.4); the scripted cap and delay run on local time.
            let now = self.local_us();
            let ts = self.clock.rc(now);
            let qrep = self.rx.get(&to).and_then(|r| r.q_us).map_or(QREP_NONE, |q| (q / 1000).min(QREP_NONE as u64 - 1) as u16);
            let mut seq = self.out_seq.get(&to).copied().unwrap_or(0);
            let ds = wire::pack(self.session.session, self.me, &items, &self.key, || {
                seq = seq.wrapping_add(1);
                Stamp { ts: ts as u32, seq, qrep }
            });
            self.out_seq.insert(to, seq);
            for d in ds {
                if link.loss_pct > 0 && self.rand() % 100 < link.loss_pct as u64 {
                    self.traffic.datagrams_dropped_by_fault += 1;
                    continue;
                }
                if link.rate_bps > 0 {
                    let sh = self.shapers.entry(to).or_default();
                    // Tail drop once the queue would take longer than queue_ms to drain.
                    if (sh.bytes + d.len() as u64) * 8_000_000 / link.rate_bps > link.queue_us {
                        self.traffic.datagrams_dropped_by_cap += 1;
                        continue;
                    }
                    sh.bytes += d.len() as u64;
                    sh.queue.push_back((now, addr, d));
                } else {
                    self.propagate(link, addr, d, now);
                }
            }
        }
        let now = self.local_us();
        self.drain_shapers(now);
        let mut i = 0;
        while i < self.delayed.len() {
            if self.delayed[i].0 <= now {
                let (_, a, d) = self.delayed.swap_remove(i);
                self.send_now(a, &d);
            } else {
                i += 1;
            }
        }
    }

    /// The scripted delay and jitter, after any cap.
    fn propagate(&mut self, link: Link, addr: SocketAddr, d: Vec<u8>, now: u64) {
        let jitter = if link.jitter_us > 0 { self.rand() % (link.jitter_us + 1) } else { 0 };
        let due = now + link.delay_us + jitter;
        if due <= now {
            self.send_now(addr, &d);
        } else {
            self.delayed.push((due, addr, d));
        }
    }

    /// Release what each scripted cap's token bucket allows (a cap that was lifted releases all).
    fn drain_shapers(&mut self, now: u64) {
        let tos: Vec<u32> = self.shapers.keys().copied().collect();
        for to in tos {
            let link = self.links.get(&to).copied().unwrap_or(self.default_link);
            let mut out = Vec::new();
            let mut codel_drops = 0;
            {
                let sh = self.shapers.get_mut(&to).unwrap();
                if link.rate_bps > 0 {
                    let dt = now.saturating_sub(sh.last_us) as f64;
                    sh.tokens = (sh.tokens + dt * link.rate_bps as f64 / 8e6).min(CAP_BURST_BYTES);
                } else {
                    sh.tokens = f64::INFINITY;
                }
                sh.last_us = now;
                while let Some((enq, _, d)) = sh.queue.front() {
                    if (d.len() as f64) > sh.tokens {
                        break;
                    }
                    let (enq, addr, d) = (*enq, sh.queue.front().unwrap().1, sh.queue.pop_front().unwrap().2);
                    sh.bytes -= d.len() as u64;
                    let sojourn = now - enq;
                    sh.row_max_us = sh.row_max_us.max(sojourn);
                    // A CoDel drop happens at dequeue and uses no link time, so tokens are spent after.
                    if link.codel {
                        // RFC 8289: never drop while less than one datagram's worth is left queued.
                        let ok_to_drop = if sojourn < CODEL_TARGET_US || sh.bytes <= wire::MTU as u64 {
                            sh.first_above = 0;
                            false
                        } else if sh.first_above == 0 {
                            sh.first_above = now + CODEL_INTERVAL_US;
                            false
                        } else {
                            now >= sh.first_above
                        };
                        let gap = |count: u64| (CODEL_INTERVAL_US as f64 / (count as f64).sqrt()) as u64;
                        if sh.dropping {
                            if !ok_to_drop {
                                sh.dropping = false;
                            } else if now >= sh.drop_next {
                                sh.count += 1;
                                sh.drop_next += gap(sh.count);
                                codel_drops += 1;
                                continue;
                            }
                        } else if ok_to_drop {
                            sh.dropping = true;
                            sh.count = if sh.count > 2 && now < sh.drop_next + 16 * CODEL_INTERVAL_US { sh.count - 2 } else { 1 };
                            sh.drop_next = now + gap(sh.count);
                            codel_drops += 1;
                            continue;
                        }
                    }
                    sh.tokens -= d.len() as f64;
                    out.push((addr, d));
                }
                if !sh.tokens.is_finite() {
                    sh.tokens = CAP_BURST_BYTES;
                }
            }
            self.traffic.datagrams_dropped_by_codel += codel_drops;
            for (addr, d) in out {
                self.propagate(link, addr, d, now);
            }
        }
    }

    fn send_now(&mut self, addr: SocketAddr, d: &[u8]) {
        if self.sock.send_to(d, addr).is_ok() {
            self.traffic.datagrams_sent += 1;
            self.traffic.bytes_sent += d.len() as u64;
            self.row_bytes += d.len() as u64;
        }
    }

    // ---- R18 ----------------------------------------------------------------------------------------

    /// The controller's input: the largest queue delay members report on my datagrams to them.
    /// Reports are held at their last value for 5 s; with none left the core gets no signal (None),
    /// which holds the level and is never read as calm.
    fn feed_link(&mut self, now: u64) {
        // Each report is capped at the reporter's round-trip excess plus a margin (design 10.4).
        let mut clamped = false;
        let v = self
            .reports
            .iter()
            .filter(|(_, r)| now < r.1 + REPORT_HOLD_US)
            .map(|(&m, r)| {
                let q = r.0 as u64 * 1000;
                match self.rtt_excess(m, now) {
                    Some(x) if q > x + RTT_CLAMP_MARGIN_US => {
                        clamped = true;
                        x + RTT_CLAMP_MARGIN_US
                    }
                    _ => q,
                }
            })
            .max();
        if clamped {
            self.clamped_ticks += 1;
        }
        self.signal_us = v;
        self.signal_held = v.is_none() && !self.reports.is_empty();
        if let Some(v) = v {
            self.max_signal_us = self.max_signal_us.max(v);
        }
        if self.link_signal {
            self.core.set_link(v);
        }
    }

    /// The queueing on the round trip to member `m` now: the latest pong's excess over the best, or,
    /// while pongs are late, the age of the oldest unanswered ping over the best (a lower bound).
    fn rtt_excess(&self, m: u32, now: u64) -> Option<u64> {
        let fresh = self.clock.rtt_excess(m, now);
        let best = self.clock.best_rtt_us();
        let waiting = self.pings_out.iter().filter(|p| p.0 == m).map(|p| now.saturating_sub(p.1)).max().filter(|&a| a > SYNC_EVERY_US && best < u64::MAX).map(|a| a - best.min(a));
        match (fresh, waiting) {
            (None, None) => None,
            (f, w) => Some(f.unwrap_or(0).max(w.unwrap_or(0))),
        }
    }

    /// One hand pose per clock frame along a sweep across the pit; the core keeps only those due at
    /// its congestion level (and gives them their span).
    fn run_hand(&mut self, cf: u64) {
        let Some(obj) = self.hand else { return };
        if cf == self.hand_frame || cf < 120 {
            return;
        }
        self.hand_frame = cf;
        // A 4 s period: across and back in x, 1.5 sine waves in z, just above the balls.
        let u = (cf % 240) as f64 / 240.0;
        let x = -0.9 + 1.8 * (1.0 - (2.0 * u - 1.0).abs());
        let z = 0.6 * (std::f64::consts::TAU * 1.5 * u).sin();
        let target_mm = [(x * 1000.0).round() as i32, 200, (z * 1000.0).round() as i32];
        if self.core.author(Payload::Hand { obj, target_mm, active: true, span: 1 }, 0).is_ok() {
            self.hand_entries += 1;
            self.authored += 1;
        }
    }

    fn record_row(&mut self, now: u64) {
        let Some(t_ms) = self.session_ms() else { return };
        if !self.core.started() || now < self.row_start_us + SERIES_US {
            return;
        }
        let dt = (now - self.row_start_us).max(1) as f64;
        let true_q = self.shapers.values().map(|s| s.row_max_us).max().unwrap_or(0);
        self.max_true_queue_us = self.max_true_queue_us.max(true_q);
        for sh in self.shapers.values_mut() {
            sh.row_max_us = 0;
        }
        let row = LinkRow {
            t_ms,
            level: self.core.level,
            signal_ms: self.signal_us.map(|v| v as f64 / 1000.0),
            held: self.signal_held,
            true_queue_ms: true_q as f64 / 1000.0,
            kbit_s: if self.row_start_us == 0 { 0.0 } else { self.row_bytes as f64 * 8.0 / dt * 1000.0 },
            conf_lag: self.core.sim.now.saturating_sub(self.core.sim.conf),
            rx_q_ms: self.rx.iter().filter_map(|(&m, r)| r.q_us.map(|q| (m, q as f64 / 1000.0))).collect(),
        };
        self.series.push(row);
        self.row_start_us = now;
        self.row_bytes = 0;
    }

    // ---- the plan and the end ---------------------------------------------------------------------

    /// The background touches due by clock frame `cf`, authored right after the sim reached it.
    fn run_plan(&mut self, cf: u64) {
        while self.core.ready() && self.plan.first_key_value().is_some_and(|(f, _)| *f <= cf) {
            let (_, list) = self.plan.pop_first().unwrap();
            for p in list {
                if self.core.author(p, 0).is_ok() {
                    self.authored += 1;
                }
            }
        }
    }

    fn drain_events(&mut self) {
        for e in self.core.events() {
            match e {
                Event::Epoch { epoch, removed } => self.note(format!("membership decision: epoch {epoch}, removed {removed:?}")),
                Event::Paused { frame } => self.note(format!("paused at frame {frame} (speculation limit)")),
                Event::Resumed { frame } => self.note(format!("resumed at frame {frame}")),
                Event::CheckFailed { total } => self.note(format!("CHECK HASH MISMATCH (total {total})")),
                // Joiners only (state transfer, S5g); M1 has none, and the wire refuses its messages.
                Event::Transfer { from, frame, result } => self.note(format!("state transfer from member {from} at frame {frame}: {result:?}")),
                Event::Outcome { .. } | Event::Confirmed { .. } => {}
            }
        }
    }

    fn finish(&mut self) {
        let frames = self.session.frames;
        let now = self.local_us();
        if self.written.is_none() && self.core.sim.conf >= frames {
            match self.write_results() {
                Ok(s) => self.note(format!(
                    "DONE frames 0..{frames}: digest {} replay {} ({}), conf {}, members {:?}",
                    s.digest,
                    s.replay_digest,
                    if s.replay_equal { "EQUAL" } else { "DIFFERENT" },
                    s.conf,
                    s.members_at_end
                )),
                Err(e) => self.note(format!("could not write results: {e}")),
            }
            self.written = Some(now);
        }
        // Keep acking a while, so the others can confirm too.
        if self.written.is_some_and(|w| now >= w + LINGER_US) {
            self.finished.store(true, Ordering::SeqCst);
        }
    }

    pub fn summary(&self) -> Summary {
        let s = &self.core.sim;
        let frames = self.session.frames;
        let members: Vec<u32> = s.membership().members().iter().copied().collect();
        let scene = self.session.scene_kind().expect("validated");
        let replay = replay_sim_like(s, scene, &members, &final_log(s), frames).hashes;
        let (d, rd) = (digest(&s.hashes, frames), digest(&replay, frames));
        Summary {
            session: format!("{:016x}", self.session.session),
            member: self.me,
            frames,
            conf: s.conf,
            digest: format!("{d:016x}"),
            replay_digest: format!("{rd:016x}"),
            replay_equal: d == rd,
            members_at_end: members,
            evicted: s.members_evicted().keys().copied().collect(),
            epoch: s.membership().epoch(),
            rollbacks: s.stats.rollbacks,
            max_rollback_depth: s.stats.max_depth,
            checks_ok: s.stats.checks_ok,
            checks_bad: s.stats.checks_bad,
            entries_authored: self.authored,
            clock_offset_us: self.offset_us,
            best_rtt_us: self.clock.best_rtt_us(),
            clock_rate_ppm: self.clock.rate() * 1e6,
            traffic: Traffic { datagrams_dropped_rx_full: self.rx_full.load(Ordering::Relaxed), ..self.traffic.clone() },
            link: LinkSummary {
                signal_fed: self.link_signal,
                level_ticks: self.core.level_ms.to_vec(),
                max_level: self.core.level_ms.iter().rposition(|&t| t > 0).unwrap_or(0),
                max_signal_ms: self.max_signal_us as f64 / 1000.0,
                max_true_queue_ms: self.max_true_queue_us as f64 / 1000.0,
                hand_entries: self.hand_entries,
                clamped_ticks: self.clamped_ticks,
                rx: self.rx.iter().map(|(&m, r)| (m, (r.received, r.lost))).collect(),
            },
            events: self.log.clone(),
        }
    }

    fn write_results(&self) -> std::io::Result<Summary> {
        std::fs::create_dir_all(&self.out_dir)?;
        let s = &self.core.sim;
        let mut f = std::fs::File::create(self.out_dir.join(format!("hashes-{}.txt", self.me)))?;
        for fr in 0..self.session.frames {
            match s.hashes.get(&fr) {
                Some(h) => writeln!(f, "{fr} {h:016x}")?,
                // Never a stand-in value: two members both missing a frame must not compare equal.
                None => writeln!(f, "{fr} missing")?,
            }
        }
        let sum = self.summary();
        std::fs::write(self.out_dir.join(format!("summary-{}.json", self.me)), serde_json::to_string_pretty(&sum).unwrap() + "\n")?;
        let mut f = std::fs::File::create(self.out_dir.join(format!("link-{}.csv", self.me)))?;
        writeln!(f, "t_ms,level,signal_ms,held,true_queue_ms,kbit_s,conf_lag,rx_q_ms")?;
        for r in &self.series {
            let rx: Vec<String> = r.rx_q_ms.iter().map(|(m, q)| format!("{m}:{q:.1}")).collect();
            let sig = r.signal_ms.map_or(String::new(), |v| format!("{v:.1}"));
            writeln!(f, "{},{},{},{},{:.1},{:.1},{},{}", r.t_ms, r.level, sig, r.held as u8, r.true_queue_ms, r.kbit_s, r.conf_lag, rx.join(" "))?;
        }
        Ok(sum)
    }

    /// One pass of the node: network in, clock sync, faults, the core, the plan, network out.
    pub fn step(&mut self) {
        if let (Some(c), Some(ms)) = (self.crash_at_ms, self.session_ms()) {
            if ms >= c {
                eprintln!("[m{}] scripted drop-out at {ms} ms: exiting without a word", self.me);
                std::process::exit(3);
            }
        }
        let t0 = self.local_us();
        self.receive_all();
        let t1 = self.local_us();
        self.sync_clock();
        self.apply_faults();
        if self.epoch.is_some() {
            // Entries authored at a frame's start precede that tick's ack, as in the harness.
            let now = self.local_us();
            self.feed_link(now);
            if let Some(cf) = self.core.begin_tick(now, false) {
                self.run_plan(cf);
                self.run_hand(cf);
                if self.hold.tick(&mut self.core).is_some() {
                    self.authored += 1;
                }
            }
            self.core.end_tick(now);
            self.record_row(now);
        }
        let t2 = self.local_us();
        self.drain_events();
        self.flush_out();
        self.finish();
        let t3 = self.local_us();
        if t3 - t0 > 100_000 {
            self.note(format!("slow step: {} ms (receive {} ms, tick {} ms, send {} ms)", (t3 - t0) / 1000, (t1 - t0) / 1000, (t2 - t1) / 1000, (t3 - t2) / 1000));
        }
        let local = self.local_us();
        if self.verbose && self.epoch.is_some() && local >= self.last_status + 2_000_000 {
            self.last_status = local;
            let (s, t) = (&self.core.sim, &self.traffic);
            let line = format!(
                "status: frame {} now {} conf {} members {:?} rollbacks {} | sent {} faulted {} capped {} recv {} rejected {:?} bad entry sigs {} | R18 level {} signal {:?} ms{} | clock offset {} us, rate {:.0} ppm, rtt excess ms {:?}",
                s.frame, s.now, s.conf, s.membership().members(), s.stats.rollbacks, t.datagrams_sent, t.datagrams_dropped_by_fault, t.datagrams_dropped_by_cap, t.datagrams_received, t.rejected, s.stats.bad_sigs,
                self.core.level, self.signal_us.map(|v| v / 1000), if self.link_signal { "" } else { " (not fed)" },
                self.offset_us, self.clock.rate() * 1e6,
                s.membership().members().iter().filter(|&&m| m != self.me).map(|&m| (m, self.rtt_excess(m, local).map(|x| x / 1000))).collect::<Vec<_>>()
            );
            self.note(line);
        }
    }
}

impl DrivenPeer for Node {
    fn scenario_label(&self) -> String {
        format!("{}: live UDP session {:016x}, member {} of {:?}", self.session.name, self.session.session, self.me, self.session.ids())
    }
    fn backend_name(&self) -> &'static str {
        "pe-app (UDP)"
    }
    fn me(&self) -> MemberId {
        self.me
    }
    fn world(&self) -> u32 {
        WORLD
    }
    fn members(&self) -> Vec<MemberId> {
        self.session.ids()
    }
    fn scene_label(&self) -> String {
        format!("{} {}", self.session.scene, self.session.size)
    }
    fn dt_ms(&self) -> f64 {
        self.core.dt_us as f64 / 1000.0
    }
    fn time_ms(&self) -> u64 {
        self.session_ms().unwrap_or(0)
    }
    fn frame(&self) -> u64 {
        self.core.sim.frame
    }
    fn conf(&self) -> u64 {
        self.core.sim.conf
    }
    fn object_count(&self) -> u32 {
        self.core.sim.obj_slots()
    }
    fn state(&self, offset: u32, limit: u32) -> WorldState {
        sim_view::state(&self.core.sim, offset, limit)
    }
    fn object(&self, object: u32) -> ToolResult<ObjectState> {
        sim_view::object(&self.core.sim, object)
    }
    fn propose(&mut self, i: Interaction) -> ToolResult<Authored> {
        if self.epoch.is_none() {
            return Err(ToolError::new(ErrorCode::NotReady, "the session has not started yet (waiting for every member)"));
        }
        sim_view::check_interaction(&self.core.sim, &i, self.me)?;
        // A hand target is held and re-sent at every pose slot; its first pose goes out at the next
        // slot (at most one stride), stepping the node meanwhile.
        match i {
            Interaction::Hand { object, target_mm, active: true } => {
                if self.hand == Some(object) {
                    return Err(ToolError::arg("object", "this hand is driven by --hand-sweep"));
                }
                self.hold.set(object, target_mm);
                let end = self.local_us() + sim_view::HOLD_WAIT_MS * 1000;
                loop {
                    if self.hold.tick(&mut self.core).is_some() {
                        self.authored += 1;
                    }
                    if let Some(e) = self.hold.first() {
                        let a = Authored { entry: e.id(), stamp: e.stamp, seq: e.seq };
                        self.flush_out();
                        return Ok(a);
                    }
                    if self.local_us() >= end {
                        self.hold.release();
                        return Err(ToolError::new(ErrorCode::NotReady, "no pose slot for the hand within 1 s; step and retry"));
                    }
                    self.step();
                }
            }
            Interaction::Hand { active: false, .. } => self.hold.release(),
            Interaction::Touch { .. } => {}
        }
        let payload = match i {
            Interaction::Touch { object, impulse, grab } => Payload::Touch { obj: object, impulse, grab },
            Interaction::Hand { object, target_mm, active } => Payload::Hand { obj: object, target_mm, active, span: 1 },
        };
        let e = self.core.author(payload, 0).map_err(|e| match e {
            AuthorError::NotReady => ToolError::new(ErrorCode::NotReady, "the peer is behind its clock or paused; step and retry"),
            AuthorError::NotOwner => ToolError::new(ErrorCode::NotOwner, "not your hand"),
            AuthorError::Stride => ToolError::new(ErrorCode::NotReady, "a pose for this hand was already sent this frame; step and retry"),
        })?;
        self.authored += 1;
        self.flush_out();
        Ok(Authored { entry: e.id(), stamp: e.stamp, seq: e.seq })
    }
    fn status(&self, entry: EntryId) -> Option<EntryStatus> {
        sim_view::status(&self.core.sim, entry, self.me)
    }
    /// Real time cannot be advanced: this waits `ms` milliseconds while the node keeps running.
    fn advance_ms(&mut self, ms: u64) -> ToolResult<()> {
        let end = self.local_us() + ms * 1000;
        while self.local_us() < end {
            self.step();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    }
    fn counters(&self) -> Counters {
        sim_view::counters(&self.core.sim)
    }
    fn paused(&self) -> bool {
        self.core.sim.frame < self.core.sim.now
    }
    fn events_since(&self, n: usize) -> (Vec<String>, usize) {
        (self.log.get(n..).unwrap_or_default().to_vec(), self.log.len())
    }
    fn hashes(&self, from: u64, to: u64) -> Hashes {
        sim_view::hashes(&self.core.sim, from, to)
    }
    fn member_info(&self) -> Members {
        sim_view::members(&self.core.sim, self.me, &self.session.ids())
    }
    fn sync(&self) -> SyncStatus {
        sim_view::sync(&self.core.sim)
    }
    fn scenario(&mut self) -> Option<&mut dyn ScenarioControl> {
        None
    }
    fn tick(&mut self) {
        self.step();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A datagram sent at `send_us` (sender clock) arriving at `recv_us` (receiver clock).
    fn feed(r: &mut RxLink, seq: u16, send_us: u64, recv_us: u64) {
        r.sample(Stamp { ts: send_us as u32, seq, qrep: QREP_NONE }, recv_us);
    }

    #[test]
    fn queue_delay_over_the_base() {
        let mut r = RxLink::default();
        // An arbitrary clock offset (3 s) and a steady 25 ms path: no queue.
        for i in 0..50u64 {
            feed(&mut r, i as u16, i * 16_667, i * 16_667 + 3_025_000);
        }
        assert_eq!(r.q_us, Some(0));
        // A queue builds to 60 ms and stands: the current filter's minimum follows it.
        for i in 50..100u64 {
            feed(&mut r, i as u16, i * 16_667, i * 16_667 + 3_085_000);
        }
        assert_eq!(r.q_us, Some(60_000));
        // One early (unqueued) datagram among late ones: within 4 samples or 100 ms the minimum drops.
        feed(&mut r, 100, 100 * 16_667, 100 * 16_667 + 3_025_000);
        assert_eq!(r.q_us, Some(0));
        assert_eq!((r.received, r.lost), (101, 0));
    }

    #[test]
    fn timestamps_wrap_and_buckets_expire_by_time() {
        let mut r = RxLink::default();
        // The sender's u32 clock wraps (every 71.6 minutes) between these two datagrams.
        let s0 = u32::MAX as u64 - 5_000;
        feed(&mut r, 1, s0, 1_000_000);
        feed(&mut r, 2, s0 + 10_000, 1_010_000);
        assert_eq!(r.q_us, Some(0));
        // 11 minutes of silence (longer than the 2-minute window), then the path is 30 ms longer: the
        // old base has expired, so the new path is the base, not a 30 ms queue (review AR-5).
        let t = 1_010_000 + 11 * 60_000_000;
        feed(&mut r, 3, s0 + 10_000 + 11 * 60_000_000, t + 30_000);
        assert_eq!(r.q_us, Some(0));
        assert_eq!(r.base.len(), 1);
    }

    #[test]
    fn loss_from_sequence_gaps() {
        let mut r = RxLink::default();
        for (i, seq) in [1u16, 2, 5, 4, 6, 65535, 0].into_iter().enumerate() {
            feed(&mut r, seq, i as u64 * 1000, i as u64 * 1000 + 500);
        }
        // 2 → 5 counts 3 and 4 as lost (the late 4 is ignored, not undone); 65535 and 0 are backward
        // jumps from 6 (reordered or duplicated), ignored.
        assert_eq!(r.received, 7);
        assert_eq!(r.lost, 2);
    }
}

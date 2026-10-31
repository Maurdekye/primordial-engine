//! P2-3: forked from spikes/session-sim at b01ebdf to run on crates/pe-session.
//!
//! In-process session simulator: N peers, M worlds, a virtual clock and virtual links with delay,
//! jitter, loss and scripted faults. Every message is encoded to bytes, so byte counts are real.
//! The same peer logic drives the UDP spike (spikes/p2p) with a real clock and sockets.
//!
//! R18 here (coordinator ruling 2026-10-31): pe-session's `SessionCore` defaults to R18 on (`adapt`,
//! `ack_on_frames`), but this test IO sends one datagram per message and turns the controller off
//! unless a run opts in (`Net::batching`, `Replica::adapt`), so the scenarios keep the transport
//! they were written for. The R18 runs (`capped`, the capped faults, the matrix) opt in.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use pe_session::session::*;
use pe_session::splitmix64;

pub type WorldId = u32;

#[derive(Clone, Copy, Debug)]
pub struct Link {
    pub delay_us: u64,
    pub jitter_us: u64,
    pub loss_pct: u32,
}

/// Wire overhead per datagram (IP/UDP/AEAD/session), as in the ball-pit wire estimate.
pub const DATAGRAM_OVERHEAD: u64 = 52;
/// Largest datagram payload a batch may fill (review R3-9).
pub const MTU: usize = 1200;
/// A member answers a fetch only while its uplink to the requester holds less than this; the
/// requester asks again later, with backoff (review R3-9).
pub const REPAIR_MAX_BACKLOG_US: u64 = 100_000;

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
    /// R18 (viz-4): merge everything one member sends one peer in one tick into one datagram.
    pub batching: bool,
    outbox: BTreeMap<(WorldId, MemberId, MemberId), Vec<Msg>>,
    /// Every send attempt, when `tracing` is on.
    pub tracing: bool,
    pub trace: Vec<SendRec>,
}

pub fn kind(m: &Msg) -> &'static str {
    match m {
        Msg::Entry(e) => match e.payload {
            Payload::Touch { .. } => "entry/touch",
            Payload::Hand { .. } => "entry/hand",
            Payload::Ack { .. } => "ack",
            Payload::Vote { .. } | Payload::Propose { .. } | Payload::Prevote { .. } | Payload::Precommit { .. } => "vote",
            Payload::Kick { .. } | Payload::SetRole { .. } | Payload::DefineRole { .. } => "admin",
            Payload::Attest { .. } => "attest",
            Payload::Provenance { .. } => "provenance",
            _ => "entry/transfer",
        },
        Msg::Fetch { .. } | Msg::FetchValue { .. } | Msg::FetchEntry { .. } => "fetch",
        Msg::Join { .. } => "join",
        Msg::FetchState | Msg::State(_) => "state",
        Msg::Batch(_) => "batch",
    }
}

impl Net {
    pub fn new(default_link: Link, seed: u64) -> Self {
        Net {
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
            // The test IO sends one datagram per message unless a run opts into R18's batching (the
            // scenario suite reproduces protocol situations under the spike's transport; R18 runs,
            // the capped faults and the matrix, opt in).
            batching: false,
            outbox: BTreeMap::new(),
            tracing: false,
            trace: Vec::new(),
        }
    }

    fn rand(&mut self) -> u64 {
        self.rng = splitmix64(self.rng);
        self.rng
    }

    pub fn send(&mut self, world: WorldId, from: MemberId, to: MemberId, m: &Msg) {
        let bytes = encode_msg(m);
        if self.now_us >= self.count_from_us && self.now_us < self.count_to_us {
            let c = self.sent.entry((world, kind(m))).or_default();
            c.0 += 1;
            c.1 += bytes.len() as u64;
        }
        let link = self.links.get(&(from, to)).copied().unwrap_or(self.default_link);
        let mut extra = 0;
        if let Some(f) = self.filter.as_mut() {
            match f(self.now_us, world, from, to, m) {
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
            *self.wire_from.entry(from).or_default() += bytes.len() as u64 + DATAGRAM_OVERHEAD;
        }
        // Bandwidth cap: the datagram leaves the bottleneck queue at `depart` (or is tail-dropped).
        let depart = match self.shape(from, to, bytes.len() as u64) {
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

    /// Departure time through the link's bandwidth cap for a payload of `payload` bytes (now, if
    /// the link is uncapped), or None if its queue is full.
    pub fn shape(&mut self, from: MemberId, to: MemberId, payload: u64) -> Option<u64> {
        let Some(cap) = self.caps.get(&(from, to)).copied().or(self.default_cap) else { return Some(self.now_us) };
        self.shapers.entry((from, to)).or_default().offer(&cap, self.now_us, payload + DATAGRAM_OVERHEAD)
    }

    /// Current queue backlog of a capped link, in microseconds (0 if uncapped).
    pub fn backlog_us(&self, from: MemberId, to: MemberId) -> u64 {
        self.shapers.get(&(from, to)).map_or(0, |s| s.backlog_us(self.now_us))
    }

    /// Send now, or queue for this tick's batch to that peer.
    pub fn post(&mut self, world: WorldId, from: MemberId, to: MemberId, m: &Msg) {
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
        for ((w, from, to), v) in std::mem::take(&mut self.outbox) {
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
    }

    fn send_batch(&mut self, w: WorldId, from: MemberId, to: MemberId, mut v: Vec<Msg>) {
        let m = if v.len() == 1 { v.pop().unwrap() } else { Msg::Batch(v) };
        self.send(w, from, to, &m);
    }

    fn record(&mut self, from: MemberId, to: MemberId, payload: u64, fate: u8, arrive_us: u64) {
        if self.tracing {
            self.trace.push(SendRec { at_us: self.now_us, from, to, wire: payload + DATAGRAM_OVERHEAD, fate, arrive_us });
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
    SetRole { member: MemberId, role: RoleId },
    DefineRole { role: RoleId, perms: u64 },
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
    /// The member's session core (P2-3 S6): the replica, its timers, R18 and its outbox. The
    /// replica derefs to it, so `r.sim`, `r.adapt`, `r.level_ms`... read the core's.
    pub core: SessionCore,
    pub scene: SceneKind,
    pub plan: BTreeMap<u64, Vec<Action>>,
    /// Remote state-carrying entries awaiting the acceptance check: (entry, author, obj).
    pub pending_accept: Vec<(EntryId, MemberId, u32)>,
}

impl std::ops::Deref for Replica {
    type Target = SessionCore;
    fn deref(&self) -> &SessionCore {
        &self.core
    }
}

impl std::ops::DerefMut for Replica {
    fn deref_mut(&mut self) -> &mut SessionCore {
        &mut self.core
    }
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

pub struct Sim {
    pub net: Net,
    pub peers: Vec<Peer>,
    pub acceptance: Vec<Acceptance>,
    pub end_us: u64,
    /// Process peers in reverse order (order-independence check).
    pub reverse_order: bool,
    /// R14: a carrier's deadline, in frames of the destination after its present (review R3-6).
    pub carry_deadline_frames: u64,
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
    pub fn join(&mut self, world: WorldId, members: &[MemberId], scene: SceneKind, mut cfg: SimConfig, epoch_us: u64, plan: BTreeMap<u64, Vec<Action>>) {
        // P2-3 S4 comparison only (scripts/compare_claim_rules.sh): run with the spike's claim rule.
        if std::env::var("PE_CLAIM_RULE").as_deref() == Ok("spike") {
            cfg.claim_rule = ClaimRule::Spike;
        }
        // P2-3 S5 evidence only: compaction at every ring trim (or never) must leave every log unchanged.
        match std::env::var("PE_COMPACT").as_deref() {
            Ok("every") => cfg.compact = CompactPolicy::EveryTrim,
            Ok("never") => cfg.compact = CompactPolicy::Never,
            _ => {}
        }
        let mut core = SessionCore::new(WorldSim::new(world, self.id, members, scene, cfg, KeyRing::test(self.id)));
        core.start(epoch_us);
        // R18's controller is on by default in pe-session; the test IO opts in per run (`adapt`).
        core.adapt = false;
        core.set_offset(self.skew_us);
        self.worlds.insert(world, Replica { core, scene, plan, pending_accept: Vec::new() });
    }
}

/// The IO's half of the core's outbox: every message to each of its recipients, in order. Review
/// R3-9: a repair yields to a long uplink queue (the requester asks again). A reply (consecutive
/// repairs to one member) is sent or dropped whole, decided at its first message, as the spike
/// decides per fetch request: cutting replies short once the queue passes the limit starved repair
/// (P2-3 stall diagnosis: at 36 kbit/s on one link every member paused about 160 frames).
fn drain(net: &mut Net, w: WorldId, from: MemberId, core: &mut SessionCore) {
    let mut reply: Option<(MemberId, bool)> = None;
    for o in core.outbox() {
        for to in o.to {
            if o.class == Class::Repair {
                let send = match reply {
                    Some((t, send)) if t == to => send,
                    _ => net.backlog_us(from, to) <= REPAIR_MAX_BACKLOG_US,
                };
                reply = Some((to, send));
                if !send {
                    continue;
                }
            } else {
                reply = None;
            }
            net.post(w, from, to, &o.msg);
        }
    }
}

#[allow(dead_code)]
fn broadcast(net: &mut Net, w: WorldId, from: MemberId, members: &[MemberId], m: &Msg) {
    for &to in members {
        if to != from {
            net.post(w, from, to, m);
        }
    }
}

fn flatten(m: Msg, out: &mut Vec<Msg>) {
    match m {
        Msg::Batch(v) => v.into_iter().for_each(|x| flatten(x, out)),
        x => out.push(x),
    }
}

pub use pe_session::peer::{AuthorError, Class, Out, SessionCore, ACK_FRAMES, HAND_STRIDE};

impl Sim {
    pub fn new(net: Net, mut peers: Vec<Peer>, end_us: u64) -> Self {
        // R14 trust config (D46): each world knows the member lists of the worlds on its own route
        // list, and of no other (reviews R3-4 to R3-6).
        let mut worlds: BTreeMap<WorldId, Vec<MemberId>> = BTreeMap::new();
        for p in &peers {
            for (w, r) in &p.worlds {
                worlds.entry(*w).or_insert_with(|| r.sim.vcm.members.iter().copied().collect());
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
        let mut epochs = EpochBook { keys: PublicKeys::test(), ..EpochBook::default() };
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
        Sim { net, peers, acceptance: Vec::new(), end_us, reverse_order: false, carry_deadline_frames: 600 }
    }

    fn peer_index(&self, id: MemberId) -> usize {
        self.peers.iter().position(|p| p.id == id).expect("peer")
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
            let mut v = Vec::new();
            flatten(m, &mut v);
            due.extend(v.into_iter().map(|x| (to, w, from, x)));
        }
        for (to, w, from, m) in due {
            let pi = self.peer_index(to);
            let peer = &mut self.peers[pi];
            if peer.crashed {
                continue;
            }
            let Some(r) = peer.worlds.get_mut(&w) else { continue };
            if let Msg::Entry(e) = &m {
                if let Some((obj, _)) = e.carried {
                    if e.member != to {
                        r.pending_accept.push((e.id(), e.member, obj));
                    }
                }
            }
            r.core.receive(from, m, now);
            drain(&mut self.net, w, to, &mut r.core);
        }
        // 2. peers tick
        let order: Vec<usize> = if self.reverse_order { (0..self.peers.len()).rev().collect() } else { (0..self.peers.len()).collect() };
        for pi in order {
            self.tick_peer(pi, now);
        }
        // 3. acceptance checks whose receiver is caught up and whose author has the frame
        self.resolve_acceptance();
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
            if r.adapt {
                // R18's signal: this member's own uplink queue delay (the spike reads the bottleneck
                // directly; production estimates it from the one-way delay gradient of acks, as
                // WebRTC's GCC does).
                let b = r.sim.audience().iter().filter(|&&m| m != pid).map(|&m| self.net.backlog_us(pid, m)).max().unwrap_or(0);
                r.core.set_link(b);
            }
            r.core.ack_on_frames = self.net.batching;
            let clock_frame = r.core.clock_frame(now);
            // A background world catches up before its member authors anything that carries state
            // (design 22.3 rule 2): a due action forces a burst.
            let due = clock_frame.is_some_and(|f| r.plan.get(&f).is_some_and(|v| v.iter().any(|a| !matches!(a, Action::Hand { .. }))));
            if let Some(clock_frame) = r.core.begin_tick(now, due) {
                // Planned actions for this frame (authored at the start of the frame). Hand poses
                // stream even while the sim is behind (stamped at the present, R18), at the
                // congestion level's stride; anything else needs a caught-up sim and is dropped
                // otherwise (as in 3d0f98a).
                for a in r.plan.remove(&clock_frame).unwrap_or_default() {
                    let p = match a {
                        Action::Touch { obj, impulse, grab } => Payload::Touch { obj, impulse, grab },
                        Action::Hand { obj, target_mm, active, .. } => Payload::Hand { obj, target_mm, active, span: 1 },
                        Action::Portal { open } => Payload::Portal { open },
                        Action::Backdated { obj, impulse, back } => {
                            if r.core.ready() {
                                let e = r.core.sim.author_backdated(Payload::Touch { obj, impulse, grab: false }, back);
                                r.core.sent(e);
                            }
                            continue;
                        }
                        Action::Kick { target } => Payload::Kick { epoch: 0, target },
                        Action::SetRole { member, role } => Payload::SetRole { epoch: 0, member, role },
                        Action::DefineRole { role, perms } => Payload::DefineRole { epoch: 0, role, perms },
                        Action::Carry { obj, dest, .. } => {
                            if !r.core.ready() {
                                continue;
                            }
                            let deadline = presents.get(&dest).map_or(u64::MAX, |p| p + deadline_frames);
                            let hop = r.sim.pending_hop(obj);
                            let Ok(e) = r.core.author(Payload::Depart { obj, dest, deadline, hop }, 0) else { continue };
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
                            continue;
                        }
                    };
                    let ahead = if let Action::Hand { ahead, .. } = a { ahead } else { 0 };
                    match r.core.author(p.clone(), ahead) {
                        // Scenario fixture (handowner): an adversarial member moves another's hand,
                        // past the core's own refusal; every replica refuses it (8.4).
                        Err(AuthorError::NotOwner) => {
                            let e = r.core.sim.author_ahead(p, ahead);
                            r.core.sent(e);
                        }
                        Ok(_) | Err(AuthorError::NotReady | AuthorError::Stride) => {}
                    }
                }
            }
            r.core.end_tick(now);
            drain(&mut self.net, w, pid, &mut r.core);
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
                            let Some(cert) = src.cert(fact_departed(j.src, j.depart, &rec, j.dst, deadline, chain_hash(&chain))) else { continue };
                            let dst = self.peers[pi].worlds.get_mut(&j.dst).unwrap();
                            if !dst.sim.caught_up() || dst.sim.frame < dst.sim.now {
                                continue;
                            }
                            j.record = Some(rec);
                            let e = dst.sim.author(Payload::Arrive { transfer: j.transfer, record: rec, depart: j.depart, deadline, cert, chain }, None);
                            j.log.push(format!(
                                "t={}ms depart confirmed in world {} (frame {f}); arrive authored in world {} at frame {}",
                                now / 1000, j.src, j.dst, e.stamp
                            ));
                            j.arrive = Some(e.id());
                            j.phase = JobPhase::WaitArrive;
                            let members: Vec<MemberId> = dst.sim.vcm.members.iter().copied().collect();
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
                    let accepted = matches!(dst.t.arrivals.get(&j.transfer), Some(Arrival::Accepted { .. }));
                    // Relay only with B's certificate over the verdict (review NEW-3).
                    let Some(cert) = dst.cert(fact_verdict(j.dst, j.transfer, accepted)) else { continue };
                    let src = self.peers[pi].worlds.get_mut(&j.src).unwrap();
                    // State-free: stamped at the present even if this replica lags (design 22.3).
                    let sim_at = src.sim.frame;
                    let e = src.sim.author(Payload::Verdict { transfer: j.transfer, accepted, cert }, None);
                    j.log.push(format!(
                        "t={}ms arrival confirmed in world {} (frame {f}, accepted={accepted}); verdict authored in world {} at frame {} (its sim there at frame {sim_at})",
                        now / 1000, j.dst, j.src, e.stamp
                    ));
                    j.accepted = Some(accepted);
                    j.verdict = Some(e.id());
                    j.phase = JobPhase::WaitVerdict;
                    let members: Vec<MemberId> = src.sim.vcm.members.iter().copied().collect();
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
                    let fact = fact_committed(j.src, j.transfer, rec.global, j.dst, rec.via);
                    // P25-5: signed by the epoch active at the commit frame, with A's certificates
                    // up to it, so any world can check the hop from A's genesis.
                    let Some(Seen::Left { frame, .. }) = src.t.seen.get(&rec.global).filter(|s| matches!(s, Seen::Left { transfer, .. } if *transfer == j.transfer)).cloned() else { continue };
                    let Some((epoch, cert)) = src.cert_at(fact, frame) else { continue };
                    let Some(certs) = src.epoch_certs_upto(epoch) else { continue };
                    let hop = Hop { src: j.src, dest: j.dst, transfer: j.transfer, via: rec.via, epoch, frame, certs, cert };
                    let dst = self.peers[pi].worlds.get_mut(&j.dst).unwrap();
                    let e = dst.sim.author(Payload::Provenance { hop }, None);
                    j.log.push(format!("t={}ms commit certified in world {}; hop relayed into world {} at frame {}", now / 1000, j.src, j.dst, e.stamp));
                    j.phase = JobPhase::Done;
                    let members: Vec<MemberId> = dst.sim.vcm.members.iter().copied().collect();
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

    pub fn replica_mut(&mut self, peer: MemberId, w: WorldId) -> &mut Replica {
        let i = self.peer_index(peer);
        self.peers[i].worlds.get_mut(&w).expect("world")
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

/// An entry id no entry has, for forged-reference fixtures (S3b: ids are 256-bit).
pub fn fake_id(x: u64) -> EntryId {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&x.to_le_bytes());
    b[8] = 0xfa;
    EntryId(b)
}

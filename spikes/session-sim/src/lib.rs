//! In-process session simulator: N peers, M worlds, a virtual clock and virtual links with delay,
//! jitter, loss and scripted faults. Every message is encoded to bytes, so byte counts are real.
//! The same peer logic drives the UDP spike (spikes/p2p) with a real clock and sockets.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use pe_sim_core::session::*;
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

/// Bandwidth cap for one directed link: a token bucket feeding a FIFO queue. Tokens refill at
/// `rate_bps` bits/s up to `burst_bytes`; a datagram leaves the queue when the bucket holds its
/// wire size. A datagram that would wait in the queue longer than `max_queue_us` is dropped
/// (tail drop), as a router with a full buffer would.
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
    /// Every send attempt, when `tracing` is on.
    pub tracing: bool,
    pub trace: Vec<SendRec>,
}

pub fn kind(m: &Msg) -> &'static str {
    match m {
        Msg::Entry(e) => match e.payload {
            Payload::Touch { .. } => "entry/touch",
            Payload::Hand { .. } => "entry/hand",
            _ => "entry/transfer",
        },
        Msg::Ack(_) => "ack",
        Msg::Fetch { .. } => "fetch",
        Msg::Hash { .. } => "hash",
        Msg::Vote { .. } => "vote",
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobPhase {
    WaitDepart,
    WaitArrive,
    WaitVerdict,
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
    last_frame_seen: u64,
    /// Remote state-carrying entries awaiting the acceptance check: (entry, author, obj).
    pub pending_accept: Vec<(EntryId, MemberId, u32)>,
}

pub struct Peer {
    pub id: MemberId,
    pub skew_us: i64,
    pub worlds: BTreeMap<WorldId, Replica>,
    pub jobs: Vec<Job>,
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
}

impl Peer {
    pub fn new(id: MemberId, skew_us: i64) -> Self {
        Peer { id, skew_us, worlds: BTreeMap::new(), jobs: Vec::new() }
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
                ack_every_us: 2 * dt_us,
                last_frame_seen: u64::MAX,
                pending_accept: Vec::new(),
            },
        );
    }
}

fn broadcast(net: &mut Net, w: WorldId, from: MemberId, members: &[MemberId], m: &Msg) {
    for &to in members {
        if to != from {
            net.send(w, from, to, m);
        }
    }
}

impl Sim {
    pub fn new(net: Net, peers: Vec<Peer>, end_us: u64) -> Self {
        Sim { net, peers, acceptance: Vec::new(), end_us, reverse_order: false }
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
        // 1. deliveries
        for (to, w, from, m) in self.net.due() {
            let pi = self.peer_index(to);
            let peer = &mut self.peers[pi];
            let Some(r) = peer.worlds.get_mut(&w) else { continue };
            let members: Vec<MemberId> = r.sim.members.iter().copied().collect();
            match m {
                Msg::Entry(e) => {
                    let carried = e.carried.map(|(o, _)| o);
                    let author = e.member;
                    let id = e.id();
                    let votes = r.sim.insert(e, r.sim.now);
                    if let Some(obj) = carried {
                        if author != to {
                            r.pending_accept.push((id, author, obj));
                        }
                    }
                    for v in votes {
                        broadcast(&mut self.net, w, to, &members, &v);
                    }
                }
                Msg::Ack(a) => {
                    let reqs = r.sim.on_ack(a);
                    for (dest, f) in r.sim.take_fetches(reqs, now / 1000) {
                        if dest != to {
                            self.net.send(w, to, dest, &f);
                        }
                    }
                }
                Msg::Fetch { author, from: a, to: b } => {
                    for e in r.sim.on_fetch(author, a, b) {
                        self.net.send(w, to, from, &e);
                    }
                }
                Msg::Hash { frame, hash } => r.sim.on_hash(from, frame, hash),
                Msg::Vote { target, seq } => {
                    r.sim.on_vote(from, target, seq);
                }
            }
        }
        // 2. peers tick
        let order: Vec<usize> = if self.reverse_order { (0..self.peers.len()).rev().collect() } else { (0..self.peers.len()).collect() };
        for pi in order {
            self.tick_peer(pi, now);
        }
        // 3. acceptance checks whose receiver is caught up and whose author has the frame
        self.resolve_acceptance();
    }

    fn tick_peer(&mut self, pi: usize, now: u64) {
        let pid = self.peers[pi].id;
        let skew = self.peers[pi].skew_us;
        let wids: Vec<WorldId> = self.peers[pi].worlds.keys().copied().collect();
        let mut new_jobs = Vec::new();
        for w in wids {
            let r = self.peers[pi].worlds.get_mut(&w).unwrap();
            let clock = (now as i64 + skew).max(0) as u64;
            if clock < r.epoch_us {
                continue;
            }
            let clock_frame = (clock - r.epoch_us) / r.dt_us;
            r.sim.now = clock_frame;
            let members: Vec<MemberId> = r.sim.members.iter().copied().collect();
            // Simulate once per new clock frame (coalesces all late arrivals of a frame into one
            // rollback: option (c)); background worlds advance in periodic bursts.
            let throttled = r.throttle_us.is_some_and(|p| now >= r.throttle_from_us && now < r.last_burst_us + p);
            if !throttled && (clock_frame != r.last_frame_seen || !r.sim.caught_up()) {
                r.last_frame_seen = clock_frame;
                if r.throttle_us.is_some() && now >= r.throttle_from_us {
                    r.last_burst_us = now;
                }
                r.sim.advance(clock_frame);
                // Planned actions for this frame (authored at the start of the frame).
                if r.sim.caught_up() && r.sim.frame == clock_frame {
                    if let Some(actions) = r.plan.remove(&clock_frame) {
                        for a in actions {
                            let e = match a {
                                Action::Touch { obj, impulse, grab } => {
                                    r.sim.author(Payload::Touch { obj, impulse, grab }, Some(obj))
                                }
                                Action::Hand { obj, target_mm, active, ahead } => {
                                    r.sim.author_ahead(Payload::Hand { obj, target_mm, active }, ahead)
                                }
                                Action::Portal { open } => r.sim.author(Payload::Portal { open }, None),
                                Action::Backdated { obj, impulse, back } => {
                                    r.sim.author_backdated(Payload::Touch { obj, impulse, grab: false }, back)
                                }
                                Action::Carry { transfer, obj, dest } => {
                                    let e = r.sim.author(Payload::Depart { transfer, obj, deadline: u64::MAX }, None);
                                    let id = e.id();
                                    new_jobs.push(Job {
                                        transfer,
                                        src: w,
                                        dst: dest,
                                        depart: id,
                                        arrive: None,
                                        verdict: None,
                                        phase: JobPhase::WaitDepart,
                                        accepted: None,
                                        log: vec![format!("t={}ms depart authored in world {w} at frame {}", now / 1000, e.stamp)],
                                    });
                                    broadcast(&mut self.net, w, pid, &members, &Msg::Entry(e));
                                    continue;
                                }
                            };
                            broadcast(&mut self.net, w, pid, &members, &Msg::Entry(e));
                        }
                    }
                }
            }
            let r = self.peers[pi].worlds.get_mut(&w).unwrap();
            if now >= r.next_ack_us {
                r.next_ack_us = now + r.ack_every_us;
                let a = r.sim.make_ack();
                let _ = r.sim.on_ack(a.clone());
                broadcast(&mut self.net, w, pid, &members, &Msg::Ack(a));
            }
        }
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
                        Some(Escrow { status: EscrowStatus::Pending, record: Some(rec), .. }) => {
                            let dst = self.peers[pi].worlds.get_mut(&j.dst).unwrap();
                            if !dst.sim.caught_up() || dst.sim.frame < dst.sim.now {
                                continue;
                            }
                            let e = dst.sim.author(Payload::Arrive { transfer: j.transfer, record: rec }, None);
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
                    let accepted = matches!(dst.t.arrivals.get(&j.transfer), Some(Arrival::Accepted { .. }));
                    let src = self.peers[pi].worlds.get_mut(&j.src).unwrap();
                    let e = src.sim.author(Payload::Verdict { transfer: j.transfer, accepted }, None);
                    j.log.push(format!(
                        "t={}ms arrival confirmed in world {} (frame {f}, accepted={accepted}); verdict authored in world {} at frame {}",
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
                        j.phase = JobPhase::Done;
                    }
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

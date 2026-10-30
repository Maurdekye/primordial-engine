//! The node: one peer process's IO around its `PeerCore`. It owns the UDP socket, the clock and its
//! sync with the leader, signing, batching, scripted faults, the background plan, and the results
//! written at the end. It runs on pe-mcp's engine thread (`EngineHandle::spawn_ticking`) and
//! implements pe-mcp's `DrivenPeer`, so an agent can drive it.
//!
//! Session start: the leader is the lowest member id. Every other member pings the leader, estimates
//! its clock offset from the lowest-RTT sample (as spikes/p2p does), and reports Ready. Once every
//! member is Ready, the leader picks an epoch 1 s ahead on its own clock and sends Start until each
//! member acks. Session time = local time + offset (0 for the leader).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use ed25519_dalek::{SigningKey, VerifyingKey};
use pe_mcp::backend::{Authored, Counters, DrivenPeer, EntryStatus, Interaction, ScenarioControl};
use pe_mcp::model::*;
use pe_mcp::sim_view;
use pe_sim_core::session::{digest, final_log, replay_sim_like, EntryId, Msg, Payload};
use pe_sim_core::splitmix64;
use serde::{Deserialize, Serialize};

use crate::config::SessionFile;
use crate::core::{AuthorError, Class, Event, PeerCore};
use crate::wire::{self, Item};

pub const WORLD: u32 = 1;
const PINGS: usize = 12;
const LINGER_US: u64 = 3_000_000;

/// A scripted change to this node's outgoing links, at a session time.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FaultStep {
    /// Session milliseconds since the epoch.
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
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Link {
    loss_pct: u32,
    delay_us: u64,
    jitter_us: u64,
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
    pub out_dir: PathBuf,
    /// Set once the results are written and the linger time has passed.
    pub finished: Arc<AtomicBool>,
    pub verbose: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Traffic {
    pub datagrams_sent: u64,
    pub datagrams_dropped_by_fault: u64,
    pub datagrams_received: u64,
    pub bytes_sent: u64,
    pub rejected: BTreeMap<String, u64>,
    pub entries_without_signature: u64,
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
    pub traffic: Traffic,
    pub events: Vec<String>,
}

pub struct Node {
    core: PeerCore,
    me: u32,
    session: SessionFile,
    key: SigningKey,
    keys: BTreeMap<u32, VerifyingKey>,
    sock: UdpSocket,
    origin: Instant,
    leader: u32,
    /// Session time = local time + offset.
    offset_us: i64,
    best_rtt_us: u64,
    pongs: usize,
    ready_from: BTreeSet<u32>,
    acked: BTreeSet<u32>,
    epoch: Option<u64>,
    last_ping: u64,
    last_ready: u64,
    last_start: u64,
    /// Author signatures of every entry held (mine and relayed), so repair can forward them.
    sigs: HashMap<EntryId, [u8; wire::SIG]>,
    control: BTreeMap<u32, Vec<Item>>,
    faults: Vec<FaultStep>,
    links: BTreeMap<u32, Link>,
    default_link: Link,
    delayed: Vec<(u64, SocketAddr, Vec<u8>)>,
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
        let core = PeerCore::new(WORLD, o.me, &ids, scene, s.sim_config());
        let keys = ids.iter().map(|&m| (m, s.key_of(m).expect("validated key"))).collect();
        // Background touches: deterministic in (seed, member), on non-hand objects.
        let mut plan: BTreeMap<u64, Vec<Payload>> = BTreeMap::new();
        if o.touches_per_s > 0 {
            let objects = match scene {
                pe_sim_core::session::SceneKind::Pile { side, layers } => (side * side * layers) as u64,
                pe_sim_core::session::SceneKind::BallPit { balls, .. } => balls as u64,
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
        o.socket.set_nonblocking(true).expect("non-blocking socket");
        Node {
            core,
            me: o.me,
            leader: ids[0],
            keys,
            session: o.session,
            key: o.key,
            sock: o.socket,
            origin: Instant::now(),
            offset_us: 0,
            best_rtt_us: u64::MAX,
            pongs: 0,
            ready_from: BTreeSet::new(),
            acked: BTreeSet::new(),
            epoch: None,
            last_ping: 0,
            last_ready: 0,
            last_start: 0,
            sigs: HashMap::new(),
            control: BTreeMap::new(),
            faults: o.faults,
            links: BTreeMap::new(),
            default_link: Link::default(),
            delayed: Vec::new(),
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
        self.origin.elapsed().as_micros() as u64
    }

    /// Now on the session timeline.
    fn now_us(&self) -> u64 {
        (self.local_us() as i64 + self.offset_us).max(0) as u64
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
        let mut buf = [0u8; 65536];
        loop {
            let (n, _from) = match self.sock.recv_from(&mut buf) {
                Ok(x) => x,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                // Windows reports an ICMP port-unreachable for an earlier send (a member not up yet,
                // or gone) as a reset on the next receive: skip it, more datagrams may be waiting.
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Err(_) => break,
            };
            self.traffic.datagrams_received += 1;
            let (keys, sigs) = (&self.keys, &self.sigs);
            let r = wire::unpack(self.session.session, &buf[..n], |m| keys.get(&m).copied(), |e| sigs.contains_key(&e.id()));
            match r {
                Ok((sender, items)) if sender != self.me => {
                    for i in items {
                        self.on_item(sender, i);
                    }
                }
                Ok(_) => {}
                Err(e) => *self.traffic.rejected.entry(format!("{e:?}")).or_default() += 1,
            }
        }
    }

    fn on_item(&mut self, sender: u32, i: Item) {
        let local = self.local_us();
        match i {
            Item::Entry(e, sig) => {
                self.sigs.entry(e.id()).or_insert(sig);
                let now = self.now_us();
                self.core.receive(sender, Msg::Entry(e), now);
            }
            Item::Fetch(m) => {
                let now = self.now_us();
                self.core.receive(sender, m, now);
            }
            Item::Ping(t) => self.push_control(sender, Item::Pong(t, local)),
            Item::Pong(t, r) if sender == self.leader => {
                let rtt = local.saturating_sub(t);
                self.pongs += 1;
                if rtt < self.best_rtt_us {
                    self.best_rtt_us = rtt;
                    // The leader's clock read r halfway through the round trip.
                    self.offset_us = r as i64 - (t as i64 + rtt as i64 / 2);
                }
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
                    self.note(format!("session start received: epoch {e} (leader clock), my offset {off} us, best rtt {} us", self.best_rtt_us));
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
        if self.me != self.leader {
            if self.pongs < PINGS && local >= self.last_ping + 25_000 {
                self.last_ping = local;
                let l = self.leader;
                self.push_control(l, Item::Ping(local));
            }
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
        let Some(ms) = self.session_ms() else { return };
        while self.faults.first().is_some_and(|f| f.at_ms <= ms) {
            let f = self.faults.remove(0);
            let l = Link { loss_pct: f.loss_pct, delay_us: f.delay_ms * 1000, jitter_us: f.jitter_ms * 1000 };
            match f.to {
                Some(m) => {
                    self.links.insert(m, l);
                }
                None => {
                    self.links.clear();
                    self.default_link = l;
                }
            }
            self.note(format!("fault: links to {} now {}% loss, {} ms +-{} ms", f.to.map_or("all".into(), |m| format!("member {m}")), f.loss_pct, f.delay_ms, f.jitter_ms));
        }
    }

    fn flush_out(&mut self) {
        let mut per: BTreeMap<u32, Vec<Item>> = std::mem::take(&mut self.control);
        for o in self.core.outbox() {
            let item = match o.msg {
                Msg::Entry(e) => {
                    let id = e.id();
                    let sig = if e.member == self.me {
                        *self.sigs.entry(id).or_insert_with(|| wire::sign_entry(&self.key, self.session.session, &e))
                    } else {
                        match self.sigs.get(&id) {
                            Some(s) => *s,
                            None => {
                                self.traffic.entries_without_signature += 1;
                                continue;
                            }
                        }
                    };
                    Item::Entry(e, sig)
                }
                m @ (Msg::Fetch { .. } | Msg::FetchValue { .. }) => Item::Fetch(m),
                // Joins and batches are not sent in M1.
                _ => continue,
            };
            // Repair could wait on a long queue (o.class == Class::Repair); a LAN has none.
            let _ = o.class == Class::Repair;
            for to in o.to {
                per.entry(to).or_default().push(item.clone());
            }
        }
        let now = self.local_us();
        for (to, items) in per {
            let Some(addr) = self.session.addr_of(to) else { continue };
            let link = self.links.get(&to).copied().unwrap_or(self.default_link);
            for d in wire::pack(self.session.session, self.me, &items, &self.key) {
                if link.loss_pct > 0 && self.rand() % 100 < link.loss_pct as u64 {
                    self.traffic.datagrams_dropped_by_fault += 1;
                    continue;
                }
                let jitter = if link.jitter_us > 0 { self.rand() % (link.jitter_us + 1) } else { 0 };
                let due = now + link.delay_us + jitter;
                if due <= now {
                    self.send_now(addr, &d);
                } else {
                    self.delayed.push((due, addr, d));
                }
            }
        }
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

    fn send_now(&mut self, addr: SocketAddr, d: &[u8]) {
        if self.sock.send_to(d, addr).is_ok() {
            self.traffic.datagrams_sent += 1;
            self.traffic.bytes_sent += d.len() as u64;
        }
    }

    // ---- the plan and the end ---------------------------------------------------------------------

    fn run_plan(&mut self) {
        let now = self.now_us();
        let Some(cf) = self.core.clock_frame(now) else { return };
        while self.core.ready() && self.plan.first_key_value().is_some_and(|(f, _)| *f <= cf) {
            let (_, list) = self.plan.pop_first().unwrap();
            for p in list {
                if self.core.author(p).is_ok() {
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
        let members: Vec<u32> = s.members.iter().copied().collect();
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
            epoch: s.epoch,
            rollbacks: s.stats.rollbacks,
            max_rollback_depth: s.stats.max_depth,
            checks_ok: s.stats.checks_ok,
            checks_bad: s.stats.checks_bad,
            entries_authored: self.authored,
            clock_offset_us: self.offset_us,
            best_rtt_us: self.best_rtt_us,
            traffic: self.traffic.clone(),
            events: self.log.clone(),
        }
    }

    fn write_results(&self) -> std::io::Result<Summary> {
        std::fs::create_dir_all(&self.out_dir)?;
        let s = &self.core.sim;
        let mut f = std::fs::File::create(self.out_dir.join(format!("hashes-{}.txt", self.me)))?;
        for fr in 0..self.session.frames {
            writeln!(f, "{fr} {:016x}", s.hashes.get(&fr).copied().unwrap_or(0))?;
        }
        let sum = self.summary();
        std::fs::write(self.out_dir.join(format!("summary-{}.json", self.me)), serde_json::to_string_pretty(&sum).unwrap() + "\n")?;
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
            let now = self.now_us();
            self.core.tick(now);
            self.run_plan();
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
                "status: frame {} now {} conf {} members {:?} rollbacks {} | sent {} faulted {} recv {} rejected {:?} unsigned {} | sigs {}",
                s.frame, s.now, s.conf, s.members, s.stats.rollbacks, t.datagrams_sent, t.datagrams_dropped_by_fault, t.datagrams_received, t.rejected, t.entries_without_signature, self.sigs.len()
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
        self.core.dt_us() as f64 / 1000.0
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
        self.core.sim.t.objs.len() as u32
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
        let payload = match i {
            Interaction::Touch { object, impulse, grab } => Payload::Touch { obj: object, impulse, grab },
            Interaction::Hand { object, target_mm, active } => Payload::Hand { obj: object, target_mm, active, span: 1 },
        };
        let e = self.core.author(payload).map_err(|e| match e {
            AuthorError::NotReady => ToolError::new(ErrorCode::NotReady, "the peer is behind its clock or paused; step and retry"),
            AuthorError::NotOwner => ToolError::new(ErrorCode::NotOwner, "not your hand"),
        })?;
        self.authored += 1;
        self.flush_out();
        Ok(Authored { entry: e.id(), stamp: e.stamp, seq: e.seq })
    }
    fn status(&self, entry: u64) -> Option<EntryStatus> {
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

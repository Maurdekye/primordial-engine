//! The phase-2 backend: one member of an in-process session simulator (`spikes/session-sim`), with
//! the other members running scripted plans over virtual links. The agent's interactions are
//! authored with the same `WorldSim` calls a player's planned actions use, and broadcast over the
//! simulated network.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use pe_session_sim::{Action, Fate, Link, Net, Peer, Sim};
use pe_session::session::{
    final_log, replay_sim_like, Msg, SceneKind, SignedEntry, SimConfig, WorldSim,
};
use pe_session::splitmix64;

use crate::backend::*;
use crate::model::*;

pub const WORLD: u32 = 1;
const DEFAULT_SEED: u64 = 0x5052_494D_4F52_4449;
/// Background plans are generated this far ahead (10 minutes at 60 Hz).
const PLAN_FRAMES: u64 = 36_000;

use crate::sim_view::{self, hex};

pub struct SimSession {
    sim: Sim,
    me: MemberId,
    members: Vec<MemberId>,
    label: String,
    scene_label: String,
    /// Partition: member -> group. Shared with the network filter.
    partition: Rc<RefCell<BTreeMap<MemberId, usize>>>,
    faults: Vec<String>,
    /// The agent's held hand target (D52 ruling (a)).
    hold: sim_view::HandHold,
}

fn range<T: PartialOrd + Copy + std::fmt::Display>(field: &str, v: Option<T>, default: T, lo: T, hi: T) -> ToolResult<T> {
    let v = v.unwrap_or(default);
    if v < lo || v > hi {
        return Err(ToolError::arg(field, format!("{field} must be in {lo}..={hi}, got {v}")));
    }
    Ok(v)
}

impl SimSession {
    pub fn load(p: &LoadParams) -> ToolResult<SimSession> {
        let n = range("members", p.members, 4, 2, 8)?;
        let me = range("me", p.me, 1, 1, n)?;
        let scene_choice = p.scene.unwrap_or(SceneChoice::Pile);
        let (scene, scene_label) = match scene_choice {
            SceneChoice::Pile => {
                let side = range("size", p.size, 4, 1, 8)? as usize;
                (SceneKind::Pile { side, layers: 2 }, format!("pile {side}x{side}x2"))
            }
            SceneChoice::Ballpit => {
                let balls = range("size", p.size, 100, 1, 500)? as usize;
                (SceneKind::BallPit { balls, hands: n as usize }, format!("ballpit {balls} balls, {n} hands"))
            }
        };
        let delay = range("delay_ms", p.delay_ms, 20, 0, 1000)?;
        let jitter = range("jitter_ms", p.jitter_ms, 0, 0, 500)?;
        let loss = range("loss_pct", p.loss_pct, 0, 0, 50)?;
        let rate = range("background_touches_per_s", p.background_touches_per_s, 0, 0, 30)?;
        let grabs = p.background_grabs.unwrap_or(false);
        let window = range("window", p.window, 120, 30, 600)?;
        let seed = p.seed.unwrap_or(DEFAULT_SEED);
        let cfg = SimConfig { window, suspect_after: window * 3 / 2, seed, ..SimConfig::default() };

        let members: Vec<MemberId> = (1..=n).collect();
        let objects = match scene {
            SceneKind::Pile { side, layers } => side * side * layers,
            SceneKind::BallPit { balls, .. } => balls,
            SceneKind::Empty => 0,
        } as u64;
        let mut peers = Vec::new();
        for &m in &members {
            let mut plan: BTreeMap<u64, Vec<Action>> = BTreeMap::new();
            if m != me && rate > 0 {
                // Deterministic in (seed, member): the same parameters give the same run.
                let period = (60 / rate as u64).max(1);
                let mut r = splitmix64(seed ^ (m as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
                let mut f = 60 + r % period;
                while f < PLAN_FRAMES {
                    r = splitmix64(r);
                    let obj = (r % objects) as u32;
                    let c = |k: u32| ((r >> (16 + 8 * k)) & 0xff) as f32 / 255.0;
                    let impulse = [4.0 * c(0) - 2.0, 4.0 * c(1), 4.0 * c(2) - 2.0];
                    plan.entry(f).or_default().push(Action::Touch { obj, impulse, grab: grabs });
                    f += period;
                }
            }
            let mut peer = Peer::new(m, 0);
            peer.join(WORLD, &members, scene, cfg, 0, plan);
            peers.push(peer);
        }
        let net = Net::new(Link { delay_us: delay * 1000, jitter_us: jitter * 1000, loss_pct: loss }, seed);
        let mut sim = Sim::new(net, peers, u64::MAX);
        let partition: Rc<RefCell<BTreeMap<MemberId, usize>>> = Rc::default();
        let part = partition.clone();
        sim.net.filter = Some(Box::new(move |_, _, from, to, _| {
            let p = part.borrow();
            // Every member has a group while a partition is in force.
            if p.is_empty() || p.get(&from) == p.get(&to) {
                Fate::Deliver
            } else {
                Fate::Drop
            }
        }));
        let label = format!(
            "{scene_label}; {n} members, driving member {me}; links {delay} ms +-{jitter} ms, {loss}% loss; background {rate} {}/s; window {window}; seed {seed:x}",
            if grabs { "grabs" } else { "touches" }
        );
        Ok(SimSession { sim, me, members, label, scene_label, partition, faults: Vec::new(), hold: Default::default() })
    }

    fn replica(&self) -> &WorldSim {
        &self.sim.replica(self.me, WORLD).sim
    }

    fn peer(&self, m: MemberId) -> &Peer {
        self.sim.peers.iter().find(|p| p.id == m).expect("member")
    }

    /// One millisecond of simulated time, then the held hand pose if one is due. It goes out with
    /// the member's next tick (the harness drains the core's outbox there).
    fn step_one_ms(&mut self) {
        self.sim.step_ms();
        self.sim.net.now_us += 1000;
        self.tick_hold();
    }

    fn tick_hold(&mut self) -> Option<SignedEntry> {
        let me = self.me;
        let peer = self.sim.peers.iter_mut().find(|p| p.id == me).expect("me");
        if peer.crashed {
            return None;
        }
        self.hold.tick(&mut peer.worlds.get_mut(&WORLD).expect("world").core)
    }

    fn check_member(&self, field: &str, m: Option<u32>) -> ToolResult<MemberId> {
        let m = m.ok_or_else(|| ToolError::arg(field, format!("{field} is required")))?;
        if !self.members.contains(&m) {
            return Err(ToolError::arg(field, format!("no member {m}; members are 1..={}", self.members.len())));
        }
        Ok(m)
    }
}

impl DrivenPeer for SimSession {
    fn scenario_label(&self) -> String {
        self.label.clone()
    }
    fn backend_name(&self) -> &'static str {
        "session-sim"
    }
    fn me(&self) -> MemberId {
        self.me
    }
    fn world(&self) -> u32 {
        WORLD
    }
    fn members(&self) -> Vec<MemberId> {
        self.members.clone()
    }
    fn scene_label(&self) -> String {
        self.scene_label.clone()
    }
    fn dt_ms(&self) -> f64 {
        self.replica().cfg.dt as f64 * 1000.0
    }
    fn time_ms(&self) -> u64 {
        self.sim.net.now_us / 1000
    }
    fn frame(&self) -> u64 {
        self.replica().frame
    }
    fn conf(&self) -> u64 {
        self.replica().conf
    }
    fn object_count(&self) -> u32 {
        self.replica().obj_slots()
    }

    fn state(&self, offset: u32, limit: u32) -> WorldState {
        sim_view::state(self.replica(), offset, limit)
    }

    fn object(&self, object: u32) -> ToolResult<ObjectState> {
        sim_view::object(self.replica(), object)
    }

    fn propose(&mut self, i: Interaction) -> ToolResult<Authored> {
        let me = self.me;
        if self.peer(me).crashed {
            return Err(ToolError::new(ErrorCode::PeerCrashed, format!("member {me} (the driven peer) has crashed")));
        }
        sim_view::check_interaction(self.replica(), &i, me)?;
        // A hand target is held: its first pose goes out at the next pose slot (at most one stride).
        match i {
            Interaction::Hand { object, target_mm, active: true } => {
                self.hold.set(object, target_mm);
                self.tick_hold();
                let mut waited = 0;
                while self.hold.first().is_none() && waited < sim_view::HOLD_WAIT_MS {
                    self.step_one_ms();
                    waited += 1;
                }
                let Some(e) = self.hold.first() else {
                    self.hold.release();
                    return Err(ToolError::new(ErrorCode::NotReady, "no pose slot for the hand within 1 s; step and retry"));
                };
                return Ok(Authored { entry: e.id(), stamp: e.stamp, seq: e.seq });
            }
            Interaction::Hand { active: false, .. } => self.hold.release(),
            Interaction::Touch { .. } => {}
        }
        let peer = self.sim.peers.iter_mut().find(|p| p.id == me).expect("me");
        let r = peer.worlds.get_mut(&WORLD).expect("world");
        let e = sim_view::author(&mut r.sim, i);
        let to: Vec<MemberId> = r.sim.audience().into_iter().filter(|&m| m != me).collect();
        let msg = Msg::Entry(e.clone());
        for m in to {
            self.sim.net.post(WORLD, me, m, &msg);
        }
        Ok(Authored { entry: e.id(), stamp: e.stamp, seq: e.seq })
    }

    fn status(&self, entry: EntryId) -> Option<EntryStatus> {
        sim_view::status(self.replica(), entry, self.me)
    }

    fn advance_ms(&mut self, ms: u64) -> ToolResult<()> {
        for _ in 0..ms {
            self.step_one_ms();
        }
        Ok(())
    }

    fn counters(&self) -> Counters {
        sim_view::counters(self.replica())
    }

    fn paused(&self) -> bool {
        let s = self.replica();
        s.frame < s.now
    }

    fn events_since(&self, n: usize) -> (Vec<String>, usize) {
        sim_view::events_since(self.replica(), n)
    }

    fn hashes(&self, from: u64, to: u64) -> Hashes {
        sim_view::hashes(self.replica(), from, to)
    }

    fn member_info(&self) -> Members {
        sim_view::members(self.replica(), self.me, &self.members)
    }

    fn sync(&self) -> SyncStatus {
        sim_view::sync(self.replica())
    }

    fn indicators(&self) -> Option<Indicators> {
        sim_view::indicators(&self.sim.replica(self.me, WORLD).core, self.me, self.sim.net.now_us)
    }

    fn scenario(&mut self) -> Option<&mut dyn ScenarioControl> {
        Some(self)
    }
}

impl ScenarioControl for SimSession {
    fn fault(&mut self, p: &FaultParams) -> ToolResult<FaultReport> {
        let applied = match p.fault {
            FaultKind::Crash => {
                let m = self.check_member("member", p.member)?;
                let peer = self.sim.peers.iter_mut().find(|x| x.id == m).expect("member");
                if peer.crashed {
                    return Err(ToolError::arg("member", format!("member {m} has already crashed")));
                }
                peer.crashed = true;
                format!("member {m} crashed")
            }
            FaultKind::Link => {
                let a = self.check_member("from", p.from)?;
                let b = self.check_member("to", p.to)?;
                if a == b {
                    return Err(ToolError::arg("to", "from and to must differ"));
                }
                let delay = range("delay_ms", p.delay_ms, 20, 0, 1000)?;
                let jitter = range("jitter_ms", p.jitter_ms, 0, 0, 500)?;
                let loss = range("loss_pct", p.loss_pct, 0, 0, 100)?;
                let link = Link { delay_us: delay * 1000, jitter_us: jitter * 1000, loss_pct: loss };
                self.sim.net.links.insert((a, b), link);
                let one_way = p.one_way.unwrap_or(false);
                if !one_way {
                    self.sim.net.links.insert((b, a), link);
                }
                format!("link {a}{}{b}: {delay} ms +-{jitter} ms, {loss}% loss", if one_way { "->" } else { "<->" })
            }
            FaultKind::Partition => {
                let groups = p.groups.as_ref().filter(|g| !g.is_empty()).ok_or_else(|| ToolError::arg("groups", "groups is required: a list of member lists"))?;
                let mut map = BTreeMap::new();
                for (gi, g) in groups.iter().enumerate() {
                    if g.is_empty() {
                        return Err(ToolError::arg("groups", "a group is empty"));
                    }
                    for &m in g {
                        self.check_member("groups", Some(m))?;
                        if map.insert(m, gi).is_some() {
                            return Err(ToolError::arg("groups", format!("member {m} is in two groups")));
                        }
                    }
                }
                // A member in no group is alone.
                for &m in &self.members {
                    let n = groups.len() + m as usize;
                    map.entry(m).or_insert(n);
                }
                *self.partition.borrow_mut() = map;
                self.faults.retain(|f| !f.starts_with("partition"));
                format!("partition {groups:?}")
            }
            FaultKind::Heal => {
                if self.partition.borrow().is_empty() {
                    return Err(ToolError::arg("fault", "no partition is in force"));
                }
                self.partition.borrow_mut().clear();
                self.faults.retain(|f| !f.starts_with("partition"));
                return Ok(FaultReport { applied: "partition healed".into(), active: self.faults.clone() });
            }
        };
        self.faults.push(applied.clone());
        Ok(FaultReport { applied, active: self.faults.clone() })
    }

    fn check(&self) -> CheckReport {
        let live: Vec<&Peer> = self.sim.peers.iter().filter(|p| !p.crashed).collect();
        let crashed: Vec<MemberId> = self.sim.peers.iter().filter(|p| p.crashed).map(|p| p.id).collect();
        let reference = live.iter().find(|p| p.id == self.me).or(live.first()).map(|p| &p.worlds[&WORLD]);
        let Some(reference) = reference else {
            return CheckReport {
                frames: 0,
                live_members: vec![],
                crashed_members: crashed,
                replicas_equal: true,
                replay_equal: true,
                first_divergence: None,
            };
        };
        let frames = live.iter().map(|p| p.worlds[&WORLD].sim.conf.min(p.worlds[&WORLD].sim.frame)).min().unwrap_or(0);
        let rs = &reference.sim;
        let mut first: Option<Divergence> = None;
        for p in &live {
            let h = &p.worlds[&WORLD].sim.hashes;
            if let Some(f) = (0..frames).find(|f| h.get(f) != rs.hashes.get(f)) {
                if first.as_ref().is_none_or(|d| f < d.frame) {
                    first = Some(Divergence {
                        frame: f,
                        member: p.id,
                        hash: h.get(&f).map(|&x| hex(x)),
                        reference: rs.hashes.get(&f).map(|&x| hex(x)),
                    });
                }
            }
        }
        let replicas_equal = first.is_none();
        let members: Vec<MemberId> = rs.membership().members().iter().copied().collect();
        let replay = replay_sim_like(rs, reference.scene, &members, &final_log(rs), frames).hashes;
        let replay_div = (0..frames).find(|f| replay.get(f) != rs.hashes.get(f));
        if let (Some(f), None) = (replay_div, &first) {
            first = Some(Divergence {
                frame: f,
                member: rs.me,
                hash: rs.hashes.get(&f).map(|&x| hex(x)),
                reference: replay.get(&f).map(|&x| hex(x)),
            });
        }
        CheckReport {
            frames,
            live_members: live.iter().map(|p| p.id).collect(),
            crashed_members: crashed,
            replicas_equal,
            replay_equal: replay_div.is_none(),
            first_divergence: first,
        }
    }
}

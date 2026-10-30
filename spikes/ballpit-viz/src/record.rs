//! Per-peer display recording. Each cell runs the real protocol on the session simulator's
//! virtual clock and records, at every 60 Hz display tick of each peer, exactly what that peer
//! would draw: the transform of every ball and both hands after its own predictions, rollbacks
//! and corrections. Nothing is smoothed or idealized.
//!
//! * Pose-source cells run `pe_session_sim::Sim` unchanged (the ball-pit scenario of spike 5d):
//!   member 1 (the engager) authors one Hand entry per frame; member 2 (the observer) predicts
//!   "hand holds still", then rolls back and re-simulates when the entry lands.
//! * Naive cells stream state: the engager simulates locally and sends, every frame, the full
//!   state of every object whose state changed (56 B each plus a 16 B header per datagram, in
//!   datagrams of at most 1200 B payload). The observer does not simulate; it draws the newest
//!   state it has received for each object, with no interpolation or extrapolation.

use std::collections::{BTreeMap, BinaryHeap};
use std::cmp::Reverse;

use pe_session_sim::*;
use pe_sim_core::session::*;
use pe_sim_core::splitmix64;

pub const HZ: u64 = 60;
pub const BALLS: usize = 500;
/// 500 balls, the engager's hand (500) and member 2's parked hand (501).
pub const N_OBJ: usize = 502;
pub const ENGAGER_HAND: usize = 500;
pub const EPOCH_US: u64 = 100_000;
pub const DT_US: u64 = 16_667;
/// Recorded display ticks: 1.6 s .. 10.1 s after the start of the run (the sweep is 2.1-5.76 s).
pub const K0: u64 = 90;
pub const K1: u64 = 600;
pub const END_US: u64 = 10_300_000;
/// Naive streaming starts here (sim frame); before it both peers run the same local sim and the
/// pit is settled (spike 5d: 0 balls changing per frame from 1.0 s).
pub const NAIVE_FROM: u64 = 90;
pub const JITTER_MS: u64 = 10;
pub const LOSS_PCT: u32 = 1;
pub const QUEUE_MS: u64 = 500;
pub const NAIVE_DGRAM_PAYLOAD: usize = 1200;
pub const NAIVE_HDR: usize = 16;
pub const NAIVE_PER_OBJ: usize = 56;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Naive,
    Pose { ahead: u64 },
}

#[derive(Clone, Debug)]
pub struct Cell {
    pub id: String,
    pub mode: Mode,
    /// One-way base delay; each datagram adds 0..JITTER_MS.
    pub latency_ms: u64,
    pub cap_kbps: Option<u64>,
}

impl Cell {
    pub fn mode_label(&self) -> String {
        match self.mode {
            Mode::Naive => "naive state streaming (no rate control, no resend)".into(),
            Mode::Pose { ahead: 0 } => "pose-source, input delay 0".into(),
            Mode::Pose { ahead } => format!("pose-source, input delay {ahead} (+{:.0} ms engager latency)", ahead as f64 * 1000.0 / HZ as f64),
        }
    }
    pub fn net_label(&self) -> String {
        let bw = match self.cap_kbps {
            None => "unlimited".to_string(),
            Some(k) => format!("{k} kbit/s cap"),
        };
        format!("{} ms one-way (+0-{JITTER_MS} jitter), {LOSS_PCT}% loss, {bw}", self.latency_ms)
    }
    pub fn cap(&self) -> Option<Cap> {
        self.cap_kbps.map(|k| {
            let rate_bps = k * 1000;
            Cap { rate_bps, burst_bytes: (rate_bps / 8 / 50).max(1500), max_queue_us: QUEUE_MS * 1000 }
        })
    }
}

/// The matrix: three configurations x six network conditions (see docs/ballpit-video-matrix.md).
pub fn matrix() -> Vec<Cell> {
    let modes = [("naive", Mode::Naive), ("pose-d0", Mode::Pose { ahead: 0 }), ("pose-d4", Mode::Pose { ahead: 4 })];
    let nets: [(u64, Option<u64>); 6] = [(0, None), (50, None), (150, None), (300, None), (50, Some(256)), (50, Some(64))];
    let mut v = Vec::new();
    for (mname, mode) in modes {
        for (lat, cap) in nets {
            let bw = cap.map_or("unl".to_string(), |k| format!("{k}k"));
            v.push(Cell { id: format!("{mname}_{lat}ms_{bw}"), mode, latency_ms: lat, cap_kbps: cap });
        }
    }
    v
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Metrics {
    /// Wire bytes per second over the last second (payload + 52 B per datagram).
    pub tx_bps: f64,
    pub rx_bps: f64,
    pub rollbacks_s: f64,
    /// Measured CPU time of re-simulation, averaged per display frame over the last second.
    pub resim_ms: f64,
    /// Outgoing bottleneck queue backlog.
    pub queue_ms: f64,
    /// Cumulative datagrams this peer sent that a full queue dropped / the link lost.
    pub cap_drops: u64,
    pub lost: u64,
    /// Rollback snapshot bytes held (naive observer: its latest-state table).
    pub snapshot_bytes: u64,
    pub snapshots: u64,
    /// Displayed sim frame minus confirmed frame.
    pub conf_lag: u64,
    /// The sim did not advance this tick (speculation limit or nothing to do).
    pub stalled: bool,
    pub restamps: u64,
}

#[derive(Clone, Debug)]
pub struct Shot {
    pub k: u64,
    pub t_us: u64,
    pub sim_frame: u64,
    pub pos: Vec<[f32; 3]>,
    pub vel: Vec<[f32; 3]>,
    /// Per object: the sim frame its drawn state belongs to (pose-source and the naive engager:
    /// the displayed frame for every object; naive observer: the frame of the newest update
    /// received for that object).
    pub frames: Vec<u64>,
    pub m: Metrics,
}

pub struct Recording {
    pub cell: Cell,
    pub eng: Vec<Shot>,
    pub obs: Vec<Shot>,
    /// Verification lines (end-state equality, protocol stats).
    pub notes: Vec<String>,
}

fn link(delay_ms: u64) -> Link {
    Link { delay_us: delay_ms * 1000, jitter_us: JITTER_MS * 1000, loss_pct: LOSS_PCT }
}

fn plan(items: &[(u64, Action)]) -> BTreeMap<u64, Vec<Action>> {
    let mut m: BTreeMap<u64, Vec<Action>> = BTreeMap::new();
    for (f, a) in items {
        m.entry(*f).or_default().push(a.clone());
    }
    m
}

fn cfg() -> SimConfig {
    SimConfig { dt: 1.0 / HZ as f32, window: 2 * HZ, ..SimConfig::default() }
}

const SCENE: SceneKind = SceneKind::BallPit { balls: BALLS, hands: 2 };

fn read_state(s: &WorldSim) -> (Vec<[f32; 3]>, Vec<[f32; 3]>) {
    let mut pos = Vec::with_capacity(N_OBJ);
    let mut vel = Vec::with_capacity(N_OBJ);
    for i in 0..N_OBJ {
        let st = s.obj_state(i as u32).expect("object exists");
        let (p, v) = (st.pos(), st.lin());
        pos.push([p.x, p.y, p.z]);
        vel.push([v.x, v.y, v.z]);
    }
    (pos, vel)
}

/// Rolling one-second deltas of cumulative per-peer counters.
#[derive(Default)]
struct Roll {
    hist: Vec<(u64, u64, u64)>, // (rollbacks, resim_frames, resim_us) per recorded tick
}

impl Roll {
    fn push(&mut self, s: &Stats) -> (f64, f64, f64) {
        self.hist.push((s.rollbacks, s.resim_frames, s.resim_us));
        let n = self.hist.len();
        let back = n.saturating_sub(HZ as usize + 1);
        let (a, b) = (self.hist[back], self.hist[n - 1]);
        let ticks = (n - 1 - back).max(1) as f64;
        let secs = ticks / HZ as f64;
        ((b.0 - a.0) as f64 / secs, (b.1 - a.1) as f64 / secs, (b.2 - a.2) as f64 / 1000.0 / ticks)
    }
}

/// tx/rx wire bytes per second over (t - 1 s, t] from a send trace.
fn traffic(trace: &[SendRec], me: MemberId, t: u64) -> (f64, f64, u64, u64) {
    let lo = t.saturating_sub(1_000_000);
    let (mut tx, mut rx, mut capd, mut lost) = (0u64, 0u64, 0u64, 0u64);
    for r in trace {
        if r.from == me {
            if r.at_us > lo && r.at_us <= t {
                tx += r.wire;
            }
            if r.at_us <= t {
                match r.fate {
                    1 => capd += 1,
                    2 => lost += 1,
                    _ => {}
                }
            }
        }
        if r.to == me && r.fate == 0 && r.arrive_us > lo && r.arrive_us <= t {
            rx += r.wire;
        }
    }
    (tx as f64, rx as f64, capd, lost)
}

fn clock_k(now: u64, skew: i64) -> Option<u64> {
    let clock = (now as i64 + skew).max(0) as u64;
    (clock >= EPOCH_US).then(|| (clock - EPOCH_US) / DT_US)
}

pub fn record(cell: &Cell) -> Recording {
    match cell.mode {
        Mode::Pose { ahead } => record_pose(cell, ahead),
        Mode::Naive => record_naive(cell),
    }
}

fn record_pose(cell: &Cell, ahead: u64) -> Recording {
    let ids = [1, 2];
    let peers: Vec<Peer> = ids
        .iter()
        .map(|&id| {
            let mut p = Peer::new(id, if id == 1 { 1000 } else { -1000 });
            let pl = if id == 1 { plan(&ballpit_sweep(HZ, 1, ahead)) } else { BTreeMap::new() };
            p.join(1, &ids, SCENE, cfg(), EPOCH_US, pl);
            p
        })
        .collect();
    let mut net = Net::new(link(cell.latency_ms), 17);
    net.default_cap = cell.cap();
    net.tracing = true;
    let mut sim = Sim::new(net, peers, END_US);
    let mut last_k = [u64::MAX; 2];
    let mut last_frame = [0u64; 2];
    let mut rolls = [Roll::default(), Roll::default()];
    let mut shots: [Vec<Shot>; 2] = [Vec::new(), Vec::new()];
    while sim.net.now_us <= END_US {
        sim.step_ms();
        let now = sim.net.now_us;
        for pi in 0..2 {
            let Some(k) = clock_k(now, sim.peers[pi].skew_us) else { continue };
            if k == last_k[pi] {
                continue;
            }
            last_k[pi] = k;
            let r = &sim.peers[pi].worlds[&1];
            let s = &r.sim;
            let (rb, _, rms) = rolls[pi].push(&s.stats);
            let stalled = s.frame == last_frame[pi];
            last_frame[pi] = s.frame;
            if !(K0..K1).contains(&k) {
                continue;
            }
            let (pos, vel) = read_state(s);
            let other = ids[1 - pi];
            let m = Metrics {
                rollbacks_s: rb,
                resim_ms: rms,
                queue_ms: sim.net.backlog_us(ids[pi], other) as f64 / 1000.0,
                snapshot_bytes: s.ring_bytes_with_objs() as u64,
                snapshots: s.ring_len() as u64,
                conf_lag: s.frame.saturating_sub(s.conf),
                stalled,
                restamps: s.events.iter().filter(|e| e.contains("re-stamped")).count() as u64,
                ..Default::default()
            };
            shots[pi].push(Shot { k, t_us: now, sim_frame: s.frame, pos, vel, frames: vec![s.frame; N_OBJ], m });
        }
        sim.net.now_us += 1000;
    }
    let trace = std::mem::take(&mut sim.net.trace);
    for (pi, v) in shots.iter_mut().enumerate() {
        for sh in v.iter_mut() {
            let (tx, rx, c, l) = traffic(&trace, ids[pi], sh.t_us);
            sh.m.tx_bps = tx;
            sh.m.rx_bps = rx;
            sh.m.cap_drops = c;
            sh.m.lost = l;
        }
    }
    // Verification: both members' full-state hash at the last frame both simulated, and their
    // digests against an offline replay of the final log.
    let (s1, s2) = (&sim.replica(1, 1).sim, &sim.replica(2, 1).sim);
    let last = s1.frame.min(s2.frame) - 1;
    let (h1, h2) = (s1.hashes[&last], s2.hashes[&last]);
    let mut notes = vec![format!(
        "end state at frame {last} (all objects and tables): engager {h1:016x} observer {h2:016x} -> {}",
        if h1 == h2 { "IDENTICAL" } else { "DIFFERENT" }
    )];
    let frames = last + 1;
    let rep = replay(1, SCENE, cfg(), &ids, &final_log(s1), frames);
    let (d1, d2, dr) = (digest(&s1.hashes, frames), digest(&s2.hashes, frames), digest(&rep, frames));
    notes.push(format!(
        "frames 0..{frames} digests engager {d1:016x} observer {d2:016x} offline replay {dr:016x} -> {}",
        if d1 == d2 && d2 == dr { "ALL EQUAL" } else { "MISMATCH" }
    ));
    for (who, s) in [("engager", s1), ("observer", s2)] {
        let st = s.stats;
        notes.push(format!(
            "{who}: rollbacks {} max depth {} resim frames {} ({:.1} ms CPU) paused frames {} checks ok {} bad {} re-stamps {} conf {} frame {}",
            st.rollbacks, st.max_depth, st.resim_frames, st.resim_us as f64 / 1000.0, st.paused_frames, st.checks_ok, st.checks_bad,
            s.events.iter().filter(|e| e.contains("re-stamped")).count(), s.conf, s.frame
        ));
    }
    notes.push(format!("datagrams dropped by full bandwidth queue {}, lost {}", sim.net.cap_dropped, sim.net.dropped - sim.net.cap_dropped));
    let [eng, obs] = shots;
    Recording { cell: cell.clone(), eng, obs, notes }
}

fn record_naive(cell: &Cell) -> Recording {
    // The engager simulates alone (a one-member session: nothing to roll back).
    let mut p = Peer::new(1, 1000);
    p.join(1, &[1], SCENE, cfg(), EPOCH_US, plan(&ballpit_sweep(HZ, 1, 0)));
    let mut sim = Sim::new(Net::new(link(cell.latency_ms), 17), vec![p], END_US);
    let lk = link(cell.latency_ms);
    let cap = cell.cap();
    let mut shaper = Shaper::default();
    let mut rng: u64 = 17;
    let mut rand = || {
        rng = splitmix64(rng);
        rng
    };
    // In-flight datagrams: (arrival, seq, frame, objects).
    let mut flight: BinaryHeap<Reverse<(u64, u64)>> = BinaryHeap::new();
    let mut payloads: BTreeMap<u64, (u64, Vec<(u32, BodyState)>)> = BTreeMap::new();
    let mut seq = 0u64;
    let mut trace: Vec<SendRec> = Vec::new();
    let (mut cap_dropped, mut lost) = (0u64, 0u64);
    // Observer display table: newest received state per object and its frame.
    let mut disp: Vec<Option<(u64, BodyState)>> = vec![None; N_OBJ];
    let mut prev: Vec<Option<BodyState>> = vec![None; N_OBJ];
    let mut streamed_to = 0u64;
    let mut last_k = [u64::MAX; 2];
    let mut last_frame = 0u64;
    let mut obs_last_change = 0u64;
    let mut roll = Roll::default();
    let mut shots: [Vec<Shot>; 2] = [Vec::new(), Vec::new()];
    while sim.net.now_us <= END_US {
        sim.step_ms();
        let now = sim.net.now_us;
        let s = &sim.peers[0].worlds[&1].sim;
        // Stream every newly simulated frame from NAIVE_FROM on.
        if s.frame > streamed_to && s.caught_up() {
            let f = s.frame;
            let states: Vec<BodyState> = (0..N_OBJ).map(|i| s.obj_state(i as u32).unwrap()).collect();
            if f == NAIVE_FROM {
                // Shared starting point: both peers simulated the identical settled pit so far.
                for (i, st) in states.iter().enumerate() {
                    disp[i] = Some((f, *st));
                }
            }
            if f > NAIVE_FROM {
                let changed: Vec<(u32, BodyState)> = states
                    .iter()
                    .enumerate()
                    .filter(|(i, st)| prev[*i] != Some(**st))
                    .map(|(i, st)| (i as u32, *st))
                    .collect();
                let per = (NAIVE_DGRAM_PAYLOAD - NAIVE_HDR) / NAIVE_PER_OBJ;
                for chunk in changed.chunks(per) {
                    let payload = (NAIVE_HDR + NAIVE_PER_OBJ * chunk.len()) as u64;
                    let wire = payload + DATAGRAM_OVERHEAD;
                    let depart = match cap {
                        Some(c) => shaper.offer(&c, now, wire),
                        None => Some(now),
                    };
                    let Some(depart) = depart else {
                        cap_dropped += 1;
                        trace.push(SendRec { at_us: now, from: 1, to: 2, wire, fate: 1, arrive_us: 0 });
                        continue;
                    };
                    if (rand() % 100) < lk.loss_pct as u64 {
                        lost += 1;
                        trace.push(SendRec { at_us: now, from: 1, to: 2, wire, fate: 2, arrive_us: 0 });
                        continue;
                    }
                    let at = depart + lk.delay_us + rand() % (lk.jitter_us + 1);
                    trace.push(SendRec { at_us: now, from: 1, to: 2, wire, fate: 0, arrive_us: at });
                    seq += 1;
                    flight.push(Reverse((at, seq)));
                    payloads.insert(seq, (f, chunk.to_vec()));
                }
            }
            for (i, st) in states.into_iter().enumerate() {
                prev[i] = Some(st);
            }
            streamed_to = f;
        }
        // Observer: apply every datagram that has arrived (newest frame wins per object).
        while let Some(Reverse((at, ..))) = flight.peek() {
            if *at > now {
                break;
            }
            let Reverse((_, sq)) = flight.pop().unwrap();
            let (f, objs) = payloads.remove(&sq).unwrap();
            for (o, st) in objs {
                let slot = &mut disp[o as usize];
                if slot.is_none_or(|(g, _)| f > g) {
                    *slot = Some((f, st));
                    obs_last_change = now;
                }
            }
        }
        // Engager display tick.
        if let Some(k) = clock_k(now, 1000) {
            if k != last_k[0] {
                last_k[0] = k;
                let (rb, _, rms) = roll.push(&s.stats);
                let stalled = s.frame == last_frame;
                last_frame = s.frame;
                if (K0..K1).contains(&k) {
                    let (pos, vel) = read_state(s);
                    let m = Metrics {
                        rollbacks_s: rb,
                        resim_ms: rms,
                        queue_ms: shaper.backlog_us(now) as f64 / 1000.0,
                        // A naive engager never rolls back, so it needs no snapshots (the
                        // harness's local WorldSim keeps a ring regardless; not counted).
                        snapshot_bytes: 0,
                        snapshots: 0,
                        conf_lag: s.frame.saturating_sub(s.conf),
                        stalled,
                        ..Default::default()
                    };
                    shots[0].push(Shot { k, t_us: now, sim_frame: s.frame, pos, vel, frames: vec![s.frame; N_OBJ], m });
                }
            }
        }
        // Observer display tick.
        if let Some(k) = clock_k(now, -1000) {
            if k != last_k[1] {
                last_k[1] = k;
                if (K0..K1).contains(&k) {
                    let newest = disp.iter().filter_map(|d| d.map(|(f, _)| f)).max().unwrap_or(0);
                    let pos = disp.iter().map(|d| d.map(|(_, st)| { let p = st.pos(); [p.x, p.y, p.z] }).unwrap_or([0.0; 3])).collect();
                    let vel = disp.iter().map(|d| d.map(|(_, st)| { let v = st.lin(); [v.x, v.y, v.z] }).unwrap_or([0.0; 3])).collect();
                    let m = Metrics {
                        snapshot_bytes: (N_OBJ * (NAIVE_PER_OBJ + 8)) as u64,
                        stalled: obs_last_change + DT_US < now,
                        ..Default::default()
                    };
                    let frames = disp.iter().map(|d| d.map_or(0, |(f, _)| f)).collect();
                    shots[1].push(Shot { k, t_us: now, sim_frame: newest, pos, vel, frames, m });
                }
            }
        }
        sim.net.now_us += 1000;
    }
    for (pi, v) in shots.iter_mut().enumerate() {
        for sh in v.iter_mut() {
            let (tx, rx, c, l) = traffic(&trace, pi as u32 + 1, sh.t_us);
            sh.m.tx_bps = tx;
            sh.m.rx_bps = rx;
            sh.m.cap_drops = c;
            sh.m.lost = l;
        }
    }
    // Verification: the observer's final table against the engager's final state.
    let s = &sim.peers[0].worlds[&1].sim;
    let mut worst = 0.0f32;
    let mut off = 0;
    for i in 0..N_OBJ {
        let a = s.obj_state(i as u32).unwrap();
        let b = disp[i].map(|(_, st)| st);
        if b != Some(a) {
            let (pa, pb) = (a.pos(), b.unwrap().pos());
            let d = ((pa.x - pb.x).powi(2) + (pa.y - pb.y).powi(2) + (pa.z - pb.z).powi(2)).sqrt();
            worst = worst.max(d);
            off += 1;
        }
    }
    let notes = vec![
        format!(
            "end: observer table vs engager state at frame {}: {off} objects not bit-identical, worst position error {:.1} mm -> {}",
            s.frame,
            worst * 1000.0,
            if off == 0 { "IDENTICAL" } else { "DIFFERENT" }
        ),
        format!("naive datagrams: {} offered, {cap_dropped} dropped by full bandwidth queue, {lost} lost", trace.len()),
    ];
    let [eng, obs] = shots;
    Recording { cell: cell.clone(), eng, obs, notes }
}

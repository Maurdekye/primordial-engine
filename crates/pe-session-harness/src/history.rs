//! Perturbation-history study (design 6.7-6.9) on the real protocol:
//!
//! * `snap`: the ball pit under each snapshot policy (the local memory/CPU knob), plus a run
//!   that moves the knob every few frames. Reports image memory held, CPU per frame, the worst
//!   single tick, and the end-state hash (identical across policies or the knob is broken).
//! * `observer`: what member 2 sees of member 1's hand and the balls it pushes, with the hold
//!   and dead-reckoning hand rules, raw and with render-only correction smoothing.

use std::collections::BTreeMap;

use pe_session_sim::*;
use pe_session::session::*;

#[derive(Clone)]
pub struct PitRun {
    pub balls: usize,
    pub hz: u64,
    /// 2, or 3 with member 3 an observer whose acks are dropped during `stall` (us).
    pub members: u32,
    pub stall: Option<(u64, u64)>,
    pub policy: SnapPolicy,
    /// Move member 2's knob through these policies, one step every `switch_every` frames.
    pub switch: Vec<SnapPolicy>,
    pub switch_every: u64,
    pub hand_predict: u64,
    pub delay_ms: u64,
    pub jitter_ms: u64,
    pub loss: u32,
    pub track: bool,
}

impl Default for PitRun {
    fn default() -> Self {
        PitRun {
            balls: 500,
            hz: 60,
            members: 2,
            stall: None,
            policy: SnapPolicy::Every(4),
            switch: Vec::new(),
            switch_every: 7,
            hand_predict: 0,
            delay_ms: 60,
            jitter_ms: 10,
            loss: 2,
            track: false,
        }
    }
}

/// Member 1's hand: descend over 0.33 s, sweep for 3 s in a sine across the pit, rise, park
/// (the 3d0f98a ball-pit path).
pub(crate) fn sweep(hz: u64, obj: u32) -> BTreeMap<u64, Vec<Action>> {
    let f = |s: f64| (s * hz as f64).round() as u64;
    let (s0, s1, s2, s3) = (f(2.0), f(2.33), f(5.33), f(5.66));
    let mut m: BTreeMap<u64, Vec<Action>> = BTreeMap::new();
    for fr in s0..=s3 {
        let (x, z) = if fr < s1 {
            (-0.9, -0.6)
        } else if fr <= s2 {
            let u = (fr - s1) as f64 / (s2 - s1) as f64;
            (-0.9 + 1.8 * u, 0.6 * (std::f64::consts::TAU * 1.5 * u).sin())
        } else {
            (0.9, 0.6 * (std::f64::consts::TAU * 1.5).sin())
        };
        let y = if fr < s1 {
            1.2 - (fr - s0) as f64 / (s1 - s0) as f64
        } else if fr <= s2 {
            0.2
        } else {
            0.2 + (fr - s2) as f64 / (s3 - s2) as f64
        };
        let mm = [(x * 1000.0).round() as i32, (y * 1000.0).round() as i32, (z * 1000.0).round() as i32];
        m.entry(fr).or_default().push(Action::Hand { obj, target_mm: mm, active: true, ahead: 0 });
    }
    m.entry(s3 + 1).or_default().push(Action::Hand { obj, target_mm: [0, 1500, 0], active: false, ahead: 0 });
    m
}

#[derive(Default)]
pub struct PitOut {
    pub frames: u64,
    pub secs: f64,
    pub ring_mean: f64,
    pub ring_max: u64,
    pub kf_mean: f64,
    pub kf_max: usize,
    pub image_bytes: u64,
    pub cpu_per_frame_ms: f64,
    pub save_per_frame_ms: f64,
    pub restore_per_frame_ms: f64,
    pub saves_per_s: f64,
    pub tick_p99_ms: f64,
    pub tick_max_ms: f64,
    pub rollbacks_per_s: f64,
    pub resim_frames_per_s: f64,
    pub max_depth: u64,
    pub max_lag: u64,
    /// Mean over sampled frames of (full, delta-vs-base, delta-vs-previous) ring bytes.
    pub delta: (f64, f64, f64),
    pub end_hash: u64,
    pub end_frame: u64,
    pub equal: bool,
    /// Observer series on member 2: frame -> positions shown, and corrections seen next tick.
    pub shown: BTreeMap<u64, Vec<[f32; 3]>>,
    pub corrected: BTreeMap<u64, Vec<[f32; 3]>>,
    pub truth: BTreeMap<u64, Vec<[f32; 3]>>,
}

pub fn run_pit(c: &PitRun) -> PitOut {
    let dt = 1.0 / c.hz as f32;
    let cfg = SimConfig { dt, window: 2 * c.hz, snap: c.policy, hand_predict: c.hand_predict, ..SimConfig::default() };
    let scene = SceneKind::BallPit { balls: c.balls, hands: 2 };
    let ids: Vec<MemberId> = (1..=c.members).collect();
    let hand = c.balls as u32;
    let peers: Vec<Peer> = ids
        .iter()
        .map(|&id| {
            let skew = match id {
                1 => 1000,
                2 => -1000,
                _ => 0,
            };
            let mut p = Peer::new(id, skew);
            let pl = if id == 1 { sweep(c.hz, hand) } else { BTreeMap::new() };
            p.join(1, &ids, scene, cfg, 100_000, pl);
            p
        })
        .collect();
    let mut net = Net::new(Link { delay_us: c.delay_ms * 1000, jitter_us: c.jitter_ms * 1000, loss_pct: c.loss }, 17);
    if let Some((a, b)) = c.stall {
        net.filter = Some(Box::new(move |now, _w, from, _to, m| {
            if from == 3 && matches!(m, Msg::Entry(e) if matches!(e.payload, Payload::Ack { .. })) && now >= a && now < b {
                Fate::Drop
            } else {
                Fate::Deliver
            }
        }));
    }
    let end_us = 9_000_000;
    let mut sim = Sim::new(net, peers, end_us);
    sim.peers[1].worlds.get_mut(&1).unwrap().sim.track = c.track;
    let (w0, w1) = (100_000 + 2_000_000, 100_000 + 5_660_000);
    let mut out = PitOut::default();
    let (mut ring_sum, mut kf_sum, mut samples) = (0u64, 0usize, 0u64);
    let mut ticks: Vec<u64> = Vec::new();
    let mut prev_adv = 0u64;
    let mut s_before = Stats::default();
    let mut last_frame = u64::MAX;
    let mut last_disp: Option<u64> = None;
    let mut switch_i = 0usize;
    let (mut dsum, mut dn) = ((0u64, 0u64, 0u64), 0u64);
    while sim.net.now_us <= sim.end_us {
        sim.step_ms();
        let now = sim.net.now_us;
        let r = sim.peers[1].worlds.get_mut(&1).unwrap();
        if !c.switch.is_empty() && r.sim.frame / c.switch_every != switch_i as u64 {
            switch_i = (r.sim.frame / c.switch_every) as usize;
            let p = c.switch[switch_i % c.switch.len()];
            r.sim.set_snap(p);
        }
        let s = r.sim.stats;
        let in_window = (w0..w1).contains(&now);
        if now == w0 {
            s_before = s;
        }
        if now == w1 {
            let secs = (w1 - w0) as f64 / 1e6;
            out.secs = secs;
            let d = |a: u64, b: u64| (a - b) as f64;
            out.rollbacks_per_s = d(s.rollbacks, s_before.rollbacks) / secs;
            out.resim_frames_per_s = d(s.resim_frames, s_before.resim_frames) / secs;
            out.saves_per_s = d(s.saves, s_before.saves) / secs;
            out.max_depth = s.max_depth;
            let frames = out.frames.max(1) as f64;
            out.cpu_per_frame_ms = d(s.advance_us, s_before.advance_us) / 1000.0 / frames;
            out.save_per_frame_ms = d(s.save_us, s_before.save_us) / 1000.0 / frames;
            out.restore_per_frame_ms = d(s.restore_us, s_before.restore_us) / 1000.0 / frames;
        }
        if in_window && s.advance_us > prev_adv {
            ticks.push(s.advance_us - prev_adv);
        }
        prev_adv = s.advance_us;
        if r.sim.frame != last_frame && r.sim.caught_up() {
            last_frame = r.sim.frame;
            if in_window {
                out.frames += 1;
                let b = r.sim.ring_bytes();
                ring_sum += b;
                out.ring_max = out.ring_max.max(b);
                kf_sum += r.sim.ring_len();
                out.kf_max = out.kf_max.max(r.sim.ring_len());
                samples += 1;
                out.max_lag = out.max_lag.max(r.sim.frame.saturating_sub(r.sim.conf));
                if out.frames % 4 == 0 {
                    let (f, b, p) = r.sim.ring_delta_bytes();
                    dsum = (dsum.0 + f, dsum.1 + b, dsum.2 + p);
                    dn += 1;
                }
            }
            if c.track && r.sim.frame > 0 {
                let disp = r.sim.frame - 1;
                if let Some(p) = last_disp {
                    if let Some(v) = r.sim.pos_hist.get(&p) {
                        out.corrected.insert(p, v.clone());
                    }
                }
                if let Some(v) = r.sim.pos_hist.get(&disp) {
                    out.shown.insert(disp, v.clone());
                }
                last_disp = Some(disp);
            }
        }
        sim.net.now_us += 1000;
    }
    out.ring_mean = ring_sum as f64 / samples.max(1) as f64;
    let dn = dn.max(1) as f64;
    out.delta = (dsum.0 as f64 / dn, dsum.1 as f64 / dn, dsum.2 as f64 / dn);
    out.kf_mean = kf_sum as f64 / samples.max(1) as f64;
    ticks.sort_unstable();
    if !ticks.is_empty() {
        out.tick_p99_ms = ticks[(ticks.len() * 99 / 100).min(ticks.len() - 1)] as f64 / 1000.0;
        out.tick_max_ms = *ticks.last().unwrap() as f64 / 1000.0;
    }
    let r2 = &sim.peers[1].worlds[&1].sim;
    out.image_bytes = r2.ring_bytes() / r2.ring_len().max(1) as u64;
    let last = sim.peers.iter().map(|p| p.worlds[&1].sim.frame).min().unwrap() - 1;
    out.end_frame = last;
    out.end_hash = r2.hashes[&last];
    out.equal = sim.peers.iter().all(|p| p.worlds[&1].sim.hashes.get(&last) == Some(&out.end_hash));
    if c.track {
        out.truth = r2.pos_hist.clone();
    }
    out
}

fn mb(b: f64) -> f64 {
    b / 1_048_576.0
}

/// The knob sweep: one row per policy, then one run that moves the knob while rollbacks are
/// pending. All end hashes must be identical.
pub fn snap(balls: usize, stall: bool, reps: usize) {
    let policies = [
        SnapPolicy::Every(1),
        SnapPolicy::Every(2),
        SnapPolicy::Every(4),
        SnapPolicy::Every(8),
        SnapPolicy::Every(16),
        SnapPolicy::Log(2),
        SnapPolicy::Log(4),
        SnapPolicy::Log(8),
        SnapPolicy::Log(16),
        SnapPolicy::Base(1),
        SnapPolicy::Base(4),
        SnapPolicy::Window(4),
        SnapPolicy::Window(8),
    ];
    let base = PitRun {
        balls,
        members: if stall { 3 } else { 2 },
        stall: if stall { Some((100_000 + 3_000_000, 100_000 + 4_600_000)) } else { None },
        ..PitRun::default()
    };
    println!(
        "RESULT snap {balls} balls, {} members{}, 60 Hz, 60+-10 ms, 2% loss; member 2 observes member 1's hand; window = the 3.66 s sweep; timings = fastest of {reps}",
        base.members,
        if stall { ", member 3's acks dropped 3.0-4.6 s (confirmation stalls)" } else { "" }
    );
    println!("RESULT   policy     | image MB held mean / max | images mean / max | CPU ms per frame (save / restore) | saves/s | worst tick ms (p99) | rollbacks/s | re-sim frames/s | max lag | end hash");
    let mut hashes = Vec::new();
    let mut depth = 0;
    let only: Vec<String> = std::env::var("PE_ONLY").map(|v| v.split(',').map(String::from).collect()).unwrap_or_default();
    let mut runs: Vec<(String, PitRun)> = policies
        .iter()
        .filter(|p| only.is_empty() || only.contains(&p.label()))
        .map(|p| (p.label(), PitRun { policy: *p, ..base.clone() }))
        .collect();
    runs.push((
        "knob-moving".into(),
        PitRun {
            policy: SnapPolicy::Every(1),
            switch: vec![SnapPolicy::Every(1), SnapPolicy::Base(4), SnapPolicy::Log(4), SnapPolicy::Window(4), SnapPolicy::Every(8), SnapPolicy::Base(1)],
            switch_every: 7,
            ..base.clone()
        },
    ));
    for (label, run) in runs {
        // The sim is deterministic; only timings vary between repetitions: keep the fastest.
        let o = (0..reps.max(1)).map(|_| run_pit(&run)).min_by(|a, b| a.cpu_per_frame_ms.total_cmp(&b.cpu_per_frame_ms)).unwrap();
        println!(
            "RESULT   {label:<10} | {:6.2} / {:6.2} | {:5.1} / {:3} | {:6.2} ({:5.2} / {:5.2}) | {:6.1} | {:6.2} ({:5.2}) | {:5.1} | {:6.1} | {:3} | {:016x} {}",
            mb(o.ring_mean),
            mb(o.ring_max as f64),
            o.kf_mean,
            o.kf_max,
            o.cpu_per_frame_ms,
            o.save_per_frame_ms,
            o.restore_per_frame_ms,
            o.saves_per_s,
            o.tick_max_ms,
            o.tick_p99_ms,
            o.rollbacks_per_s,
            o.resim_frames_per_s,
            o.max_lag,
            o.end_hash,
            if o.equal { "all members equal" } else { "MEMBERS DIFFER" }
        );
        if o.delta.0 > 0.0 {
            println!(
                "RESULT     {label}: ring as base + 64 B block deltas: full {:.2} MB -> vs base {:.2} MB ({:.0}%), vs previous {:.2} MB ({:.0}%)",
                mb(o.delta.0),
                mb(o.delta.1),
                100.0 * o.delta.1 / o.delta.0,
                mb(o.delta.2),
                100.0 * o.delta.2 / o.delta.0
            );
        }
        hashes.push(o.end_hash);
        depth = depth.max(o.max_depth);
    }
    let same = hashes.windows(2).all(|w| w[0] == w[1]);
    println!("RESULT   end hash identical across all {} runs: {}; deepest rollback {} frames", hashes.len(), if same { "YES" } else { "NO" }, depth);
}

fn dist(a: &[f32; 3], b: &[f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn pct(v: &mut [f32], p: usize) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[(v.len() * p / 100).min(v.len() - 1)]
}

/// What member 2 sees of member 1's hand and the balls it moves.
pub fn observer(delay_ms: u64, hz: u64) {
    let jitter = (delay_ms / 6).max(1);
    println!(
        "RESULT observer: 500 balls, {hz} Hz, one-way {delay_ms}+-{jitter} ms, 2% loss; member 1 sweeps its hand, member 2 watches; errors vs the final (confirmed) history, sweep window"
    );
    for predict in [0u64, 6] {
        let run = PitRun { hz, hand_predict: predict, track: true, delay_ms, jitter_ms: jitter, ..PitRun::default() };
        let o = run_pit(&run);
        let f = |s: f64| (s * hz as f64).round() as u64;
        let (a, b) = (f(2.0), f(5.66));
        // Hand statistics over the sweep proper: the scripted path jumps at its start and end.
        let (ha, hb) = (f(2.33) + 1, f(5.33));
        let hand = 500usize;
        let dtf = 1.0 / hz as f32;
        let mut hand_err = Vec::new();
        // Review PH-6: the error right after the two changes of motion, where dead reckoning
        // overshoots: 0.25 s after the descent ends, and 0.25 s after the sweep ends.
        let turns = [(f(2.33), f(2.58)), (f(5.33), f(5.58))];
        let mut turn_err = Vec::new();
        let mut pops = Vec::new();
        let mut frame_pop_max = Vec::new();
        let mut visible_pops = 0u64;
        let mut raw_err = Vec::new();
        let taus = [0.05f32, 0.1];
        let mut sm_err: Vec<Vec<f32>> = vec![Vec::new(); taus.len()];
        let mut sm_speed: Vec<f32> = vec![0.0; taus.len()];
        let mut offs: Vec<Vec<[f32; 3]>> = vec![vec![[0.0; 3]; 501]; taus.len()];
        let mut prev: Option<u64> = None;
        let mut frames = 0u64;
        for (&n, shown) in o.shown.range(a..b) {
            let Some(truth) = o.truth.get(&n) else { continue };
            frames += 1;
            if (ha..hb).contains(&n) {
                hand_err.push(dist(&shown[hand], &truth[hand]) * 1000.0);
            }
            if turns.iter().any(|(x, y)| (*x..*y).contains(&n)) {
                turn_err.push(dist(&shown[hand], &truth[hand]) * 1000.0);
            }
            // The correction this tick applied to the frame shown last tick.
            let mut fmax = 0.0f32;
            let mut d = vec![[0.0f32; 3]; 501];
            if let (Some(p), Some(c)) = (prev, prev.and_then(|p| o.corrected.get(&p))) {
                let sp = &o.shown[&p];
                for i in 0..500 {
                    let di = [c[i][0] - sp[i][0], c[i][1] - sp[i][1], c[i][2] - sp[i][2]];
                    let m = (di[0] * di[0] + di[1] * di[1] + di[2] * di[2]).sqrt() * 1000.0;
                    if m > 0.0 {
                        pops.push(m);
                    }
                    if m > 1.0 {
                        visible_pops += 1;
                    }
                    fmax = fmax.max(m);
                    d[i] = di;
                }
            }
            frame_pop_max.push(fmax);
            let steps = prev.map_or(1, |p| n - p) as i32;
            for (k, tau) in taus.iter().enumerate() {
                let decay = (-dtf / tau).exp().powi(steps);
                for i in 0..500 {
                    // Keep last tick's rendered position, then drain the offset: the eye sees
                    // the correction only as this drain speed.
                    let pre = [offs[k][i][0] - d[i][0], offs[k][i][1] - d[i][1], offs[k][i][2] - d[i][2]];
                    offs[k][i] = [pre[0] * decay, pre[1] * decay, pre[2] * decay];
                    let r = [shown[i][0] + offs[k][i][0], shown[i][1] + offs[k][i][1], shown[i][2] + offs[k][i][2]];
                    sm_err[k].push(dist(&r, &truth[i]) * 1000.0);
                    sm_speed[k] = sm_speed[k].max(dist(&pre, &offs[k][i]) / (dtf * steps as f32));
                }
            }
            for i in 0..500 {
                raw_err.push(dist(&shown[i], &truth[i]) * 1000.0);
            }
            prev = Some(n);
        }
        let secs = frames as f64 / hz as f64;
        let wrong = raw_err.iter().filter(|e| **e > 1.0).count() as f64 / frames.max(1) as f64;
        println!(
            "RESULT   hand rule {}: remote hand shown vs true during the sweep proper: mean {:.1} mm, p95 {:.1}, max {:.1}",
            if predict == 0 { "HOLD (3d0f98a)" } else { "DEAD-RECKON <=6 frames, <=3 m/s" },
            hand_err.iter().sum::<f32>() / hand_err.len().max(1) as f32,
            pct(&mut hand_err.clone(), 95),
            pct(&mut hand_err, 100)
        );
        println!(
            "RESULT     remote hand in the 0.25 s after each change of motion (review PH-6): mean {:.1} mm, p95 {:.1}, max {:.1}",
            turn_err.iter().sum::<f32>() / turn_err.len().max(1) as f32,
            pct(&mut turn_err.clone(), 95),
            pct(&mut turn_err, 100)
        );
        println!(
            "RESULT     balls: corrections >1 mm {:.0}/s ({:.1} balls/frame), per-frame worst correction p50 {:.1} mm p99 {:.1} max {:.1}; balls off by >1 mm vs truth {:.1}/frame",
            visible_pops as f64 / secs,
            visible_pops as f64 / frames.max(1) as f64,
            pct(&mut frame_pop_max.clone(), 50),
            pct(&mut frame_pop_max.clone(), 99),
            pct(&mut frame_pop_max, 100),
            wrong
        );
        println!(
            "RESULT     ball error vs truth (all balls x frames) raw: p99 {:.1} mm, max {:.1}; smoothed tau 50 ms: p99 {:.1} max {:.1} (max drain {:.2} m/s); tau 100 ms: p99 {:.1} max {:.1} (max drain {:.2} m/s)",
            pct(&mut raw_err.clone(), 99),
            pct(&mut raw_err, 100),
            pct(&mut sm_err[0].clone(), 99),
            pct(&mut sm_err[0], 100),
            sm_speed[0],
            pct(&mut sm_err[1].clone(), 99),
            pct(&mut sm_err[1], 100),
            sm_speed[1]
        );
        println!(
            "RESULT     rollbacks {:.1}/s, sim CPU {:.2} ms per frame (step + re-sim + snapshots), worst tick {:.2} ms; end {:016x} {}",
            o.rollbacks_per_s,
            o.cpu_per_frame_ms,
            o.tick_max_ms,
            o.end_hash,
            if o.equal { "members equal" } else { "MEMBERS DIFFER" }
        );
    }
}

//! Quantities derived from the recorded displays: per-ball divergence from the engager at the
//! same instant, visual snaps, hand lag behind the controller, and convergence time.

use pe_session_sim::ballpit_target_mm;

use crate::record::*;

/// A ball "snaps" in a displayed frame when its drawn displacement differs from what its own
/// drawn velocity explains over the sim frames that state advanced: |dx - v*dt*dframes| > SNAP_M.
/// Physically continuous motion (including collisions, since Box3D integrates position with the
/// new velocity) stays well under it; a rollback correction or a late state update does not. A
/// tick whose state did not advance (dframes = 0: a stalled sim, or a naive observer with no new
/// update for that ball) is a snap only if the drawn position changed anyway; a moving ball that
/// stays put is counted as *frozen* instead (see FROZEN_V). Calibrated against the engager of
/// the pose-source cells, which never rolls back (see main.rs). The 1 cm threshold equals a
/// 0.6 m/s velocity error at 60 Hz: fitted to this pit and gesture, not a general constant.
pub const SNAP_M: f32 = 0.01;
/// A ball is frozen in a tick when its drawn state did not advance although its drawn velocity
/// would move it more than 1 mm per frame.
pub const FROZEN_V: f32 = 0.06;
/// Divergence threshold for "this ball is visibly somewhere else".
pub const DIV_M: f32 = 0.01;

#[derive(Clone, Copy, Debug, Default)]
pub struct Derived {
    /// vs the engager's display at the same tick (balls only)
    pub div_max_m: f32,
    pub div_mean_m: f32,
    pub div_count: u32,
    /// Engager hand vs observer hand, same tick.
    pub hand_div_m: f32,
    /// Balls snapping this frame, and snaps per second over the last second.
    pub snaps: u32,
    pub snaps_s: f64,
    /// Balls frozen this frame (state did not advance while moving).
    pub frozen: u32,
}

pub struct Analysis {
    pub eng: Vec<Derived>,
    pub obs: Vec<Derived>,
    /// Per-ball snap magnitude per frame (for tinting): [frame][ball].
    pub eng_snap: Vec<Vec<f32>>,
    pub obs_snap: Vec<Vec<f32>>,
    /// Per-ball frozen flag per frame: [frame][ball].
    pub eng_frozen: Vec<Vec<bool>>,
    pub obs_frozen: Vec<Vec<bool>>,
    pub summary: Summary,
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub eng_hand_lag_frames: f64,
    pub obs_hand_lag_frames: f64,
    pub max_div_m: f32,
    pub max_div_count: u32,
    pub sweep_mean_div_count: f64,
    /// Seconds after the gesture ends (5.76 s) until no ball differs by > DIV_M for the rest of
    /// the recording; None = never within the recording.
    pub converge_s: Option<f64>,
    pub final_div_count: u32,
    pub final_div_max_m: f32,
    pub eng_snaps: u64,
    pub obs_snaps: u64,
    pub eng_frozen: u64,
    pub obs_frozen: u64,
    pub sweep_tx_eng_bps: f64,
    pub sweep_tx_obs_bps: f64,
    pub sweep_rollbacks_s: f64,
    pub sweep_resim_ms: f64,
    pub max_queue_ms: f64,
    pub cap_drops: u64,
    pub max_snapshot_bytes: u64,
    pub eng_rollbacks_s: f64,
    pub stalled_frames: u64,
}

fn d3(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// Per frame and ball: the snap magnitude |dx - v*dt*dframes| and whether the ball is frozen.
fn snaps(shots: &[Shot]) -> (Vec<Vec<f32>>, Vec<Vec<bool>>) {
    let dt = 1.0 / HZ as f32;
    shots
        .iter()
        .enumerate()
        .map(|(n, s)| {
            if n == 0 {
                return (vec![0.0; BALLS], vec![false; BALLS]);
            }
            let p = &shots[n - 1];
            (0..BALLS)
                .map(|i| {
                    let v = s.vel[i];
                    let df = s.frames[i].saturating_sub(p.frames[i]) as f32;
                    let pred = [p.pos[i][0] + v[0] * dt * df, p.pos[i][1] + v[1] * dt * df, p.pos[i][2] + v[2] * dt * df];
                    let speed = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
                    let frozen = df == 0.0 && speed > FROZEN_V && d3(s.pos[i], p.pos[i]) == 0.0;
                    (d3(s.pos[i], pred), frozen)
                })
                .unzip()
        })
        .unzip()
}

/// Display ticks between the controller (scripted target at tick k) and where the hand is drawn:
/// the d that best matches hand(k) ~ target(k - d), averaged over the sweep.
fn hand_lag(shots: &[Shot]) -> f64 {
    let mut tot = 0.0f64;
    let mut n = 0.0f64;
    for s in shots {
        let k = s.k;
        if !(145..=315).contains(&k) {
            continue; // the steady sweep only
        }
        let h = s.pos[ENGAGER_HAND];
        let mut best = (f32::MAX, 0u64);
        for d in 0..40u64 {
            if let Some(t) = ballpit_target_mm(HZ, 1, k - d) {
                let t = [t[0] as f32 / 1000.0, t[1] as f32 / 1000.0, t[2] as f32 / 1000.0];
                let e = d3(h, t);
                if e < best.0 {
                    best = (e, d);
                }
            }
        }
        tot += best.1 as f64;
        n += 1.0;
    }
    tot / n.max(1.0)
}

pub fn analyze(r: &Recording) -> Analysis {
    let n = r.eng.len().min(r.obs.len());
    let (eng_snap, eng_frozen) = snaps(&r.eng);
    let (obs_snap, obs_frozen) = snaps(&r.obs);
    let mut eng = Vec::with_capacity(n);
    let mut obs = Vec::with_capacity(n);
    for f in 0..n {
        let (e, o) = (&r.eng[f], &r.obs[f]);
        assert_eq!(e.k, o.k, "display ticks aligned");
        let mut d = Derived::default();
        let mut sum = 0.0;
        for i in 0..BALLS {
            let x = d3(e.pos[i], o.pos[i]);
            d.div_max_m = d.div_max_m.max(x);
            sum += x;
            if x > DIV_M {
                d.div_count += 1;
            }
        }
        d.div_mean_m = sum / BALLS as f32;
        d.hand_div_m = d3(e.pos[ENGAGER_HAND], o.pos[ENGAGER_HAND]);
        let mut de = Derived { div_max_m: 0.0, ..Default::default() };
        de.snaps = eng_snap[f].iter().filter(|&&x| x > SNAP_M).count() as u32;
        d.snaps = obs_snap[f].iter().filter(|&&x| x > SNAP_M).count() as u32;
        de.frozen = eng_frozen[f].iter().filter(|&&x| x).count() as u32;
        d.frozen = obs_frozen[f].iter().filter(|&&x| x).count() as u32;
        eng.push(de);
        obs.push(d);
    }
    for v in [&mut eng, &mut obs] {
        for f in 0..n {
            let lo = f.saturating_sub(HZ as usize - 1);
            let s: u32 = v[lo..=f].iter().map(|d| d.snaps).sum();
            v[f].snaps_s = s as f64 * HZ as f64 / (f - lo + 1) as f64;
        }
    }
    // Summary.
    let sweep = |k: u64| (120..=340).contains(&k); // wall 2.1 s .. 5.77 s
    let gesture_end_k = 340u64;
    let mut sm = Summary { eng_hand_lag_frames: hand_lag(&r.eng), obs_hand_lag_frames: hand_lag(&r.obs), ..Default::default() };
    let mut last_bad: Option<u64> = None;
    let (mut ns, mut dc, mut txe, mut txo, mut rb, mut rs, mut erb) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for f in 0..n {
        let (e, o, d) = (&r.eng[f], &r.obs[f], &obs[f]);
        sm.max_div_m = sm.max_div_m.max(d.div_max_m);
        sm.max_div_count = sm.max_div_count.max(d.div_count);
        if d.div_count > 0 {
            last_bad = Some(e.k);
        }
        sm.eng_snaps += eng[f].snaps as u64;
        sm.obs_snaps += d.snaps as u64;
        sm.eng_frozen += eng[f].frozen as u64;
        sm.obs_frozen += d.frozen as u64;
        sm.max_queue_ms = sm.max_queue_ms.max(e.m.queue_ms).max(o.m.queue_ms);
        sm.cap_drops = sm.cap_drops.max(e.m.cap_drops + o.m.cap_drops);
        sm.max_snapshot_bytes = sm.max_snapshot_bytes.max(e.m.snapshot_bytes).max(o.m.snapshot_bytes);
        if o.m.stalled && o.k > 100 {
            sm.stalled_frames += 1;
        }
        if sweep(e.k) {
            ns += 1.0;
            dc += d.div_count as f64;
            txe += e.m.tx_bps;
            txo += o.m.tx_bps;
            rb += o.m.rollbacks_s;
            rs += o.m.resim_ms;
            erb += e.m.rollbacks_s;
        }
    }
    let ns = f64::max(ns, 1.0);
    sm.sweep_mean_div_count = dc / ns;
    sm.sweep_tx_eng_bps = txe / ns;
    sm.sweep_tx_obs_bps = txo / ns;
    sm.sweep_rollbacks_s = rb / ns;
    sm.sweep_resim_ms = rs / ns;
    sm.eng_rollbacks_s = erb / ns;
    let last = &obs[n - 1];
    sm.final_div_count = last.div_count;
    sm.final_div_max_m = last.div_max_m;
    sm.converge_s = match last_bad {
        None => Some(0.0),
        Some(k) if k + 1 < r.eng[n - 1].k => Some(((k + 1) as f64 - gesture_end_k as f64).max(0.0) / HZ as f64),
        Some(_) => None,
    };
    Analysis { eng, obs, eng_snap, obs_snap, eng_frozen, obs_frozen, summary: sm }
}

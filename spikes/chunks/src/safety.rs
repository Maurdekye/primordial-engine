//! Review N1: is the neighbour bound *safe* when footprints change in steps?
//!
//! A frame-level model of two regions on a line [C: it simulates the rules, not the protocol]. Region `R`
//! is fixed; region `q`'s footprint edge facing `R` moves by events (chunk steps, scale-ups, joiners at
//! the edge, region creation). `R` simulates as far as the neighbour bound allows, from what it knows of
//! `q`'s confirmed log. A **late merge** is a merge frame that `R` had already simulated past when it
//! first learned the event causing it: the merge would need a rollback, possibly below `R`'s confirmed
//! frame, which design 6.4 forbids. The revised rules must give 0 late merges.
//!
//! * `2a228c6`: an anchor takes effect at `stamp + Lambda`; `R` uses `q`'s footprint *in effect* at its
//!   known confirmed frame; region creation is announced to nobody.
//! * `revised`:
//!   - **growth clock:** any growth of `q`'s footprint by `J` metres takes effect at
//!     `max(stamp + Lambda, previous growth's effect frame) + ceil(60 J / v_max)`, so outsiders never see
//!     it grow faster than `v_max`;
//!   - **envelope:** `R` uses every anchor *stamped* up to `q`'s known confirmed frame, including growth
//!     not yet in effect;
//!   - **creation:** a new region is announced in the world log, may only be created at least
//!     `D_merge + v_max * S_create` from every region in the confirmed directory (otherwise the newcomer
//!     joins the nearest region), and every region simulates `f` only while it holds the directory
//!     confirmed through `f - S_create`.
//!
//! Parameters: region confirmation lag 9 frames (8.5, ed347ac 6.4), 3 frames to learn a neighbour's
//! confirmed frame, world-log lag 66 frames, Lambda 30 frames, v_max 100 m/s, D_merge 3 km,
//! S_create 10 s, eviction 132 frames.

const LAG_R: i64 = 9;
const INFO: i64 = 3;
const LAG_W: i64 = 66;
const LAMBDA: i64 = 30;
const V: f64 = 100.0;
const D: f64 = 3000.0;
const S_CREATE: i64 = 600;
const EVICT: i64 = 132;
const T: i64 = 30_000;

#[derive(Clone, Copy)]
enum Ev {
    /// q's edge moves toward R by this many metres.
    Grow(f64),
    /// q comes into existence with its edge this far from R's edge.
    Create(f64),
}

struct Case {
    name: &'static str,
    gap0: f64,
    events: Vec<(i64, Ev)>,
    /// (start, frames): q's confirmation stalls (one member silent until evicted).
    q_stall: Option<(i64, i64)>,
    /// R refreshes q's confirmed prefix only every this many frames, plus on demand if `on_demand`.
    refresh: Option<i64>,
    on_demand: bool,
    /// (start, frames): the world log stops confirming.
    world_stall: Option<(i64, i64)>,
}

struct Out {
    late: bool,
    refused: bool,
    f_m: Option<i64>,
    learn: Option<i64>,
    sim_at_learn: i64,
    r_lost_frames: i64,
}

fn run(c: &Case, revised: bool) -> Out {
    // Effect frames of each event under the rule.
    let mut clock = i64::MIN;
    let mut eff: Vec<(i64, i64, Ev)> = Vec::new(); // (stamp, effect, ev)
    let mut refused = false;
    for &(stamp, ev) in &c.events {
        match ev {
            Ev::Grow(j) => {
                let e = if revised {
                    let e = (stamp + LAMBDA).max(clock) + (60.0 * j / V).ceil() as i64;
                    clock = e;
                    e
                } else {
                    stamp + LAMBDA
                };
                eff.push((stamp, e, ev));
            }
            Ev::Create(g) => {
                if revised && g < D + V * S_CREATE as f64 / 60.0 {
                    refused = true; // must join R instead: no separate region
                    continue;
                }
                eff.push((stamp, stamp + LAMBDA, ev));
            }
        }
    }
    let created = c.events.iter().any(|e| matches!(e.1, Ev::Create(_)));
    // q's true gap to R at frame f (None: q does not exist).
    let gap_true = |f: i64| -> Option<f64> {
        let mut g = if created { None } else { Some(c.gap0) };
        for &(_, e, ev) in &eff {
            if e <= f {
                match ev {
                    Ev::Grow(j) => g = g.map(|x| x - j),
                    Ev::Create(x) => g = Some(x),
                }
            }
        }
        g
    };
    let f_m = (0..T).find(|&f| gap_true(f).is_some_and(|g| g < D));
    // The event that makes the gap drop below D_merge.
    let cause = f_m.map(|fm| eff.iter().filter(|e| e.1 <= fm).map(|e| e.0).max().unwrap());
    // q's confirmed frame as R could know it at present t (before R's pull policy).
    let live_conf_q = |t: i64| -> i64 {
        let tt = t - INFO;
        if let Some((s, len)) = c.q_stall {
            if tt >= s && tt < s + len.min(EVICT) {
                return s - LAG_R;
            }
        }
        tt - LAG_R
    };
    let world_conf = |t: i64| -> i64 {
        let mut tt = t;
        if let Some((s, len)) = c.world_stall {
            if t >= s && t < s + len {
                tt = s;
            }
        }
        tt - LAG_W
    };
    let creation = eff.iter().find(|e| matches!(e.2, Ev::Create(_))).map(|e| e.0);
    // Whether R knows q exists at present t (always, unless q is created during the run).
    let exists_known = |t: i64| -> bool {
        match creation {
            None => !created,
            Some(s0) => revised && world_conf(t) >= s0,
        }
    };
    // What R believes q's gap is, at present t, holding q's confirmed prefix up to frame k.
    let gap_known = |t: i64, k: i64| -> Option<f64> {
        if !exists_known(t) {
            return None;
        }
        let mut g = c.gap0;
        for &(stamp, e, ev) in &eff {
            match ev {
                Ev::Create(x) => g = x,
                Ev::Grow(j) => {
                    let counted = if revised { stamp <= k } else { e <= k };
                    if counted {
                        g -= j;
                    }
                }
            }
        }
        Some(g)
    };
    let learns = |t: i64, k: i64| -> bool {
        match cause {
            None => false,
            Some(cs) => exists_known(t) && (Some(cs) == creation || k >= cs),
        }
    };
    let mut sim: i64 = 0;
    let mut lost = 0;
    let mut learn = None;
    let mut sim_at_learn = 0;
    // R pulls q's confirmed prefix every `refresh` frames, and early whenever its bound comes within
    // 2 s of its present (pull on demand).
    let mut k = live_conf_q(0);
    let mut last_pull = 0;
    let mut bound_prev = i64::MAX;
    for t in 0..T {
        let due = match c.refresh {
            None => true,
            Some(r) => t - last_pull >= r || (c.on_demand && bound_prev - t < 120),
        };
        if due {
            k = live_conf_q(t);
            last_pull = t;
        }
        if learn.is_none() && learns(t, k) {
            learn = Some(t);
            sim_at_learn = sim;
        }
        let mut bound = t;
        if let Some(g) = gap_known(t, k) {
            let slack = ((g - D) / V * 60.0).floor() as i64;
            bound = bound.min(k + slack.max(0));
        }
        if revised {
            bound = bound.min(world_conf(t) + S_CREATE);
        }
        bound_prev = bound;
        // Once the merge is known, both regions handle it at f_m; R proceeds as the merged region.
        if learn.is_some() {
            bound = t;
        }
        let next = sim.max(bound);
        if next < t && learn.is_none() {
            lost += 1;
        }
        sim = next;
    }
    let late = match (f_m, learn) {
        (Some(fm), Some(_)) => sim_at_learn >= fm,
        (Some(_), None) => true, // R never learns: the regions overlap
        _ => false,
    };
    Out { late, refused, f_m, learn, sim_at_learn, r_lost_frames: lost }
}

pub fn run_all() {
    println!("== safety (review N1): late merges under footprint steps ==");
    let walker: Vec<(i64, Ev)> = (0..12).map(|i| (300 + 615 * i, Ev::Grow(1024.0))).collect();
    let cases = vec![
        Case { name: "chunk step during a q stall (gap 4 km)", gap0: 4000.0, events: vec![(330, Ev::Grow(1024.0))], q_stall: Some((300, 600)), refresh: None, world_stall: None, on_demand: true },
        Case { name: "chunk step, no stall (gap 4 km)", gap0: 4000.0, events: vec![(330, Ev::Grow(1024.0))], q_stall: None, refresh: None, world_stall: None, on_demand: true },
        Case { name: "scale x8 (+14 km), pull every 60 s or on demand (gap 10 km)", gap0: 10_000.0, events: vec![(1000, Ev::Grow(14.0 * 1024.0))], q_stall: None, refresh: Some(3600), world_stall: None, on_demand: true },
        Case { name: "joiner at q's near edge (+3 km), pull every 60 s or on demand (gap 5 km)", gap0: 5000.0, events: vec![(1000, Ev::Grow(3.0 * 1024.0))], q_stall: None, refresh: Some(3600), world_stall: None, on_demand: true },
        Case { name: "scale x8 (+14 km), pull every 60 s only (gap 10 km)", gap0: 10_000.0, events: vec![(1000, Ev::Grow(14.0 * 1024.0))], q_stall: None, refresh: Some(3600), world_stall: None, on_demand: false },
        Case { name: "joiner at q's near edge (+3 km), pull every 60 s only (gap 5 km)", gap0: 5000.0, events: vec![(1000, Ev::Grow(3.0 * 1024.0))], q_stall: None, refresh: Some(3600), world_stall: None, on_demand: false },
        Case { name: "walker at v_max toward R, 12 chunk steps, q stall mid-way (gap 12 km)", gap0: 12_000.0, events: walker, q_stall: Some((3000, 600)), refresh: None, world_stall: None, on_demand: true },
        Case { name: "region created 4.5 km from R, then walks toward it", gap0: 0.0, events: vec![(600, Ev::Create(4500.0)), (700, Ev::Grow(1024.0)), (1400, Ev::Grow(1024.0))], q_stall: None, refresh: None, world_stall: None, on_demand: true },
        Case { name: "region created 3.5 km from R, then walks toward it", gap0: 0.0, events: vec![(600, Ev::Create(3500.0)), (700, Ev::Grow(1024.0))], q_stall: None, refresh: None, world_stall: None, on_demand: true },
        Case { name: "region created 4.5 km from R and walks toward it, world log stalled 5 s", gap0: 0.0, events: vec![(600, Ev::Create(4500.0)), (700, Ev::Grow(1024.0)), (1400, Ev::Grow(1024.0))], q_stall: None, refresh: None, world_stall: Some((500, 300)), on_demand: true },
        Case { name: "region created 4.5 km from R and walks toward it, world log stalled 12 s", gap0: 0.0, events: vec![(600, Ev::Create(4500.0)), (700, Ev::Grow(1024.0)), (1400, Ev::Grow(1024.0))], q_stall: None, refresh: None, world_stall: Some((500, 720)), on_demand: true },
    ];
    for c in &cases {
        for (rule, revised) in [("2a228c6", false), ("revised", true)] {
            let o = run(c, revised);
            println!(
                "RESULT safety case=\"{}\" rule={rule} late_merge={} refused_as_join={} merge_frame={} R_learns_at={} R_had_simulated_to={} R_frames_lost_before_learning={}",
                c.name,
                o.late,
                o.refused,
                o.f_m.map(|f| f.to_string()).unwrap_or("-".into()),
                o.learn.map(|f| f.to_string()).unwrap_or("never".into()),
                o.sim_at_learn,
                o.r_lost_frames
            );
        }
    }
}

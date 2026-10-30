//! Reviews N1 and M1: is the neighbour bound *safe* when footprints change in steps, when both
//! regions grow toward each other, and when regions are created concurrently?
//!
//! A frame-level model of two regions on a line [C: it simulates the rules, not the protocol]. `R` holds
//! `(-inf, R_edge]` and `q` holds `[q_edge, inf)`; growth moves the facing edges. Each region simulates as
//! far as the neighbour bound allows, from what it knows of the other. A **late merge** is a merge frame
//! that a region had already simulated past when it first held every fact that fixes it: the merge would
//! need a rollback, possibly below its confirmed frame, which design 6.4 forbids. Both regions are checked.
//!
//! Rule sets:
//! * `2a228c6`: anchors take effect at `stamp + Lambda`; a neighbour is seen by its footprint *in effect*
//!   at its known confirmed frame; region creation is announced to nobody.
//! * `05d3cc0`: the growth clock (growth J takes effect at `max(stamp + Lambda, previous growth's
//!   effect) + ceil(60 J / v_max)`), envelopes from every anchor *stamped* by the known confirmed frame,
//!   region creation through the world log taking effect at `stamp + Lambda`, valid if at least
//!   `D_merge + v_max * S_create` from every region in the *confirmed* directory, and every region gated on
//!   the directory confirmed through `f - S_create`.
//! * `revised` (review M1): as 05d3cc0, but creation takes effect at `stamp + S_create`, and its validity is
//!   checked in world-log order against the directory *including earlier creations still pending*; a
//!   creation within range of one becomes a join of it.
//!
//! Parameters: region confirmation lag 9 frames, 3 frames to learn a neighbour's confirmed frame,
//! world-log lag 66 frames, Lambda 30 frames, D_merge 3 km, eviction 132 frames; v_max (100 m/s) and
//! S_create (10 s) vary by case, as they are world settings.

const LAG_R: i64 = 9;
const INFO: i64 = 3;
const LAG_W: i64 = 66;
const LAMBDA: i64 = 30;
const D: f64 = 3000.0;
const EVICT: i64 = 132;
const T: i64 = 30_000;
const CHUNK: f64 = 1024.0;
/// RegionHead cadence in the world log (25.5.8): 5 s.
const HEAD: i64 = 300;

#[derive(Clone, Copy, PartialEq)]
enum Rules {
    Old,
    V05,
    Revised,
}

#[derive(Clone)]
struct Side {
    /// Creation stamp, or None if the region exists from the start.
    created: Option<i64>,
    /// Facing edge at creation (R: its right edge; q: its left edge).
    edge0: f64,
    /// Growth toward the other region: (stamp, metres).
    growth: Vec<(i64, f64)>,
    /// (start, frames): this region's confirmation stalls (a silent member, until evicted).
    stall: Option<(i64, i64)>,
}

#[derive(Clone)]
struct Case {
    name: String,
    v: f64,
    s_create: i64,
    r: Side,
    q: Side,
    /// The other region refreshes this one's prefix every `refresh` frames, and on demand if set.
    refresh: Option<i64>,
    on_demand: bool,
    world_stall: Option<(i64, i64)>,
}

struct Out {
    late_r: bool,
    late_q: bool,
    q_is_join: bool,
    f_m: Option<i64>,
    lost_r: i64,
}

type Growth = Vec<(i64, i64, f64)>; // (stamp, effect, metres)

fn run(c: &Case, rules: Rules) -> Out {
    let world_conf = |t: i64| -> i64 {
        let mut tt = t;
        if let Some((s, len)) = c.world_stall {
            if t >= s && t < s + len {
                tt = s;
            }
        }
        tt - LAG_W
    };
    let v = c.v;
    let effects = |s: &Side| -> Growth {
        let mut clock = i64::MIN;
        s.growth
            .iter()
            .map(|&(stamp, j)| {
                let e = if rules == Rules::Old {
                    stamp + LAMBDA
                } else {
                    let e = (stamp + LAMBDA).max(clock) + (60.0 * j / v).ceil() as i64;
                    clock = e;
                    e
                };
                (stamp, e, j)
            })
            .collect()
    };
    let gr = effects(&c.r);
    let gq = effects(&c.q);
    let delay = if rules == Rules::Revised { c.s_create } else { LAMBDA };
    // Creation validity of q against R as the world-log directory shows R at q's stamp.
    let mut q_is_join = false;
    if let Some(sq) = c.q.created {
        if rules != Rules::Old {
            let r_in_directory = match c.r.created {
                None => true,
                Some(sr) => {
                    if rules == Rules::Revised {
                        sr < sq // earlier in world-log order, pending or not
                    } else {
                        world_conf(sq) >= sr // only once confirmed in the directory
                    }
                }
            };
            if r_in_directory {
                // The directory knows R through its latest RegionHead (every 5 s) confirmed in the world log.
                let k = (world_conf(sq).max(0) / HEAD) * HEAD - LAG_R;
                let r_env = c.r.edge0 + gr.iter().filter(|g| g.0 <= k).map(|g| g.2).sum::<f64>();
                let age = (sq - k).max(0) as f64 / 60.0;
                if c.q.edge0 - r_env < D + v * c.s_create as f64 / 60.0 + v * age {
                    q_is_join = true;
                }
            }
        }
    }
    let live = |s: &Side, f: i64| s.created.is_none_or(|cs| f >= cs + delay);
    let edge_r = |f: i64| c.r.edge0 + gr.iter().filter(|g| g.1 <= f).map(|g| g.2).sum::<f64>();
    let edge_q = |f: i64| c.q.edge0 - gq.iter().filter(|g| g.1 <= f).map(|g| g.2).sum::<f64>();
    let f_m = if q_is_join { None } else { (0..T).find(|&f| live(&c.r, f) && live(&c.q, f) && edge_q(f) - edge_r(f) < D) };

    // One side: X knows its own growth (its own log); it learns Y's by pulling Y's confirmed prefix.
    // `dir` is +1 when Y lies at larger coordinates than X.
    let side = |x: &Side, y: &Side, gx: &Growth, gy: &Growth, dir: f64| -> (bool, i64) {
        let live_conf_y = |t: i64| -> i64 {
            let tt = t - INFO;
            if let Some((s, len)) = y.stall {
                if tt >= s && tt < s + len.min(EVICT) {
                    return s - LAG_R;
                }
            }
            tt - LAG_R
        };
        let knows_y = |t: i64| -> bool {
            match y.created {
                None => true,
                Some(cy) => rules != Rules::Old && world_conf(t) >= cy,
            }
        };
        let (mut sim, mut k, mut last_pull, mut bound_prev) = (-1i64, live_conf_y(0), 0i64, i64::MAX);
        let (mut learned, mut late, mut lost) = (false, false, 0i64);
        for t in 0..T {
            if x.created.is_some_and(|cx| t < cx + delay) {
                continue;
            }
            let due = match c.refresh {
                None => true,
                Some(r) => t - last_pull >= r || (c.on_demand && bound_prev - t < 120),
            };
            if due {
                k = live_conf_y(t);
                last_pull = t;
            }
            if let Some(fm) = f_m {
                let facts = knows_y(t) && gy.iter().filter(|g| g.1 <= fm).all(|g| g.0 <= k);
                if !learned && facts {
                    learned = true;
                    late = sim >= fm;
                }
            }
            let mut bound = t;
            if rules != Rules::Old {
                bound = bound.min(world_conf(t) + c.s_create);
            }
            if knows_y(t) && !q_is_join {
                // Own footprint with all own stamped growth; Y's envelope from what X holds of Y.
                let own = x.edge0 + dir * gx.iter().filter(|g| g.0 <= t).map(|g| g.2).sum::<f64>();
                let counted = |g: &&(i64, i64, f64)| if rules == Rules::Old { g.1 <= k } else { g.0 <= k };
                let env_y = y.edge0 - dir * gy.iter().filter(counted).map(|g| g.2).sum::<f64>();
                let gap = (env_y - own) * dir;
                bound = bound.min(k + ((gap - D) / v * 60.0).floor().max(0.0) as i64);
            }
            bound_prev = bound;
            if learned {
                bound = t;
            }
            let next = sim.max(bound);
            if next < t && !learned {
                lost += 1;
            }
            sim = next;
        }
        if f_m.is_some() && !learned {
            late = true;
        }
        (late, lost)
    };
    let (late_r, lost_r) = side(&c.r, &c.q, &gr, &gq, 1.0);
    let late_q = if q_is_join { false } else { side(&c.q, &c.r, &gq, &gr, -1.0).0 };
    Out { late_r, late_q, q_is_join, f_m, lost_r }
}

fn fixed(edge: f64) -> Side {
    Side { created: None, edge0: edge, growth: vec![], stall: None }
}

fn walk(start: i64, every: i64, n: i64) -> Vec<(i64, f64)> {
    (0..n).map(|i| (start + every * i, CHUNK)).collect()
}

pub fn run_all() {
    println!("== safety (reviews N1, M1): late merges under footprint steps, two-sided approach, concurrent creation ==");
    let base = |name: &str| Case {
        name: name.to_string(),
        v: 100.0,
        s_create: 600,
        r: fixed(0.0),
        q: fixed(4000.0),
        refresh: None,
        on_demand: true,
        world_stall: None,
    };
    let mut cases = Vec::new();
    // One-sided steps (R fixed), as in the N1 review.
    let mut c = base("chunk step during a q stall (gap 4 km)");
    c.q.growth = vec![(330, CHUNK)];
    c.q.stall = Some((300, 600));
    cases.push(c);
    let mut c = base("chunk step, no stall (gap 4 km)");
    c.q.growth = vec![(330, CHUNK)];
    cases.push(c);
    for (name, od) in [("scale x8 (+14 km), pull every 60 s only (gap 10 km)", false), ("scale x8 (+14 km), pull every 60 s or on demand (gap 10 km)", true)] {
        let mut c = base(name);
        c.q = fixed(10_000.0);
        c.q.growth = vec![(1000, 14.0 * CHUNK)];
        c.refresh = Some(3600);
        c.on_demand = od;
        cases.push(c);
    }
    for (name, od) in [("joiner at q's near edge (+3 km), pull every 60 s only (gap 5 km)", false), ("joiner at q's near edge (+3 km), pull every 60 s or on demand (gap 5 km)", true)] {
        let mut c = base(name);
        c.q = fixed(5000.0);
        c.q.growth = vec![(1000, 3.0 * CHUNK)];
        c.refresh = Some(3600);
        c.on_demand = od;
        cases.push(c);
    }
    let mut c = base("walker at v_max toward R, 12 chunk steps, q stall mid-way (gap 12 km)");
    c.q = fixed(12_000.0);
    c.q.growth = walk(300, 615, 12);
    c.q.stall = Some((3000, 600));
    cases.push(c);
    // Creation (q created during the run), then q walks toward R.
    for (name, gap, stall) in [
        ("region created 4.5 km from R, then walks toward it", 4500.0, None),
        ("region created 3.5 km from R, then walks toward it", 3500.0, None),
        ("region created 4.5 km from R and walks toward it, world log stalled 5 s", 4500.0, Some((500, 300))),
        ("region created 4.5 km from R and walks toward it, world log stalled 12 s", 4500.0, Some((500, 720))),
    ] {
        let mut c = base(name);
        c.q = Side { created: Some(600), edge0: gap, growth: walk(1300, 615, 3), stall: None };
        c.world_stall = stall;
        cases.push(c);
    }
    // Review M1: both regions walk toward each other; q is created at the closest allowed distance.
    for (v, s_create, stall) in [(100.0, 600, None), (100.0, 600, Some((560, 480))), (200.0, 600, None), (200.0, 600, Some((560, 480))), (100.0, 1200, Some((560, 480)))] {
        let name = format!(
            "both walk toward each other, q created at the closest allowed gap, v_max {v} m/s, S_create {} s{}",
            s_create / 60,
            if stall.is_some() { ", world log stalled 8 s" } else { "" }
        );
        let every = (60.0 * CHUNK / v).ceil() as i64;
        let mut c = base(&name);
        c.v = v;
        c.s_create = s_create;
        c.r = Side { created: None, edge0: 0.0, growth: walk(0, every, 40), stall: None };
        // R's envelope at q's creation (frame 600) as the directory shows it, plus the minimum gap.
        let wc = stall.filter(|st| st.0 <= 600 && 600 < st.0 + st.1).map(|st| st.0).unwrap_or(600) - LAG_W;
        let k = (wc / HEAD) * HEAD - LAG_R;
        let r_env: f64 = c.r.growth.iter().filter(|g| g.0 <= k).map(|g| g.1).sum();
        let gap = r_env + D + v * s_create as f64 / 60.0 + v * (600 - k) as f64 / 60.0 + 1.0;
        c.q = Side { created: Some(600), edge0: gap, growth: walk(601, every, 40), stall: None };
        c.world_stall = stall;
        cases.push(c);
    }
    // Review M1: two newcomers create regions at the same spawn point, 10 frames apart.
    let mut c = base("two regions created at the same spawn point, 10 frames apart");
    c.r = Side { created: Some(600), edge0: 0.0, growth: vec![], stall: None };
    c.q = Side { created: Some(610), edge0: 0.0, growth: vec![], stall: None };
    cases.push(c);

    let mut late_count = [0usize; 3];
    for c in &cases {
        for (i, (rule, rules)) in [("2a228c6", Rules::Old), ("05d3cc0", Rules::V05), ("revised", Rules::Revised)].iter().enumerate() {
            // A created region can only grow once it exists: stamp + S_create under the revised rule.
            let mut cc = c.clone();
            if *rules == Rules::Revised {
                for s in [&mut cc.r, &mut cc.q] {
                    if let Some(cs) = s.created {
                        for g in s.growth.iter_mut() {
                            g.0 = g.0.max(cs + cc.s_create + 1);
                        }
                    }
                }
            }
            let o = run(&cc, *rules);
            let late = o.late_r || o.late_q;
            late_count[i] += late as usize;
            println!(
                "RESULT safety case=\"{}\" rule={rule} late_merge={late} late_R={} late_q={} q_becomes_join={} merge_frame={} R_frames_lost_before_learning={}",
                c.name,
                o.late_r,
                o.late_q,
                o.q_is_join,
                o.f_m.map(|f| f.to_string()).unwrap_or("-".into()),
                o.lost_r
            );
        }
    }
    println!("RESULT safety-summary cases={} late_2a228c6={} late_05d3cc0={} late_revised={}", cases.len(), late_count[0], late_count[1], late_count[2]);
}

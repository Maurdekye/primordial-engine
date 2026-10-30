//! Reviews N1, M1, P1, P2: is the neighbour bound *safe* when footprints change in steps, when both
//! regions grow toward each other, when regions are created concurrently, when growth may skip the growth
//! clock, and across scale bands?
//!
//! A frame-level model of two regions on a line [C: it simulates the rules, not the protocol]. `R` holds
//! `(-inf, R_edge]` and `q` holds `[q_edge, inf)`; growth moves the facing edges. Each region simulates as
//! far as the bounds allow, from what it knows of the other. A **late merge** is a merge frame that a region
//! had already simulated past when it first held every fact that fixes it: the merge would need a rollback,
//! possibly below its confirmed frame, which design 6.4 forbids. Both regions are checked. In the cross-band
//! cases `q` is a larger band's region `L`, the "merge" is the frame from which `R` must read `L` (the
//! footprints overlap, margin 0), and only `R` is checked, because `L` never reads `R`.
//!
//! Rule sets:
//! * `2a228c6`: anchors take effect at `stamp + Lambda`; a neighbour is seen by its footprint *in effect*
//!   at its known confirmed frame; region creation is announced to nobody.
//! * `05d3cc0`: the growth clock (growth J takes effect at `max(stamp + Lambda, previous growth's
//!   effect) + ceil(60 J / v_max)`), envelopes from every anchor *stamped* by the known confirmed frame,
//!   region creation through the world log taking effect at `stamp + Lambda`, valid if at least
//!   `D_merge + v_max * S_create` from every region in the *confirmed* directory, and every region gated on
//!   the directory confirmed through `f - S_create`.
//! * `e10c905`: as 05d3cc0, but creation takes effect at `stamp + S_create` and is validated in world-log
//!   order against pending creations too, and `J` is the reduction of the gap to the other region's
//!   possible footprint (its envelope grown by `v_max (age + 60 s)`). Once that possible footprint reaches
//!   this region the gap is 0 before and after, so a step straight at the neighbour is instant (review P2).
//!   No bound between bands (review P1).
//! * `revised` (reviews P1, P2): as e10c905, but growth is instant (`J` = 0) only if every added point is at
//!   least `D_merge` from the other region's possible footprint over the horizon `T_h` = 60 s (envelope
//!   grown by the other region's `v_max (age + T_h)`); otherwise `J` is the full extension. Every region
//!   simulates `f` only while `f <= known_conf_q + T_h` (the horizon gate). A small-band region bounds
//!   itself on every larger-band region at that band's `v_max`, margin 0, with the same gate, and the larger
//!   region's `J` counts the small region's possible footprint with margin 0.
//!
//! Parameters: region confirmation lag 9 frames, 3 frames to learn a neighbour's confirmed frame,
//! world-log lag 66 frames, Lambda 30 frames, eviction 132 frames; `D_merge` (3 km), `v_max` (100 m/s)
//! and `S_create` (10 s) vary by case, as they are world and band settings.

const LAG_R: i64 = 9;
const INFO: i64 = 3;
const LAG_W: i64 = 66;
const LAMBDA: i64 = 30;
const EVICT: i64 = 132;
const T: i64 = 30_000;
const CHUNK: f64 = 1024.0;
/// RegionHead cadence in the world log (25.5.8): 5 s.
const HEAD: i64 = 300;
/// The horizon of instant growth and of the horizon gate: 60 s.
const T_H: i64 = 3600;

#[derive(Clone, Copy, PartialEq)]
enum Rules {
    Old,
    V05,
    E10,
    Revised,
    /// Revised without the start condition for created regions.
    NoStart,
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
    /// The stall is never cut short by eviction (a two-member log, which never removes anyone: D7).
    no_evict: bool,
}

#[derive(Clone)]
struct Case {
    name: String,
    /// `v_max` of q's band (L's, in cross-band cases); R's growth is clocked at it too in same-band cases.
    v: f64,
    /// `v_max` of R's band: how fast q must assume R can approach.
    v_r: f64,
    d_merge: f64,
    s_create: i64,
    r: Side,
    q: Side,
    /// The other region refreshes this one's prefix every `refresh` frames, and on demand if set.
    refresh: Option<i64>,
    on_demand: bool,
    world_stall: Option<(i64, i64)>,
    /// q is a larger band's region that R must read once their footprints overlap.
    cross_band: bool,
}

struct Out {
    late_r: bool,
    late_q: bool,
    q_is_join: bool,
    f_m: Option<i64>,
    lost_r: i64,
    instant: usize,
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
    let d = c.d_merge;
    // A region's confirmed frame as a neighbour learns it at t.
    let conf_of = |s: &Side, t: i64| -> i64 {
        let tt = t - INFO;
        let cap = if s.no_evict { i64::MAX } else { EVICT };
        if let Some((st, len)) = s.stall {
            if tt >= st && tt < st + len.min(cap) {
                return st - LAG_R;
            }
        }
        tt - LAG_R
    };
    let revised = matches!(rules, Rules::Revised | Rules::NoStart);
    // Growth effects under each rule set's J. `me` judges `other` from what its own log holds at the stamp:
    // `other` must be in the directory by then, and its envelope counts growth stamped by its known
    // confirmed frame. `dir` is +1 when `other` lies at larger coordinates than `me`.
    let mut instant = 0usize;
    let mut effects = |me: &Side, other: &Side, dir: f64, v_me: f64, v_other: f64| -> Growth {
        let mut clock = i64::MIN;
        let mut edge = me.edge0;
        let mut out = Vec::new();
        for &(stamp, j) in &me.growth {
            let new_edge = edge + dir * j;
            let knows_other = other.created.is_none_or(|co| world_conf(stamp) >= co);
            let jj = match rules {
                Rules::Old => 0.0,
                Rules::V05 => j,
                // e10c905 measured J against known regions of the same band only.
                Rules::E10 if c.cross_band => 0.0,
                _ if !knows_other => 0.0,
                _ => {
                    let k = conf_of(other, stamp);
                    let env = other.edge0 - dir * other.growth.iter().filter(|g| g.0 <= k).map(|g| g.1).sum::<f64>();
                    let age = (stamp - k).max(0) as f64 / 60.0;
                    let possible = env - dir * v_other * (age + T_H as f64 / 60.0);
                    let gap_before = dir * (possible - edge);
                    let gap_after = dir * (possible - new_edge);
                    if rules == Rules::E10 {
                        // e10c905: the reduction of the (non-negative) gap to the possible footprint.
                        gap_before.max(0.0) - gap_after.max(0.0)
                    } else if gap_after >= d {
                        0.0
                    } else {
                        j
                    }
                }
            };
            let e = if rules == Rules::Old {
                stamp + LAMBDA
            } else if jj <= 0.0 {
                instant += 1;
                stamp + LAMBDA
            } else {
                let e = (stamp + LAMBDA).max(clock) + (60.0 * jj / v_me).ceil() as i64;
                clock = e;
                e
            };
            out.push((stamp, e, j));
            edge = new_edge;
        }
        out
    };
    let gr = effects(&c.r, &c.q, 1.0, c.v_r, v);
    let gq = effects(&c.q, &c.r, -1.0, v, c.v_r);
    let delay = if matches!(rules, Rules::E10 | Rules::Revised | Rules::NoStart) { c.s_create } else { LAMBDA };
    // Creation validity of q against R as the world-log directory shows R at q's stamp.
    let mut q_is_join = false;
    if let Some(sq) = c.q.created {
        if rules != Rules::Old {
            let r_in_directory = match c.r.created {
                None => true,
                Some(sr) => {
                    if matches!(rules, Rules::E10 | Rules::Revised | Rules::NoStart) {
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
                if c.q.edge0 - r_env < d + v * c.s_create as f64 / 60.0 + v * age {
                    q_is_join = true;
                }
            }
        }
    }
    let live = |s: &Side, f: i64| s.created.is_none_or(|cs| f >= cs + delay);
    let edge_r = |f: i64| c.r.edge0 + gr.iter().filter(|g| g.1 <= f).map(|g| g.2).sum::<f64>();
    let edge_q = |f: i64| c.q.edge0 - gq.iter().filter(|g| g.1 <= f).map(|g| g.2).sum::<f64>();
    let f_m = if q_is_join { None } else { (0..T).find(|&f| live(&c.r, f) && live(&c.q, f) && edge_q(f) - edge_r(f) < d) };
    // Before the revision, nothing bounds a small-band region on a larger band's region (review P1).
    let bounds = !(c.cross_band && !revised);

    // One side: X knows its own growth (its own log); it learns Y's by pulling Y's confirmed prefix.
    // `dir` is +1 when Y lies at larger coordinates than X; `v_y` is Y's band v_max.
    let side = |x: &Side, y: &Side, gx: &Growth, gy: &Growth, dir: f64, v_y: f64| -> (bool, i64) {
        let knows_y = |t: i64| -> bool {
            match y.created {
                None => true,
                Some(cy) => rules != Rules::Old && world_conf(t) >= cy,
            }
        };
        let (mut sim, mut k, mut last_pull, mut bound_prev) = (-1i64, conf_of(y, 0), 0i64, i64::MAX);
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
                k = conf_of(y, t);
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
            if bounds && knows_y(t) && !q_is_join {
                // Own footprint with all own stamped growth; Y's envelope from what X holds of Y.
                let own = x.edge0 + dir * gx.iter().filter(|g| g.0 <= t).map(|g| g.2).sum::<f64>();
                let counted = |g: &&(i64, i64, f64)| if rules == Rules::Old { g.1 <= k } else { g.0 <= k };
                let env_y = y.edge0 - dir * gy.iter().filter(counted).map(|g| g.2).sum::<f64>();
                let gap = (env_y - own) * dir;
                bound = bound.min(k + ((gap - d) / v_y * 60.0).floor().max(0.0) as i64);
                if revised {
                    bound = bound.min(k + T_H); // the horizon gate
                }
            }
            // Start condition: a created region simulates its first frame f0 only once it holds the other
            // region's confirmed prefix through f0 - 1, which covers growth stamped before that region's log
            // recorded the creation.
            if rules == Rules::Revised && !q_is_join {
                // (Only toward a region that existed or was ordered before this one.)
                if let Some(cx) = x.created.filter(|&cx| y.created.is_none_or(|cy| cy < cx)) {
                    let f0 = cx + delay;
                    if conf_of(y, t) < f0 - 1 {
                        bound = bound.min(f0 - 1);
                    }
                }
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
    let (late_r, lost_r) = side(&c.r, &c.q, &gr, &gq, 1.0, v);
    let late_q = if q_is_join || c.cross_band { false } else { side(&c.q, &c.r, &gq, &gr, -1.0, c.v_r).0 };
    Out { late_r, late_q, q_is_join, f_m, lost_r, instant }
}

fn fixed(edge: f64) -> Side {
    Side { created: None, edge0: edge, growth: vec![], stall: None, no_evict: false }
}

fn walk(start: i64, every: i64, n: i64, step: f64) -> Vec<(i64, f64)> {
    (0..n).map(|i| (start + every * i, step)).collect()
}

pub fn run_all() {
    println!("== safety (reviews N1, M1, P1, P2): late merges and late reader starts ==");
    let base = |name: &str| Case {
        name: name.to_string(),
        v: 100.0,
        v_r: 100.0,
        d_merge: 3000.0,
        s_create: 600,
        r: fixed(0.0),
        q: fixed(4000.0),
        refresh: None,
        on_demand: true,
        world_stall: None,
        cross_band: false,
    };
    let mut cases = Vec::new();
    // One-sided steps, as in the N1 review, and review P2: q steps straight at R while one side stalls.
    let mut c = base("chunk step, no stall (gap 4 km)");
    c.q.growth = vec![(330, CHUNK)];
    cases.push(c);
    for (gap, who, r_walks) in [
        (4000.0, "q stall", false),
        (4000.0, "R stall", false),
        (4000.0, "R stall, R walks toward q", true),
        (8000.0, "q stall, R walks toward q", true),
        (8000.0, "R stall, R walks toward q", true),
    ] {
        {
            let mut c = base(&format!("chunk step toward R, {who} (gap {} km)", gap / 1000.0));
            c.q = fixed(gap);
            c.q.growth = vec![(330, CHUNK)];
            if who.starts_with("q") {
                c.q.stall = Some((300, 600));
            } else {
                c.r.stall = Some((300, 600));
            }
            if r_walks {
                c.r.growth = walk(0, 615, 12, CHUNK);
            }
            cases.push(c);
        }
    }
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
    c.q.growth = walk(300, 615, 12, CHUNK);
    c.q.stall = Some((3000, 600));
    cases.push(c);
    // Review P2: fast steps from far away (instant at first), then a stall on the way in.
    for stall in [(1500, 600), (4000, 600)] {
        let mut c = base(&format!("chunk step every 5 s from 20 km toward R, q stall at frame {}", stall.0));
        c.q = fixed(20_000.0);
        c.q.growth = walk(200, 300, 19, CHUNK);
        c.q.stall = Some(stall);
        cases.push(c);
    }
    // Creation (q created during the run), then q walks toward R.
    for (name, gap, stall) in [
        ("region created 4.5 km from R, then walks toward it", 4500.0, None),
        ("region created 3.5 km from R, then walks toward it", 3500.0, None),
        ("region created 4.5 km from R and walks toward it, world log stalled 5 s", 4500.0, Some((500, 300))),
        ("region created 4.5 km from R and walks toward it, world log stalled 12 s", 4500.0, Some((500, 720))),
    ] {
        let mut c = base(name);
        c.q = Side { created: Some(600), edge0: gap, growth: walk(1300, 615, 3, CHUNK), stall: None, no_evict: false };
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
        c.v_r = v;
        c.s_create = s_create;
        c.r = Side { created: None, edge0: 0.0, growth: walk(0, every, 40, CHUNK), stall: None, no_evict: false };
        // R's envelope at q's creation (frame 600) as the directory shows it, plus the minimum gap.
        let wc = stall.filter(|st: &(i64, i64)| st.0 <= 600 && 600 < st.0 + st.1).map(|st| st.0).unwrap_or(600) - LAG_W;
        let k = (wc / HEAD) * HEAD - LAG_R;
        let r_env: f64 = c.r.growth.iter().filter(|g| g.0 <= k).map(|g| g.1).sum();
        let gap = r_env + c.d_merge + v * s_create as f64 / 60.0 + v * (600 - k) as f64 / 60.0 + 1.0;
        c.q = Side { created: Some(600), edge0: gap, growth: walk(601, every, 40, CHUNK), stall: None, no_evict: false };
        c.world_stall = stall;
        cases.push(c);
    }
    // Review M1: two newcomers create regions at the same spawn point, 10 frames apart.
    let mut c = base("two regions created at the same spawn point, 10 frames apart");
    c.r = Side { created: Some(600), edge0: 0.0, growth: vec![], stall: None, no_evict: false };
    c.q = Side { created: Some(610), edge0: 0.0, growth: vec![], stall: None, no_evict: false };
    cases.push(c);
    // A region grows instantly toward a spot where a region is being created, before its own log records
    // the creation, then stalls without eviction (two members, D7).
    let mut c = base("two-member region grows +3 km instantly toward a region being created 4.6 km away, then stalls 30 s");
    c.r = Side { created: None, edge0: 0.0, growth: vec![(620, 3.0 * CHUNK)], stall: Some((615, 1800)), no_evict: true };
    c.q = Side { created: Some(600), edge0: 4600.0, growth: vec![], stall: None, no_evict: false };
    cases.push(c);
    // Review P1: a x128 giant's region L (band unit 128 m: v_max 12.8 km/s, 131 km chunks) and a human
    // region R; R must read L from the first frame their footprints overlap (margin 0).
    let giant = |name: &str| {
        let mut c = base(name);
        c.cross_band = true;
        c.v = 12_800.0;
        c.d_merge = 0.0;
        c
    };
    let mut c = giant("giant region walks toward a human region from 1,000 km at its v_max, L stalls on the way");
    c.q = fixed(1_000_000.0);
    c.q.growth = walk(200, 615, 12, 128.0 * CHUNK);
    c.q.stall = Some((4000, 600));
    cases.push(c);
    let mut c = giant("giant scales up x8 (+14 of its 131 km chunks) 1,500 km from a human region, L stalled");
    c.q = fixed(1_500_000.0);
    c.q.growth = vec![(400, 14.0 * 128.0 * CHUNK)];
    c.q.stall = Some((380, 600));
    cases.push(c);
    let mut c = giant("giant steps once onto a human region 100 km away, L stalled");
    c.q = fixed(100_000.0);
    c.q.growth = vec![(400, 128.0 * CHUNK)];
    c.q.stall = Some((380, 600));
    cases.push(c);
    let mut c = giant("two-member giant region walks toward a human region from 1,000 km, L stalled 30 s (never evicted)");
    c.q = fixed(1_000_000.0);
    c.q.growth = walk(200, 615, 12, 128.0 * CHUNK);
    c.q.stall = Some((3000, 1800));
    c.q.no_evict = true;
    cases.push(c);

    let rule_sets = [
        ("2a228c6", Rules::Old),
        ("05d3cc0", Rules::V05),
        ("e10c905", Rules::E10),
        ("revised-no-start", Rules::NoStart),
        ("revised", Rules::Revised),
    ];
    let (mut late_count, mut counted) = ([0usize; 5], [0usize; 5]);
    for c in &cases {
        for (i, (rule, rules)) in rule_sets.iter().enumerate() {
            // Separate bands that read each other exist only from e10c905 on.
            if c.cross_band && matches!(*rules, Rules::Old | Rules::V05) {
                println!("RESULT safety case=\"{}\" rule={rule} n/a", c.name);
                continue;
            }
            counted[i] += 1;
            // A created region can only grow once it exists: stamp + S_create from e10c905 on.
            let mut cc = c.clone();
            if matches!(*rules, Rules::E10 | Rules::Revised | Rules::NoStart) {
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
                "RESULT safety case=\"{}\" rule={rule} late={late} late_R={} late_q={} q_becomes_join={} merge_or_read_frame={} instant_growths={} R_frames_lost_before_learning={}",
                c.name,
                o.late_r,
                o.late_q,
                o.q_is_join,
                o.f_m.map(|f| f.to_string()).unwrap_or("-".into()),
                o.instant,
                o.lost_r
            );
        }
    }
    println!(
        "RESULT safety-summary cases={} late_2a228c6={}/{} late_05d3cc0={}/{} late_e10c905={}/{} late_revised_no_start={}/{} late_revised={}/{}",
        cases.len(),
        late_count[0],
        counted[0],
        late_count[1],
        counted[1],
        late_count[2],
        counted[2],
        late_count[3],
        counted[3],
        late_count[4],
        counted[4]
    );
}

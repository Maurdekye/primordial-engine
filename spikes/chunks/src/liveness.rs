//! Review R16-1: does one stalled member slow down regions far away from it?
//!
//! A frame-level model of the two dependency rules [C: it simulates the rules, not the network
//! protocol]. Parameters are the design's: 60 Hz, region confirmation lag 5 frames (measured, 6.4),
//! world-log confirmation lag 66 frames (about 1.1 s at 0.5 s compact acks, @engine-architect [I]),
//! speculation limit W = 120 frames, Lambda = 120 frames, eviction W + 12 frames after a member's acks stop.
//!
//! * `world-log` (45d4940, 25.5.2): region R simulates frame f only when the world log, confirmed by
//!   every member, is confirmed through f - Lambda.
//! * `neighbour-bound` (revised): the world log is off every region's per-frame path. R simulates f
//!   only while every other region q is provably too far to merge with R by f: q's last confirmed
//!   frame known to R (50 ms old) plus q's slack, `(distance - D_merge) / v_max`, with footprints that
//!   move at most v_max = 100 m/s and D_merge = 3 km.
//!
//! One member stops acking at t = 5 s for S seconds. Counted: frames in which an *unrelated* region
//! (one that does not contain the stalled member) cannot simulate the present frame.

const T: i64 = 1200;
const T0: i64 = 300;
const LAG_R: i64 = 9; // 8.5 frames (ed347ac 6.4), rounded up
const LAG_W: i64 = 66;
const W: i64 = 120;
const LAMBDA: i64 = 120;
const EVICT: i64 = W + 12;
/// A one-member region has nobody to vote its silent member out: eviction is a world-log view change
/// (review N3): the in-region timeout, plus world-log confirmation, plus a session-wide vote round.
const EVICT_WORLD: i64 = EVICT + LAG_W + 30;
const INFO_LAG: i64 = 3;
const V_MAX: f64 = 100.0;
const D_MERGE: f64 = 3000.0;

fn acked(m: usize, t: i64, stall: i64, group: usize) -> Option<i64> {
    if m != 0 || t < T0 || t >= T0 + stall {
        return Some(t);
    }
    let evict = if group == 1 { EVICT_WORLD } else { EVICT };
    if t >= T0 + evict {
        None // evicted by a view change: no longer counted
    } else {
        Some(T0)
    }
}

fn run(n: usize, group: usize, spacing: f64, stall: i64, old_rule: bool) -> (i64, i64, usize) {
    let regions = n / group;
    let side = (regions as f64).sqrt().ceil() as usize;
    let pos: Vec<[f64; 2]> = (0..regions).map(|r| [(r % side) as f64 * spacing, (r / side) as f64 * spacing]).collect();
    let conf = |r: usize, t: i64| -> i64 {
        (r * group..(r + 1) * group).filter_map(|m| acked(m, t, stall, group)).min().unwrap_or(t) - LAG_R
    };
    let world_conf = |t: i64| -> i64 { (0..n).filter_map(|m| acked(m, t, stall, group)).min().unwrap() - LAG_W };
    let (mut worst, mut total, mut affected) = (0i64, 0i64, 0usize);
    for r in 1..regions {
        let mut stalled = 0;
        for t in 0..T {
            let mut bound = (conf(r, t) + W).min(t);
            if old_rule {
                bound = bound.min(world_conf(t) + LAMBDA);
            } else {
                for q in 0..regions {
                    if q == r {
                        continue;
                    }
                    let d = ((pos[r][0] - pos[q][0]).powi(2) + (pos[r][1] - pos[q][1]).powi(2)).sqrt();
                    let slack = ((d - D_MERGE).max(0.0) / V_MAX * 60.0) as i64;
                    bound = bound.min(conf(q, (t - INFO_LAG).max(0)) + slack);
                }
            }
            if bound < t {
                stalled += 1;
            }
        }
        worst = worst.max(stalled);
        total += stalled;
        affected += (stalled > 0) as usize;
    }
    (worst, total, affected)
}

pub fn run_all() {
    println!("== liveness (review R16-1): one stalled member, frames lost in unrelated regions ==");
    for (n, group) in [(64usize, 1usize), (64, 8), (256, 1), (256, 8)] {
        for spacing in [4000.0, 3500.0, 3200.0] {
            for s in [30i64, 60, 120, 180, 600] {
                for (name, old) in [("world-log", true), ("neighbour-bound", false)] {
                    let (worst, total, affected) = run(n, group, spacing, s, old);
                    println!(
                        "RESULT liveness members={n} per_region={group} footprint_gap_m={spacing} stall_s={:.1} rule={name} unrelated_regions={} regions_affected={affected} worst_region_stall_frames={worst} unrelated_region_stall_frames_total={total}",
                        s as f64 / 60.0,
                        n / group - 1,
                    );
                }
            }
        }
    }
}

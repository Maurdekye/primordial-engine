//! D2 parallel-physics bench: step cost and stage breakdown on mixed piles of 500 / 1,000 /
//! 2,000 / 4,000 bodies with sleeping off (every body stays awake, the worst case for the
//! budget), plus rollback re-simulation cost (restore + 6 frames) per frame.
//!
//! `par-bench [frames] [sizes] [threads]`, sizes and threads comma-separated. Stage times need
//! the `stage-profile` feature (`--features stage-profile`); without it they print as 0.

use std::time::{Duration, Instant};

use box3d_rust::world::{world_enable_sleeping, world_get_profile, World};
use pe_sim_core::*;

use crate::{SEED, world_def_threads};

fn pile_for(count: usize) -> (usize, usize) {
    match count {
        500 => (10, 5),
        1000 => (10, 10),
        2000 => (20, 5),
        4000 => (20, 10),
        n => {
            let side = ((n as f64 / 10.0).sqrt().round() as usize).max(1);
            (side, (n / (side * side)).max(1))
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Acc {
    step: f64,
    pairs: f64,
    collide: f64,
    solve: f64,
    prepare: f64,
    integrate: f64,
    warm: f64,
    solve_imp: f64,
    relax: f64,
    restitution: f64,
    store: f64,
    finalize: f64,
    refit: f64,
    sleep: f64,
    sensors: f64,
}

fn add(acc: &mut Acc, w: &World) {
    let p = world_get_profile(w);
    acc.step += p.step as f64;
    acc.pairs += p.pairs as f64;
    acc.collide += p.collide as f64;
    acc.solve += p.solve as f64;
    acc.prepare += (p.prepare_constraints + p.split_islands) as f64;
    acc.integrate += (p.integrate_velocities + p.integrate_positions) as f64;
    acc.warm += p.warm_start as f64;
    acc.solve_imp += p.solve_impulses as f64;
    acc.relax += p.relax_impulses as f64;
    acc.restitution += p.restitution as f64;
    acc.store += p.store_impulses as f64;
    acc.finalize += p.transforms as f64;
    acc.refit += p.refit as f64;
    acc.sleep += (p.sleep_islands + p.bullets + p.hit_events + p.joint_events + p.sensor_hits) as f64;
    acc.sensors += p.sensors as f64;
}

pub fn run(frames: u64, sizes: &[usize], threads: &[i32]) -> bool {
    let warmup = 60u64;
    let mut all_same = true;
    println!(
        "par-bench: {frames} measured frames after {warmup} warm-up, sleep off, 4 substeps, {} profile; times in ms per frame",
        if cfg!(debug_assertions) { "debug" } else { "release" }
    );
    println!(
        "{:>5} {:>3} | {:>7} {:>7} | {:>6} {:>6} {:>6} | {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} | {:>7} | hash",
        "N", "thr", "wall", "step", "pairs", "coll", "solve", "prep", "integ", "warm", "push", "relax", "rest", "store", "final", "refit", "other", "rb6/f"
    );
    for &count in sizes {
        let (side, layers) = pile_for(count);
        let mut first_hash: Option<u64> = None;
        for &t in threads {
            let def = world_def_threads(t);
            let scene = build_pile_scene_def(side, layers, &def);
            let n = scene.bodies.len();
            let mut gs = GameState::new(scene, SEED);
            world_enable_sleeping(&mut gs.scene.world, false);
            for _ in 0..warmup {
                gs.advance(&[]);
            }
            let mut acc = Acc::default();
            let mut wall = Duration::ZERO;
            for _ in 0..frames {
                let t0 = Instant::now();
                gs.advance(&[]);
                wall += t0.elapsed();
                add(&mut acc, &gs.scene.world);
            }
            let hash = gs.world_hash();
            // Rollback re-simulation: restore a snapshot 6 frames back and re-run 6 frames.
            let mut snapper = Snapshotter::new();
            let snap = snapper.save(&gs);
            let reps = 5;
            let mut rb = Duration::ZERO;
            for _ in 0..reps {
                let t0 = Instant::now();
                snapper.restore(&mut gs, &snap);
                for _ in 0..6 {
                    gs.advance(&[]);
                }
                rb += t0.elapsed();
            }
            let rb_hash = gs.world_hash();
            let f = frames as f64;
            let ms = |d: Duration| d.as_secs_f64() * 1e3;
            let same = *first_hash.get_or_insert(hash) == hash;
            all_same &= same;
            println!(
                "{n:>5} {t:>3} | {:7.3} {:7.3} | {:6.3} {:6.3} {:6.3} | {:6.3} {:6.3} {:6.3} {:6.3} {:6.3} {:6.3} {:6.3} {:6.3} {:6.3} {:6.3} | {:7.3} | {hash:016x}{} rb {rb_hash:016x}",
                ms(wall) / f,
                acc.step / f,
                acc.pairs / f,
                acc.collide / f,
                acc.solve / f,
                acc.prepare / f,
                acc.integrate / f,
                acc.warm / f,
                acc.solve_imp / f,
                acc.relax / f,
                acc.restitution / f,
                acc.store / f,
                acc.finalize / f,
                acc.refit / f,
                (acc.sleep + acc.sensors) / f,
                ms(rb) / reps as f64,
                if same { "" } else { " MISMATCH" },
            );
        }
    }
    println!("RESULT: {}", if all_same { "hashes identical across thread counts" } else { "HASH MISMATCH" });
    all_same
}

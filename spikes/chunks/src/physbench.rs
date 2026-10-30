//! Physics benchmarks run in both builds (`f32` and `--features f64`):
//! * `cost`: step, save, restore and image size for the b6 piles and a 500-ball pit;
//! * `far`: the same scenes placed 0 m to 100,000 km from the origin;
//! * `scale`: an 8-cube tower from 0.1 mm to 10 km, unscaled vs per-region length units.

use std::panic::{catch_unwind, AssertUnwindSafe};

use box3d_rust::body::{body_get_position, body_is_awake};
use box3d_rust::id::BodyId;
use box3d_rust::recording::hash_world_state;
use box3d_rust::world::{world_get_awake_body_count, world_get_counters, World};

use pe_spike_chunks::phys::{
    build_ballpit, build_pile, hand_target, make_body, mode, new_world, pos_f64, set_kinematic_target,
    static_box, world_def, BodyInit, Shape, Snapshotter, DT, SUBSTEPS,
};
use pe_spike_chunks::{median, time_ms};

fn step_avg(world: &mut World, frames: usize, mut per_frame: impl FnMut(&mut World, usize)) -> f64 {
    let mut runs = Vec::new();
    for rep in 0..3 {
        let (_, ms) = time_ms(|| {
            for f in 0..frames {
                per_frame(world, rep * frames + f);
                world.step(DT, SUBSTEPS);
            }
        });
        runs.push(ms / frames as f64);
    }
    median(&mut runs)
}

pub fn cost() {
    println!("== physics cost ({}) ==", mode());
    for (side, layers) in [(8usize, 4usize), (16, 4), (16, 8)] {
        let mut w = new_world(&world_def(1.0, 10.0, false));
        let bodies = build_pile(&mut w, side, layers, [0.0; 3]);
        for _ in 0..30 {
            w.step(DT, SUBSTEPS);
        }
        let step = step_avg(&mut w, 60, |_, _| {});
        let mut snap = Snapshotter::new();
        let mut saves = Vec::new();
        let mut img = Vec::new();
        for _ in 0..10 {
            let (i, ms) = time_ms(|| snap.save(&w));
            img = i;
            saves.push(ms);
        }
        let mut restores = Vec::new();
        for _ in 0..10 {
            let (_, ms) = time_ms(|| snap.restore(&mut w, &img));
            restores.push(ms);
        }
        let lz4 = lz4_flex::compress(&img).len();
        let n = bodies.len();
        println!(
            "RESULT physics-cost mode={} bodies={n} step_ms={step:.3} save_ms={:.3} restore_ms={:.3} image_bytes={} image_lz4_bytes={lz4} bytes_per_body={:.0} awake={}",
            mode(),
            median(&mut saves),
            median(&mut restores),
            img.len(),
            img.len() as f64 / n as f64,
            world_get_awake_body_count(&w)
        );
    }
    // The 500-ball pit with a hand sweeping through it (the design's "main question" scene).
    let mut w = new_world(&world_def(1.0, 10.0, false));
    let (_, hand) = build_ballpit(&mut w, 500, [0.0; 3]);
    for f in 0..120u64 {
        set_kinematic_target(&mut w, hand, hand_target([0.0; 3], f, 0.0));
        w.step(DT, SUBSTEPS);
    }
    let step = step_avg(&mut w, 120, |w, f| set_kinematic_target(w, hand, hand_target([0.0; 3], 120 + f as u64, 0.0)));
    let mut snap = Snapshotter::new();
    let img = snap.save(&w);
    println!(
        "RESULT physics-cost mode={} scene=ballpit500 step_ms={step:.3} image_bytes={} awake={}",
        mode(),
        img.len(),
        world_get_awake_body_count(&w)
    );
}

struct FarRun {
    rel: Vec<Vec<[f64; 3]>>,
    step_ms: f64,
    awake_end: i32,
    contacts: i32,
    sleep_frame: Option<usize>,
    hash: u64,
}

fn run_far(scene: &str, off: [f64; 3]) -> FarRun {
    let mut w = new_world(&world_def(1.0, 10.0, false));
    let bodies: Vec<BodyId> = match scene {
        "pile256" => build_pile(&mut w, 8, 4, off),
        "tower" => {
            static_box(&mut w, [off[0], off[1] - 0.5, off[2]], [5.0, 0.5, 5.0]);
            (0..8)
                .map(|i| make_body(&mut w, &BodyInit::at([off[0], off[1] + 0.5 + 1.01 * i as f64, off[2]]), Shape::Cube(0.5)))
                .collect()
        }
        "roll" => {
            static_box(&mut w, [off[0] + 20.0, off[1] - 0.5, off[2]], [40.0, 0.5, 5.0]);
            let mut b = BodyInit::at([off[0], off[1] + 0.25, off[2]]);
            b.lin = pe_spike_chunks::phys::v3(2.0, 0.0, 0.0);
            vec![make_body(&mut w, &b, Shape::Ball(0.25))]
        }
        _ => unreachable!(),
    };
    let frames = 600;
    let mut rel = Vec::new();
    let mut sleep_frame = None;
    let (_, ms) = time_ms(|| {
        for f in 0..frames {
            w.step(DT, SUBSTEPS);
            if f % 10 == 9 {
                rel.push(
                    bodies
                        .iter()
                        .map(|&b| {
                            let p = pos_f64(body_get_position(&w, b));
                            [p[0] - off[0], p[1] - off[1], p[2] - off[2]]
                        })
                        .collect(),
                );
            }
            if sleep_frame.is_none() && bodies.iter().all(|&b| !body_is_awake(&w, b)) {
                sleep_frame = Some(f + 1);
            }
        }
    });
    FarRun {
        rel,
        step_ms: ms / frames as f64,
        awake_end: world_get_awake_body_count(&w),
        contacts: world_get_counters(&w).contact_count,
        sleep_frame,
        hash: hash_world_state(&w),
    }
}

pub fn far() {
    println!("== physics far from the origin ({}) ==", mode());
    let ds = [0.0, 1.0e3, 1.0e4, 1.0e5, 1.0e6, 1.0e7, 1.0e8];
    for scene in ["pile256", "tower", "roll"] {
        let base = run_far(scene, [0.0; 3]);
        for d in ds {
            // Far on all three axes (a planet-surface frame is far from its origin in 3D), with
            // fractional parts so nothing lands on a coarse float grid by accident. `+ 0.0` keeps
            // the origin run free of negative zeros.
            let off = if d == 0.0 { [0.0; 3] } else { [d + 0.37, 0.5 * d + 0.21, -0.75 * d + 0.13] };
            // Region-local: the same object, but box3d coordinates are measured from the centre of
            // the object's level-0 chunk (the proposed region origin), so they stay below ~900 m.
            let wp = pe_spike_chunks::coords::WorldPos::from_abs(off);
            let local = wp.to_region(wp.chunk, 0);
            for (frame, o) in [("absolute", off), ("region-local", local)] {
            let r = catch_unwind(AssertUnwindSafe(|| run_far(scene, o)));
            let off = o;
            let _ = off;
            match r {
                Err(_) => println!("RESULT far mode={} frame={frame} scene={scene} D={d:e} PANIC", mode()),
                Ok(r) => {
                    // Largest deviation of any body's origin-relative position from the run at the
                    // origin, over all sampled frames.
                    let mut dev: f64 = 0.0;
                    for (a, b) in r.rel.iter().zip(&base.rel) {
                        for (p, q) in a.iter().zip(b) {
                            for k in 0..3 {
                                let e = (p[k] - q[k]).abs();
                                dev = if e.is_nan() { f64::INFINITY } else { dev.max(e) };
                            }
                        }
                    }
                    let last = r.rel.last().unwrap();
                    let extra = match scene {
                        "tower" => {
                            let top = last[7];
                            format!(" top_xz_drift_m={:.4} top_y_m={:.3}", (top[0].powi(2) + top[2].powi(2)).sqrt(), top[1])
                        }
                        "roll" => format!(" final_x_m={:.4}", last[0][0]),
                        _ => String::new(),
                    };
                    println!(
                        "RESULT far mode={} frame={frame} scene={scene} D={d:e} max_dev_vs_origin_m={dev:.3e} same_hash_as_origin={} step_ms={:.3} awake_end={} contacts={} sleep_frame={}{extra}",
                        mode(),
                        r.hash == base.hash,
                        r.step_ms,
                        r.awake_end,
                        r.contacts,
                        r.sleep_frame.map(|f| f.to_string()).unwrap_or("-".into())
                    );
                }
            }
            }
        }
    }
}

struct ScaleRun {
    survived: bool,
    pen_rel: f64,
    drift_rel: f64,
    sleep_frame: Option<usize>,
    step_ms: f64,
    hash: u64,
}

/// An 8-cube tower of edge `s` metres on a ground slab. `unit` is the region's length unit (box3d
/// sees every length divided by it). `invariant` keeps gravity at 10 units/s^2.
fn run_tower(s: f64, unit: f64, invariant: bool, substeps: i32, dt: f32) -> ScaleRun {
    let k = s / unit; // edge in box3d units
    let mut w = new_world(&world_def(unit, 10.0, invariant));
    static_box(&mut w, [0.0, -0.5 * k, 0.0], [(5.0 * k) as f32, (0.5 * k) as f32, (5.0 * k) as f32]);
    let bodies: Vec<BodyId> = (0..8)
        .map(|i| {
            let mut b = BodyInit::at([0.0, 0.5 * k + 1.01 * k * i as f64, 0.0]);
            b.sleep_threshold = (0.05 * k) as f32; // velocity threshold relative to the cube size
            make_body(&mut w, &b, Shape::Cube((0.5 * k) as f32))
        })
        .collect();
    let frames = 600;
    let mut min_y = f64::INFINITY;
    let mut sleep_frame = None;
    let (_, ms) = time_ms(|| {
        for f in 0..frames {
            w.step(dt, substeps);
            let y0 = pos_f64(body_get_position(&w, bodies[0]))[1];
            if f >= 120 {
                min_y = min_y.min(y0);
            }
            if sleep_frame.is_none() && bodies.iter().all(|&b| !body_is_awake(&w, b)) {
                sleep_frame = Some(f + 1);
            }
        }
    });
    let top = pos_f64(body_get_position(&w, bodies[7]));
    let drift = (top[0] * top[0] + top[2] * top[2]).sqrt() / k;
    let survived = drift < 0.25 && top[1] / k > 7.0;
    ScaleRun {
        survived,
        pen_rel: (0.5 * k - min_y) / k,
        drift_rel: drift,
        sleep_frame,
        step_ms: ms / frames as f64,
        hash: hash_world_state(&w),
    }
}

pub fn scale() {
    println!("== scale: 8-cube tower, edge s, 600 frames ({}) ==", mode());
    let sizes: [f64; 9] = [1.0e-4, 1.0e-3, 1.0e-2, 0.1, 1.0, 10.0, 100.0, 1000.0, 1.0e4];
    let ref_hash = run_tower(1.0, 1.0, true, SUBSTEPS, DT).hash;
    for s in sizes {
        // Real time constant sqrt(s/g): keep about as many sub-steps per time constant as at 1 m.
        let scaled_sub = if s < 1.0 { (SUBSTEPS as f64 * (1.0 / s).sqrt()).ceil().min(256.0) as i32 } else { SUBSTEPS };
        // Real gravity at scale s is the scale-invariant system with time running 1/sqrt(s) times
        // faster: one 60 Hz tick of real time is DT/sqrt(s) of invariant time, at the same
        // 240 Hz invariant sub-step rate.
        let k = (1.0 / s).sqrt();
        let ts_dt = (DT as f64 * k) as f32;
        let ts_sub = ((SUBSTEPS as f64 * k).ceil() as i32).max(1);
        let modes: [(&str, f64, bool, i32, f32); 5] = [
            ("unscaled", 1.0, false, SUBSTEPS, DT),
            ("unit=s,invariant-g", s, true, SUBSTEPS, DT),
            ("unit=s,real-g", s, false, SUBSTEPS, DT),
            ("unit=s,real-g,scaled-substeps", s, false, scaled_sub, DT),
            ("unit=s,real-g,time-scaled", s, true, ts_sub, ts_dt),
        ];
        for (name, unit, inv, sub, dt) in modes {
            // The time-scaled variant runs 600 ticks of real time = 600 * k invariant frames.
            let r = catch_unwind(AssertUnwindSafe(|| run_tower(s, unit, inv, sub, dt)));
            match r {
                Err(_) => println!("RESULT scale mode={} s={s:e} variant={name} PANIC", mode()),
                Ok(r) => println!(
                    "RESULT scale mode={} s={s:e} variant={name} substeps={sub} survived={} penetration_rel={:.4} top_drift_rel={:.4} sleep_frame={} step_ms={:.4} same_hash_as_1m={}",
                    mode(),
                    r.survived,
                    r.pen_rel,
                    r.drift_rel,
                    r.sleep_frame.map(|f| f.to_string()).unwrap_or("-".into()),
                    r.step_ms,
                    r.hash == ref_hash
                ),
            }
        }
    }
}

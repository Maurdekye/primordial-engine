//! R15: how often box3d-rust's existing hooks fire per step, and what they cost natively. Combined
//! with the per-call cost of a wasm guest (spikes/out/r15/callback.jsonl) this gives the cost of
//! letting a world override them.
//!
//! Hooks [S box3d-rust 0.5.0]: custom pair filter (`world_set_custom_filter_callback`), pre-solve
//! (`world_set_pre_solve_callback`, per touching contact per step), friction and restitution
//! mixing (`world_set_friction_callback`, `world_set_restitution_callback`, per new contact).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use box3d_rust::body::{body_get_shape_count, body_get_shapes, create_body};
use box3d_rust::geometry::Sphere;
use box3d_rust::hull::make_box_hull;
use box3d_rust::id::ShapeId;
use box3d_rust::math_functions::{Pos, Vec3};
use box3d_rust::shape::{create_hull_shape, create_sphere_shape, shape_enable_pre_solve_events};
use box3d_rust::types::{default_body_def, default_shape_def, BodyType};
use box3d_rust::world::{
    world_get_awake_body_count, world_set_custom_filter_callback, world_set_friction_callback, world_set_pre_solve_callback,
    world_set_restitution_callback, World,
};
use pe_sim_core::{build_pile_scene, DT, SUBSTEPS};

static PRE: AtomicU64 = AtomicU64::new(0);
static FILTER: AtomicU64 = AtomicU64::new(0);
static MIX: AtomicU64 = AtomicU64::new(0);

fn pre_solve(_a: ShapeId, _b: ShapeId, _p: Pos, _n: Vec3, _ctx: u64) -> bool {
    PRE.fetch_add(1, Relaxed);
    true
}
fn filter(_w: &World, _a: ShapeId, _b: ShapeId, _ctx: u64) -> bool {
    FILTER.fetch_add(1, Relaxed);
    true
}
fn friction(fa: f32, _ma: u64, fb: f32, _mb: u64) -> f32 {
    MIX.fetch_add(1, Relaxed);
    (fa * fb).sqrt()
}
fn restitution(ra: f32, _ma: u64, rb: f32, _mb: u64) -> f32 {
    MIX.fetch_add(1, Relaxed);
    ra.max(rb)
}

/// A pile of `n` boxes and balls (alternating) on the ground, with hooks enabled on every shape.
fn pile(n: usize, hooks: bool) -> World {
    let mut w = World::new(&pe_sim_core::engine_world_def());
    let mut g = default_body_def();
    g.type_ = BodyType::Static;
    g.position = Pos { x: 0.0, y: -0.5, z: 0.0 };
    let gb = create_body(&mut w, &g);
    let mut gs = default_shape_def();
    gs.enable_pre_solve_events = hooks;
    gs.enable_custom_filtering = hooks;
    create_hull_shape(&mut w, gb, &gs, &make_box_hull(30.0, 0.5, 30.0).base);
    let side = ((n as f64 / 4.0).sqrt().ceil()) as usize;
    for i in 0..n {
        let (x, z, y) = (i % side, (i / side) % side, i / (side * side));
        let mut d = default_body_def();
        d.type_ = BodyType::Dynamic;
        d.position = Pos { x: x as f32 * 0.9 - side as f32 * 0.45, y: 0.5 + y as f32 * 0.9, z: z as f32 * 0.9 - side as f32 * 0.45 };
        let b = create_body(&mut w, &d);
        let mut sd = default_shape_def();
        sd.density = 1.0;
        sd.enable_pre_solve_events = hooks;
        sd.enable_custom_filtering = hooks;
        if i % 2 == 0 {
            create_hull_shape(&mut w, b, &sd, &make_box_hull(0.4, 0.4, 0.4).base);
        } else {
            create_sphere_shape(&mut w, b, &sd, &Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: 0.4 });
        }
    }
    if hooks {
        world_set_pre_solve_callback(&mut w, Some(pre_solve), 0);
        world_set_custom_filter_callback(&mut w, Some(filter), 0);
        world_set_friction_callback(&mut w, Some(friction));
        world_set_restitution_callback(&mut w, Some(restitution));
    }
    w
}

/// The phase-1 pile (stays active), with pre-solve events on every dynamic shape and the global
/// mixing callbacks registered.
fn phase1_pile(side: usize, layers: usize, hooks: bool) -> World {
    let s = build_pile_scene(side, layers);
    let mut w = s.world;
    if hooks {
        for b in &s.bodies {
            let n = body_get_shape_count(&w, *b) as usize;
            for sh in body_get_shapes(&w, *b, n) {
                shape_enable_pre_solve_events(&mut w, sh, true);
            }
        }
        world_set_pre_solve_callback(&mut w, Some(pre_solve), 0);
        world_set_friction_callback(&mut w, Some(friction));
        world_set_restitution_callback(&mut w, Some(restitution));
    }
    w
}

pub fn run() -> bool {
    println!("RESULT r15 hooks, phase-1 pile (stays active), 240 frames; timings noisy (low priority, shared machine)");
    for (side, layers) in [(8usize, 4usize), (16, 4)] {
        let mut plain = phase1_pile(side, layers, false);
        let mut hooked = phase1_pile(side, layers, true);
        let (mut t_plain, mut t_hooked) = (0.0f64, 0.0f64);
        let (mut max_pre, mut sum_pre, mut sum_mix) = (0u64, 0u64, 0u64);
        let frames = 240u64;
        for _ in 0..frames {
            let t = Instant::now();
            plain.step(DT, SUBSTEPS);
            t_plain += t.elapsed().as_secs_f64();
            let (p0, m0) = (PRE.load(Relaxed), MIX.load(Relaxed));
            let t = Instant::now();
            hooked.step(DT, SUBSTEPS);
            t_hooked += t.elapsed().as_secs_f64();
            let p = PRE.load(Relaxed) - p0;
            max_pre = max_pre.max(p);
            sum_pre += p;
            sum_mix += MIX.load(Relaxed) - m0;
        }
        let f = frames as f64;
        println!(
            "RESULT   {:4} bodies (awake at end {}): pre-solve {:.0} calls/step avg, {max_pre} max; friction+restitution mixing {:.1}/step; step {:.3} ms plain, {:.3} ms with native hooks",
            side * side * layers,
            world_get_awake_body_count(&hooked),
            sum_pre as f64 / f,
            sum_mix as f64 / f,
            t_plain * 1e3 / f,
            t_hooked * 1e3 / f
        );
    }
    println!("RESULT r15 hooks: calls per step of box3d-rust's existing hooks, 240 frames of a collapsing pile (4 substeps, 60 Hz); timings noisy (low priority, shared machine)");
    for n in [256usize, 1024] {
        let mut plain = pile(n, false);
        let mut hooked = pile(n, true);
        let (mut t_plain, mut t_hooked) = (0.0f64, 0.0f64);
        let (mut max_pre, mut sum_pre, mut sum_filter, mut sum_mix) = (0u64, 0u64, 0u64, 0u64);
        let frames = 240u64;
        for _ in 0..frames {
            let t = Instant::now();
            plain.step(DT, SUBSTEPS);
            t_plain += t.elapsed().as_secs_f64();
            let (p0, f0, m0) = (PRE.load(Relaxed), FILTER.load(Relaxed), MIX.load(Relaxed));
            let t = Instant::now();
            hooked.step(DT, SUBSTEPS);
            t_hooked += t.elapsed().as_secs_f64();
            let p = PRE.load(Relaxed) - p0;
            max_pre = max_pre.max(p);
            sum_pre += p;
            sum_filter += FILTER.load(Relaxed) - f0;
            sum_mix += MIX.load(Relaxed) - m0;
        }
        let f = frames as f64;
        println!(
            "RESULT   {n:4} bodies (awake at end {}): pre-solve {:.0} calls/step avg, {max_pre} max; pair filter {:.1}/step; friction+restitution mixing {:.1}/step; step {:.3} ms plain, {:.3} ms with native hooks",
            world_get_awake_body_count(&hooked),
            sum_pre as f64 / f,
            sum_filter as f64 / f,
            sum_mix as f64 / f,
            t_plain * 1e3 / f,
            t_hooked * 1e3 / f
        );
    }
    true
}

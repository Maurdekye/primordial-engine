//! A/B benchmark: upstream fixed contact-recycling distance vs the fork's scale-dependent radius
//! ("van der Waals simulation"), across object scales from millimetres to tens of metres.
//!
//! Scenes (s = nominal body size, the edge of the reference cube; every length scales with s):
//!   stack    10 cubes in a column, dropped with 0.1 s gaps and +-2.5% s lateral offsets
//!   pyramid  base-10 box pyramid (55 boxes), rows dropped with 0.05 s gaps
//!   pile     216 boxes of mixed size (largest edge s*{0.25,0.5,1,2}) and aspect (1 or 0.5 per
//!            axis), dropped in 3 layers into a walled bin
//!   tower    16-level Jenga-style tower of 3s x 0.6s x 1s planks (48 planks), 0.05 s gaps: the
//!            large-structure case
//!   ledge    8 frictionless cubes sliding at 0.5..4 s/second off the edge of a platform; measures
//!            how far past the edge a cube's centre gets before it starts to fall (stale-support
//!            artifact of recycling)
//!
//! Modes:
//!   off      recycling disabled (contact_recycle_distance = 0): narrow phase every frame
//!   fixed    upstream: one world-wide 0.05 m tolerance
//!   vdw      scale-dependent, default parameters (k = 0.1, Min combine, clamp [0.005, 0.5] m)
//!   vdw-hm   scale-dependent, harmonic-mean combine
//!
//! Every physics result is deterministic; only the wall-clock columns vary between runs. The
//! scenes are chaotic, so each configuration runs SEEDS slightly different initial conditions and
//! the table reports means over seeds. Seed 0 is run a second time in the same process and its
//! final world hash must match (an in-process determinism check).

use std::io::Write;
use std::time::Instant;

use box3d_rust::body::{body_get_linear_velocity, body_get_position, create_body};
use box3d_rust::hull::make_box_hull;
use box3d_rust::id::BodyId;
use box3d_rust::math_functions::{Pos, Vec3};
use box3d_rust::recording::hash_world_state;
use box3d_rust::recycle_scale::{ContactRecycleScaling, RecycleLengthCombine};
use box3d_rust::shape::create_hull_shape;
use box3d_rust::types::{default_body_def, default_shape_def, default_world_def, BodyType};
use box3d_rust::world::{
    world_get_awake_body_count, world_get_counters, world_set_contact_recycle_distance, World,
};

const SCALES: [f32; 6] = [0.001, 0.01, 0.1, 1.0, 10.0, 30.0];
const SCENES: [&str; 5] = ["stack", "pyramid", "pile", "tower", "ledge"];
const MODES: [&str; 4] = ["off", "fixed", "vdw", "vdw-hm"];
const SEEDS: usize = 5;
const DT: f32 = 1.0 / 60.0;
const SUBSTEPS: i32 = 4;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// Has an ideal rest pose; reports collapse and sag.
    Structural,
    /// Reports tunnelling (bodies below the ground).
    Pile,
    /// Reports the mean overhang at drop.
    Ledge,
}

struct Built {
    world: World,
    bodies: Vec<BodyId>,
    /// Ideal resting pose (structural scenes) or initial pose.
    reference: Vec<Pos>,
    kind: Kind,
    /// Ledge: platform-top height and edge x.
    ledge_top: f32,
}

fn mode_world(mode: &str) -> World {
    let mut def = default_world_def();
    match mode {
        "vdw" => def.contact_recycle_scaling = ContactRecycleScaling::enabled(),
        "vdw-hm" => {
            def.contact_recycle_scaling = ContactRecycleScaling {
                combine: RecycleLengthCombine::HarmonicMean,
                ..ContactRecycleScaling::enabled()
            }
        }
        _ => {}
    }
    let mut world = World::new(&def);
    if mode == "off" {
        world_set_contact_recycle_distance(&mut world, 0.0);
    }
    world
}

fn static_box(world: &mut World, p: Pos, hx: f32, hy: f32, hz: f32) {
    let mut bd = default_body_def();
    bd.type_ = BodyType::Static;
    bd.position = p;
    let b = create_body(world, &bd);
    let hull = make_box_hull(hx, hy, hz);
    create_hull_shape(world, b, &default_shape_def(), &hull.base);
}

fn dyn_box_v(world: &mut World, p: Pos, he: Vec3, v: Vec3, friction: f32, sleep: bool) -> BodyId {
    let mut bd = default_body_def();
    bd.enable_sleep = sleep;
    bd.type_ = BodyType::Dynamic;
    bd.position = p;
    bd.linear_velocity = v;
    let b = create_body(world, &bd);
    let hull = make_box_hull(he.x, he.y, he.z);
    let mut sd = default_shape_def();
    sd.density = 1.0;
    sd.base_material.friction = friction;
    create_hull_shape(world, b, &sd, &hull.base);
    b
}

fn dyn_box(world: &mut World, p: Pos, hx: f32, hy: f32, hz: f32) -> BodyId {
    let f = default_shape_def().base_material.friction;
    dyn_box_v(world, p, Vec3 { x: hx, y: hy, z: hz }, Vec3 { x: 0.0, y: 0.0, z: 0.0 }, f, true)
}

/// Deterministic value in [0, 1) from an index (golden-ratio sequence).
fn frac(i: usize, salt: f32) -> f32 {
    ((i as f32 + salt) * 0.618_034).fract()
}

fn build(scene: &str, mode: &str, s: f32, seed: usize) -> Built {
    let mut world = mode_world(mode);
    // Seeded initial-condition variation: the scenes are chaotic, so every metric is averaged
    // over SEEDS slightly different starts.
    let frac = |i: usize, salt: f32| frac(i + 37 * seed, salt);
    // Ground: top face at y = 0.
    let ground_half = 200.0 * s + 5.0;
    static_box(&mut world, Pos { x: 0.0, y: -s, z: 0.0 }, ground_half, s, ground_half);
    let h = 0.5 * s;
    let mut bodies = Vec::new();
    let mut reference = Vec::new();
    let mut kind = Kind::Structural;
    let mut ledge_top = 0.0;
    match scene {
        "stack" => {
            for i in 0..10 {
                let off = (frac(i, 0.3) - 0.5) * 0.05 * s;
                let y_rest = h + s * i as f32;
                bodies.push(dyn_box(&mut world, Pos { x: off, y: y_rest + 0.1 * s * (i + 1) as f32, z: -off }, h, h, h));
                reference.push(Pos { x: off, y: y_rest, z: -off });
            }
        }
        "pyramid" => {
            let base = 10;
            let pitch = 1.05 * s;
            for row in 0..base {
                let n = base - row;
                for j in 0..n {
                    let x = (j as f32 - (n as f32 - 1.0) * 0.5) * pitch + (frac(row * 16 + j, 0.21) - 0.5) * 0.02 * s;
                    let y_rest = h + s * row as f32;
                    bodies.push(dyn_box(&mut world, Pos { x, y: y_rest + 0.05 * s * (row + 1) as f32, z: 0.0 }, h, h, h));
                    reference.push(Pos { x, y: y_rest, z: 0.0 });
                }
            }
        }
        "pile" | "bigpile" => {
            kind = Kind::Pile;
            let (nx, nz, layers) = if scene == "pile" { (12usize, 6usize, 3usize) } else { (18, 12, 4) };
            let pitch = 2.4 * s;
            let hbx = nx as f32 * pitch * 0.5 + s;
            let hbz = nz as f32 * pitch * 0.5 + s;
            // Bin walls, 4 s high.
            for (x, z, hx, hz) in [
                (hbx + 0.5 * s, 0.0, 0.5 * s, hbz + s),
                (-hbx - 0.5 * s, 0.0, 0.5 * s, hbz + s),
                (0.0, hbz + 0.5 * s, hbx + s, 0.5 * s),
                (0.0, -hbz - 0.5 * s, hbx + s, 0.5 * s),
            ] {
                static_box(&mut world, Pos { x, y: 2.0 * s, z }, hx, 2.0 * s, hz);
            }
            let mut n = 0usize;
            for layer in 0..layers {
                for ix in 0..nx {
                    for iz in 0..nz {
                        let a = 0.5 * s * [0.25, 0.5, 1.0, 2.0][(frac(n, 0.11) * 4.0) as usize];
                        let ry = if frac(n, 0.37) < 0.5 { 1.0 } else { 0.5 };
                        let rz = if frac(n, 0.53) < 0.5 { 1.0 } else { 0.5 };
                        let jitter = (frac(n, 0.7) - 0.5) * 0.3 * s;
                        let p = Pos {
                            x: (ix as f32 - (nx as f32 - 1.0) * 0.5) * pitch + jitter,
                            y: 1.2 * s + layer as f32 * pitch,
                            z: (iz as f32 - (nz as f32 - 1.0) * 0.5) * pitch - jitter,
                        };
                        bodies.push(dyn_box(&mut world, p, a, a * ry, a * rz));
                        reference.push(p);
                        n += 1;
                    }
                }
            }
        }
        "tower" => {
            // Planks 3s long, 1s wide, 0.6s thick; 3 per level, alternating direction.
            let (hl, hw, ht) = (1.5 * s, 0.5 * s, 0.3 * s);
            for level in 0..16 {
                let y_rest = ht + level as f32 * 2.0 * ht;
                let y = y_rest + 0.05 * s * (level + 1) as f32;
                for k in 0..3 {
                    let o = (k as f32 - 1.0) * 1.02 * s + (frac(level * 3 + k, 0.43) - 0.5) * 0.02 * s;
                    let (b, r) = if level % 2 == 0 {
                        (dyn_box(&mut world, Pos { x: o, y, z: 0.0 }, hw, ht, hl), Pos { x: o, y: y_rest, z: 0.0 })
                    } else {
                        (dyn_box(&mut world, Pos { x: 0.0, y, z: o }, hl, ht, hw), Pos { x: 0.0, y: y_rest, z: o })
                    };
                    bodies.push(b);
                    reference.push(r);
                }
            }
        }
        "ledge" => {
            kind = Kind::Ledge;
            // Platform top at y = 2 s, edge at x = 0.
            ledge_top = 2.0 * s;
            static_box(&mut world, Pos { x: -10.0 * s, y: s, z: 0.0 }, 10.0 * s, s, 10.0 * s);
            for k in 0..8 {
                let v = (0.5 + 0.5 * k as f32) * s;
                let p = Pos { x: -6.0 * s - frac(k, 0.9) * 0.5 * s, y: ledge_top + h, z: (k as f32 - 3.5) * 2.0 * s };
                let he = Vec3 { x: h, y: h, z: h };
                bodies.push(dyn_box_v(&mut world, p, he, Vec3 { x: v, y: 0.0, z: 0.0 }, 0.0, false));
                reference.push(p);
            }
        }
        _ => unreachable!(),
    }
    Built { world, bodies, reference, kind, ledge_top }
}

#[derive(Clone, Debug)]
struct RunResult {
    kind_structural: bool,
    kind_ledge: bool,
    step_ms_mean: f64,
    /// Mean step time over frames that started with at least one awake body.
    step_ms_awake: f64,
    step_ms_max: f64,
    contacts_mean: f64,
    hit_rate: f64,
    sleep_s: Option<f64>,
    jitter: f64,
    drift_mean: f64,
    drift_max: f64,
    /// Structural: mean drop below the ideal rest pose / s. Pile: bodies below the ground.
    /// Ledge: mean x of the cube centre past the edge when it has dropped 0.05 s, / s.
    extra: f64,
    displaced: usize,
    final_hash: u64,
}

fn run_one(scene: &str, mode: &str, s: f32, frames: u64, seed: usize) -> RunResult {
    let mut b = build(scene, mode, s, seed);
    let window_start = frames * 2 / 3;
    let mut p_window = b.reference.clone();
    let mut step_total = 0.0f64;
    let mut step_awake_total = 0.0f64;
    let mut awake_frames = 0u64;
    let mut step_max = 0.0f64;
    let mut contacts_sum = 0.0f64;
    let mut awake_contacts_sum = 0.0f64;
    let mut recycled_sum = 0.0f64;
    let mut sleep_frame = None;
    let mut jitter_sum = 0.0f64;
    let mut jitter_frames = 0u64;
    let mut overhang: Vec<Option<f64>> = vec![None; b.bodies.len()];
    for f in 0..frames {
        if f == window_start {
            p_window = b.bodies.iter().map(|&id| body_get_position(&b.world, id)).collect();
        }
        let was_awake = world_get_awake_body_count(&b.world) > 0;
        let t = Instant::now();
        b.world.step(DT, SUBSTEPS);
        let e = t.elapsed().as_secs_f64() * 1e3;
        step_total += e;
        if was_awake {
            step_awake_total += e;
            awake_frames += 1;
        }
        step_max = step_max.max(e);
        let c = world_get_counters(&b.world);
        contacts_sum += c.contact_count as f64;
        // collide() returns before resetting its per-task counters when no contact is awake,
        // so recycled_contact_count is stale once everything sleeps: count it only while awake.
        if c.awake_contact_count > 0 {
            awake_contacts_sum += c.awake_contact_count as f64;
            recycled_sum += c.recycled_contact_count as f64;
        }
        if sleep_frame.is_none() && world_get_awake_body_count(&b.world) == 0 {
            sleep_frame = Some(f + 1);
        }
        if b.kind == Kind::Ledge {
            for (k, &id) in b.bodies.iter().enumerate() {
                if overhang[k].is_none() {
                    let p = body_get_position(&b.world, id);
                    if p.y < b.ledge_top + 0.5 * s - 0.05 * s {
                        overhang[k] = Some(p.x as f64 / s as f64);
                    }
                }
            }
        }
        if f >= window_start {
            let mut v_sum = 0.0f64;
            for &id in &b.bodies {
                let v = body_get_linear_velocity(&b.world, id);
                v_sum += ((v.x * v.x + v.y * v.y + v.z * v.z) as f64).sqrt();
            }
            jitter_sum += v_sum / b.bodies.len() as f64 / s as f64;
            jitter_frames += 1;
        }
    }
    let pf: Vec<Pos> = b.bodies.iter().map(|&id| body_get_position(&b.world, id)).collect();
    let dist = |a: Pos, c: Pos| {
        (((a.x - c.x) as f64).powi(2) + ((a.y - c.y) as f64).powi(2) + ((a.z - c.z) as f64).powi(2)).sqrt()
    };
    let n = b.bodies.len() as f64;
    let drifts: Vec<f64> = pf.iter().zip(&p_window).map(|(a, c)| dist(*a, *c) / s as f64).collect();
    let mut displaced = 0usize;
    let mut sag = 0.0f64;
    let mut tunneled = 0usize;
    for (a, o) in pf.iter().zip(&b.reference) {
        let horiz = (((a.x - o.x) as f64).powi(2) + ((a.z - o.z) as f64).powi(2)).sqrt();
        let drop = (o.y - a.y) as f64;
        if horiz > 0.5 * s as f64 || drop.abs() > 0.5 * s as f64 {
            displaced += 1;
        }
        sag += drop / s as f64;
        if a.y < -2.0 * s {
            tunneled += 1;
        }
    }
    let extra = match b.kind {
        Kind::Structural => sag / n,
        Kind::Pile => tunneled as f64,
        Kind::Ledge => {
            // A cube that never dropped counts as NaN so it cannot hide in the mean.
            overhang.iter().map(|o| o.unwrap_or(f64::NAN)).sum::<f64>() / n
        }
    };
    RunResult {
        kind_structural: b.kind == Kind::Structural,
        kind_ledge: b.kind == Kind::Ledge,
        step_ms_mean: step_total / frames as f64,
        step_ms_awake: step_awake_total / awake_frames.max(1) as f64,
        step_ms_max: step_max,
        contacts_mean: contacts_sum / frames as f64,
        hit_rate: if awake_contacts_sum > 0.0 { recycled_sum / awake_contacts_sum } else { 0.0 },
        sleep_s: sleep_frame.map(|f| f as f64 * DT as f64),
        jitter: jitter_sum / jitter_frames.max(1) as f64,
        drift_mean: drifts.iter().sum::<f64>() / n,
        drift_max: drifts.iter().cloned().fold(0.0, f64::max),
        extra,
        displaced,
        final_hash: hash_world_state(&b.world),
    }
}

fn body_count(scene: &str) -> usize {
    match scene {
        "stack" => 10,
        "pyramid" => 55,
        "pile" => 216,
        "tower" => 48,
        "bigpile" => 864,
        _ => 8,
    }
}

pub fn run(frames: u64, out: Option<&str>) -> bool {
    let mut ok = true;
    let mut jsonl = out.map(|p| std::fs::File::create(p).expect("create vdw-bench output"));
    println!(
        "vdw-bench: {frames} frames at 60 Hz, {SUBSTEPS} substeps, {SEEDS} seeds/config (means over seeds; drift max = max over seeds; \
         sleep = mean over seeds that slept, +Nn = N seeds never slept; collapse = seeds collapsed; step ms = all frames, awake ms = frames with awake bodies); \
         jitter = mean |v| / s over the last third (body sizes per second); drift = displacement over the last third / s; \
         hit = recycled / awake contacts; extra = sag / s (stack, pyramid, tower), tunnelled bodies (pile), \
         overhang at drop / s (ledge)"
    );
    println!(
        "{:<8} {:>7} {:<7} {:>9} {:>9} {:>8} {:>6} {:>9} {:>10} {:>10} {:>10} {:>8} {:<9} {:>16}",
        "scene", "s (m)", "mode", "step ms", "awake ms", "contacts", "hit", "sleep s", "jitter", "drift", "drift max", "extra", "collapse", "final hash"
    );
    for scene in SCENES {
        for s in SCALES {
            for mode in MODES {
                let runs: Vec<RunResult> = (0..SEEDS).map(|seed| run_one(scene, mode, s, frames, seed)).collect();
                // Determinism: re-run seed 0 in the same process; the final world hash must match.
                let again = run_one(scene, mode, s, frames, 0);
                let det = again.final_hash == runs[0].final_hash;
                ok &= det;
                let k = SEEDS as f64;
                let mean = |f: &dyn Fn(&RunResult) -> f64| runs.iter().map(f).sum::<f64>() / k;
                let slept: Vec<f64> = runs.iter().filter_map(|r| r.sleep_s).collect();
                let never = SEEDS - slept.len();
                let sleep_label = if slept.is_empty() {
                    "never".to_string()
                } else {
                    let m = slept.iter().sum::<f64>() / slept.len() as f64;
                    if never > 0 { format!("{m:.2}+{never}n") } else { format!("{m:.2}") }
                };
                let collapsed = runs.iter().filter(|r| r.kind_structural && r.displaced > 0).count();
                let collapse_label = if runs[0].kind_structural { format!("{collapsed}/{SEEDS}") } else { "-".into() };
                let extra_vals: Vec<f64> = runs.iter().map(|r| r.extra).filter(|x| x.is_finite()).collect();
                let extra = if extra_vals.len() == SEEDS { extra_vals.iter().sum::<f64>() / k } else { f64::NAN };
                println!(
                    "{:<8} {:>7} {:<7} {:>9.4} {:>9.4} {:>8.1} {:>5.1}% {:>9} {:>10.2e} {:>10.2e} {:>10.2e} {:>8.4} {:<9} {:016x}{}",
                    scene,
                    s,
                    mode,
                    mean(&|r| r.step_ms_mean),
                    mean(&|r| r.step_ms_awake),
                    mean(&|r| r.contacts_mean),
                    mean(&|r| r.hit_rate) * 100.0,
                    sleep_label,
                    mean(&|r| r.jitter),
                    mean(&|r| r.drift_mean),
                    runs.iter().map(|r| r.drift_max).fold(0.0, f64::max),
                    extra,
                    collapse_label,
                    runs[0].final_hash,
                    if det { "" } else { "  NONDETERMINISTIC" }
                );
                if let Some(f) = jsonl.as_mut() {
                    let num = |x: f64| if x.is_finite() { format!("{x:.6e}") } else { "null".into() };
                    for (seed, r) in runs.iter().enumerate() {
                        writeln!(
                            f,
                            "{{\"scene\":\"{scene}\",\"scale_m\":{s},\"mode\":\"{mode}\",\"seed\":{seed},\"frames\":{frames},\"bodies\":{},\"step_ms_mean\":{:.6},\"step_ms_awake\":{:.6},\"step_ms_max\":{:.5},\"contacts_mean\":{:.2},\"hit_rate\":{:.5},\"sleep_s\":{},\"jitter\":{},\"drift_mean\":{},\"drift_max\":{},\"extra\":{},\"collapsed\":{},\"displaced\":{},\"ledge\":{},\"final_hash\":\"{:016x}\",\"deterministic_rerun\":{}}}",
                            body_count(scene),
                            r.step_ms_mean,
                            r.step_ms_awake,
                            r.step_ms_max,
                            r.contacts_mean,
                            r.hit_rate,
                            r.sleep_s.map(|x| format!("{x:.4}")).unwrap_or_else(|| "null".into()),
                            num(r.jitter),
                            num(r.drift_mean),
                            num(r.drift_max),
                            num(r.extra),
                            r.kind_structural && r.displaced > 0,
                            r.displaced,
                            r.kind_ledge,
                            r.final_hash,
                            if seed == 0 { det.to_string() } else { "null".into() }
                        )
                        .unwrap();
                    }
                }
            }
        }
    }
    ok
}

/// Performance-focused A/B: an 864-box mixed pile, modes interleaved per repeat so slow drift of
/// the machine (thermal, background load) hits every mode equally. Reports the median over
/// repeats of the mean step time, the mean step time over frames with awake bodies, the recycle
/// hit rate and the mean contact count. Physics is deterministic, so only timing varies.
pub fn perf(frames: u64, repeats: usize, out: Option<&str>) -> bool {
    let scales = [0.1f32, 1.0, 10.0, 30.0];
    let mut ok = true;
    let mut jsonl = out.map(|p| std::fs::File::create(p).expect("create vdw-perf output"));
    println!("vdw-perf: bigpile (864 boxes), {frames} frames, {repeats} interleaved repeats, median of per-run means");
    println!(
        "{:>6} {:<7} {:>10} {:>10} {:>10} {:>7} {:>9} {:>16}",
        "s (m)", "mode", "step ms", "awake ms", "min awake", "hit", "contacts", "final hash"
    );
    for s in scales {
        let mut per_mode: Vec<Vec<RunResult>> = vec![Vec::new(); MODES.len()];
        for _ in 0..repeats {
            for (m, mode) in MODES.iter().enumerate() {
                per_mode[m].push(run_one("bigpile", mode, s, frames, 0));
            }
        }
        for (m, mode) in MODES.iter().enumerate() {
            let runs = &mut per_mode[m];
            let h0 = runs[0].final_hash;
            let det = runs.iter().all(|r| r.final_hash == h0);
            ok &= det;
            let median = |v: &mut Vec<f64>| {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                v[v.len() / 2]
            };
            let step = median(&mut runs.iter().map(|r| r.step_ms_mean).collect());
            let awake = median(&mut runs.iter().map(|r| r.step_ms_awake).collect());
            let min_awake = runs.iter().map(|r| r.step_ms_awake).fold(f64::INFINITY, f64::min);
            let r = &runs[0];
            println!(
                "{:>6} {:<7} {:>10.4} {:>10.4} {:>10.4} {:>6.1}% {:>9.1} {:016x}{}",
                s,
                mode,
                step,
                awake,
                min_awake,
                r.hit_rate * 100.0,
                r.contacts_mean,
                h0,
                if det { "" } else { "  NONDETERMINISTIC" }
            );
            if let Some(f) = jsonl.as_mut() {
                let all: Vec<String> = runs.iter().map(|r| format!("{:.6}", r.step_ms_awake)).collect();
                writeln!(
                    f,
                    "{{\"scene\":\"bigpile\",\"scale_m\":{s},\"mode\":\"{mode}\",\"frames\":{frames},\"bodies\":864,\"step_ms_median\":{step:.6},\"awake_ms_median\":{awake:.6},\"awake_ms_runs\":[{}],\"hit_rate\":{:.5},\"contacts_mean\":{:.2},\"sleep_s\":{},\"final_hash\":\"{h0:016x}\",\"deterministic_repeats\":{det}}}",
                    all.join(","),
                    r.hit_rate,
                    r.contacts_mean,
                    r.sleep_s.map(|x| format!("{x:.4}")).unwrap_or_else(|| "null".into()),
                )
                .unwrap();
            }
        }
    }
    ok
}

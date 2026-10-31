//! Perturbation-history study, physics side (design 6.7-6.8):
//!
//! * re-simulation cost against rollback depth (1..120 frames) from one restored base image,
//!   for the 256- and 1,024-body piles, while the pile is active and once it has settled;
//! * whether a partial restore through box3d-rust's public API (pose and velocity of the
//!   perturbed bodies only) is bit-exact;
//! * how much of a world image changes between frames (block deltas against a base).

use std::time::Instant;

use box3d_rust::body::{body_apply_linear_impulse_to_center, create_body};
use box3d_rust::hull::make_box_hull;
use box3d_rust::id::BodyId;
use box3d_rust::math_functions::Vec3;
use box3d_rust::recording::{deserialize_into_shell, hash_world_state, serialize_world, GeometryRegistry, RecBuffer, RegistrySlot};
use box3d_rust::shape::create_hull_shape;
use box3d_rust::types::{default_body_def, default_shape_def, BodyType};
use box3d_rust::world::{world_get_awake_body_count, World};
use pe_sim_core::session::{block_delta, build_scene, BodyState, SceneKind};
use pe_sim_core::{pos, DT, SUBSTEPS};

/// Full world images, the way the session layer takes them.
struct Imager {
    reg: GeometryRegistry,
    slots: Vec<RegistrySlot>,
}

impl Imager {
    fn new() -> Self {
        Imager { reg: GeometryRegistry::new(), slots: Vec::new() }
    }
    fn save(&mut self, w: &World) -> Vec<u8> {
        let mut b = RecBuffer::new();
        serialize_world(w, &mut b, &mut self.reg);
        self.slots = self.reg.to_slots();
        b.data
    }
    fn restore(&mut self, img: &[u8], w: &mut World) {
        assert!(deserialize_into_shell(img, w, &mut self.slots), "restore failed");
    }
}

fn step(w: &mut World, n: u64) {
    for _ in 0..n {
        w.step(DT, SUBSTEPS);
    }
}

fn states(w: &World, bodies: &[BodyId]) -> Vec<BodyState> {
    bodies.iter().map(|&b| BodyState::read(w, b)).collect()
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

const DEPTHS: [u64; 9] = [1, 2, 4, 8, 15, 30, 45, 60, 120];

/// Restore one base image, then re-simulate to each depth: the cost of a rollback that has to
/// start from the base (nothing but the log in between).
fn resim_vs_depth() -> bool {
    println!("RESULT re-sim from one base image: restore + N steps (median of 5), Box3D 4 substeps at 60 Hz, single thread");
    for (side, layers) in [(8usize, 4usize), (16, 4)] {
        for (label, at) in [("active (base at frame 60, pile collapsing)", 60u64), ("late (base at frame 900)", 900)] {
            let (mut w, objs) = build_scene(SceneKind::Pile { side, layers }, 0);
            let n = objs.len();
            step(&mut w, at);
            let awake = world_get_awake_body_count(&w);
            let mut im = Imager::new();
            let mut saves = Vec::new();
            let mut img = Vec::new();
            for _ in 0..5 {
                let t = Instant::now();
                img = im.save(&w);
                saves.push(t.elapsed().as_secs_f64() * 1e3);
            }
            let mut restore = Vec::new();
            let mut cum: Vec<Vec<f64>> = vec![Vec::new(); DEPTHS.len()];
            let mut end_hash = None;
            for _ in 0..5 {
                let t = Instant::now();
                im.restore(&img, &mut w);
                restore.push(t.elapsed().as_secs_f64() * 1e3);
                let t = Instant::now();
                let mut done = 0;
                for (i, &d) in DEPTHS.iter().enumerate() {
                    step(&mut w, d - done);
                    done = d;
                    cum[i].push(t.elapsed().as_secs_f64() * 1e3);
                }
                let h = hash_world_state(&w);
                assert!(end_hash.is_none_or(|e| e == h), "re-sim from the same image must be bit-exact");
                end_hash = Some(h);
            }
            // Whether the pile stays awake through the deepest re-simulation decides how deep
            // re-simulations scale (a sleeping island costs almost nothing).
            let awake_end = world_get_awake_body_count(&w);
            let r = median(restore);
            let row: Vec<String> = DEPTHS.iter().zip(cum).map(|(d, v)| format!("{d}:{:.1}", r + median(v))).collect();
            println!(
                "RESULT   {n:4} bodies {label}: awake {awake}, image {:.2} MB, save {:.2} ms, restore {:.2} ms | depth:ms {} | awake after {} frames {awake_end}",
                img.len() as f64 / 1_048_576.0,
                median(saves),
                r,
                row.join(" "),
                DEPTHS[DEPTHS.len() - 1]
            );
        }
    }
    true
}

/// 64 separate 4-box stacks, 3 m apart, on one static ground: 64 islands that never touch.
fn stacks() -> (World, Vec<BodyId>) {
    let mut w = World::new(&pe_sim_core::engine_world_def());
    let mut g = default_body_def();
    g.type_ = BodyType::Static;
    g.position = pos(0.0, -0.5, 0.0);
    let gb = create_body(&mut w, &g);
    create_hull_shape(&mut w, gb, &default_shape_def(), &make_box_hull(20.0, 0.5, 20.0).base);
    let mut bodies = Vec::new();
    for s in 0..64 {
        for k in 0..4 {
            let mut d = default_body_def();
            d.type_ = BodyType::Dynamic;
            d.position = pos(-10.5 + 3.0 * (s % 8) as f32, 0.25 + 0.5 * k as f32, -10.5 + 3.0 * (s / 8) as f32);
            let b = create_body(&mut w, &d);
            let mut sd = default_shape_def();
            sd.density = 1.0;
            create_hull_shape(&mut w, b, &sd, &make_box_hull(0.25, 0.25, 0.25).base);
            bodies.push(b);
        }
    }
    (w, bodies)
}

fn compare(a: &[BodyState], b: &[BodyState]) -> (usize, f32) {
    let mut equal = 0;
    let mut err = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        if x == y {
            equal += 1;
        }
        let (p, q) = (x.pos(), y.pos());
        err = err.max(((p.x - q.x).powi(2) + (p.y - q.y).powi(2) + (p.z - q.z).powi(2)).sqrt() as f32);
    }
    (equal, err)
}

/// Partial restores through the public API, against a full image restore.
fn partial_restore() -> bool {
    let mut ok = true;
    println!("RESULT partial restore through the box3d-rust API (body_set_transform + set velocities) vs full image restore");
    // (1) Every body's pose and velocity written back, after the world ran on.
    {
        let (mut w, objs) = build_scene(SceneKind::Pile { side: 8, layers: 4 }, 0);
        let bodies: Vec<BodyId> = objs.iter().map(|o| o.unwrap().body).collect();
        step(&mut w, 60);
        let mut im = Imager::new();
        let img = im.save(&w);
        let s0 = states(&w, &bodies);
        let mut reference = Vec::new();
        im.restore(&img, &mut w);
        for _ in 0..30 {
            step(&mut w, 1);
            reference.push(states(&w, &bodies));
        }
        // Control: a second full restore reproduces the reference exactly.
        im.restore(&img, &mut w);
        step(&mut w, 30);
        let (eq_full, _) = compare(&states(&w, &bodies), &reference[29]);
        // Partial: from a world that ran 20 frames further, write back every body's state.
        step(&mut w, 20);
        for (b, s) in bodies.iter().zip(&s0) {
            s.write(&mut w, *b);
        }
        let mut first_bad = None;
        let mut last = (0, 0.0);
        for (i, r) in reference.iter().enumerate() {
            step(&mut w, 1);
            last = compare(&states(&w, &bodies), r);
            if last.0 != bodies.len() && first_bad.is_none() {
                first_bad = Some(i + 1);
            }
        }
        println!(
            "RESULT   256-body pile, active: full restore {}/{} bodies bit-equal after 30 frames; ALL bodies' pose+velocity written back: first mismatch after frame {:?}, after 30 frames {}/{} bit-equal, max position error {:.2} mm",
            eq_full,
            bodies.len(),
            first_bad,
            last.0,
            bodies.len(),
            last.1 * 1000.0
        );
        ok &= eq_full == bodies.len();
    }
    // (2) Island-scoped: only the perturbed stack written back; the 63 others sleep untouched.
    {
        let (mut w, bodies) = stacks();
        step(&mut w, 600);
        let awake0 = world_get_awake_body_count(&w);
        let mut im = Imager::new();
        let img = im.save(&w);
        let s0 = states(&w, &bodies);
        let top = bodies[3];
        let p = Vec3 { x: 0.12, y: 0.0, z: 0.0 };
        let wrong = Vec3 { x: -0.08, y: 0.04, z: 0.0 };
        im.restore(&img, &mut w);
        body_apply_linear_impulse_to_center(&mut w, top, p, true);
        step(&mut w, 60);
        let reference = states(&w, &bodies);
        im.restore(&img, &mut w);
        body_apply_linear_impulse_to_center(&mut w, top, wrong, true);
        step(&mut w, 30);
        let untouched_same = states(&w, &bodies)[4..] == s0[4..];
        for i in 0..4 {
            s0[i].write(&mut w, bodies[i]);
        }
        body_apply_linear_impulse_to_center(&mut w, top, p, true);
        step(&mut w, 60);
        let got = states(&w, &bodies);
        let (eq_island, err) = compare(&got[..4], &reference[..4]);
        let (eq_rest, _) = compare(&got[4..], &reference[4..]);
        println!(
            "RESULT   64 isolated 4-box stacks, settled (awake {awake0}): perturbed stack restored by API, others asleep and untouched ({}): perturbed stack {}/4 bit-equal after 60 frames (max error {:.2} mm), other stacks {}/252 bit-equal",
            if untouched_same { "verified unchanged" } else { "CHANGED" },
            eq_island,
            err * 1000.0,
            eq_rest
        );
    }
    ok
}

/// How much of the image changes: 64-byte block deltas against a base image k frames older.
fn image_deltas() -> bool {
    println!("RESULT image deltas: bytes to store an image as 64 B blocks changed vs a base k frames older (8 B per block index)");
    for (side, layers) in [(8usize, 4usize), (16, 4)] {
        for (label, at) in [("active (frame 60)", 60u64), ("late (frame 900)", 900)] {
            let (mut w, objs) = build_scene(SceneKind::Pile { side, layers }, 0);
            let n = objs.len();
            step(&mut w, at);
            let mut im = Imager::new();
            let base = im.save(&w);
            let mut row = Vec::new();
            let mut done = 0;
            let mut cost = 0.0;
            for k in [1u64, 4, 16, 60] {
                step(&mut w, k - done);
                done = k;
                let img = im.save(&w);
                let t = Instant::now();
                let d = block_delta(&base, &img, 64);
                cost = t.elapsed().as_secs_f64() * 1e3;
                row.push(format!("k={k}: {:.0}%", 100.0 * d as f64 / img.len() as f64));
            }
            println!("RESULT   {n:4} bodies {label}: image {:.2} MB | {} | diff scan {:.2} ms", base.len() as f64 / 1_048_576.0, row.join(", "), cost);
        }
    }
    // A settled pile with one touch: how local is the change?
    let (mut w, objs) = build_scene(SceneKind::Pile { side: 16, layers: 4 }, 0);
    step(&mut w, 900);
    let mut im = Imager::new();
    let base = im.save(&w);
    body_apply_linear_impulse_to_center(&mut w, objs[1000].unwrap().body, Vec3 { x: 0.0, y: 6.0, z: 0.0 }, true);
    let mut row = Vec::new();
    let mut done = 0;
    for k in [1u64, 4, 16, 60] {
        step(&mut w, k - done);
        done = k;
        let img = im.save(&w);
        row.push(format!("k={k}: {:.0}% (awake {})", 100.0 * block_delta(&base, &img, 64) as f64 / img.len() as f64, world_get_awake_body_count(&w)));
    }
    println!("RESULT   1024 bodies late (frame 900), one body pushed: {}", row.join(", "));
    true
}

pub fn run() -> bool {
    let a = resim_vs_depth();
    let b = partial_restore();
    let c = image_deltas();
    a && b && c
}

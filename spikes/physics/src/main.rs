//! Phase-1 spike (a): box3d-rust determinism, and spike (b): rollback snapshot/restore.
//!
//! Subcommands:
//!   goldens                      the four Box3D determinism scenes vs engine goldens (C goldens with --upstream)
//!   trace <frames> <out-file>    pile scene + scripted interactions, write per-frame hashes
//!   rollback                     snapshot/restore + late-input rollback tests, with timings
//!   bench                        step / snapshot / restore cost at several body counts
//!   vdw-bench [frames] [out.jsonl]  A/B of fixed vs scale-dependent contact recycling (vdw_bench.rs)
//!   vdw-perf [frames] [repeats] [out.jsonl] [scene]  timing A/B on an 864-box pile
//!                                (scene bigpile, or bigpile-awake with sleeping off)
//!   images <out-file>             FNV-64 of the pile's snapshot image every 50 frames
//!
//! Every world the subcommand creates uses the engine world definition
//! (`pe_sim_core::engine_world_def`: scale-dependent contact recycling on, user ruling
//! 2026-10-30). Global flag `--upstream` uses Box3D's own `default_world_def` instead (fixed
//! recycle distance), which is what the C goldens apply to. `--vdw` is accepted and means the
//! default. `--no-recycle` (goldens only) sets the recycle distance to 0, as a
//! no-approximation reference.
//!
//! Exit code is non-zero if any check fails.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use box3d_rust::determinism::{
    create_falling_ragdolls, create_mesh_drop, create_query_spawn, create_wave_pile,
    update_falling_ragdolls, update_mesh_drop, update_query_spawn, update_wave_pile,
};
use box3d_rust::math_functions::POS_ZERO;
use box3d_rust::types::{default_world_def, WorldDef};
use box3d_rust::world::World;
use pe_sim_core::*;

mod vdw_bench;

const SEED: u64 = 0x5052_494D_4F52_4449; // "PRIMORDI"

static UPSTREAM: AtomicBool = AtomicBool::new(false);
static NO_RECYCLE: AtomicBool = AtomicBool::new(false);

/// True unless `--upstream`: worlds use scale-dependent contact recycling.
fn vdw_on() -> bool {
    !UPSTREAM.load(Ordering::Relaxed)
}

/// World definition for every world this binary creates: the engine's, or upstream's with
/// `--upstream`.
fn world_def() -> WorldDef {
    if vdw_on() { engine_world_def() } else { default_world_def() }
}

/// Goldens world: `--no-recycle` additionally disables contact recycling (reference point only).
fn goldens_world() -> World {
    let mut world = World::new(&world_def());
    if NO_RECYCLE.load(Ordering::Relaxed) {
        box3d_rust::world::world_set_contact_recycle_distance(&mut world, 0.0);
    }
    world
}

fn pile_scene(side: usize, layers: usize) -> Scene {
    build_pile_scene_def(side, layers, &world_def())
}

fn main() {
    let all_args: Vec<String> = std::env::args().collect();
    UPSTREAM.store(all_args.iter().any(|a| a == "--upstream"), Ordering::Relaxed);
    NO_RECYCLE.store(all_args.iter().any(|a| a == "--no-recycle"), Ordering::Relaxed);
    let args: Vec<String> =
        all_args.into_iter().filter(|a| !matches!(a.as_str(), "--vdw" | "--upstream" | "--no-recycle")).collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    let ok = match cmd {
        "goldens" => goldens(),
        "trace" => {
            let frames: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(600);
            let out = args.get(3).cloned().unwrap_or_else(|| "trace.txt".into());
            trace(frames, &out)
        }
        "rollback" => rollback(),
        "bench" => bench(),
        "vdw-bench" => {
            let frames: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(900);
            vdw_bench::run(frames, args.get(3).map(String::as_str))
        }
        "vdw-perf" => {
            let frames: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1200);
            let repeats: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
            let scene = args.get(5).map(String::as_str).unwrap_or("bigpile");
            vdw_bench::perf(frames, repeats, args.get(4).map(String::as_str), scene)
        }
        "images" => images(args.get(2).map(String::as_str).unwrap_or("images.txt")),
        _ => {
            eprintln!("usage: pe-spike-physics [--upstream] [--no-recycle] goldens | trace <frames> <out> | rollback | bench | vdw-bench [frames] [out] | vdw-perf [frames] [repeats] [out] [bigpile|bigpile-awake] | images <out>");
            false
        }
    };
    println!(
        "build: {} profile, target {}, contact recycling: {}",
        profile(),
        std::env::consts::ARCH,
        if NO_RECYCLE.load(Ordering::Relaxed) {
            "none (--no-recycle, distance 0)"
        } else if vdw_on() {
            "scale-dependent (engine default)"
        } else {
            "upstream fixed (--upstream)"
        }
    );
    if !ok {
        println!("RESULT: FAIL");
        std::process::exit(1);
    }
    println!("RESULT: PASS");
}

fn profile() -> &'static str {
    if cfg!(debug_assertions) { "debug" } else { "release" }
}

fn check(label: &str, pass: bool, detail: String) -> bool {
    println!("[{}] {label}: {detail}", if pass { "PASS" } else { "FAIL" });
    pass
}

// ---------------------------------------------------------------------------------------------
// (a) Goldens. C goldens from Box3D's test/test_determinism.c, float build, as carried in
// box3d-rust 0.5.0's determinism_tests.rs; engine goldens recorded with engine_world_def.
// ---------------------------------------------------------------------------------------------

/// One goldens scene's outcome.
#[derive(Clone, Copy, PartialEq)]
struct Golden {
    sleep_step: i32,
    hash: u32,
    query: Option<(i32, u32)>,
}

impl std::fmt::Display for Golden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sleepStep={} hash=0x{:08X}", self.sleep_step, self.hash)?;
        if let Some((hits, qh)) = self.query {
            write!(f, " hits={hits} queryHash=0x{qh:08X}")?;
        }
        Ok(())
    }
}

/// Compare one scene with its golden: Box3D's C golden with `--upstream`, the engine golden
/// otherwise (recorded 2026-10-30 with `engine_world_def`, HarmonicMean). `--no-recycle` has no
/// golden and only prints the values.
fn check_golden(scene: &str, got: Golden, c: Golden, engine: Golden, t: std::time::Duration) -> bool {
    if NO_RECYCLE.load(Ordering::Relaxed) {
        println!("[INFO] {scene}: N/A (no golden for --no-recycle): {got} in {t:?}");
        true
    } else if vdw_on() {
        check(&format!("{scene} vs engine golden"), got == engine, format!("{got} (engine: {engine}) in {t:?}"))
    } else {
        check(&format!("{scene} vs C golden"), got == c, format!("{got} (C: {c}) in {t:?}"))
    }
}

fn goldens() -> bool {
    let mut all = true;
    let dt = 1.0 / 60.0;
    let g = |sleep_step, hash| Golden { sleep_step, hash, query: None };

    {
        let t = Instant::now();
        let mut world = goldens_world();
        let mut d = create_falling_ragdolls(&mut world);
        for _ in 0..1000 {
            world.step(dt, 4);
            if update_falling_ragdolls(&world, &mut d) {
                break;
            }
        }
        let got = g(d.sleep_step, d.hash);
        all &= check_golden("falling ragdolls", got, g(291, 0xFF4C_605B), g(358, 0xB1EB_47AE), t.elapsed());
    }
    {
        let t = Instant::now();
        let mut world = goldens_world();
        let mut d = create_wave_pile(&mut world);
        let mut done = false;
        for _ in 0..1000 {
            if done {
                break;
            }
            world.step(dt, 4);
            done = update_wave_pile(&world, &mut d);
        }
        let got = g(d.sleep_step, d.hash);
        all &= check_golden("wave pile", got, g(272, 0x266F_83B0), g(307, 0x0809_EA96), t.elapsed());
    }
    {
        let t = Instant::now();
        let mut world = goldens_world();
        let mut d = create_query_spawn(&mut world);
        let mut done = false;
        for _ in 0..1000 {
            if done {
                break;
            }
            world.step(dt, 4);
            done = update_query_spawn(&mut world, &mut d);
        }
        let q = |sleep_step, hash, hits, qh| Golden { sleep_step, hash, query: Some((hits, qh)) };
        let got = q(d.sleep_step, d.hash, d.query_hit_count, d.query_hash);
        all &= check_golden(
            "query spawn",
            got,
            q(242, 0xF3AD_9BA6, 59, 0xE327_1F3D),
            q(242, 0x9EA3_A8B9, 59, 0xE327_1F3D),
            t.elapsed(),
        );
    }
    {
        let t = Instant::now();
        let mut world = goldens_world();
        let mut d = create_mesh_drop(&mut world, POS_ZERO);
        let mut done = false;
        for _ in 0..1000 {
            if done {
                break;
            }
            world.step(dt, 4);
            done = update_mesh_drop(&world, &mut d);
        }
        let got = g(d.sleep_step, d.hash);
        all &= check_golden("mesh drop", got, g(226, 0xD2FF_069D), g(220, 0xCE3B_7A62), t.elapsed());
    }
    all
}

// ---------------------------------------------------------------------------------------------
// Reference run: plain fixed-step loop with every interaction known up front (no rollback).
// ---------------------------------------------------------------------------------------------

fn reference_run(frames: u64) -> (Vec<u64>, Vec<u64>, GameState) {
    let inputs = scripted_inputs();
    let mut gs = GameState::new(pile_scene(8, 4), SEED);
    let mut state_hashes = Vec::with_capacity(frames as usize);
    let mut world_hashes = Vec::with_capacity(frames as usize);
    while gs.frame < frames {
        let f = gs.frame;
        let now: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == f).collect();
        gs.advance(&now);
        state_hashes.push(gs.hash());
        world_hashes.push(gs.world_hash());
    }
    (state_hashes, world_hashes, gs)
}

fn trace(frames: u64, out: &str) -> bool {
    let t = Instant::now();
    let (state, world, _) = reference_run(frames);
    let elapsed = t.elapsed();
    let mut f = std::fs::File::create(out).expect("create trace file");
    for (i, (s, w)) in state.iter().zip(&world).enumerate() {
        writeln!(f, "{i} {w:016x} {s:016x}").unwrap();
    }
    let digest = fnv_u64s(state.iter().copied().chain(world.iter().copied()));
    println!(
        "trace: {frames} frames, 256 bodies, {} profile, {:?} total ({:.1} us/frame), digest {digest:016x}, final world hash {:016x} -> {out}",
        profile(),
        elapsed,
        elapsed.as_secs_f64() * 1e6 / frames as f64,
        world.last().copied().unwrap_or(0)
    );
    true
}

/// Snapshot-image bytes check: FNV-64 of the serialized world every 50 frames of the reference
/// run, plus its length. Toggle-off images must equal the unmodified crate's byte for byte.
fn images(out: &str) -> bool {
    let inputs = scripted_inputs();
    let mut gs = GameState::new(pile_scene(8, 4), SEED);
    let mut snapper = Snapshotter::new();
    let mut f = std::fs::File::create(out).expect("create images file");
    let mut all = Vec::new();
    while gs.frame <= 600 {
        if gs.frame % 50 == 0 {
            let s = snapper.save(&gs);
            let h = fnv_bytes(&s.world_image);
            writeln!(f, "{} {} {h:016x}", gs.frame, s.world_image.len()).unwrap();
            all.push(h);
        }
        let fr = gs.frame;
        let now: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == fr).collect();
        gs.advance(&now);
    }
    println!("images: 13 snapshots, digest {:016x} -> {out}", fnv_u64s(all));
    true
}

fn fnv_bytes(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h = (h ^ x as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------------------------
// (b) Rollback.
// ---------------------------------------------------------------------------------------------

fn first_mismatch(a: &[u64], b: &[u64]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x != y).or(if a.len() != b.len() { Some(a.len().min(b.len())) } else { None })
}

fn rollback() -> bool {
    const FRAMES: u64 = 600;
    let mut all = true;
    let (reference, _, _) = reference_run(FRAMES);
    let inputs = scripted_inputs();

    // T1: snapshot at 300, step to 400, restore in place, step to 400 again.
    {
        let mut gs = GameState::new(pile_scene(8, 4), SEED);
        let mut snapper = Snapshotter::new();
        let step_to = |gs: &mut GameState, end: u64| -> Vec<u64> {
            let mut hs = Vec::new();
            while gs.frame < end {
                let f = gs.frame;
                let now: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == f).collect();
                gs.advance(&now);
                hs.push(gs.hash());
            }
            hs
        };
        step_to(&mut gs, 300);
        let snap = snapper.save(&gs);
        let first = step_to(&mut gs, 400);
        let restored = snapper.restore(&mut gs, &snap);
        let valid = gs.all_bodies_valid();
        let second = step_to(&mut gs, 400);
        all &= check(
            "T1 restore-in-place replays identically",
            restored && valid && first == second && first[..] == reference[300..400],
            format!(
                "restore ok={restored}, body ids valid={valid}, 100 frames: replay==first {} , ==reference {} (image {} bytes)",
                first == second,
                first[..] == reference[300..400],
                snap.world_image.len()
            ),
        );
    }

    // T2: one interaction arrives 60 frames late -> single rollback of depth 60.
    {
        let late = inputs.iter().copied().find(|i| i.frame == 200).unwrap();
        let mut sim = RollbackSim::new(GameState::new(pile_scene(8, 4), SEED), 120);
        for i in inputs.iter().filter(|i| i.frame != 200) {
            sim.add_input(*i).unwrap();
        }
        sim.advance_to(260);
        let diverged = sim.hashes[&259] != reference[259];
        let t = Instant::now();
        let was_late = sim.add_input(late).unwrap();
        sim.advance_to(260);
        let rb = t.elapsed();
        sim.advance_to(FRAMES);
        let got: Vec<u64> = (0..FRAMES).map(|f| sim.hashes[&f]).collect();
        let mm = first_mismatch(&got, &reference);
        all &= check(
            "T2 late interaction (frame 200 learned at 260) rolls back and converges",
            was_late && diverged && mm.is_none(),
            format!(
                "pre-rollback diverged={diverged}, rollbacks={} depth={} resim={} frames in {:?} ({:.0} us/frame), first mismatch vs reference: {:?}",
                sim.stats.rollbacks,
                sim.stats.max_depth,
                sim.stats.resimulated_frames,
                rb,
                rb.as_secs_f64() * 1e6 / 60.0,
                mm
            ),
        );
    }

    // T3: every interaction delivered with a pseudo-random 1..=45 frame delay (network-like).
    {
        let mut sim = RollbackSim::new(GameState::new(pile_scene(8, 4), SEED), 120);
        let mut arrivals: Vec<(u64, Interaction)> = inputs
            .iter()
            .map(|i| (i.frame + 1 + splitmix64(i.frame ^ ((i.peer as u64) << 32)) % 45, *i))
            .collect();
        arrivals.sort_by_key(|(a, i)| (*a, i.peer, i.seq));
        let t = Instant::now();
        for f in 0..FRAMES {
            for (_, i) in arrivals.iter().filter(|(a, _)| *a == f) {
                sim.add_input(*i).unwrap();
            }
            sim.advance_to(f + 1);
        }
        let elapsed = t.elapsed();
        let got: Vec<u64> = (0..FRAMES).map(|f| sim.hashes[&f]).collect();
        let mm = first_mismatch(&got, &reference);
        all &= check(
            "T3 all interactions late by 1..45 frames converge to reference",
            mm.is_none(),
            format!(
                "rollbacks={} max depth={} resimulated={} frames, total {:?} for {FRAMES} frames incl. per-frame snapshots, first mismatch: {:?}",
                sim.stats.rollbacks, sim.stats.max_depth, sim.stats.resimulated_frames, elapsed, mm
            ),
        );
        // Report the "soonest wins" verdicts for the contested object.
        for f in [120u64, 125, 126] {
            if let Some(v) = sim.verdicts.get(&f) {
                for (i, verdict) in v {
                    println!("       frame {f}: peer {} seq {} on object {} -> {:?}", i.peer, i.seq, i.body, verdict);
                }
            }
        }
    }

    // T4: snapshot at 300 restored into a brand-new World (late-joiner path).
    {
        let mut sim = RollbackSim::new(GameState::new(pile_scene(8, 4), SEED), 120);
        for i in &inputs {
            sim.add_input(*i).unwrap();
        }
        sim.advance_to(300);
        let snap = sim.snap.save(&sim.state);
        let bodies = sim.state.scene.bodies.clone();
        let mut fresh_snapper = Snapshotter::new();
        // A joining peer has its own registry: geometry must come from the image + slots it is sent.
        let _ = fresh_snapper.save(&sim.state); // stands in for "receive the geometry registry"
        let mut gs = fresh_snapper.restore_fresh(&snap, bodies, SEED).expect("fresh restore");
        let mut hs = Vec::new();
        while gs.frame < FRAMES {
            let f = gs.frame;
            let now: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == f).collect();
            gs.advance(&now);
            hs.push(gs.hash());
        }
        let mm = first_mismatch(&hs, &reference[300..]);
        all &= check(
            "T4 snapshot restored into a fresh World continues identically",
            mm.is_none() && gs.all_bodies_valid(),
            format!("300 frames after restore, first mismatch: {mm:?}"),
        );
    }

    // T5: an interaction older than the rollback window is refused (needs resync instead).
    {
        let mut sim = RollbackSim::new(GameState::new(pile_scene(8, 4), SEED), 120);
        sim.advance_to(400);
        let r = sim.add_input(Interaction { frame: 100, peer: 9, seq: 0, body: 1, impulse: [0.0, 1.0, 0.0] });
        all &= check(
            "T5 interaction older than the 120-frame window is refused",
            matches!(r, Err(InputError::TooLate { .. })),
            format!("{r:?}"),
        );
    }

    // Timings for snapshot / restore / step at 256 bodies.
    {
        let mut gs = GameState::new(pile_scene(8, 4), SEED);
        let mut snapper = Snapshotter::new();
        for _ in 0..120 {
            gs.advance(&[]);
        }
        let n = 200;
        let t = Instant::now();
        let mut snap = snapper.save(&gs);
        for _ in 1..n {
            snap = snapper.save(&gs);
        }
        let save = t.elapsed() / n;
        let t = Instant::now();
        for _ in 0..n {
            snapper.restore(&mut gs, &snap);
        }
        let restore = t.elapsed() / n;
        let t = Instant::now();
        for _ in 0..n {
            gs.advance(&[]);
        }
        let step = t.elapsed() / n;
        println!(
            "       timings @256 bodies (frames 120-320, pile settling): save {save:?}, restore {restore:?}, step {step:?}, image {} bytes",
            snap.world_image.len()
        );
    }
    all
}

fn bench() -> bool {
    for (side, layers) in [(8usize, 4usize), (16, 4), (16, 8)] {
        let count = side * side * layers;
        let mut gs = GameState::new(pile_scene(side, layers), SEED);
        let mut snapper = Snapshotter::new();
        let frames = 300u64;
        let mut step_total = std::time::Duration::ZERO;
        let mut save_total = std::time::Duration::ZERO;
        let mut restore_total = std::time::Duration::ZERO;
        let mut max_step = std::time::Duration::ZERO;
        let mut bytes = 0usize;
        for _ in 0..frames {
            let t = Instant::now();
            let s = snapper.save(&gs);
            save_total += t.elapsed();
            bytes = bytes.max(s.world_image.len());
            let t = Instant::now();
            snapper.restore(&mut gs, &s);
            restore_total += t.elapsed();
            let t = Instant::now();
            gs.advance(&[]);
            let e = t.elapsed();
            max_step = max_step.max(e);
            step_total += e;
        }
        let per = |d: std::time::Duration| d.as_secs_f64() * 1e6 / frames as f64;
        println!(
            "bench {count:5} bodies: step avg {:8.1} us (max {:8.1} us), save avg {:7.1} us, restore avg {:7.1} us, max image {:8} bytes, awake at end {}",
            per(step_total),
            max_step.as_secs_f64() * 1e6,
            per(save_total),
            per(restore_total),
            bytes,
            box3d_rust::world::world_get_awake_body_count(&gs.scene.world)
        );
    }
    true
}

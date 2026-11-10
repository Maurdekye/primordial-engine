//! pe-physics's record generator and physics bench, moved in from `spikes/physics` (plan 2.1 and 2.4) so the
//! physics records can be regenerated once the spike is archived.
//!
//! Subcommands:
//!   records <dir> [workers]   write the records pe-physics's tests read, for both configurations, into <dir>:
//!                             trace-{vdw,off}-win-{profile}.txt (600 frames of the pile, the spike's `trace`) and
//!                             images-{vdw,off}-win-{profile}.txt (the world image every 50 frames, the spike's
//!                             `images`). Run it in release and in debug: the image records differ by profile.
//!                             `workers` (default 1) is the world's worker count; the records must not depend on
//!                             it (D2; scripts/check_par_determinism.sh).
//!   bench [workers]           step, save, restore and image size at 256 to 4,096 bodies (the spike's `bench`),
//!                             with a comma-separated list of worker counts (D2; the default is 1).
//!
//! The goldens and rollback T1-T5 have no record files: `cargo test -p pe-physics` checks them
//! (tests/spike_records.rs). The scenes, inputs and hashes are the tests' own (tests/common), which mirror
//! `spikes/sim-core` exactly, so a record written here is the record the tests compare against.
//!
//! "win" in the record names is historical: the records are the same on every platform the matrix covers.

#[path = "../tests/common/mod.rs"]
mod common;

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use common::*;
use pe_physics::PhysicsConfig;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let ok = match args.get(1).map(String::as_str) {
        Some("records") => match (args.get(2), args.get(3).map(|s| s.parse::<i32>())) {
            (Some(dir), None) => records(Path::new(dir), 1),
            (Some(dir), Some(Ok(w))) if w >= 1 => records(Path::new(dir), w),
            _ => usage(),
        },
        Some("bench") => {
            let list = args.get(2).map(String::as_str).unwrap_or("1");
            let workers: Option<Vec<i32>> = list.split(',').map(|s| s.parse().ok()).collect();
            match workers {
                Some(w) if !w.is_empty() => bench(&w),
                _ => usage(),
            }
        }
        _ => usage(),
    };
    println!("build: {} profile, target {}", profile(), std::env::consts::ARCH);
    if ok {
        println!("RESULT: PASS");
        ExitCode::SUCCESS
    } else {
        println!("RESULT: FAIL");
        ExitCode::FAILURE
    }
}

fn usage() -> bool {
    eprintln!("usage: records records <dir> [workers] | records bench [workers,...]");
    false
}

/// The trace and image records of both configurations, written as the spike wrote them, with `workers`
/// worker threads per world.
fn records(dir: &Path, workers: i32) -> bool {
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("cannot create {}: {e}", dir.display());
        return false;
    }
    println!("records: {workers} worker(s) per world");
    let mut ok = true;
    for (config, mode) in modes() {
        let config = config.with_workers(workers);
        ok &= write_trace(config, mode, dir);
        ok &= write_images(config, mode, dir);
    }
    ok
}

fn write_file(path: &Path, text: &str) -> bool {
    match std::fs::File::create(path).and_then(|mut f| f.write_all(text.as_bytes())) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("cannot write {}: {e}", path.display());
            false
        }
    }
}

/// The spike's `trace 600`: per frame, the Box3D hash and the state hash of the pile's reference run.
fn write_trace(config: PhysicsConfig, mode: &str, dir: &Path) -> bool {
    let t = Instant::now();
    let (state, world) = reference_run(config, 600);
    let elapsed = t.elapsed();
    let text: String =
        state.iter().zip(&world).enumerate().map(|(i, (s, w))| format!("{i} {w:016x} {s:016x}\n")).collect();
    let name = format!("trace-{mode}-win-{}.txt", profile());
    let digest = fnv_u64s(state.iter().copied().chain(world.iter().copied()));
    println!(
        "trace {mode}: 600 frames, 256 bodies, {elapsed:?} ({:.1} us/frame), digest {digest:016x} -> {name}",
        elapsed.as_secs_f64() * 1e6 / 600.0
    );
    write_file(&dir.join(name), &text)
}

/// The spike's `images`: the length and FNV-64 of Box3D's world bytes every 50 frames, to frame 600.
fn write_images(config: PhysicsConfig, mode: &str, dir: &Path) -> bool {
    let inputs = scripted_inputs();
    let mut g = Game::pile(config);
    let mut text = String::new();
    let mut hashes = Vec::new();
    while g.frame <= 600 {
        if g.frame.is_multiple_of(50) {
            let image = match g.pw.save_image() {
                Ok(image) => image,
                Err(e) => {
                    eprintln!("save_image at frame {}: {e:?}", g.frame);
                    return false;
                }
            };
            let bytes = image.world_bytes();
            let h = fnv_bytes(bytes);
            text += &format!("{} {} {h:016x}\n", g.frame, bytes.len());
            hashes.push(h);
        }
        let f = g.frame;
        let now: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == f).collect();
        g.advance(&now);
    }
    let name = format!("images-{mode}-win-{}.txt", profile());
    println!("images {mode}: {} snapshots, digest {:016x} -> {name}", hashes.len(), fnv_u64s(hashes));
    write_file(&dir.join(name), &text)
}

/// The spike's `bench`: for each pile size, 300 frames of save, restore in place, then step.
fn bench(workers: &[i32]) -> bool {
    for &w in workers {
        for (side, layers) in [(8usize, 4usize), (16, 4), (16, 8), (32, 4)] {
            let count = side * side * layers;
            let config = PhysicsConfig::engine().with_workers(w);
            let t = Instant::now();
            let (pw, objects) = pile_world(config, side, layers);
            let build = t.elapsed();
            let mut g = Game { pw, objects, frame: 0, claims: Default::default(), seed: SEED };
            let frames = 300u32;
            let (mut step, mut save, mut restore, mut max_step) =
                (Duration::ZERO, Duration::ZERO, Duration::ZERO, Duration::ZERO);
            let mut bytes = 0usize;
            for _ in 0..frames {
                let t = Instant::now();
                let s = g.save();
                save += t.elapsed();
                bytes = bytes.max(s.image.world_bytes().len());
                let t = Instant::now();
                g.restore(&s);
                restore += t.elapsed();
                let t = Instant::now();
                g.advance(&[]);
                let e = t.elapsed();
                max_step = max_step.max(e);
                step += e;
            }
            let per = |d: Duration| d.as_secs_f64() * 1e6 / frames as f64;
            println!(
                "bench {count:5} bodies, {w} workers: build {:7.1} us, step avg {:8.1} us (max {:8.1} us), save avg {:7.1} us, restore avg {:7.1} us, max image {bytes:8} bytes, awake at end {}",
                build.as_secs_f64() * 1e6,
                per(step),
                max_step.as_secs_f64() * 1e6,
                per(save),
                per(restore),
                g.pw.awake_body_count()
            );
        }
    }
    true
}

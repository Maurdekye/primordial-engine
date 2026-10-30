//! iter-bench host: logic-runtime matrix and the hot-swap end of the iteration loop.
//!
//! usage:
//!   iter-host matrix  <logic.wasm> <runtime>...        throughput / compile / instantiate / host calls /
//!                                                      fuel / determinism; one JSON object per line
//!   iter-host systems <logic.wasm> <runtime>...        per-system cost (each system alone, mask)
//!   iter-host physics <physics.wasm> <runtime>...      phase-1 Box3D pile compiled to wasm32
//!   iter-host serve   <logic.wasm> <runtime> [n]       read `swap <path>` lines on stdin, answer with timings
//!
//! runtime specs: native, wasmi-eager-fuel, wasmi-eager, wasmi-lazy-fuel, wasmtime-cranelift[-fuel][-epoch]
//! [-nancanon][-serial], wasmtime-winch[-fuel][-epoch][-serial]

mod rt;

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use rt::{Guest, HostState, Runtime};

const SEED: u64 = 0x5052_494D_4F52_4449;
const ALL: u32 = 63;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn read(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn fresh(r: &dyn Runtime, n: u32) -> Box<dyn Guest> {
    let mut g = r.instantiate(HostState::new()).expect("instantiate");
    g.init(n, SEED);
    g
}

/// Times `frames` ticks after `warm` warm-up ticks. Returns per-frame ms and total fuel.
fn run_frames(g: &mut dyn Guest, start: u64, warm: u64, frames: u64, mask: u32) -> (Vec<f64>, u64) {
    for f in start..start + warm {
        g.tick(f, ALL);
    }
    let mut t = Vec::with_capacity(frames as usize);
    let mut fuel = 0;
    for f in start + warm..start + warm + frames {
        let s = Instant::now();
        let used = g.tick(f, mask);
        t.push(ms(s.elapsed()));
        fuel += used.unwrap_or(0);
    }
    (t, fuel)
}

fn matrix(wasm_path: &str, specs: &[String]) {
    let wasm = read(wasm_path);
    for spec in specs {
        let mut r = match rt::make(spec) {
            Ok(r) => r,
            Err(e) => {
                println!("{{\"kind\":\"error\",\"rt\":\"{spec}\",\"error\":{:?}}}", e);
                continue;
            }
        };
        // Compile (validate + translate/JIT): 5 runs, fresh each time.
        let mut c = Vec::new();
        for _ in 0..5 {
            let s = Instant::now();
            if let Err(e) = r.compile(&wasm) {
                println!("{{\"kind\":\"error\",\"rt\":\"{spec}\",\"error\":{:?}}}", e);
                break;
            }
            c.push(ms(s.elapsed()));
        }
        if c.len() < 5 {
            continue;
        }
        let mut inst = Vec::new();
        for _ in 0..20 {
            let s = Instant::now();
            let g = r.instantiate(HostState::new()).unwrap();
            inst.push(ms(s.elapsed()));
            drop(g);
        }
        println!(
            "{{\"kind\":\"compile\",\"rt\":\"{}\",\"wasm\":{:?},\"wasm_bytes\":{},\"compile_ms_median\":{:.3},\"compile_ms_min\":{:.3},\"compile_ms_max\":{:.3},\"instantiate_ms_median\":{:.4},\"instantiate_ms_min\":{:.4}}}",
            r.name(),
            wasm_path,
            wasm.len(),
            median(&mut c.clone()),
            c.iter().cloned().fold(f64::MAX, f64::min),
            c.iter().cloned().fold(0.0, f64::max),
            median(&mut inst.clone()),
            inst.iter().cloned().fold(f64::MAX, f64::min)
        );
        // Throughput.
        for &n in &[1000u32, 10000] {
            let mut g = fresh(r.as_ref(), n);
            let s = Instant::now();
            let (mut t, fuel) = run_frames(g.as_mut(), 0, 30, 200, ALL);
            let total = ms(s.elapsed());
            let alive = g.alive();
            println!(
                "{{\"kind\":\"tick\",\"rt\":\"{}\",\"n\":{n},\"frames\":200,\"tick_ms_median\":{:.4},\"tick_ms_p10\":{:.4},\"tick_ms_p90\":{:.4},\"tick_ms_mean\":{:.4},\"fuel_per_frame\":{},\"alive_end\":{alive},\"wall_ms_incl_warmup\":{total:.1}}}",
                r.name(),
                median(&mut t),
                pct(&mut t, 0.1),
                pct(&mut t, 0.9),
                t.iter().sum::<f64>() / t.len() as f64,
                fuel / 200
            );
        }
        // Host calls: 1M calls of each kind.
        {
            let mut g = fresh(r.as_ref(), 16);
            for kind in 0..2 {
                g.hostcalls(1000, kind);
                let mut v = Vec::new();
                for _ in 0..5 {
                    let s = Instant::now();
                    std::hint::black_box(g.hostcalls(1_000_000, kind));
                    v.push(s.elapsed().as_secs_f64() * 1e9 / 1e6);
                }
                println!(
                    "{{\"kind\":\"hostcall\",\"rt\":\"{}\",\"call\":\"{}\",\"ns_per_call_median\":{:.2}}}",
                    r.name(),
                    if kind == 0 { "rand_u32()" } else { "terrain_height(f32,f32)" },
                    median(&mut v)
                );
            }
        }
        // State migration cost (what a hot-swap does): save in one instance, load in a fresh one.
        {
            let mut g = fresh(r.as_ref(), 10000);
            run_frames(g.as_mut(), 0, 10, 1, ALL);
            let mut sv = Vec::new();
            let mut ld = Vec::new();
            let mut bytes = 0;
            let mut ok = true;
            for _ in 0..5 {
                let s = Instant::now();
                let st = g.save();
                sv.push(ms(s.elapsed()));
                bytes = st.len();
                let mut g2 = r.instantiate(g.host()).unwrap();
                let s = Instant::now();
                ok &= g2.load(&st);
                ld.push(ms(s.elapsed()));
                ok &= g2.digest() == g.digest();
            }
            println!(
                "{{\"kind\":\"migrate\",\"rt\":\"{}\",\"n\":10000,\"state_bytes\":{bytes},\"save_ms_median\":{:.3},\"load_ms_median\":{:.3},\"roundtrip_digest_equal\":{ok}}}",
                r.name(),
                median(&mut sv),
                median(&mut ld)
            );
        }
        // Determinism: two instances in this process, 300 frames, n = 2000; digest every 10 frames.
        {
            let mut runs = Vec::new();
            for _ in 0..2 {
                let mut g = fresh(r.as_ref(), 2000);
                let mut digests = Vec::new();
                let mut fuels = Vec::new();
                for f in 0..300u64 {
                    fuels.push(g.tick(f, ALL).unwrap_or(0));
                    if f % 10 == 9 {
                        digests.push(g.digest());
                    }
                }
                runs.push((digests, fuels, g.nan_probe()));
            }
            let same = runs[0] == runs[1];
            let fuel_total: u64 = runs[0].1.iter().sum();
            println!(
                "{{\"kind\":\"determinism\",\"rt\":\"{}\",\"n\":2000,\"frames\":300,\"final_digest\":\"{:016x}\",\"fuel_total\":{fuel_total},\"two_instances_identical\":{same},\"nan_probe\":\"{:016x}\"}}",
                r.name(),
                runs[0].0.last().unwrap(),
                runs[0].2
            );
        }
        std::io::stdout().flush().unwrap();
    }
}

fn systems(wasm_path: &str, specs: &[String]) {
    let wasm = read(wasm_path);
    let names = [(1u32, "movement"), (2, "ai"), (4, "combat"), (8, "inventory"), (16, "spawn"), (32, "effects"), (ALL, "all"), (ALL & !1, "all-but-movement"), (ALL & !3, "all-but-movement-ai")];
    for spec in specs {
        let mut r = rt::make(spec).unwrap();
        r.compile(&wasm).unwrap();
        for &(mask, name) in &names {
            let mut g = fresh(r.as_ref(), 10000);
            let (mut t, _) = run_frames(g.as_mut(), 0, 30, 150, mask);
            println!(
                "{{\"kind\":\"system\",\"rt\":\"{}\",\"n\":10000,\"system\":\"{name}\",\"mask\":{mask},\"tick_ms_median\":{:.4}}}",
                r.name(),
                median(&mut t)
            );
        }
        std::io::stdout().flush().unwrap();
    }
}

fn physics(wasm_path: &str, specs: &[String]) {
    let wasm = read(wasm_path);
    for spec in specs {
        let mut r = rt::make(spec).unwrap();
        let s = Instant::now();
        r.compile(&wasm).unwrap();
        let c = ms(s.elapsed());
        let mut v = Vec::new();
        let mut digest = 0;
        for _ in 0..3 {
            let s = Instant::now();
            digest = r.run_physics(600).unwrap();
            v.push(ms(s.elapsed()) / 600.0);
        }
        println!(
            "{{\"kind\":\"physics\",\"rt\":\"{}\",\"compile_ms\":{c:.2},\"ms_per_frame_median\":{:.4},\"digest\":\"{digest:016x}\",\"digest_matches_native\":{}}}",
            r.name(),
            median(&mut v),
            digest == 0x7903_109a_ab28_ed9b
        );
        std::io::stdout().flush().unwrap();
    }
}

/// Live-sim side of the iteration loop. Keeps a running sim; on `swap <path>` it reads the new
/// module, compiles it, instantiates it, migrates the state and runs the first frame on it.
fn serve(wasm_path: &str, spec: &str, n: u32) {
    let mut r = rt::make(spec).unwrap();
    r.compile(&read(wasm_path)).unwrap();
    let mut g = fresh(r.as_ref(), n);
    let mut frame = 0u64;
    for _ in 0..30 {
        g.tick(frame, ALL);
        frame += 1;
    }
    println!("{{\"kind\":\"ready\",\"rt\":\"{}\",\"n\":{n}}}", r.name());
    std::io::stdout().flush().unwrap();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let line = line.trim();
        if line == "quit" {
            break;
        }
        let Some(path) = line.strip_prefix("swap ") else { continue };
        let t0 = Instant::now();
        let bytes = read(path);
        let t1 = Instant::now();
        let res = r.compile(&bytes);
        let t2 = Instant::now();
        if let Err(e) = res {
            println!("{{\"kind\":\"swap\",\"ok\":false,\"error\":{:?}}}", e);
            std::io::stdout().flush().unwrap();
            continue;
        }
        let mut next = r.instantiate(g.host()).unwrap();
        let t3 = Instant::now();
        let state = g.save();
        let ok = next.load(&state);
        let t4 = Instant::now();
        next.tick(frame, ALL);
        frame += 1;
        let t5 = Instant::now();
        g = next;
        println!(
            "{{\"kind\":\"swap\",\"ok\":{ok},\"wasm_bytes\":{},\"read_ms\":{:.3},\"compile_ms\":{:.3},\"instantiate_ms\":{:.3},\"migrate_ms\":{:.3},\"first_tick_ms\":{:.3},\"total_ms\":{:.3},\"state_bytes\":{}}}",
            bytes.len(),
            ms(t1 - t0),
            ms(t2 - t1),
            ms(t3 - t2),
            ms(t4 - t3),
            ms(t5 - t4),
            ms(t5 - t0),
            state.len()
        );
        std::io::stdout().flush().unwrap();
        for _ in 0..5 {
            g.tick(frame, ALL);
            frame += 1;
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("matrix") => matrix(&args[1], &args[2..]),
        Some("systems") => systems(&args[1], &args[2..]),
        Some("physics") => physics(&args[1], &args[2..]),
        Some("serve") => serve(&args[1], &args[2], args.get(3).map_or(2000, |s| s.parse().unwrap())),
        _ => {
            eprintln!("usage: iter-host matrix|systems|physics|serve <wasm> <runtime>...");
            std::process::exit(2);
        }
    }
}

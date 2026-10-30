//! R15: cost of a physics hook implemented in a wasm guest and called from native Box3D, per call,
//! against the same function compiled natively. Every runtime runs with fuel, as a session would.
//! Also a batched form: one call per step that reads all contacts from guest memory.

use std::hint::black_box;
use std::time::Instant;

const N: u32 = 1_000_000;
const BATCH: u32 = 1024;
const REC: usize = 8;

fn pre_solve_native(shape_a: u32, shape_b: u32, nx: f32, ny: f32, nz: f32, depth: f32) -> i32 {
    let one_way = (shape_a ^ shape_b) & 1 == 1;
    let from_below = ny < -0.5;
    (!(one_way && from_below) || depth > 0.05 + 0.0 * (nx + nz)) as i32
}

fn args(i: u32) -> (i32, i32, f32, f32, f32, f32) {
    let f = (i % 1000) as f32 * 0.001;
    (i as i32, (i >> 3) as i32, f - 0.5, -f, 0.25, f * 0.1)
}

fn report(rt: &str, call_ns: f64, mix_ns: f64, batch_ns: f64, kept: i64) {
    println!(
        "{{\"kind\":\"callback\",\"rt\":\"{rt}\",\"pre_solve_ns_per_call\":{call_ns:.1},\"mix_friction_ns_per_call\":{mix_ns:.1},\"batched_pre_solve_ns_per_contact\":{batch_ns:.1},\"checksum\":{kept}}}"
    );
}

pub fn run(wasm: &[u8]) {
    // Native baseline.
    let t = Instant::now();
    let mut kept = 0i64;
    for i in 0..N {
        let (a, b, x, y, z, d) = black_box(args(i));
        kept += pre_solve_native(a as u32, b as u32, x, y, z, d) as i64;
    }
    let native = t.elapsed().as_secs_f64() * 1e9 / N as f64;
    report("native", native, 0.0, 0.0, kept);

    // wasmi, eager, fuel (the D12 runtime).
    {
        let mut cfg = wasmi::Config::default();
        cfg.consume_fuel(true);
        cfg.compilation_mode(wasmi::CompilationMode::Eager);
        let engine = wasmi::Engine::new(&cfg);
        let module = wasmi::Module::new(&engine, wasm).unwrap();
        let mut store = wasmi::Store::new(&engine, ());
        store.set_fuel(u64::MAX / 4).unwrap();
        let inst = wasmi::Linker::<()>::new(&engine).instantiate_and_start(&mut store, &module).unwrap();
        let pre = inst.get_typed_func::<(i32, i32, f32, f32, f32, f32), i32>(&store, "pre_solve").unwrap();
        let mix = inst.get_typed_func::<(f32, i64, f32, i64), f32>(&store, "mix_friction").unwrap();
        let pb = inst.get_typed_func::<i32, i32>(&store, "pre_solve_batch").unwrap();
        let ptr = inst.get_typed_func::<(), i32>(&store, "buf_ptr").unwrap().call(&mut store, ()).unwrap() as usize;
        let mem = inst.get_memory(&store, "memory").unwrap();
        let t = Instant::now();
        let mut kept = 0i64;
        for i in 0..N {
            kept += pre.call(&mut store, black_box(args(i))).unwrap() as i64;
        }
        let call = t.elapsed().as_secs_f64() * 1e9 / N as f64;
        let t = Instant::now();
        for i in 0..N {
            let f = (i % 100) as f32 * 0.01;
            kept += mix.call(&mut store, black_box((f, (i % 9) as i64, 0.5, 3))).unwrap() as i64;
        }
        let mixn = t.elapsed().as_secs_f64() * 1e9 / N as f64;
        let t = Instant::now();
        for k in 0..N / BATCH {
            // Host writes the step's contacts into guest memory, one call decides them all.
            let data = mem.data_mut(&mut store);
            for j in 0..BATCH as usize {
                let (a, b, x, y, z, d) = args(k * BATCH + j as u32);
                let off = ptr + j * REC * 4;
                for (q, v) in [f32::from_bits(a as u32), f32::from_bits(b as u32), x, y, z, d].iter().enumerate() {
                    data[off + q * 4..off + q * 4 + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
            kept += pb.call(&mut store, BATCH as i32).unwrap() as i64;
        }
        let batched = t.elapsed().as_secs_f64() * 1e9 / N as f64;
        report("wasmi-eager-fuel", call, mixn, batched, kept);
    }

    // wasmtime: Cranelift and Winch, fuel, NaN canonicalization (the deterministic configs).
    for (label, winch, nan) in [("wasmtime-cranelift-fuel-nancanon", false, true), ("wasmtime-cranelift-fuel", false, false), ("wasmtime-winch-fuel-nancanon", true, true)] {
        let mut cfg = wasmtime::Config::new();
        cfg.strategy(if winch { wasmtime::Strategy::Winch } else { wasmtime::Strategy::Cranelift });
        cfg.consume_fuel(true);
        cfg.cranelift_nan_canonicalization(nan);
        cfg.relaxed_simd_deterministic(true);
        let engine = wasmtime::Engine::new(&cfg).unwrap();
        let module = wasmtime::Module::new(&engine, wasm).unwrap();
        let mut store = wasmtime::Store::new(&engine, ());
        store.set_fuel(u64::MAX / 4).unwrap();
        let inst = wasmtime::Instance::new(&mut store, &module, &[]).unwrap();
        let pre = inst.get_typed_func::<(i32, i32, f32, f32, f32, f32), i32>(&mut store, "pre_solve").unwrap();
        let mix = inst.get_typed_func::<(f32, i64, f32, i64), f32>(&mut store, "mix_friction").unwrap();
        let pb = inst.get_typed_func::<i32, i32>(&mut store, "pre_solve_batch").unwrap();
        let ptr = inst.get_typed_func::<(), i32>(&mut store, "buf_ptr").unwrap().call(&mut store, ()).unwrap() as usize;
        let mem = inst.get_memory(&mut store, "memory").unwrap();
        let t = Instant::now();
        let mut kept = 0i64;
        for i in 0..N {
            kept += pre.call(&mut store, black_box(args(i))).unwrap() as i64;
        }
        let call = t.elapsed().as_secs_f64() * 1e9 / N as f64;
        let t = Instant::now();
        for i in 0..N {
            let f = (i % 100) as f32 * 0.01;
            kept += mix.call(&mut store, black_box((f, (i % 9) as i64, 0.5, 3))).unwrap() as i64;
        }
        let mixn = t.elapsed().as_secs_f64() * 1e9 / N as f64;
        let t = Instant::now();
        for k in 0..N / BATCH {
            let data = mem.data_mut(&mut store);
            for j in 0..BATCH as usize {
                let (a, b, x, y, z, d) = args(k * BATCH + j as u32);
                let off = ptr + j * REC * 4;
                for (q, v) in [f32::from_bits(a as u32), f32::from_bits(b as u32), x, y, z, d].iter().enumerate() {
                    data[off + q * 4..off + q * 4 + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
            kept += pb.call(&mut store, BATCH as i32).unwrap() as i64;
        }
        let batched = t.elapsed().as_secs_f64() * 1e9 / N as f64;
        report(label, call, mixn, batched, kept);
    }
}

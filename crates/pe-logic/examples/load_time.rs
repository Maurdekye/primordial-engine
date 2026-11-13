//! The post-build part of a script edit (logic.md 4.2, 5.2): the metering pass, then Cranelift's compile and the
//! instance, timed on a built module. For scripts/iter_bench_logic_edit.py.
//!
//! `cargo run -p pe-logic --release --example load_time -- <module.wasm> [reps]` prints one JSON line per rep:
//! `{"instrument_ms":..,"compile_ms":..,"instantiate_ms":..,"bytes":..,"metered_bytes":..}`.

use pe_logic::meter;
use pe_logic::runtime::{self, NoServices, Script, Settings};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().expect("usage: load_time <module.wasm> [reps]");
    let reps: usize = args.get(1).and_then(|r| r.parse().ok()).unwrap_or(20);
    let wasm = std::fs::read(path).expect("read the module");
    let s = Settings::default();
    let engine = runtime::engine(&s).expect("engine");
    let run = move || {
        for _ in 0..reps {
            let t = Instant::now();
            let (metered, _) = meter::instrument(&wasm, s.meter).expect("instrument");
            let instrument = t.elapsed();
            // Script::load instruments again, then compiles: subtract the pass measured just before.
            let t = Instant::now();
            let script = Script::load(&engine, s, &wasm).expect("load");
            let load = t.elapsed();
            let t = Instant::now();
            let inst = script.instantiate(Box::new(NoServices)).expect("instantiate");
            let instantiate = t.elapsed();
            drop(inst);
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
            println!(
                "{{\"instrument_ms\":{:.3},\"compile_ms\":{:.3},\"instantiate_ms\":{:.3},\"bytes\":{},\"metered_bytes\":{}}}",
                ms(instrument),
                (ms(load) - ms(instrument)).max(0.0),
                ms(instantiate),
                wasm.len(),
                metered.len()
            );
        }
    };
    std::thread::Builder::new().stack_size(s.thread_stack()).spawn(run).unwrap().join().unwrap();
}

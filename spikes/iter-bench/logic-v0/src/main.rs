//! WS-logic pre-build probe (docs/spec/logic.md section 11).
//!
//!   logic-probe bench   <logic.wasm>          tick cost at n=10k: native, Cranelift, wasmtime fuel, injected
//!                                             metering, and the batch proxy with per-frame instance reset
//!   logic-probe physics <physics.wasm>        physics-in-wasm: wasmtime fuel against injected metering
//!   logic-probe traps                         fuel and depth traps: deterministic points, no native overflow
//!   logic-probe load    <wasm>...             instrument + Cranelift compile + instantiate times
//!   logic-probe cols    <cols_guest.wasm>     six systems over host-owned columns (the batch ABI, logic.md 3.3)
//!
//! Every line of output is one JSON record.

mod cols;
mod instrument;

use pe_api::{Host, World, fnv};
use std::time::Instant;
use wasmtime::{
    Caller, Config, Engine, InstanceAllocationStrategy, InstancePre, Linker, Module, OptLevel,
    PoolingAllocationConfig, Store, Strategy, Val,
};

const SEED: u64 = 0x5052_494D_4F52_4449; // as spikes/iter-bench/host
const ALL: u32 = 63;
const N: u32 = 10_000;
const WARM: u64 = 30;
const FRAMES: u64 = 150;
const BIG: i64 = 1 << 50;

#[derive(Clone)]
struct HostState {
    rng: u64,
    emits: u64,
    emit_hash: u64,
}

impl HostState {
    fn new() -> Self {
        HostState { rng: 0x9E37_79B9_7F4A_7C15, emits: 0, emit_hash: 0xcbf2_9ce4_8422_2325 }
    }
    fn terrain(&mut self, x: f32, z: f32) -> f32 {
        let xi = (x * 0.125).floor() as i32;
        let zi = (z * 0.125).floor() as i32;
        ((xi.wrapping_mul(73_856_093) ^ zi.wrapping_mul(19_349_663)) & 7) as f32 * 0.25
    }
    fn emit(&mut self, kind: u32, a: u32, b: f32) {
        self.emits += 1;
        self.emit_hash = fnv(self.emit_hash, (kind as u64) << 32 | a as u64);
        self.emit_hash = fnv(self.emit_hash, b.to_bits() as u64);
    }
    fn rand(&mut self) -> u32 {
        (pe_api::rng_next(&mut self.rng) >> 32) as u32
    }
}

impl Host for HostState {
    fn terrain_height(&mut self, x: f32, z: f32) -> f32 {
        self.terrain(x, z)
    }
    fn emit(&mut self, kind: u32, a: u32, b: f32) {
        HostState::emit(self, kind, a, b)
    }
    fn rand_u32(&mut self) -> u32 {
        self.rand()
    }
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn stats(mut t: Vec<f64>) -> (f64, f64, f64) {
    t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| t[((t.len() - 1) as f64 * p).round() as usize];
    (q(0.5), q(0.1), q(0.9))
}

fn read(p: &str) -> Vec<u8> {
    std::fs::read(p).unwrap_or_else(|e| panic!("{p}: {e}"))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Meter {
    None,
    WasmtimeFuel,
    /// v1 pass: fuel in a local, checked at entry and loop headers.
    Injected,
    /// v0 pass: fuel in the global, checked at every segment.
    InjectedGlobal,
    /// v1 pass plus NaN canonicalization at observation points (run without Cranelift's).
    InjectedNanObs,
}

impl Meter {
    fn injected(self) -> bool {
        matches!(self, Meter::Injected | Meter::InjectedGlobal | Meter::InjectedNanObs)
    }
    fn cfg(self) -> instrument::Config {
        let fuel = if self == Meter::InjectedGlobal { instrument::FuelMode::Global } else { instrument::FuelMode::Local };
        instrument::Config { fuel, nan_observe: self == Meter::InjectedNanObs, ..Default::default() }
    }
}

fn prepare(wasm: &[u8], meter: Meter) -> Vec<u8> {
    if meter.injected() { instrument::instrument(wasm, meter.cfg()).unwrap().0 } else { wasm.to_vec() }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Frame {
    /// The old benchmark's shape: the world lives in guest memory across frames.
    Persistent,
    /// Batch proxy on one instance: every frame copies the whole state in (pe_load) and out (pe_save).
    Batch,
    /// Batch proxy with a fresh instance every frame (exact reset), default allocator.
    BatchFresh,
    /// The same with the pooling allocator.
    BatchFreshPool,
}

fn engine(meter: Meter, nan: bool, pool: bool, max_stack: usize) -> Engine {
    let mut c = Config::new();
    c.strategy(Strategy::Cranelift);
    c.cranelift_opt_level(OptLevel::Speed);
    c.cranelift_nan_canonicalization(nan);
    c.consume_fuel(meter == Meter::WasmtimeFuel);
    c.wasm_relaxed_simd(false);
    c.wasm_memory64(false);
    c.wasm_tail_call(false);
    c.async_stack_size(max_stack.max(2 << 20));
    c.max_wasm_stack(max_stack);
    if pool {
        let mut p = PoolingAllocationConfig::default();
        p.total_core_instances(8);
        p.total_memories(8);
        p.total_tables(8);
        p.max_memory_size(256 << 20);
        c.allocation_strategy(InstanceAllocationStrategy::Pooling(p));
    }
    Engine::new(&c).unwrap()
}

fn linker(e: &Engine) -> Linker<HostState> {
    let mut l = Linker::<HostState>::new(e);
    l.func_wrap("pe", "terrain_height", |mut c: Caller<'_, HostState>, x: f32, z: f32| -> f32 {
        c.data_mut().terrain(x, z)
    })
    .unwrap();
    l.func_wrap("pe", "emit", |mut c: Caller<'_, HostState>, k: i32, a: i32, b: f32| {
        c.data_mut().emit(k as u32, a as u32, b)
    })
    .unwrap();
    l.func_wrap("pe", "rand_u32", |mut c: Caller<'_, HostState>| -> i32 { c.data_mut().rand() as i32 }).unwrap();
    l
}

/// One live instance with the guest's exports.
struct G {
    store: Store<HostState>,
    inst: wasmtime::Instance,
    meter: Meter,
    fuel_used: u64,
}

impl G {
    fn new(pre: &InstancePre<HostState>, e: &Engine, host: HostState, meter: Meter) -> G {
        let mut store = Store::new(e, host);
        if meter == Meter::WasmtimeFuel {
            store.set_fuel(BIG as u64).unwrap();
        }
        let inst = pre.instantiate(&mut store).unwrap();
        G { store, inst, meter, fuel_used: 0 }
    }
    fn refuel(&mut self) {
        match self.meter {
            Meter::WasmtimeFuel => self.store.set_fuel(BIG as u64).unwrap(),
            Meter::Injected | Meter::InjectedGlobal | Meter::InjectedNanObs => {
                let g = self.inst.get_global(&mut self.store, "pe_fuel").unwrap();
                g.set(&mut self.store, Val::I64(BIG)).unwrap();
            }
            Meter::None => {}
        }
    }
    fn used(&mut self) -> u64 {
        match self.meter {
            Meter::WasmtimeFuel => BIG as u64 - self.store.get_fuel().unwrap(),
            Meter::Injected | Meter::InjectedGlobal | Meter::InjectedNanObs => {
                let g = self.inst.get_global(&mut self.store, "pe_fuel").unwrap();
                (BIG - g.get(&mut self.store).unwrap_i64()) as u64
            }
            Meter::None => 0,
        }
    }
    fn call<P: wasmtime::WasmParams, R: wasmtime::WasmResults>(&mut self, name: &str, p: P) -> R {
        self.refuel();
        let f = self.inst.get_typed_func::<P, R>(&mut self.store, name).unwrap();
        let r = f.call(&mut self.store, p).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        self.fuel_used += self.used();
        r
    }
    fn mem_len(&mut self) -> usize {
        self.inst.get_memory(&mut self.store, "memory").unwrap().data_size(&self.store)
    }
    fn load(&mut self, data: &[u8]) {
        let ptr = self.call::<i32, i32>("pe_buf_alloc", data.len() as i32) as usize;
        let m = self.inst.get_memory(&mut self.store, "memory").unwrap();
        m.data_mut(&mut self.store)[ptr..ptr + data.len()].copy_from_slice(data);
        assert_eq!(self.call::<i32, i32>("pe_load", data.len() as i32), 0);
    }
    fn save(&mut self) -> Vec<u8> {
        let len = self.call::<(), i32>("pe_save", ()) as usize;
        let ptr = self.call::<(), i32>("pe_buf_ptr", ()) as usize;
        let m = self.inst.get_memory(&mut self.store, "memory").unwrap();
        m.data(&self.store)[ptr..ptr + len].to_vec()
    }
    fn digest(&mut self) -> u64 {
        let h = self.call::<(), i64>("pe_hash", ()) as u64;
        let s = self.store.data();
        fnv(fnv(h, s.emit_hash), s.emits)
    }
}

/// Rows that changed, by 64-byte block: the host's diff before writing back to the store.
fn diff_blocks(a: &[u8], b: &[u8]) -> u64 {
    if a.len() != b.len() {
        return u64::MAX;
    }
    a.chunks(64).zip(b.chunks(64)).filter(|(x, y)| x != y).count() as u64
}

struct RunOut {
    times: Vec<f64>,
    fuel_per_tick: u64,
    digest: u64,
    mem_bytes: usize,
    state_bytes: usize,
    changed_blocks: u64,
}

fn run(wasm: &[u8], meter: Meter, nan: bool, frame: Frame) -> (f64, RunOut) {
    let pool = frame == Frame::BatchFreshPool;
    let e = engine(meter, nan, pool, 8 << 20);
    let bytes = prepare(wasm, meter);
    let s = Instant::now();
    let m = Module::new(&e, &bytes).unwrap();
    let compile_ms = ms(s.elapsed());
    let pre = linker(&e).instantiate_pre(&m).unwrap();
    let mut g = G::new(&pre, &e, HostState::new(), meter);
    g.call::<(i32, i64), ()>("pe_init", (N as i32, SEED as i64));
    let mut state = if frame == Frame::Persistent { Vec::new() } else { g.save() };
    let mut times = Vec::new();
    let mut fuel_sum = 0u64;
    let mut changed = 0u64;
    let mut mem = 0;
    for f in 0..WARM + FRAMES {
        let t = Instant::now();
        g.fuel_used = 0;
        match frame {
            Frame::Persistent => {
                g.call::<(i64, i32), ()>("pe_tick", (f as i64, ALL as i32));
            }
            Frame::Batch | Frame::BatchFresh | Frame::BatchFreshPool => {
                if frame != Frame::Batch {
                    let host = g.store.data().clone();
                    g = G::new(&pre, &e, host, meter);
                }
                g.load(&state);
                g.call::<(i64, i32), ()>("pe_tick", (f as i64, ALL as i32));
                let next = g.save();
                changed = diff_blocks(&state, &next);
                state = next;
            }
        }
        let dt = ms(t.elapsed());
        if f >= WARM {
            times.push(dt);
            fuel_sum += g.fuel_used;
        }
        mem = g.mem_len();
    }
    let digest = g.digest();
    let nan = g.call::<(), i64>("pe_nan_probe", ()) as u64;
    NAN.with(|c| c.set(nan));
    (
        compile_ms,
        RunOut {
            times,
            fuel_per_tick: fuel_sum / FRAMES,
            digest,
            mem_bytes: mem,
            state_bytes: state.len(),
            changed_blocks: changed,
        },
    )
}

thread_local! {
    static NAN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn native() -> (Vec<f64>, u64) {
    let mut w = World::new(N as usize, SEED);
    let mut h = HostState::new();
    let mut t = Vec::new();
    for f in 0..WARM + FRAMES {
        let s = Instant::now();
        logic_game::tick(&mut w, f, ALL, &mut h);
        if f >= WARM {
            t.push(ms(s.elapsed()));
        }
    }
    (t, fnv(fnv(w.hash(), h.emit_hash), h.emits))
}

fn bench(path: &str) {
    let wasm = read(path);
    let s = Instant::now();
    let (inst, rep) = instrument::instrument(&wasm, Default::default()).unwrap();
    println!(
        "{{\"kind\":\"instrument\",\"in_bytes\":{},\"out_bytes\":{},\"ms\":{:.2},\"functions\":{},\"segments\":{},\"max_weight\":{},\"mutable_globals\":{}}}",
        wasm.len(),
        inst.len(),
        ms(s.elapsed()),
        rep.functions,
        rep.segments,
        rep.max_weight,
        rep.mutable_globals.len()
    );
    let (nt, nd) = native();
    let (nmed, n10, n90) = stats(nt);
    println!("{{\"kind\":\"tick\",\"mode\":\"native\",\"n\":{N},\"median_ms\":{nmed:.4},\"p10\":{n10:.4},\"p90\":{n90:.4},\"digest\":\"{nd:016x}\"}}");
    let modes: &[(&str, Meter, bool, Frame)] = &[
        ("cl", Meter::None, false, Frame::Persistent),
        ("cl-nan", Meter::None, true, Frame::Persistent),
        ("cl-wtfuel-nan", Meter::WasmtimeFuel, true, Frame::Persistent),
        ("cl-inj0-nan", Meter::InjectedGlobal, true, Frame::Persistent),
        ("cl-inj-nan", Meter::Injected, true, Frame::Persistent),
        ("cl-inj", Meter::Injected, false, Frame::Persistent),
        ("cl-inj-nanobs", Meter::InjectedNanObs, false, Frame::Persistent),
        ("cl-inj-nan-batch", Meter::Injected, true, Frame::Batch),
        ("cl-inj-nan-batch-fresh", Meter::Injected, true, Frame::BatchFresh),
        ("cl-inj-nan-batch-fresh-pool", Meter::Injected, true, Frame::BatchFreshPool),
        ("cl-wtfuel-nan-batch-fresh-pool", Meter::WasmtimeFuel, true, Frame::BatchFreshPool),
    ];
    for &(name, meter, nan, frame) in modes {
        // Two independent runs: fuel and digest must be identical.
        let (c1, r1) = run(&wasm, meter, nan, frame);
        let (_, r2) = run(&wasm, meter, nan, frame);
        let (med, p10, p90) = stats(r1.times.clone());
        let (med2, _, _) = stats(r2.times.clone());
        println!(
            "{{\"kind\":\"tick\",\"mode\":\"{name}\",\"n\":{N},\"compile_ms\":{c1:.2},\"median_ms\":{med:.4},\"p10\":{p10:.4},\"p90\":{p90:.4},\"median_ms_run2\":{med2:.4},\"x_native\":{:.2},\"fuel_per_tick\":{},\"digest\":\"{:016x}\",\"digest_eq_native\":{},\"runs_identical\":{},\"mem_bytes\":{},\"state_bytes\":{},\"changed_64b_blocks_last_frame\":{},\"nan_probe\":\"{:016x}\"}}",
            med / nmed,
            r1.fuel_per_tick,
            r1.digest,
            r1.digest == nd,
            r1.digest == r2.digest && r1.fuel_per_tick == r2.fuel_per_tick,
            r1.mem_bytes,
            r1.state_bytes,
            r1.changed_blocks,
            NAN.with(|c| c.get())
        );
    }
    // Plain memcpy of the observed memory size: the cost of a copy-based reset if the SDK fixes the heap.
    for &sz in &[128usize << 10, 1 << 20, 4 << 20, 8 << 20] {
        let src = vec![7u8; sz];
        let mut dst = vec![0u8; sz];
        let mut t = Vec::new();
        for _ in 0..200 {
            let s = Instant::now();
            dst.copy_from_slice(std::hint::black_box(&src));
            std::hint::black_box(&dst);
            t.push(ms(s.elapsed()));
        }
        let (m, _, _) = stats(t);
        println!("{{\"kind\":\"memcpy\",\"bytes\":{sz},\"median_ms\":{m:.4}}}");
    }
}

fn physics(path: &str) {
    let wasm = read(path);
    for &(name, meter, nan) in &[
        ("cl-nan", Meter::None, true),
        ("cl-wtfuel-nan", Meter::WasmtimeFuel, true),
        ("cl", Meter::None, false),
        ("cl-wtfuel", Meter::WasmtimeFuel, false),
        ("cl-inj0-nan", Meter::InjectedGlobal, true),
        ("cl-inj-nan", Meter::Injected, true),
        ("cl-inj", Meter::Injected, false),
        ("cl-inj-nanobs", Meter::InjectedNanObs, false),
    ] {
        let e = engine(meter, nan, false, 8 << 20);
        let bytes = prepare(&wasm, meter);
        let m = Module::new(&e, &bytes).unwrap();
        let mut per = Vec::new();
        let mut digest = 0u64;
        let mut fuel = Vec::new();
        for _ in 0..3 {
            let mut store = Store::new(&e, ());
            if meter == Meter::WasmtimeFuel {
                store.set_fuel(BIG as u64).unwrap();
            }
            let inst = wasmtime::Instance::new(&mut store, &m, &[]).unwrap();
            if meter.injected() {
                inst.get_global(&mut store, "pe_fuel").unwrap().set(&mut store, Val::I64(BIG)).unwrap();
            }
            let run = inst.get_typed_func::<i32, i64>(&mut store, "pe_run_trace").unwrap();
            let s = Instant::now();
            digest = run.call(&mut store, 600).unwrap() as u64;
            per.push(ms(s.elapsed()) / 600.0);
            fuel.push(match meter {
                Meter::WasmtimeFuel => BIG as u64 - store.get_fuel().unwrap(),
                Meter::Injected | Meter::InjectedGlobal | Meter::InjectedNanObs => (BIG - inst.get_global(&mut store, "pe_fuel").unwrap().get(&mut store).unwrap_i64()) as u64,
                Meter::None => 0,
            });
        }
        let (med, _, _) = stats(per);
        println!(
            "{{\"kind\":\"physics\",\"mode\":\"{name}\",\"ms_per_frame_median_of_3\":{med:.4},\"digest\":\"{digest:016x}\",\"digest_eq_native\":{},\"fuel_total\":{},\"fuel_identical_3_runs\":{}}}",
            digest == 0x7903_109a_ab28_ed9b,
            fuel[0],
            fuel.iter().all(|x| *x == fuel[0])
        );
    }
}

/// A module with `rec(n: i32) -> i32` that recurses n deep with `k` extra i64 locals, and `spin()` that
/// loops forever. Built with wasm-encoder so the test needs no toolchain.
fn trap_module(k: u32) -> Vec<u8> {
    use wasm_encoder::*;
    let mut m = Module::new();
    let mut t = TypeSection::new();
    t.ty().function([ValType::I32], [ValType::I32]);
    t.ty().function([], []);
    m.section(&t);
    let mut f = FunctionSection::new();
    f.function(0);
    f.function(1);
    m.section(&f);
    let mut mem = MemorySection::new();
    mem.memory(MemoryType { minimum: 1, maximum: Some(1), memory64: false, shared: false, page_size_log2: None });
    m.section(&mem);
    let mut g = GlobalSection::new();
    g.global(GlobalType { val_type: ValType::I32, mutable: true, shared: false }, &ConstExpr::i32_const(0));
    m.section(&g);
    let mut e = ExportSection::new();
    e.export("rec", ExportKind::Func, 0);
    e.export("spin", ExportKind::Func, 1);
    e.export("memory", ExportKind::Memory, 0);
    m.section(&e);
    let mut c = CodeSection::new();
    // rec: if n == 0 { 0 } else { rec(n - 1) + 1 }, touching k locals so the frame is bigger.
    let mut r = Function::new([(k, ValType::I64)]);
    for i in 0..k {
        r.instruction(&Instruction::LocalGet(0));
        r.instruction(&Instruction::I64ExtendI32U);
        r.instruction(&Instruction::LocalSet(1 + i));
    }
    r.instruction(&Instruction::LocalGet(0));
    r.instruction(&Instruction::I32Eqz);
    r.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    r.instruction(&Instruction::I32Const(0));
    r.instruction(&Instruction::Else);
    r.instruction(&Instruction::LocalGet(0));
    r.instruction(&Instruction::I32Const(1));
    r.instruction(&Instruction::I32Sub);
    r.instruction(&Instruction::Call(0));
    r.instruction(&Instruction::I32Const(1));
    r.instruction(&Instruction::I32Add);
    for i in 0..k {
        // Keep the locals live across the call.
        r.instruction(&Instruction::LocalGet(1 + i));
        r.instruction(&Instruction::I32WrapI64);
        r.instruction(&Instruction::I32Add);
    }
    r.instruction(&Instruction::End);
    r.instruction(&Instruction::End);
    c.function(&r);
    let mut s = Function::new([]);
    s.instruction(&Instruction::Loop(BlockType::Empty));
    s.instruction(&Instruction::GlobalGet(0));
    s.instruction(&Instruction::I32Const(1));
    s.instruction(&Instruction::I32Add);
    s.instruction(&Instruction::GlobalSet(0));
    s.instruction(&Instruction::Br(0));
    s.instruction(&Instruction::End);
    s.instruction(&Instruction::End);
    c.function(&s);
    m.section(&c);
    m.finish()
}

fn traps() {
    for &k in &[0u32, 64] {
        let raw = trap_module(k);
        for &limit in &[1u32 << 12, 1 << 16] {
            let (bytes, rep) = instrument::instrument(&raw, instrument::Config { depth_limit: limit, ..Default::default() }).unwrap();
            for &stack in &[1usize << 20, 8 << 20] {
                let e = engine(Meter::Injected, true, false, stack);
                let m = Module::new(&e, &bytes).unwrap();
                // Deepest n that completes, by bisection on fresh instances; then the trap kind just past it.
                let ok = |n: i32| -> Result<i32, String> {
                    let mut st = Store::new(&e, ());
                    let i = wasmtime::Instance::new(&mut st, &m, &[]).map_err(|e| e.to_string())?;
                    i.get_global(&mut st, "pe_fuel").unwrap().set(&mut st, Val::I64(BIG)).unwrap();
                    let f = i.get_typed_func::<i32, i32>(&mut st, "rec").unwrap();
                    f.call(&mut st, n).map_err(|e| {
                        let d = i.get_global(&mut st, "pe_depth").unwrap().get(&mut st).unwrap_i32();
                        format!("{:?} depth_global={d}", e.downcast_ref::<wasmtime::Trap>())
                    })
                };
                let (mut lo, mut hi) = (0i32, 1 << 22);
                while hi - lo > 1 {
                    let mid = (lo + hi) / 2;
                    if ok(mid).is_ok() { lo = mid } else { hi = mid }
                }
                let why = ok(hi).err().unwrap_or_default();
                println!(
                    "{{\"kind\":\"depth\",\"extra_locals\":{k},\"weight\":{},\"depth_limit\":{limit},\"max_wasm_stack\":{stack},\"deepest_ok\":{lo},\"expected\":{},\"trap\":{:?}}}",
                    rep.max_weight,
                    limit / rep.max_weight - 1,
                    why
                );
            }
        }
    }
    // Fuel: spin() with a budget traps after exactly the same work every time.
    let (bytes, _) = instrument::instrument(&trap_module(0), Default::default()).unwrap();
    let mut seen = Vec::new();
    for _ in 0..3 {
        let e = engine(Meter::Injected, true, false, 1 << 20);
        let m = Module::new(&e, &bytes).unwrap();
        let mut st = Store::new(&e, ());
        let i = wasmtime::Instance::new(&mut st, &m, &[]).unwrap();
        i.get_global(&mut st, "pe_fuel").unwrap().set(&mut st, Val::I64(1_000_000)).unwrap();
        let f = i.get_typed_func::<(), ()>(&mut st, "spin").unwrap();
        let r = f.call(&mut st, ());
                let fuel = i.get_global(&mut st, "pe_fuel").unwrap().get(&mut st).unwrap_i64();
        let counter = i.get_global(&mut st, "pe_g0").unwrap().get(&mut st).unwrap_i32();
        seen.push((counter, fuel, r.is_err()));
    }
    println!(
        "{{\"kind\":\"fuel_trap\",\"budget\":1000000,\"loop_iterations\":{},\"fuel_left\":{},\"trapped\":{},\"identical_3_runs\":{}}}",
        seen[0].0,
        seen[0].1,
        seen[0].2,
        seen.iter().all(|x| *x == seen[0])
    );
    // Refusals.
    let bad_import = {
        use wasm_encoder::*;
        let mut m = Module::new();
        let mut t = TypeSection::new();
        t.ty().function([], []);
        m.section(&t);
        let mut im = ImportSection::new();
        im.import("wasi_snapshot_preview1", "fd_write", EntityType::Function(0));
        m.section(&im);
        m.finish()
    };
    println!("{{\"kind\":\"refuse\",\"case\":\"import outside pe\",\"result\":{:?}}}", instrument::instrument(&bad_import, Default::default()).err());
}

fn load(paths: &[String]) {
    for p in paths {
        let wasm = read(p);
        let e = engine(Meter::Injected, true, false, 8 << 20);
        let mut ti = Vec::new();
        let mut tc = Vec::new();
        let mut tn = Vec::new();
        for _ in 0..5 {
            let s = Instant::now();
            let (b, _) = instrument::instrument(&wasm, Default::default()).unwrap();
            ti.push(ms(s.elapsed()));
            let s = Instant::now();
            let m = Module::new(&e, &b).unwrap();
            tc.push(ms(s.elapsed()));
            let mut l = Linker::<HostState>::new(&e);
            l.define_unknown_imports_as_traps(&m).unwrap();
            let s = Instant::now();
            let mut st = Store::new(&e, HostState::new());
            let _i = l.instantiate(&mut st, &m).unwrap();
            tn.push(ms(s.elapsed()));
        }
        let (i, _, _) = stats(ti);
        let (c, _, _) = stats(tc);
        let (n, _, _) = stats(tn);
        println!("{{\"kind\":\"load\",\"wasm\":{p:?},\"bytes\":{},\"instrument_ms\":{i:.2},\"compile_ms\":{c:.2},\"instantiate_ms\":{n:.3}}}", wasm.len());
    }
}

fn main() {
    // A 64 MiB native stack, so every max_wasm_stack setting below fits inside it.
    std::thread::Builder::new().stack_size(64 << 20).spawn(real_main).unwrap().join().unwrap();
}

fn real_main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(|s| s.as_str()) {
        Some("bench") => bench(&a[2]),
        Some("physics") => physics(&a[2]),
        Some("traps") => traps(),
        Some("load") => load(&a[2..]),
        Some("cols") => cols::cols(&a[2]),
        _ => eprintln!("usage: logic-probe bench|physics|traps|load|cols ..."),
    }
}

//! Runtime adapters: the same guest ABI driven through wasmi, wasmtime or a native build.

use pe_api::fnv;

pub const FUEL_BIG: u64 = 1 << 50;

/// Engine-side state the guest can reach through host calls. Deterministic.
#[derive(Clone)]
pub struct HostState {
    pub rng: u64,
    pub emits: u64,
    pub emit_hash: u64,
}

impl HostState {
    pub fn new() -> HostState {
        HostState { rng: 0x9E37_79B9_7F4A_7C15, emits: 0, emit_hash: 0xcbf2_9ce4_8422_2325 }
    }
    #[inline]
    pub fn terrain(&mut self, x: f32, z: f32) -> f32 {
        let xi = (x * 0.125).floor() as i32;
        let zi = (z * 0.125).floor() as i32;
        ((xi.wrapping_mul(73_856_093) ^ zi.wrapping_mul(19_349_663)) & 7) as f32 * 0.25
    }
    #[inline]
    pub fn emit(&mut self, kind: u32, a: u32, b: f32) {
        self.emits += 1;
        self.emit_hash = fnv(self.emit_hash, (kind as u64) << 32 | a as u64);
        self.emit_hash = fnv(self.emit_hash, b.to_bits() as u64);
    }
    #[inline]
    pub fn rand(&mut self) -> u32 {
        (pe_api::rng_next(&mut self.rng) >> 32) as u32
    }
}

impl pe_api::Host for HostState {
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

pub trait Guest {
    fn init(&mut self, n: u32, seed: u64);
    /// Runs one tick; returns fuel consumed if metering is on.
    fn tick(&mut self, frame: u64, mask: u32) -> Option<u64>;
    fn guest_hash(&mut self) -> u64;
    fn alive(&mut self) -> u32;
    fn save(&mut self) -> Vec<u8>;
    fn load(&mut self, data: &[u8]) -> bool;
    fn hostcalls(&mut self, n: u32, kind: u32) -> u32;
    fn nan_probe(&mut self) -> u64;
    fn host(&self) -> HostState;
    /// Guest state + host-visible effects, comparable across runtimes.
    fn digest(&mut self) -> u64 {
        let h = self.host();
        fnv(fnv(self.guest_hash(), h.emit_hash), h.emits)
    }
}

pub trait Runtime {
    fn name(&self) -> String;
    fn compile(&mut self, wasm: &[u8]) -> Result<(), String>;
    fn instantiate(&self, host: HostState) -> Result<Box<dyn Guest>, String>;
    /// Raw `pe_run_trace(frames) -> u64` for the physics guest (no imports).
    fn run_physics(&self, frames: u32) -> Result<u64, String>;
}

// ------------------------------------------------------------------ native

pub struct NativeRt;

pub struct NativeGuest {
    world: Option<pe_api::World>,
    host: HostState,
}

impl Runtime for NativeRt {
    fn name(&self) -> String {
        "native".into()
    }
    fn compile(&mut self, _wasm: &[u8]) -> Result<(), String> {
        Ok(())
    }
    fn instantiate(&self, host: HostState) -> Result<Box<dyn Guest>, String> {
        Ok(Box::new(NativeGuest { world: None, host }))
    }
    fn run_physics(&self, _frames: u32) -> Result<u64, String> {
        Err("not available natively here".into())
    }
}

impl Guest for NativeGuest {
    fn init(&mut self, n: u32, seed: u64) {
        self.world = Some(pe_api::World::new(n as usize, seed));
    }
    fn tick(&mut self, frame: u64, mask: u32) -> Option<u64> {
        logic_game::tick(self.world.as_mut().unwrap(), frame, mask, &mut self.host);
        None
    }
    fn guest_hash(&mut self) -> u64 {
        self.world.as_ref().unwrap().hash()
    }
    fn alive(&mut self) -> u32 {
        self.world.as_ref().unwrap().alive_count()
    }
    fn save(&mut self) -> Vec<u8> {
        let mut v = Vec::new();
        self.world.as_ref().unwrap().save(&mut v);
        v
    }
    fn load(&mut self, data: &[u8]) -> bool {
        self.world = pe_api::World::load(data);
        self.world.is_some()
    }
    fn hostcalls(&mut self, n: u32, kind: u32) -> u32 {
        logic_game::hostcalls(n, kind, &mut self.host)
    }
    fn nan_probe(&mut self) -> u64 {
        logic_game::nan_probe()
    }
    fn host(&self) -> HostState {
        self.host.clone()
    }
}

// ------------------------------------------------------------------ wasmi

pub struct WasmiRt {
    label: String,
    engine: wasmi::Engine,
    module: Option<wasmi::Module>,
    fuel: bool,
}

impl WasmiRt {
    pub fn new(fuel: bool, eager: bool) -> WasmiRt {
        let mut cfg = wasmi::Config::default();
        cfg.consume_fuel(fuel);
        if eager {
            cfg.compilation_mode(wasmi::CompilationMode::Eager);
        }
        let label = format!("wasmi{}{}", if eager { "-eager" } else { "-lazy" }, if fuel { "-fuel" } else { "" });
        WasmiRt { label, engine: wasmi::Engine::new(&cfg), module: None, fuel }
    }
}

macro_rules! guest_funcs {
    ($tf:ident, $get:expr) => {
        struct $tf {
            init: TF<(i32, i64), ()>,
            tick: TF<(i64, i32), ()>,
            hash: TF<(), i64>,
            alive: TF<(), i32>,
            save: TF<(), i32>,
            buf_ptr: TF<(), i32>,
            buf_alloc: TF<i32, i32>,
            load: TF<i32, i32>,
            hostcalls: TF<(i32, i32), i32>,
            nan_probe: TF<(), i64>,
        }
    };
}

mod wasmi_impl {
    use super::*;
    use wasmi::{Caller, Linker, Memory, Store, TypedFunc as TF};
    guest_funcs!(Funcs, ());

    pub struct WasmiGuest {
        store: Store<HostState>,
        mem: Memory,
        f: Funcs,
        fuel: bool,
    }

    fn linker(engine: &wasmi::Engine) -> Linker<HostState> {
        let mut l = Linker::<HostState>::new(engine);
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

    impl Runtime for WasmiRt {
        fn name(&self) -> String {
            self.label.clone()
        }
        fn compile(&mut self, wasm: &[u8]) -> Result<(), String> {
            self.module = Some(wasmi::Module::new(&self.engine, wasm).map_err(|e| e.to_string())?);
            Ok(())
        }
        fn instantiate(&self, host: HostState) -> Result<Box<dyn Guest>, String> {
            let m = self.module.as_ref().ok_or("no module")?;
            let mut store = Store::new(&self.engine, host);
            if self.fuel {
                store.set_fuel(FUEL_BIG).unwrap();
            }
            let inst = linker(&self.engine).instantiate_and_start(&mut store, m).map_err(|e| e.to_string())?;
            let mem = inst.get_memory(&store, "memory").ok_or("memory")?;
            macro_rules! g {
                ($n:literal) => {
                    inst.get_typed_func(&store, $n).map_err(|e| format!("{}: {e}", $n))?
                };
            }
            let f = Funcs {
                init: g!("pe_init"),
                tick: g!("pe_tick"),
                hash: g!("pe_hash"),
                alive: g!("pe_alive"),
                save: g!("pe_save"),
                buf_ptr: g!("pe_buf_ptr"),
                buf_alloc: g!("pe_buf_alloc"),
                load: g!("pe_load"),
                hostcalls: g!("pe_hostcalls"),
                nan_probe: g!("pe_nan_probe"),
            };
            Ok(Box::new(WasmiGuest { store, mem, f, fuel: self.fuel }))
        }
        fn run_physics(&self, frames: u32) -> Result<u64, String> {
            let m = self.module.as_ref().ok_or("no module")?;
            let mut store = Store::new(&self.engine, HostState::new());
            if self.fuel {
                store.set_fuel(FUEL_BIG).unwrap();
            }
            let inst = Linker::<HostState>::new(&self.engine)
                .instantiate_and_start(&mut store, m)
                .map_err(|e| e.to_string())?;
            let run = inst.get_typed_func::<i32, i64>(&store, "pe_run_trace").map_err(|e| e.to_string())?;
            run.call(&mut store, frames as i32).map(|x| x as u64).map_err(|e| e.to_string())
        }
    }

    impl WasmiGuest {
        fn refuel(&mut self) {
            if self.fuel {
                self.store.set_fuel(FUEL_BIG).unwrap();
            }
        }
    }

    impl Guest for WasmiGuest {
        fn init(&mut self, n: u32, seed: u64) {
            self.refuel();
            self.f.init.call(&mut self.store, (n as i32, seed as i64)).unwrap();
        }
        fn tick(&mut self, frame: u64, mask: u32) -> Option<u64> {
            self.refuel();
            self.f.tick.call(&mut self.store, (frame as i64, mask as i32)).unwrap();
            self.fuel.then(|| FUEL_BIG - self.store.get_fuel().unwrap())
        }
        fn guest_hash(&mut self) -> u64 {
            self.refuel();
            self.f.hash.call(&mut self.store, ()).unwrap() as u64
        }
        fn alive(&mut self) -> u32 {
            self.refuel();
            self.f.alive.call(&mut self.store, ()).unwrap() as u32
        }
        fn save(&mut self) -> Vec<u8> {
            self.refuel();
            let len = self.f.save.call(&mut self.store, ()).unwrap() as usize;
            let ptr = self.f.buf_ptr.call(&mut self.store, ()).unwrap() as usize;
            self.mem.data(&self.store)[ptr..ptr + len].to_vec()
        }
        fn load(&mut self, data: &[u8]) -> bool {
            self.refuel();
            let ptr = self.f.buf_alloc.call(&mut self.store, data.len() as i32).unwrap() as usize;
            self.mem.data_mut(&mut self.store)[ptr..ptr + data.len()].copy_from_slice(data);
            self.f.load.call(&mut self.store, data.len() as i32).unwrap() == 0
        }
        fn hostcalls(&mut self, n: u32, kind: u32) -> u32 {
            self.refuel();
            self.f.hostcalls.call(&mut self.store, (n as i32, kind as i32)).unwrap() as u32
        }
        fn nan_probe(&mut self) -> u64 {
            self.refuel();
            self.f.nan_probe.call(&mut self.store, ()).unwrap() as u64
        }
        fn host(&self) -> HostState {
            self.store.data().clone()
        }
    }
}

// ------------------------------------------------------------------ wasmtime

#[derive(Clone, Copy)]
pub struct WtOpts {
    pub winch: bool,
    pub fuel: bool,
    pub epoch: bool,
    pub nan_canon: bool,
    pub parallel: bool,
}

pub struct WasmtimeRt {
    label: String,
    engine: wasmtime::Engine,
    module: Option<wasmtime::Module>,
    o: WtOpts,
}

impl WasmtimeRt {
    pub fn new(o: WtOpts) -> Result<WasmtimeRt, String> {
        let mut cfg = wasmtime::Config::new();
        cfg.strategy(if o.winch { wasmtime::Strategy::Winch } else { wasmtime::Strategy::Cranelift });
        cfg.consume_fuel(o.fuel);
        cfg.epoch_interruption(o.epoch);
        cfg.parallel_compilation(o.parallel);
        // Despite the name, Winch honours this flag too (winch-codegen maybe_canonicalize_nan).
        cfg.cranelift_nan_canonicalization(o.nan_canon);
        // Determinism: relaxed SIMD is not used by the guest, but pin it anyway.
        cfg.relaxed_simd_deterministic(true);
        let engine = wasmtime::Engine::new(&cfg).map_err(|e| e.to_string())?;
        let label = format!(
            "wasmtime-{}{}{}{}{}",
            if o.winch { "winch" } else { "cranelift" },
            if o.fuel { "-fuel" } else { "" },
            if o.epoch { "-epoch" } else { "" },
            if o.nan_canon { "-nancanon" } else { "" },
            if o.parallel { "" } else { "-serial" }
        );
        Ok(WasmtimeRt { label, engine, module: None, o })
    }
}

mod wasmtime_impl {
    use super::*;
    use wasmtime::{Caller, Linker, Memory, Store, TypedFunc as TF};
    guest_funcs!(Funcs, ());

    pub struct WtGuest {
        store: Store<HostState>,
        mem: Memory,
        f: Funcs,
        fuel: bool,
    }

    fn linker(engine: &wasmtime::Engine) -> Linker<HostState> {
        let mut l = Linker::<HostState>::new(engine);
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

    fn new_store(rt: &WasmtimeRt, host: HostState) -> Store<HostState> {
        let mut store = Store::new(&rt.engine, host);
        if rt.o.fuel {
            store.set_fuel(FUEL_BIG).unwrap();
        }
        if rt.o.epoch {
            store.set_epoch_deadline(u64::MAX / 2);
        }
        store
    }

    impl Runtime for WasmtimeRt {
        fn name(&self) -> String {
            self.label.clone()
        }
        fn compile(&mut self, wasm: &[u8]) -> Result<(), String> {
            self.module = Some(wasmtime::Module::new(&self.engine, wasm).map_err(|e| e.to_string())?);
            Ok(())
        }
        fn instantiate(&self, host: HostState) -> Result<Box<dyn Guest>, String> {
            let m = self.module.as_ref().ok_or("no module")?;
            let mut store = new_store(self, host);
            let inst = linker(&self.engine).instantiate(&mut store, m).map_err(|e| e.to_string())?;
            let mem = inst.get_memory(&mut store, "memory").ok_or("memory")?;
            macro_rules! g {
                ($n:literal) => {
                    inst.get_typed_func(&mut store, $n).map_err(|e| format!("{}: {e}", $n))?
                };
            }
            let f = Funcs {
                init: g!("pe_init"),
                tick: g!("pe_tick"),
                hash: g!("pe_hash"),
                alive: g!("pe_alive"),
                save: g!("pe_save"),
                buf_ptr: g!("pe_buf_ptr"),
                buf_alloc: g!("pe_buf_alloc"),
                load: g!("pe_load"),
                hostcalls: g!("pe_hostcalls"),
                nan_probe: g!("pe_nan_probe"),
            };
            Ok(Box::new(WtGuest { store, mem, f, fuel: self.o.fuel }))
        }
        fn run_physics(&self, frames: u32) -> Result<u64, String> {
            let m = self.module.as_ref().ok_or("no module")?;
            let mut store = new_store(self, HostState::new());
            let inst = wasmtime::Instance::new(&mut store, m, &[]).map_err(|e| e.to_string())?;
            let run = inst.get_typed_func::<i32, i64>(&mut store, "pe_run_trace").map_err(|e| e.to_string())?;
            run.call(&mut store, frames as i32).map(|x| x as u64).map_err(|e| e.to_string())
        }
    }

    impl WtGuest {
        fn refuel(&mut self) {
            if self.fuel {
                self.store.set_fuel(FUEL_BIG).unwrap();
            }
        }
    }

    impl Guest for WtGuest {
        fn init(&mut self, n: u32, seed: u64) {
            self.refuel();
            self.f.init.call(&mut self.store, (n as i32, seed as i64)).unwrap();
        }
        fn tick(&mut self, frame: u64, mask: u32) -> Option<u64> {
            self.refuel();
            self.f.tick.call(&mut self.store, (frame as i64, mask as i32)).unwrap();
            self.fuel.then(|| FUEL_BIG - self.store.get_fuel().unwrap())
        }
        fn guest_hash(&mut self) -> u64 {
            self.refuel();
            self.f.hash.call(&mut self.store, ()).unwrap() as u64
        }
        fn alive(&mut self) -> u32 {
            self.refuel();
            self.f.alive.call(&mut self.store, ()).unwrap() as u32
        }
        fn save(&mut self) -> Vec<u8> {
            self.refuel();
            let len = self.f.save.call(&mut self.store, ()).unwrap() as usize;
            let ptr = self.f.buf_ptr.call(&mut self.store, ()).unwrap() as usize;
            self.mem.data(&self.store)[ptr..ptr + len].to_vec()
        }
        fn load(&mut self, data: &[u8]) -> bool {
            self.refuel();
            let ptr = self.f.buf_alloc.call(&mut self.store, data.len() as i32).unwrap() as usize;
            self.mem.data_mut(&mut self.store)[ptr..ptr + data.len()].copy_from_slice(data);
            self.f.load.call(&mut self.store, data.len() as i32).unwrap() == 0
        }
        fn hostcalls(&mut self, n: u32, kind: u32) -> u32 {
            self.refuel();
            self.f.hostcalls.call(&mut self.store, (n as i32, kind as i32)).unwrap() as u32
        }
        fn nan_probe(&mut self) -> u64 {
            self.refuel();
            self.f.nan_probe.call(&mut self.store, ()).unwrap() as u64
        }
        fn host(&self) -> HostState {
            self.store.data().clone()
        }
    }
}

/// Parse a runtime spec like `wasmi-eager-fuel`, `wasmtime-cranelift-fuel-nancanon`, `native`.
pub fn make(spec: &str) -> Result<Box<dyn Runtime>, String> {
    let has = |t: &str| spec.split('-').any(|p| p == t);
    if spec == "native" {
        return Ok(Box::new(NativeRt));
    }
    if spec.starts_with("wasmi") {
        return Ok(Box::new(WasmiRt::new(has("fuel"), !has("lazy"))));
    }
    if spec.starts_with("wasmtime") {
        let o = WtOpts {
            winch: has("winch"),
            fuel: has("fuel"),
            epoch: has("epoch"),
            nan_canon: has("nancanon"),
            parallel: !has("serial"),
        };
        return Ok(Box::new(WasmtimeRt::new(o)?));
    }
    Err(format!("unknown runtime {spec}"))
}

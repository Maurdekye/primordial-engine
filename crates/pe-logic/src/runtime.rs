//! The wasmtime host (docs/spec/logic.md 5.1): the engine's fixed settings, a script's compiled module, and an
//! instance with injected fuel and depth, the per-frame reset and deterministic faults (3.6).

use crate::abi;
use crate::meter::{self, Config as MeterConfig, DEPTH_GLOBAL, FUEL_GLOBAL, GLOBAL_PREFIX, Refusal, Report};
use wasmtime::{
    Caller, Config, Engine, Extern, Global, Instance as WtInstance, InstancePre, Linker, Memory, Module, OptLevel,
    Store, StoreLimits, StoreLimitsBuilder, Strategy, Trap, Val,
};

/// The settings every machine uses. They are part of a build's identity, like the pass version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub meter: MeterConfig,
    /// wasmtime's own stack limit. Set above what the depth meter guarantees, so a script hits the depth meter, a
    /// deterministic fault, before wasmtime's stack check, which depends on the machine's frame sizes.
    pub max_wasm_stack: usize,
    /// A memory limit per instance; `memory.grow` past it returns -1, the same on every machine.
    pub max_memory: usize,
}

impl Settings {
    /// The native stack the thread that runs guests needs: wasmtime's limit plus room for the host's own frames.
    /// wasmtime checks `max_wasm_stack` against the stack it entered on, so a thread with less than this could
    /// overflow natively before either the depth meter or wasmtime's check fires. Spawn guest threads with it.
    pub fn thread_stack(&self) -> usize {
        self.max_wasm_stack + (2 << 20)
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings { meter: MeterConfig::default(), max_wasm_stack: 8 << 20, max_memory: 64 << 20 }
    }
}

/// The engine of 5.1: Cranelift at `speed`, wasmtime's fuel and epochs off (metering is in the module), the
/// features of 5.3 and NaN canonicalization by Cranelift unless the pass canonicalizes at observation points.
pub fn engine(s: &Settings) -> wasmtime::Result<Engine> {
    let mut c = Config::new();
    c.strategy(Strategy::Cranelift);
    c.cranelift_opt_level(OptLevel::Speed);
    c.cranelift_nan_canonicalization(!s.meter.nan_observe);
    c.consume_fuel(false);
    c.epoch_interruption(false);
    c.wasm_simd(!s.meter.nan_observe);
    c.wasm_relaxed_simd(false);
    c.wasm_memory64(false);
    c.wasm_tail_call(false);
    c.wasm_multi_memory(false);
    // wasmtime refuses a max_wasm_stack above its async stack size, even with async off.
    c.async_stack_size(s.max_wasm_stack.max(2 << 20));
    c.max_wasm_stack(s.max_wasm_stack);
    Engine::new(&c)
}

/// What a running system may ask of the world (3.4). Every answer is a pure function of the state at the start of
/// the system call and the arguments. The batch runner implements it over the store.
pub trait Services: Send {
    /// One row of `component` on `object`, into `dst`; `false` when the object has no such row.
    fn get(&mut self, component: u32, object: [u8; 16], dst: &mut [u8]) -> bool;
    /// Objects near the bound object with `component`, nearest first then by id, as records written into `dst`;
    /// returns how many (at most `cap`).
    fn query_near(&mut self, component: u32, radius: f32, dst: &mut [u8], cap: u32) -> u32;
    /// 64 bits of the domain's counter-based stream for (frame, system, `object`, `stream`).
    fn rand(&mut self, object: [u8; 16], stream: u32) -> u64;
}

/// Services that know nothing: for loading, the manifest call and tests.
pub struct NoServices;

impl Services for NoServices {
    fn get(&mut self, _: u32, _: [u8; 16], _: &mut [u8]) -> bool {
        false
    }
    fn query_near(&mut self, _: u32, _: f32, _: &mut [u8], _: u32) -> u32 {
        0
    }
    fn rand(&mut self, _: [u8; 16], _: u32) -> u64 {
        0
    }
}

pub struct HostState {
    limits: StoreLimits,
    pub services: Box<dyn Services>,
    /// `pe.log` lines, kept only where the caller wants them (the author's tools).
    pub log: Option<Vec<String>>,
}

/// A host function's own error: the guest passed a range outside its memory. Deterministic, so it is a fault.
#[derive(Debug)]
struct BadPointer;

impl std::fmt::Display for BadPointer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a host call was given a range outside the module's memory")
    }
}

impl std::error::Error for BadPointer {}

fn memory(c: &mut Caller<'_, HostState>) -> wasmtime::Result<Memory> {
    match c.get_export(abi::MEMORY_EXPORT) {
        Some(Extern::Memory(m)) => Ok(m),
        _ => Err(BadPointer.into()),
    }
}

fn range(len_mem: usize, ptr: u32, len: usize) -> wasmtime::Result<std::ops::Range<usize>> {
    let a = ptr as usize;
    let b = a.checked_add(len).ok_or(BadPointer)?;
    if b > len_mem { Err(BadPointer.into()) } else { Ok(a..b) }
}

/// The host functions of ABI v0.
fn linker(e: &Engine) -> wasmtime::Result<Linker<HostState>> {
    let mut l = Linker::<HostState>::new(e);
    l.func_wrap(abi::IMPORT_MODULE, "get", |mut c: Caller<'_, HostState>, comp: i32, obj: i32, dst: i32| {
        let m = memory(&mut c)?;
        let (data, st) = m.data_and_store_mut(&mut c);
        let r = range(data.len(), obj as u32, 16)?;
        let id: [u8; 16] = data[r].try_into().expect("16 bytes");
        // The row size comes from the component's description; the services check `dst` against it.
        let rest = range(data.len(), dst as u32, 0)?.start;
        let found = st.services.get(comp as u32, id, &mut data[rest..]);
        Ok(found as i32)
    })?;
    l.func_wrap(
        abi::IMPORT_MODULE,
        "query_near",
        |mut c: Caller<'_, HostState>, comp: i32, radius: f32, dst: i32, cap: i32| {
            let m = memory(&mut c)?;
            let (data, st) = m.data_and_store_mut(&mut c);
            let start = range(data.len(), dst as u32, 0)?.start;
            Ok(st.services.query_near(comp as u32, radius, &mut data[start..], cap.max(0) as u32) as i32)
        },
    )?;
    l.func_wrap(abi::IMPORT_MODULE, "rand", |mut c: Caller<'_, HostState>, obj: i32, stream: i32| {
        let m = memory(&mut c)?;
        let (data, st) = m.data_and_store_mut(&mut c);
        let r = range(data.len(), obj as u32, 16)?;
        let id: [u8; 16] = data[r].try_into().expect("16 bytes");
        Ok(st.services.rand(id, stream as u32) as i64)
    })?;
    l.func_wrap(abi::IMPORT_MODULE, "log", |mut c: Caller<'_, HostState>, ptr: i32, len: i32| {
        let m = memory(&mut c)?;
        let (data, st) = m.data_and_store_mut(&mut c);
        let r = range(data.len(), ptr as u32, len.max(0) as usize)?;
        if let Some(log) = st.log.as_mut() {
            log.push(String::from_utf8_lossy(&data[r]).into_owned());
        }
        Ok(())
    })?;
    Ok(l)
}

/// Why a module could not be loaded.
#[derive(Debug)]
pub enum LoadError {
    Refused(Refusal),
    /// Cranelift or the linker failed on a module the validator accepted: a local fault, never a world fault.
    Compile(String),
    /// [`Script::load_metered`]: the bytes are not the module the row names.
    NotTheModule {
        expected: pe_core::Hash,
        got: pe_core::Hash,
    },
    /// [`crate::check_identity`]: this machine's runner is not the one the bundle or world recorded.
    OtherRunner,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Refused(r) => write!(f, "refused: {r}"),
            LoadError::Compile(e) => write!(f, "compile: {e}"),
            LoadError::OtherRunner => write!(f, "not the runner the bundle or world recorded (pe_logic::identity)"),
            LoadError::NotTheModule { expected, got } => {
                write!(f, "not the module the row names: expected {expected:?}, got {got:?}")
            }
        }
    }
}

impl std::error::Error for LoadError {}

/// A script's module: validated, metered and compiled for this engine.
pub struct Script {
    engine: Engine,
    settings: Settings,
    pre: InstancePre<HostState>,
    pub report: Report,
    /// The metered bytes, which every machine produces identically from the same input.
    pub metered: Vec<u8>,
}

impl Script {
    pub fn load(engine: &Engine, settings: Settings, wasm: &[u8]) -> Result<Script, LoadError> {
        let (metered, report) = meter::instrument(wasm, settings.meter).map_err(LoadError::Refused)?;
        let module = Module::new(engine, &metered).map_err(|e| LoadError::Compile(e.to_string()))?;
        let pre =
            linker(engine).and_then(|l| l.instantiate_pre(&module)).map_err(|e| LoadError::Compile(e.to_string()))?;
        Ok(Script { engine: engine.clone(), settings, pre, report, metered })
    }

    /// Loads bytes the metering pass already produced, as they are: a fault bundle carries exactly the module that
    /// ran (S2 `Replica::module`; @coordinator-opus). Refused unless `Hash::of("pe.logic.module",
    /// bytes)` equals `expected`, the row's `author_module`; then [`meter::check_metered`] checks what the runtime
    /// relies on, and the module compiles on the same engine and linker as [`Script::load`]. The pass is never run
    /// again: metering twice gives other bytes and another hash. `report.segments` and `report.max_weight` are 0.
    ///
    /// **`settings` must be the world's, the ones the bytes were metered with.** The hash pins the bytes, not the
    /// settings: the pass bakes `meter.depth_limit` into the module while the host classifies a depth trap by
    /// `settings.meter.depth_limit`, so a larger limit here turns every depth fault into `Unreachable`; and bytes
    /// metered with `nan_observe` off but loaded with it on run with neither Cranelift's NaN canonicalization nor the
    /// pass's, so NaN bits differ. Nothing here can detect either (@shader-reviewer's Low on e6f4b242). A replay
    /// caller checks [`crate::check_identity`] against the runner identity the bundle or world recorded before it
    /// loads (@coordinator-opus: replay refuses a mismatched runner).
    pub fn load_metered(
        engine: &Engine,
        settings: Settings,
        metered: &[u8],
        expected: pe_core::Hash,
    ) -> Result<Script, LoadError> {
        let got = pe_core::Hash::of("pe.logic.module", metered);
        if got != expected {
            return Err(LoadError::NotTheModule { expected, got });
        }
        let report = meter::check_metered(metered, settings.meter).map_err(LoadError::Refused)?;
        let module = Module::new(engine, metered).map_err(|e| LoadError::Compile(e.to_string()))?;
        let pre =
            linker(engine).and_then(|l| l.instantiate_pre(&module)).map_err(|e| LoadError::Compile(e.to_string()))?;
        Ok(Script { engine: engine.clone(), settings, pre, report, metered: metered.to_vec() })
    }

    pub fn instantiate(&self, services: Box<dyn Services>) -> wasmtime::Result<Instance> {
        Instance::new(self, services)
    }

    /// The hash of the metered bytes, which is what runs: `CommitScript::author_module` carries the author's, and a
    /// machine whose own differs never runs the module (6.2 step 3, a build fault).
    pub fn module_hash(&self) -> pe_core::Hash {
        pe_core::Hash::of("pe.logic.module", &self.metered)
    }
}

/// How a call ended badly (3.6). Every kind but `HostStack` is the same on every machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The fuel ran out; `budget` was the call's.
    Fuel {
        budget: u64,
    },
    /// The call depth passed the limit.
    Depth {
        limit: u32,
    },
    Unreachable,
    OutOfBounds,
    DivideByZero,
    IntegerOverflow,
    BadConversion,
    IndirectCall,
    /// A host call was handed a range outside the module's memory.
    BadPointer,
    /// The call returned a status other than 0.
    Status(i32),
    /// A row the call wrote, or a row a command carried, is not a legal value of its component (3.3 step 1).
    InvalidRow {
        component: String,
        field: String,
    },
    /// A command the system may not give, or a malformed one (3.3 step 3).
    BadCommand(String),
    /// wasmtime's own stack check fired before the depth meter: `max_wasm_stack` is below what the depth limit
    /// allows on this machine. A local fault (a configuration bug), never world state.
    HostStack,
    /// Any other trap, by wasmtime's description.
    Other(String),
}

impl Fault {
    /// True for the faults that are world state (3.6); false for a local fault of this machine.
    pub fn is_deterministic(&self) -> bool {
        !matches!(self, Fault::HostStack)
    }

    /// The fault's kind as the world records it in the script's modules row (3.6; world-model's codec reads this
    /// table). Stable: a code is never reused or renumbered. None for [`Fault::HostStack`], which is never world state.
    pub fn code(&self) -> Option<u8> {
        Some(match self {
            Fault::Fuel { .. } => 1,
            Fault::Depth { .. } => 2,
            Fault::Unreachable => 3,
            Fault::OutOfBounds => 4,
            Fault::DivideByZero => 5,
            Fault::IntegerOverflow => 6,
            Fault::BadConversion => 7,
            Fault::IndirectCall => 8,
            Fault::BadPointer => 9,
            Fault::Status(_) => 10,
            Fault::InvalidRow { .. } => 11,
            Fault::BadCommand(_) => 12,
            Fault::Other(_) => 13,
            Fault::HostStack => return None,
        })
    }
}

/// One live instance. Between frames, [`Instance::reset`] brings it back to its state just after instantiation.
pub struct Instance {
    store: Store<HostState>,
    inst: WtInstance,
    memory: Memory,
    fuel: Global,
    depth: Global,
    /// The module's own mutable globals and their values after instantiation.
    globals: Vec<(Global, Val)>,
    image: Vec<u8>,
    depth_limit: u32,
}

impl Instance {
    fn new(script: &Script, services: Box<dyn Services>) -> wasmtime::Result<Instance> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(script.settings.max_memory)
            .memories(1)
            .instances(1)
            .trap_on_grow_failure(false)
            .build();
        let mut store = Store::new(&script.engine, HostState { limits, services, log: None });
        store.limiter(|s| &mut s.limits);
        let inst = script.pre.instantiate(&mut store)?;
        let memory = inst.get_memory(&mut store, abi::MEMORY_EXPORT).expect("validated: the memory is exported");
        let fuel = inst.get_global(&mut store, FUEL_GLOBAL).expect("added by the pass");
        let depth = inst.get_global(&mut store, DEPTH_GLOBAL).expect("added by the pass");
        let mut globals = Vec::new();
        for g in &script.report.mutable_globals {
            let gl = inst.get_global(&mut store, &format!("{GLOBAL_PREFIX}{g}")).expect("exported by the pass");
            let v = gl.get(&mut store);
            globals.push((gl, v));
        }
        let image = memory.data(&store).to_vec();
        Ok(Instance {
            store,
            inst,
            memory,
            fuel,
            depth,
            globals,
            image,
            depth_limit: script.settings.meter.depth_limit,
        })
    }

    pub fn services_mut(&mut self) -> &mut Box<dyn Services> {
        &mut self.store.data_mut().services
    }

    pub fn set_services(&mut self, s: Box<dyn Services>) -> Box<dyn Services> {
        std::mem::replace(&mut self.store.data_mut().services, s)
    }

    /// Keep `pe.log` lines from now on (the author's tools only).
    pub fn keep_log(&mut self, on: bool) {
        self.store.data_mut().log = on.then(Vec::new);
    }

    pub fn take_log(&mut self) -> Vec<String> {
        self.store.data_mut().log.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// The bits of the module's own mutable globals, in index order: for tests and the inspector.
    pub fn global_bits(&mut self) -> Vec<u64> {
        let gs: Vec<Global> = self.globals.iter().map(|(g, _)| *g).collect();
        gs.iter()
            .map(|g| match g.get(&mut self.store) {
                Val::I32(x) => x as u32 as u64,
                Val::I64(x) => x as u64,
                Val::F32(x) => x as u64,
                Val::F64(x) => x,
                _ => 0,
            })
            .collect()
    }

    pub fn memory(&self) -> &[u8] {
        self.memory.data(&self.store)
    }

    pub fn memory_mut(&mut self) -> &mut [u8] {
        self.memory.data_mut(&mut self.store)
    }

    /// Restore memory and globals to their state after instantiation (5.1). Returns false when the memory has
    /// grown, which a copy cannot undo: the caller then makes a fresh instance. Tables and passive segments
    /// cannot change: the validator refuses the operators that would change them.
    pub fn reset(&mut self) -> bool {
        if self.memory.data_size(&self.store) != self.image.len() {
            return false;
        }
        self.memory.data_mut(&mut self.store).copy_from_slice(&self.image);
        for (g, v) in &self.globals {
            g.set(&mut self.store, *v).expect("same type");
        }
        self.depth.set(&mut self.store, Val::I32(0)).expect("i32");
        true
    }

    /// Call an export with a fuel budget. Returns the results and the fuel used, or the fault.
    pub fn call<P, R>(&mut self, name: &str, args: P, budget: u64) -> Result<(R, u64), Fault>
    where
        P: wasmtime::WasmParams,
        R: wasmtime::WasmResults,
    {
        // Below 2^62, so no count the module flushes can reach the fuel trap's mark (`meter::FUEL_TRAPPED`).
        let budget = budget.min(1 << 62);
        self.fuel.set(&mut self.store, Val::I64(budget as i64)).expect("i64");
        self.depth.set(&mut self.store, Val::I32(0)).expect("i32");
        let f = self
            .inst
            .get_typed_func::<P, R>(&mut self.store, name)
            .map_err(|e| Fault::Other(format!("export {name:?}: {e}")))?;
        let r = f.call(&mut self.store, args);
        let left = self.fuel.get(&mut self.store).unwrap_i64();
        match r {
            Ok(v) => Ok((v, budget - left.max(0) as u64)),
            Err(e) => Err(self.classify(&e, budget, left)),
        }
    }

    fn classify(&mut self, e: &wasmtime::Error, budget: u64, fuel_left: i64) -> Fault {
        if e.downcast_ref::<BadPointer>().is_some() {
            return Fault::BadPointer;
        }
        let Some(t) = e.downcast_ref::<Trap>() else { return Fault::Other(e.to_string()) };
        match t {
            Trap::UnreachableCodeReached => {
                // The depth meter raises the global past the limit before it traps, and only then; a fuel trap
                // leaves its own mark. Depth first: a caller may have flushed a negative count that no check has
                // seen yet (shader-reviewer L3).
                let depth = self.depth.get(&mut self.store).unwrap_i32() as u32;
                if depth > self.depth_limit {
                    Fault::Depth { limit: self.depth_limit }
                } else if fuel_left == crate::meter::FUEL_TRAPPED {
                    Fault::Fuel { budget }
                } else {
                    Fault::Unreachable
                }
            }
            Trap::MemoryOutOfBounds | Trap::TableOutOfBounds | Trap::HeapMisaligned => Fault::OutOfBounds,
            Trap::IntegerDivisionByZero => Fault::DivideByZero,
            Trap::IntegerOverflow => Fault::IntegerOverflow,
            Trap::BadConversionToInteger => Fault::BadConversion,
            Trap::IndirectCallToNull | Trap::BadSignature => Fault::IndirectCall,
            Trap::StackOverflow => Fault::HostStack,
            other => Fault::Other(format!("{other:?}")),
        }
    }

    /// `pe_run(system, batch)`: a status other than 0 is a fault.
    pub fn run_system(&mut self, system: u32, batch: u32, budget: u64) -> Result<u64, Fault> {
        let (status, used) = self.call::<(i32, i32), i32>("pe_run", (system as i32, batch as i32), budget)?;
        if status != 0 { Err(Fault::Status(status)) } else { Ok(used) }
    }
}

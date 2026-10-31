//! Phase-1 spike (d): Rust game logic compiled to wasm32-unknown-unknown, run in wasmi
//! (pure-Rust interpreter, deterministic profile, fuel metering), hot-swapped mid-simulation.
//!
//! Checks:
//!   D1  peer A (knows the v1->v2 commit in advance) and peer B (learns it 45 frames after its
//!       activation frame, together with late interactions) end with identical per-frame hashes;
//!       B gets there by rolling back physics + guest memory + module version across the swap.
//!   D2  a control peer that never learns the commit diverges from frame 300 on (the swap is real).
//!   D3  a runaway edit (infinite loop) is stopped by fuel at exactly the same point every time.
//!   D4  the physics spike's reference run compiled to WASM produces the native digest.
//!
//! usage: pe-spike-wasm-hotswap [guest-dir]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use box3d_rust::body::body_get_position;
use pe_sim_core::*;
use wasmi::{Caller, CompilationMode, Config, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

const SEED: u64 = 0x5052_494D_4F52_4449;
const FRAMES: u64 = 600;
const FUEL_PER_TICK: u64 = 1_000_000;
const SWAP_FRAME: u64 = 300;
const B_LEARNS_AT: u64 = 345;
const WINDOW: usize = 120;

#[derive(Default)]
struct HostState {
    /// Read-only view of object positions, taken before the tick (consistent for the guest).
    positions: Vec<[f32; 3]>,
    /// Command buffer: impulses the guest requested this tick.
    impulses: Vec<(u32, [f32; 3])>,
    bad_calls: u32,
}

fn linker(engine: &Engine) -> Linker<HostState> {
    let mut l = Linker::<HostState>::new(engine);
    l.func_wrap("pe", "body_count", |c: Caller<'_, HostState>| -> i32 { c.data().positions.len() as i32 })
        .unwrap();
    l.func_wrap("pe", "body_pos", |c: Caller<'_, HostState>, i: i32, axis: i32| -> f32 {
        c.data().positions.get(i as usize).and_then(|p| p.get(axis as usize)).copied().unwrap_or(0.0)
    })
    .unwrap();
    l.func_wrap("pe", "apply_impulse", |mut c: Caller<'_, HostState>, i: i32, x: f32, y: f32, z: f32| {
        // Host-side validation at the boundary: reject NaN/inf and out-of-range ids.
        let h = c.data_mut();
        if i < 0 || i as usize >= h.positions.len() || !(x.is_finite() && y.is_finite() && z.is_finite()) {
            h.bad_calls += 1;
            return;
        }
        h.impulses.push((i as u32, [x, y, z]));
    })
    .unwrap();
    l
}

struct Guest {
    module_idx: usize,
    store: Store<HostState>,
    instance: Instance,
    memory: Memory,
    tick: TypedFunc<i64, ()>,
}

impl Guest {
    fn new(engine: &Engine, modules: &[Module], idx: usize) -> Guest {
        let mut store = Store::new(engine, HostState::default());
        // Host bookkeeping calls (migrate, state access) get their own budget; ticks reset it.
        store.set_fuel(FUEL_PER_TICK).unwrap();
        let instance = linker(engine).instantiate_and_start(&mut store, &modules[idx]).expect("instantiate");
        let memory = instance.get_memory(&store, "memory").expect("memory export");
        let tick = instance.get_typed_func::<i64, ()>(&store, "pe_tick").expect("pe_tick");
        let mut g = Guest { module_idx: idx, store, instance, memory, tick };
        let migrate = g.instance.get_typed_func::<(i32, i32), i32>(&g.store, "pe_migrate").unwrap();
        assert_eq!(migrate.call(&mut g.store, (0, 0)).unwrap(), 0);
        g
    }

    fn call_u32(&mut self, name: &str) -> u32 {
        self.store.set_fuel(FUEL_PER_TICK).unwrap();
        let f = self.instance.get_typed_func::<(), i32>(&self.store, name).unwrap();
        f.call(&mut self.store, ()).unwrap() as u32
    }

    fn state_bytes(&mut self) -> Vec<u8> {
        let ptr = self.call_u32("pe_state_ptr") as usize;
        let len = self.call_u32("pe_state_len") as usize;
        self.memory.data(&self.store)[ptr..ptr + len].to_vec()
    }

    /// Hot-swap: instantiate `idx` and hand it this guest's state via pe_migrate.
    fn swap_to(&mut self, engine: &Engine, modules: &[Module], idx: usize) -> Result<(), String> {
        let old_version = self.call_u32("pe_state_version");
        let old_state = self.state_bytes();
        let mut next = Guest::new(engine, modules, idx);
        let scratch = next.call_u32("pe_scratch_ptr") as usize;
        next.memory.data_mut(&mut next.store)[scratch..scratch + old_state.len()].copy_from_slice(&old_state);
        let migrate = next.instance.get_typed_func::<(i32, i32), i32>(&next.store, "pe_migrate").unwrap();
        next.store.set_fuel(FUEL_PER_TICK).unwrap();
        let rc = migrate.call(&mut next.store, (old_version as i32, old_state.len() as i32)).map_err(|e| e.to_string())?;
        if rc != 0 {
            return Err(format!("pe_migrate({old_version}) returned {rc}"));
        }
        *self = next;
        Ok(())
    }
}

struct GuestSnap {
    module_idx: usize,
    memory: Vec<u8>,
}

struct LogicPeer {
    name: &'static str,
    gs: GameState,
    snapper: Snapshotter,
    guest: Guest,
    /// (activation frame, module index) commits this peer knows about
    commits: Vec<(u64, usize)>,
    /// interactions this peer knows about
    inputs: Vec<Interaction>,
    history: BTreeMap<u64, (Snapshot, GuestSnap)>,
    hashes: BTreeMap<u64, u64>,
    fuel: BTreeMap<u64, u64>,
    rollbacks: u32,
    resim: u64,
    snap_bytes: usize,
}

impl LogicPeer {
    fn new(name: &'static str, engine: &Engine, modules: &[Module]) -> Self {
        LogicPeer {
            name,
            gs: GameState::new(build_pile_scene(8, 4), SEED),
            snapper: Snapshotter::new(),
            guest: Guest::new(engine, modules, 0),
            commits: Vec::new(),
            inputs: Vec::new(),
            history: BTreeMap::new(),
            hashes: BTreeMap::new(),
            fuel: BTreeMap::new(),
            rollbacks: 0,
            resim: 0,
            snap_bytes: 0,
        }
    }

    fn active_module(&self, frame: u64) -> usize {
        self.commits.iter().filter(|(f, _)| *f <= frame).max_by_key(|(f, _)| *f).map_or(0, |(_, m)| *m)
    }

    fn step(&mut self, engine: &Engine, modules: &[Module]) {
        let f = self.gs.frame;
        // 1. Rollback snapshot: physics + guest linear memory + which module is loaded.
        let snap = self.snapper.save(&self.gs);
        let mem = self.guest.memory.data(&self.guest.store).to_vec();
        self.snap_bytes = snap.world_image.len() + mem.len();
        self.history.insert(f, (snap, GuestSnap { module_idx: self.guest.module_idx, memory: mem }));
        while self.history.len() > WINDOW {
            let first = *self.history.keys().next().unwrap();
            self.history.remove(&first);
        }
        // 2. Code changes land at an exact sim frame.
        let want = self.active_module(f);
        if want != self.guest.module_idx {
            self.guest.swap_to(engine, modules, want).expect("hot-swap");
        }
        // 3. Run game logic with a fuel budget against a read-only view of the world.
        {
            let world = &self.gs.scene.world;
            let h = self.guest.store.data_mut();
            h.positions.clear();
            for &b in &self.gs.scene.bodies {
                let p = body_get_position(world, b);
                // The guest ABI reads f32 positions; Box3D's are f64 (D58), narrowed for the guest.
                h.positions.push([p.x as f32, p.y as f32, p.z as f32]);
            }
            h.impulses.clear();
        }
        self.guest.store.set_fuel(FUEL_PER_TICK).unwrap();
        self.guest.tick.call(&mut self.guest.store, f as i64).expect("tick");
        let used = FUEL_PER_TICK - self.guest.store.get_fuel().unwrap();
        self.fuel.insert(f, used);
        let mut now: Vec<Interaction> = self.inputs.iter().copied().filter(|i| i.frame == f).collect();
        for (k, (body, imp)) in self.guest.store.data().impulses.iter().enumerate() {
            // Logic-issued interactions run on every peer identically; peer 0 = "world logic".
            now.push(Interaction { frame: f, peer: 0, seq: (f * 64 + k as u64) as u32, body: *body, impulse: *imp });
        }
        // 4. Physics.
        self.gs.advance(&now);
        // 5. Hash: physics + claims + guest state + module identity.
        let state = self.guest.state_bytes();
        let gh = fnv_u64s(state.chunks(8).map(|c| {
            let mut b = [0u8; 8];
            b[..c.len()].copy_from_slice(c);
            u64::from_le_bytes(b)
        }));
        self.hashes.insert(f, fnv_u64s([self.gs.hash(), gh, self.guest.module_idx as u64]));
    }

    fn rollback_to(&mut self, frame: u64, engine: &Engine, modules: &[Module]) {
        let now = self.gs.frame;
        let (snap, gsnap) = self.history.remove(&frame).expect("frame in window");
        assert!(self.snapper.restore(&mut self.gs, &snap));
        // Fresh instance of the module that was active then, with its memory restored.
        let mut g = Guest::new(engine, modules, gsnap.module_idx);
        let cur = g.memory.data(&g.store).len();
        if gsnap.memory.len() > cur {
            let pages = ((gsnap.memory.len() - cur) / 65536) as u64;
            g.memory.grow(&mut g.store, pages).unwrap();
        }
        g.memory.data_mut(&mut g.store)[..gsnap.memory.len()].copy_from_slice(&gsnap.memory);
        self.guest = g;
        self.history.retain(|f, _| *f < frame);
        self.rollbacks += 1;
        while self.gs.frame < now {
            self.step(engine, modules);
            self.resim += 1;
        }
    }
}

/// The physics spike's reference run, natively: the code `physics_in_wasm`'s `pe_run_trace`
/// runs inside the interpreter (spikes/physics-in-wasm/src/lib.rs), so D4 compares the two in
/// one process whatever precision Box3D was built with (D58).
fn native_trace_digest(frames: u64) -> u64 {
    let inputs = scripted_inputs();
    let mut gs = GameState::new(build_pile_scene(8, 4), SEED);
    let mut state = Vec::with_capacity(frames as usize);
    let mut world = Vec::with_capacity(frames as usize);
    while gs.frame < frames {
        let f = gs.frame;
        let now: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == f).collect();
        gs.advance(&now);
        state.push(gs.hash());
        world.push(gs.world_hash());
    }
    fnv_u64s(state.iter().copied().chain(world.iter().copied()))
}

fn load(dir: &str, name: &str) -> Vec<u8> {
    let p = format!("{dir}/{name}.wasm");
    std::fs::read(&p).unwrap_or_else(|e| panic!("read {p}: {e} (run scripts/build_wasm_guests.sh first)"))
}

fn check(label: &str, pass: bool, detail: String) -> bool {
    println!("[{}] {label}: {detail}", if pass { "PASS" } else { "FAIL" });
    pass
}

fn main() {
    // [logic guests dir] [physics_in_wasm dir]: the logic guests build in their own workspace,
    // physics_in_wasm in the root one (scripts/build_wasm_guests.sh).
    let dirs: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    let dir = dirs.first().cloned().unwrap_or_else(|| "spikes/wasm-guests/target/wasm32-unknown-unknown/release".into());
    let physics_dir = dirs.get(1).cloned().unwrap_or_else(|| "target/wasm32-unknown-unknown/wasm-guest".into());
    let mut cfg = Config::default();
    cfg.consume_fuel(true);
    // REQUIRED for deterministic fuel: wasmi's default (LazyTranslation) charges fuel for
    // translating a function on first use, so fuel then depends on which instance in the process
    // called it first (observed: A 49831 vs B 46800 total fuel with the default).
    // `--lazy` reproduces that failure.
    if !std::env::args().any(|a| a == "--lazy") {
        cfg.compilation_mode(CompilationMode::Eager);
    }
    let engine = Engine::new(&cfg);

    let t = Instant::now();
    let v1 = Module::new(&engine, &load(&dir, "logic_v1")).expect("v1");
    let v1_load = t.elapsed();
    let t = Instant::now();
    let v2 = Module::new(&engine, &load(&dir, "logic_v2")).expect("v2");
    let v2_load = t.elapsed();
    let modules = vec![v1, v2];
    println!("module validate+compile: v1 {v1_load:?}, v2 {v2_load:?}");

    let inputs = scripted_inputs();
    let mut all = true;

    // A: knows everything on time. B: learns the commit and all interactions in
    // [SWAP_FRAME - 10, B_LEARNS_AT) only at B_LEARNS_AT. C: never gets the commit.
    let mut a = LogicPeer::new("A", &engine, &modules);
    let mut b = LogicPeer::new("B", &engine, &modules);
    let mut c = LogicPeer::new("C", &engine, &modules);
    a.commits.push((SWAP_FRAME, 1));
    a.inputs = inputs.clone();
    c.inputs = inputs.clone();
    b.inputs = inputs.iter().copied().filter(|i| i.frame < SWAP_FRAME - 10 || i.frame >= B_LEARNS_AT).collect();

    let t = Instant::now();
    let mut swap_cost = Duration::ZERO;
    let mut rollback_cost = Duration::ZERO;
    for f in 0..FRAMES {
        let ts = Instant::now();
        a.step(&engine, &modules);
        if f == SWAP_FRAME {
            swap_cost = ts.elapsed();
        }
        if f == B_LEARNS_AT {
            let tr = Instant::now();
            b.commits.push((SWAP_FRAME, 1));
            let mut late: Vec<Interaction> =
                inputs.iter().copied().filter(|i| i.frame >= SWAP_FRAME - 10 && i.frame < B_LEARNS_AT).collect();
            let from = late.iter().map(|i| i.frame).min().unwrap_or(SWAP_FRAME).min(SWAP_FRAME);
            b.inputs.append(&mut late);
            b.rollback_to(from, &engine, &modules);
            rollback_cost = tr.elapsed();
        }
        b.step(&engine, &modules);
        c.step(&engine, &modules);
    }
    let total = t.elapsed();

    let ha: Vec<u64> = a.hashes.values().copied().collect();
    let hb: Vec<u64> = b.hashes.values().copied().collect();
    let hc: Vec<u64> = c.hashes.values().copied().collect();
    let mm = ha.iter().zip(&hb).position(|(x, y)| x != y);
    all &= check(
        "D1 hot-swap v1->v2 at frame 300; late peer rolls back across the swap and converges",
        mm.is_none() && ha.len() == FRAMES as usize && hb.len() == FRAMES as usize,
        format!(
            "A vs B over {FRAMES} frames: first mismatch {mm:?}; B rollbacks {} resimulated {} frames in {rollback_cost:?}; swap step {swap_cost:?}; snapshot {} bytes/frame (physics + guest memory)",
            b.rollbacks, b.resim, a.snap_bytes
        ),
    );
    let c_first = ha.iter().zip(&hc).position(|(x, y)| x != y);
    all &= check(
        "D2 control peer without the commit diverges exactly at the swap frame",
        c_first == Some(SWAP_FRAME as usize),
        format!("A vs C first mismatch at frame {c_first:?}"),
    );
    let fa: u64 = a.fuel.values().sum();
    let fb: u64 = b.fuel.values().sum();
    all &= check(
        "D1b fuel consumed per frame identical on A and B (fuel is deterministic)",
        a.fuel == b.fuel,
        format!("total fuel A {fa}, B {fb}; max per tick {}", a.fuel.values().max().unwrap()),
    );
    let st_a = a.guest.state_bytes();
    println!(
        "       final guest state ({} bytes, v{}): {:02x?}; host-side rejected guest calls: {}",
        st_a.len(),
        a.guest.call_u32("pe_state_version"),
        &st_a,
        a.guest.store.data().bad_calls
    );
    println!("       3 peers x {FRAMES} frames incl. per-frame snapshots: {total:?}");

    // D3: runaway module is stopped by fuel, deterministically.
    {
        let runaway = Module::new(&engine, &load(&dir, "logic_runaway")).expect("runaway");
        let mut results = Vec::new();
        for _ in 0..2 {
            let mut store = Store::new(&engine, HostState::default());
            store.set_fuel(FUEL_PER_TICK).unwrap();
            let inst = linker(&engine).instantiate_and_start(&mut store, &runaway).unwrap();
            let tick = inst.get_typed_func::<i64, ()>(&store, "pe_tick").unwrap();
            store.set_fuel(FUEL_PER_TICK).unwrap();
            let t = Instant::now();
            let r = tick.call(&mut store, 7);
            let el = t.elapsed();
            let mem = inst.get_memory(&store, "memory").unwrap();
            let fuel_left = store.get_fuel().unwrap();
            store.set_fuel(FUEL_PER_TICK).unwrap();
            let ptr = inst.get_typed_func::<(), i32>(&store, "pe_state_ptr").unwrap().call(&mut store, ()).unwrap() as usize;
            let x = u64::from_le_bytes(mem.data(&store)[ptr..ptr + 8].try_into().unwrap());
            results.push((format!("{:?}", r.err().map(|e| e.to_string())), fuel_left, x, el));
        }
        let same = results[0].0 == results[1].0 && results[0].2 == results[1].2;
        all &= check(
            "D3 runaway edit is halted by fuel at the identical instruction both times",
            same && results[0].0.to_lowercase().contains("fuel"),
            format!(
                "trap {} after {:?}; fuel left {}; guest state at halt {:016x} (run 2: {:016x})",
                results[0].0, results[0].3, results[0].1, results[0].2, results[1].2
            ),
        );
    }

    // D4: physics compiled to wasm32 and interpreted gives the native digest.
    {
        let t = Instant::now();
        let m = Module::new(&engine, &load(&physics_dir, "physics_in_wasm")).expect("physics module");
        let compile = t.elapsed();
        let mut store = Store::new(&engine, HostState::default());
        store.set_fuel(u64::MAX).unwrap();
        let inst = Linker::<HostState>::new(&engine).instantiate_and_start(&mut store, &m).expect("instantiate physics");
        let run = inst.get_typed_func::<i32, i64>(&store, "pe_run_trace").unwrap();
        store.set_fuel(u64::MAX).unwrap();
        let t = Instant::now();
        let digest = run.call(&mut store, FRAMES as i32).expect("run") as u64;
        let el = t.elapsed();
        let native = native_trace_digest(FRAMES);
        all &= check(
            "D4 Box3D pile run compiled to wasm32 + wasmi == native digest",
            digest == native,
            format!(
                "digest {digest:016x} (native {native:016x}, engine world def, double precision {}); module compile {compile:?}; {FRAMES} frames in {el:?} ({:.0} us/frame interpreted)",
                box3d_rust::core::is_double_precision(),
                el.as_secs_f64() * 1e6 / FRAMES as f64
            ),
        );
    }

    println!("peers: {} {} {}", a.name, b.name, c.name);
    if !all {
        println!("RESULT: FAIL");
        std::process::exit(1);
    }
    println!("RESULT: PASS");
}

//! Systems over host-owned columns (logic.md 3.3, measurement 1). The World's vectors are allocated once, at
//! fixed length, as the columns' landing slots; before each `pe_run` the host copies in the columns the system
//! declares and afterwards copies back the changed rows of the ones it writes. Everything else in guest memory
//! is scratch (the spatial grid, the allocator).
use logic_game::*;
use pe_api::*;

#[link(wasm_import_module = "pe")]
unsafe extern "C" {
    #[link_name = "terrain_height"]
    fn pe_terrain_height(x: f32, z: f32) -> f32;
    #[link_name = "emit"]
    fn pe_emit(kind: u32, a: u32, b: f32);
    #[link_name = "rand_u32"]
    fn pe_rand_u32() -> u32;
}

struct WasmHost;

impl Host for WasmHost {
    fn terrain_height(&mut self, x: f32, z: f32) -> f32 {
        unsafe { pe_terrain_height(x, z) }
    }
    fn emit(&mut self, kind: u32, a: u32, b: f32) {
        unsafe { pe_emit(kind, a, b) }
    }
    fn rand_u32(&mut self) -> u32 {
        unsafe { pe_rand_u32() }
    }
}

static mut WORLD: Option<World> = None;

fn world() -> &'static mut World {
    unsafe { (*(&raw mut WORLD)).as_mut().expect("pe_init first") }
}

fn span<T>(v: &[T]) -> i64 {
    (v.as_ptr() as u32 as i64) | ((core::mem::size_of_val(v) as i64) << 32)
}

fn one<T>(v: &T) -> i64 {
    (v as *const T as u32 as i64) | ((core::mem::size_of::<T>() as i64) << 32)
}

#[unsafe(no_mangle)]
pub extern "C" fn pe_abi_version() -> u32 {
    101
}

#[unsafe(no_mangle)]
pub extern "C" fn pe_init(n: u32, seed: u64) {
    unsafe { *(&raw mut WORLD) = Some(World::new(n as usize, seed)) };
}

/// Column `id`'s slot in guest memory: pointer in the low 32 bits, byte length in the high 32.
/// 0..=13 are per-object columns, 14 the world's random state, 15 its counters.
#[unsafe(no_mangle)]
pub extern "C" fn pe_col(id: u32) -> i64 {
    let w = world();
    match id {
        0 => span(&w.alive),
        1 => span(&w.kind),
        2 => span(&w.team),
        3 => span(&w.pos),
        4 => span(&w.vel),
        5 => span(&w.hp),
        6 => span(&w.max_hp),
        7 => span(&w.armor),
        8 => span(&w.ai_state),
        9 => span(&w.ai_timer),
        10 => span(&w.target),
        11 => span(&w.cooldown),
        12 => span(&w.inv),
        13 => span(&w.effects),
        14 => one(&w.rng),
        15 => one(&w.stats),
        _ => 0,
    }
}

/// The hit list (a per-frame event buffer written by one system and read by another): set its length to
/// `len` entries and return its slot, packed like `pe_col`.
#[unsafe(no_mangle)]
pub extern "C" fn pe_hits_set(len: u32) -> i64 {
    let h = &mut world().hits;
    h.clear();
    h.resize(len as usize, Hit::default());
    span(h)
}

#[unsafe(no_mangle)]
pub extern "C" fn pe_hits() -> i64 {
    span(&world().hits)
}

/// One system over the columns copied in. `system` is one of logic_game's SYS_* bits.
#[unsafe(no_mangle)]
pub extern "C" fn pe_run(system: u32, frame: u64) -> i32 {
    tick(world(), frame, system, &mut WasmHost);
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn pe_hash() -> u64 {
    world().hash()
}

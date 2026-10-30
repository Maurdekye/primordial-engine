//! Game logic v2: a different rule set (sideways pushes every 20 frames, steered by a float
//! "spin" accumulator) that migrates v1's state on hot-swap.
#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

#[link(wasm_import_module = "pe")]
unsafe extern "C" {
    fn body_count() -> i32;
    fn body_pos(i: i32, axis: i32) -> f32;
    fn apply_impulse(i: i32, x: f32, y: f32, z: f32);
}

/// v1's layout, as v2 understands it (for migration).
#[repr(C)]
struct StateV1 {
    version: u32,
    lifts: u32,
    rng: u64,
    ticks: u64,
}

#[repr(C)]
pub struct State {
    pub version: u32,
    pub lifts: u32,
    pub rng: u64,
    pub ticks: u64,
    pub pushes: u32,
    pub spin: f32,
}

static mut STATE: State =
    State { version: 2, lifts: 0, rng: 0x0BAD_5EED_0BAD_5EED, ticks: 0, pushes: 0, spin: 0.25 };
static mut SCRATCH: [u8; 256] = [0; 256];

fn st() -> &'static mut State {
    unsafe { &mut *(&raw mut STATE) }
}

fn next(rng: &mut u64) -> u64 {
    let mut x = *rng;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *rng = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

#[unsafe(no_mangle)]
pub extern "C" fn pe_abi_version() -> u32 {
    1
}
#[unsafe(no_mangle)]
pub extern "C" fn pe_state_version() -> u32 {
    2
}
#[unsafe(no_mangle)]
pub extern "C" fn pe_state_ptr() -> *const u8 {
    &raw const STATE as *const u8
}
#[unsafe(no_mangle)]
pub extern "C" fn pe_state_len() -> u32 {
    core::mem::size_of::<State>() as u32
}
#[unsafe(no_mangle)]
pub extern "C" fn pe_scratch_ptr() -> *mut u8 {
    &raw mut SCRATCH as *mut u8
}

/// Migrate from the previous version's state bytes, which the host copied into SCRATCH.
/// Returns 0 on success, negative on an unsupported source version.
#[unsafe(no_mangle)]
pub extern "C" fn pe_migrate(from_version: u32, len: u32) -> i32 {
    match from_version {
        0 => 0,
        1 if len as usize >= core::mem::size_of::<StateV1>() => {
            let old = unsafe { &*(&raw const SCRATCH as *const StateV1) };
            let s = st();
            s.lifts = old.lifts;
            s.rng = old.rng;
            s.ticks = old.ticks;
            s.pushes = 0;
            s.spin = 0.25;
            0
        }
        2 => 0,
        _ => -1,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pe_tick(frame: u64) {
    let s = st();
    s.ticks += 1;
    if frame % 20 != 0 {
        return;
    }
    let n = unsafe { body_count() };
    if n <= 0 {
        return;
    }
    let i = (next(&mut s.rng) % n as u64) as i32;
    let x = unsafe { body_pos(i, 0) };
    let z = unsafe { body_pos(i, 2) };
    // Push toward the origin, plus a spin that grows each push.
    s.spin = s.spin * 1.07 + 0.01;
    let fx = -x * 0.3 + s.spin;
    let fz = -z * 0.3 - s.spin * 0.5;
    unsafe { apply_impulse(i, fx, 1.5, fz) };
    s.pushes += 1;
}

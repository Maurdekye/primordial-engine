//! Game logic v1: every 45 frames, lift a pseudo-randomly chosen object that is near the ground.
//!
//! Guest ABI (phase-1 sketch):
//!   imports  pe.body_count() -> i32, pe.body_pos(i, axis) -> f32, pe.apply_impulse(i, x, y, z)
//!   exports  pe_abi_version, pe_state_version, pe_state_ptr, pe_state_len, pe_scratch_ptr,
//!            pe_migrate(from_version, len) -> i32, pe_tick(frame)
//! All persistent logic state lives in one `#[repr(C)]` struct so the host can snapshot it for
//! rollback and hand it to the next version on hot-swap.
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

#[repr(C)]
pub struct State {
    pub version: u32,
    pub lifts: u32,
    pub rng: u64,
    pub ticks: u64,
}

static mut STATE: State = State { version: 1, lifts: 0, rng: 0x1234_5678_9ABC_DEF1, ticks: 0 };
static mut SCRATCH: [u8; 256] = [0; 256];

fn st() -> &'static mut State {
    unsafe { &mut *(&raw mut STATE) }
}

fn next(rng: &mut u64) -> u64 {
    // xorshift64*
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
    1
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

/// v1 is the first version: it only accepts a fresh start.
#[unsafe(no_mangle)]
pub extern "C" fn pe_migrate(from_version: u32, _len: u32) -> i32 {
    if from_version == 0 { 0 } else { -1 }
}

#[unsafe(no_mangle)]
pub extern "C" fn pe_tick(frame: u64) {
    let s = st();
    s.ticks += 1;
    if frame % 45 != 0 || frame == 0 {
        return;
    }
    let n = unsafe { body_count() };
    if n <= 0 {
        return;
    }
    let i = (next(&mut s.rng) % n as u64) as i32;
    let y = unsafe { body_pos(i, 1) };
    if y < 3.0 {
        unsafe { apply_impulse(i, 0.0, 4.0 + 0.5 * y, 0.0) };
        s.lifts += 1;
    }
}

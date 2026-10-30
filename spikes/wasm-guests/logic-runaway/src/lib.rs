//! A hostile/buggy edit: `pe_tick` never returns. The host must stop it deterministically
//! (fuel) and reject the commit with a machine-readable reason.
#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

static mut STATE: [u64; 2] = [0; 2];
static mut SCRATCH: [u8; 256] = [0; 256];

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
    16
}
#[unsafe(no_mangle)]
pub extern "C" fn pe_scratch_ptr() -> *mut u8 {
    &raw mut SCRATCH as *mut u8
}
#[unsafe(no_mangle)]
pub extern "C" fn pe_migrate(_from: u32, _len: u32) -> i32 {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn pe_tick(frame: u64) {
    let mut x = frame;
    loop {
        x = core::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
        unsafe { (*(&raw mut STATE))[0] = x };
    }
}

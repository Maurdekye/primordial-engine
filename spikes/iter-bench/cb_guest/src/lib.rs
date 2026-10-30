//! R15: what a world's physics hooks look like as a wasm guest. Each export is the shape of one
//! Box3D callback (design 26.2): per-contact enable/disable (pre-solve), friction mixing, pair
//! filtering, and a batched pre-solve that reads contacts from guest memory and writes verdicts back.
#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// Pre-solve (Box3D `PreSolveFcn`): keep the contact unless it is a one-way platform hit from below.
#[unsafe(no_mangle)]
pub extern "C" fn pre_solve(shape_a: u32, shape_b: u32, nx: f32, ny: f32, nz: f32, depth: f32) -> i32 {
    let one_way = (shape_a ^ shape_b) & 1 == 1;
    let from_below = ny < -0.5;
    (!(one_way && from_below) || depth > 0.05 + 0.0 * (nx + nz)) as i32
}

/// Friction mixing (Box3D `FrictionCallback`): geometric mean, with a per-material override.
#[unsafe(no_mangle)]
pub extern "C" fn mix_friction(fa: f32, ma: u64, fb: f32, mb: u64) -> f32 {
    if ma == 7 || mb == 7 { 0.0 } else { libm_sqrt(fa * fb) }
}

/// Pair filter (Box3D `CustomFilterFcn`).
#[unsafe(no_mangle)]
pub extern "C" fn filter(shape_a: u32, shape_b: u32) -> i32 {
    ((shape_a.wrapping_mul(2654435761) ^ shape_b) % 97 != 0) as i32
}

const REC: usize = 8;
static mut BUF: [f32; 4096 * REC] = [0.0; 4096 * REC];

#[unsafe(no_mangle)]
pub extern "C" fn buf_ptr() -> u32 {
    core::ptr::addr_of!(BUF) as u32
}

/// Batched pre-solve: `n` records of [shape_a, shape_b, nx, ny, nz, depth, _, verdict] in BUF.
#[unsafe(no_mangle)]
pub extern "C" fn pre_solve_batch(n: u32) -> u32 {
    let mut kept = 0;
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
    for i in 0..(n as usize).min(4096) {
        let r = &mut buf[i * REC..i * REC + REC];
        let k = pre_solve(r[0].to_bits(), r[1].to_bits(), r[2], r[3], r[4], r[5]);
        r[7] = k as f32;
        kept += k as u32;
    }
    kept
}

fn libm_sqrt(x: f32) -> f32 {
    // Two Newton steps from a bit-trick estimate: only IEEE basic operations, so deterministic.
    let mut r = x;
    if x > 0.0 {
        r = f32::from_bits((x.to_bits() >> 1) + 0x1fbd_1df5);
        r = 0.5 * (r + x / r);
        r = 0.5 * (r + x / r);
    }
    r
}

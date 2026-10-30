//! Coordinate precision at distance: representation step (ulp) and one hour of 60 Hz integration
//! (`x += v * dt`) in four representations. Pure IEEE arithmetic, so the numbers are exact and
//! identical on every machine.

use pe_spike_chunks::coords::{WorldPos, CHUNK};

fn ulp32(x: f32) -> f64 {
    (f32::from_bits(x.abs().to_bits() + 1) - x.abs()) as f64
}

fn ulp64(x: f64) -> f64 {
    f64::from_bits(x.abs().to_bits() + 1) - x.abs()
}

pub fn run() {
    println!("== precision: representation step at distance D from the origin ==");
    println!(
        "{:>12} {:>14} {:>14} {:>16} {:>16}",
        "D (m)", "f32 abs", "f64 abs", "chunk+f32 local", "chunk+f64 local"
    );
    let ds = [
        ("1 km", 1.0e3),
        ("100 km", 1.0e5),
        ("10,000 km", 1.0e7),
        ("1e9 m", 1.0e9),
        ("100 AU", 1.496e13),
        ("1 ly", 9.461e15),
        ("100,000 ly", 9.461e20),
    ];
    for (label, d) in ds {
        // Inside a chunk the offset is < 1024 m, so the step is that of the largest local value.
        let local32 = ulp32(1023.9);
        let local64 = ulp64(1023.9);
        println!(
            "{:>12} {:>14.3e} {:>14.3e} {:>16.3e} {:>16.3e}",
            label,
            ulp32(d as f32),
            ulp64(d),
            local32,
            local64
        );
        println!("RESULT precision-ulp D={d:e} f32={:e} f64={:e} chunk_f32={:e} chunk_f64={:e}", ulp32(d as f32), ulp64(d), local32, local64);
    }
    let max_range = i64::MAX as f64 * CHUNK;
    println!("chunk index range: +-{max_range:.3e} m (+-{:.3e} ly)", max_range / 9.461e15);

    println!();
    println!("== precision: 1 h at 60 Hz, v = 1.3 m/s along x, start at D (true travel 4680 m) ==");
    println!(
        "{:>12} {:>14} {:>14} {:>16} {:>16}",
        "D (m)", "f32 abs err", "f64 abs err", "chunk+f32 err", "chunk+f64 err"
    );
    let steps: u64 = 216_000;
    let v = 1.3f64;
    let dt = 1.0f64 / 60.0;
    let step64 = v * dt;
    let step32 = step64 as f32;
    for (label, d) in ds {
        let truth = steps as f64 * step64; // distance travelled; errors are measured on it
        // f32 absolute
        let mut x32 = d as f32;
        let x32_0 = x32;
        for _ in 0..steps {
            x32 += step32;
        }
        let e32 = ((x32 as f64 - x32_0 as f64) - truth).abs();
        // f64 absolute
        let mut x64 = d;
        for _ in 0..steps {
            x64 += step64;
        }
        let e64 = ((x64 - d) - truth).abs();
        // chunk + f32 local
        let start = WorldPos::from_abs([d, 0.0, 0.0]);
        let mut chunk = start.chunk[0];
        let mut loc32 = start.local[0] as f32;
        let loc32_0 = loc32;
        for _ in 0..steps {
            loc32 += step32;
            if loc32 >= CHUNK as f32 {
                loc32 -= CHUNK as f32;
                chunk += 1;
            }
        }
        let travelled = (chunk - start.chunk[0]) as f64 * CHUNK + (loc32 as f64 - loc32_0 as f64);
        let ec32 = (travelled - truth).abs();
        // chunk + f64 local (the proposed representation)
        let mut p = start;
        for _ in 0..steps {
            p = p.offset([step64, 0.0, 0.0]);
        }
        let ec64 = (p.sub(&start)[0] - truth).abs();
        println!("{:>12} {:>14.3e} {:>14.3e} {:>16.3e} {:>16.3e}", label, e32, e64, ec32, ec64);
        println!("RESULT precision-drift D={d:e} f32_err_m={e32:e} f64_err_m={e64:e} chunk_f32_err_m={ec32:e} chunk_f64_err_m={ec64:e}");
    }
    println!("(f32 abs at D >= 2^19 m (524 km): the ulp is 6.25 cm, so a 2.17 cm step rounds to 0 and a 1.3 m/s object cannot move.)");
}

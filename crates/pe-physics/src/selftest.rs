//! The physics self-test (docs/spec/divergence.md 5, step 2; R-G1): a fixed scene, stepped a fixed number of frames
//! under the engine's fixed configuration, whose whole-state [`CheckHash`] is the digest.
//!
//! A machine takes the digest at start-up and again whenever a root split asks for a verdict. A different digest
//! means its physics computes differently now than it did at start-up, so its verdict for the split is `Failed`. The
//! scene is a fresh world, so it sees how this machine computes now (the CPU, the code and its static data), never a
//! fault in the session's own state. Nor can equal digests clear a fault that is the same on both runs. Nothing at two
//! members can; the split's pick decides those. The start-up digest is also what R-G1's hello will carry, so two
//! machines whose physics differ refuse to join (pe-app net.rs).
//!
//! **The floating-point mode is reset first** (@coordinator-opus mee7cf585bfb6, after @fable-reviewer
//! m14fda3aed4d2). The replica resets the simulation thread to the default mode before every frame
//! (`pe_core::fpenv::check_and_reset`), so every simulated frame computes in it, whatever a library set on that thread
//! between frames. The self-test does the same before it runs, on whichever thread calls it, so its digest is what the
//! session's frames compute, never what a thread's leftover mode would. A mode that was not the default is reported
//! beside the digest ([`SelfTestDigest::mode_reset`]), as the frames count theirs.
//!
//! The scene never depends on a world's settings or tick rate: every machine runs the same one, so digests compare
//! across machines too.

use crate::config::PhysicsConfig;
use crate::desc::{BodyDesc, BodyKind, Geometry, ShapeDesc};
use crate::error::PhysicsError;
use crate::image::CheckHash;
use crate::world::{PhysicsWorld, Push};
use pe_core::ids::ObjectId;

/// Frames the scene is stepped, at [`SELF_TEST_DT`].
pub const SELF_TEST_FRAMES: u32 = 60;
/// The scene's step: 60 Hz, whatever the world's own rate.
pub const SELF_TEST_DT: f32 = 1.0 / 60.0;

/// Side and layers of the pile: 32 bodies of three shapes, like shapes stacked, so every pair of the three touches,
/// like pairs included (box-box is the largest manifold; @vdw-physics's probe on abe1c3fd).
const SIDE: usize = 4;
const LAYERS: usize = 2;

/// The scene before its first step: a static ground, a small pile of mixed dynamic bodies, and a small sphere on open
/// ground that [`self_test_run`] drops fast enough for a time of impact, created in a fixed order.
pub(crate) fn self_test_scene() -> Result<PhysicsWorld, PhysicsError> {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine())?;
    let ground = BodyDesc::new(BodyKind::Static, [0.0, -1.0, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [10.0, 1.0, 10.0] }));
    pw.add_body(ObjectId::from_u128(1), &ground)?;
    for n in 0..SIDE * SIDE * LAYERS {
        let (layer, ix, iz) = (n / (SIDE * SIDE), (n / SIDE) % SIDE, n % SIDE);
        let jitter = (n as f32 * 0.618_034).fract() * 0.2 - 0.1;
        let position = [
            (ix as f32 - SIDE as f32 * 0.5) * 1.1 + jitter,
            0.5 + layer as f32 * 1.2,
            (iz as f32 - SIDE as f32 * 0.5) * 1.1 - jitter,
        ]
        .map(f64::from);
        let geometry = match (n % (SIDE * SIDE)) % 3 {
            0 => Geometry::Box { half_extents: [0.4, 0.4, 0.4] },
            1 => Geometry::Sphere { center: [0.0; 3], radius: 0.45 },
            _ => Geometry::Capsule { center1: [-0.3, 0.0, 0.0], center2: [0.3, 0.0, 0.0], radius: 0.25 },
        };
        let desc = BodyDesc::new(BodyKind::Dynamic, position).with_shape(ShapeDesc::new(geometry).with_density(1.0));
        pw.add_body(body(n), &desc)?;
    }
    let drop = BodyDesc::new(BodyKind::Dynamic, [6.0, 3.0, 6.0])
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.25 }).with_density(1.0));
    pw.add_body(body(DROP), &drop)?;
    Ok(pw)
}

/// The fast sphere's body: the one after the pile.
const DROP: usize = SIDE * SIDE * LAYERS;

/// Steps `pw` [`SELF_TEST_FRAMES`] frames, with two fixed impulses so the pile moves and collides and one that drops
/// the sphere at about 40 m/s, and returns its whole-state check hash.
pub(crate) fn self_test_run(pw: &mut PhysicsWorld) -> Result<CheckHash, PhysicsError> {
    for frame in 0..SELF_TEST_FRAMES {
        match frame {
            10 => pw.push(body(5), Push::Impulse([0.0, 6.0, 2.0]))?,
            20 => pw.push(body(DROP), Push::Impulse([0.0, -2.6, 0.0]))?,
            30 => pw.push(body(20), Push::Impulse([-3.0, 4.0, 0.0]))?,
            _ => {}
        }
        pw.step(SELF_TEST_DT)?;
    }
    pw.check_hash()
}

/// The self-test's result: its digest, taken in the default floating-point mode, and whether the calling thread's mode
/// was something else when it ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelfTestDigest {
    /// The scene's whole-state check hash after [`SELF_TEST_FRAMES`] frames.
    pub digest: CheckHash,
    /// The calling thread's floating-point mode was not the default (round to nearest, no flush-to-zero or
    /// denormals-are-zero, every exception masked), so the self-test reset it before running: a library on this thread
    /// changed it. Reported, never hidden, as the frames count `fp_resets`.
    pub mode_reset: bool,
}

/// The self-test: [`self_test_run`] on a fresh [`self_test_scene`], in the default floating-point mode. The calling
/// thread's mode is reset to the default first, as the replica does before every frame, and stays so after.
pub fn self_test_digest() -> Result<SelfTestDigest, PhysicsError> {
    let mode_reset = !pe_core::fpenv::check_and_reset();
    let digest = self_test_run(&mut self_test_scene()?)?;
    Ok(SelfTestDigest { digest, mode_reset })
}

fn body(n: usize) -> ObjectId {
    ObjectId::from_u128(2 + n as u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two runs give one digest: the test itself is deterministic.
    #[test]
    fn the_digest_is_the_same_on_every_run() {
        assert_eq!(self_test_digest().unwrap(), self_test_digest().unwrap());
    }

    /// A world that computes one body differently gives another digest: what a `Failed` verdict reads.
    #[test]
    fn a_perturbed_scene_gives_another_digest() {
        let mut pw = self_test_scene().unwrap();
        pw.push(body(0), Push::Impulse([0.0, 0.001, 0.0])).unwrap();
        assert_ne!(self_test_run(&mut pw).unwrap(), self_test_digest().unwrap().digest);
    }

    /// The thread's floating-point control register: MXCSR on x86-64, FPCR on aarch64.
    #[cfg(target_arch = "x86_64")]
    fn control() -> u64 {
        let mut csr: u32 = 0;
        // SAFETY: stmxcsr writes 4 bytes to a valid, aligned u32.
        unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) &mut csr, options(nostack, preserves_flags)) };
        u64::from(csr)
    }

    #[cfg(target_arch = "x86_64")]
    fn set_control(v: u64) {
        let csr = v as u32;
        // SAFETY: ldmxcsr reads 4 bytes from a valid u32; every value written here keeps exceptions masked.
        unsafe { core::arch::asm!("ldmxcsr [{}]", in(reg) &csr, options(nostack, preserves_flags, readonly)) };
    }

    /// Round toward zero, and flush-to-zero with denormals-are-zero: MXCSR's RC = 11 and its FTZ and DAZ bits.
    #[cfg(target_arch = "x86_64")]
    const NOT_DEFAULT: u64 = 0x1F80 | 0x6000 | (1 << 15) | (1 << 6);

    #[cfg(target_arch = "aarch64")]
    fn control() -> u64 {
        let v: u64;
        // SAFETY: reading FPCR has no side effects.
        unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }

    #[cfg(target_arch = "aarch64")]
    fn set_control(v: u64) {
        // SAFETY: writing FPCR changes only this thread's floating-point modes; no trap bit is set here.
        unsafe { core::arch::asm!("msr fpcr, {}", in(reg) v, options(nomem, nostack, preserves_flags)) };
    }

    /// Round toward zero (RMode = 11) and flush-to-zero (FZ): FPCR's bits 22-23 and 24.
    #[cfg(target_arch = "aarch64")]
    const NOT_DEFAULT: u64 = (0b11 << 22) | (1 << 24);

    /// A thread left in another floating-point mode, by a library say, still gives the default mode's digest: the
    /// self-test resets the mode first, reports that it had to, and leaves the thread in the default mode.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[test]
    fn a_thread_in_another_floating_point_mode_gives_the_default_mode_s_digest_and_says_so() {
        let reference = self_test_digest().unwrap();
        assert!(!reference.mode_reset, "the test thread starts in the default mode");
        let default = control();
        set_control(NOT_DEFAULT);
        let got = self_test_digest();
        let after = control();
        set_control(default);
        assert_eq!(got.unwrap(), SelfTestDigest { digest: reference.digest, mode_reset: true });
        assert_eq!(after, default, "left in the default mode");
    }

    /// The mode does matter on this scene [V on x86-64, the probe on abe1c3fd]: run in it without the reset, the scene
    /// gives another digest, which a `Failed` verdict would read.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn without_the_reset_rounding_toward_zero_gives_another_digest() {
        let reference = self_test_digest().unwrap().digest;
        let default = control();
        let mut pw = self_test_scene().unwrap();
        set_control(NOT_DEFAULT);
        let got = self_test_run(&mut pw);
        set_control(default);
        assert_ne!(got.unwrap(), reference);
    }

    /// It is cheap enough to run on the simulation thread in M1 (divergence.md 5; app.md 1): reported, not asserted,
    /// since a test machine's load varies.
    #[test]
    fn its_cost_is_reported() {
        let t = std::time::Instant::now();
        self_test_digest().unwrap();
        println!("self-test: {SELF_TEST_FRAMES} frames in {:?}", t.elapsed());
    }
}

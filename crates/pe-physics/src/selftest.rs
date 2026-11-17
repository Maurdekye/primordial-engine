//! The physics self-test (docs/spec/divergence.md 5, step 2; R-G1): a fixed scene, stepped a fixed number of frames
//! under the engine's fixed configuration, whose whole-state [`CheckHash`] is the digest.
//!
//! A machine takes the digest at start-up and again whenever a root split asks for a verdict. A different digest
//! means its physics computes differently now than it did at start-up, so its verdict for the split is `Failed`. The
//! scene is a fresh world, so it sees how this machine computes now (the CPU, the thread's floating-point mode, the
//! code and its static data), never a fault in the session's own state. Nor can equal digests clear a fault that is
//! the same on both runs. Nothing at two members can; the split's pick decides those. The start-up digest is also
//! what R-G1's hello will carry, so two machines whose physics differ refuse to join (pe-app net.rs).
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

/// The self-test's digest: [`self_test_run`] on a fresh [`self_test_scene`].
pub fn self_test_digest() -> Result<CheckHash, PhysicsError> {
    self_test_run(&mut self_test_scene()?)
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
        assert_ne!(self_test_run(&mut pw).unwrap(), self_test_digest().unwrap());
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

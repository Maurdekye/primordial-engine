//! Review NEW-7 through `pe_physics::check_hash` (asked by @vdw-physics before pe-session drops its
//! scratch-restore step): after a destroy, a world that stepped straight through and a world
//! restored from a keyframe (taken before the destroy, or after it) hold the same physics. Their
//! check hashes must be equal, not only their per-frame state hashes.
//!
//! Measured at this commit: with pe-physics the raw image bytes are already equal on both paths
//! (its restore and save are exact), so the spike's hazard does not arise here; the test keeps the
//! check-hash equality as a regression guard and prints whether the raw bytes differ.

use pe_physics::{BodyDesc, BodyKind, Geometry, ObjectId, PhysicsConfig, PhysicsWorld, Push, ShapeDesc};

const DT: f32 = 1.0 / 60.0;

fn pile() -> PhysicsWorld {
    let mut pw = PhysicsWorld::new(PhysicsConfig::default()).unwrap();
    let ground = BodyDesc::new(BodyKind::Static, [0.0, -1.0, 0.0]).with_shape(ShapeDesc::new(Geometry::Box { half_extents: [20.0, 1.0, 20.0] }));
    pw.add_body(ObjectId::new(0, 1), &ground).unwrap();
    for n in 0..24u32 {
        let (x, y, z) = ((n % 4) as f64 * 1.1 - 2.0, 0.5 + (n / 16) as f64 * 1.2, ((n / 4) % 4) as f64 * 1.1 - 2.0);
        let g = match n % 3 {
            0 => Geometry::Box { half_extents: [0.4, 0.4, 0.4] },
            1 => Geometry::Sphere { center: [0.0; 3], radius: 0.45 },
            _ => Geometry::Capsule { center1: [-0.3, 0.0, 0.0], center2: [0.3, 0.0, 0.0], radius: 0.25 },
        };
        pw.add_body(ObjectId::new(0, 2 + n), &BodyDesc::new(BodyKind::Dynamic, [x, y, z]).with_shape(ShapeDesc::new(g).with_density(1.0))).unwrap();
    }
    pw
}

/// The same inputs every path sees: a push now and then, a body destroyed at frame 10 and one
/// created at frame 25 (it may reuse the destroyed body's slot).
fn frame(pw: &mut PhysicsWorld, f: u64) {
    if f % 7 == 0 {
        pw.push(ObjectId::new(0, 5), Push::Impulse([0.3, 1.5, 0.0])).unwrap();
    }
    if f == 10 {
        pw.remove_body(ObjectId::new(0, 9)).unwrap();
    }
    if f == 25 {
        let d = BodyDesc::new(BodyKind::Dynamic, [0.0, 4.0, 0.0]).with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.3 }));
        pw.add_body(ObjectId::new(0, 100), &d).unwrap();
    }
    pw.step(DT).unwrap();
}

#[test]
fn check_hash_is_the_same_after_a_destroy_whichever_path() {
    let mut straight = pile();
    let mut before = pile();
    let mut after = pile();
    let mut k_before = None;
    let mut k_after = None;
    for f in 0..40 {
        if f == 5 {
            k_before = Some(before.save_image().unwrap());
        }
        if f == 15 {
            k_after = Some(after.save_image().unwrap());
        }
        frame(&mut straight, f);
        frame(&mut before, f);
        frame(&mut after, f);
    }
    // Restore each from its keyframe and re-simulate to frame 40 with the same inputs.
    before.restore_image(&k_before.unwrap()).unwrap();
    for f in 5..40 {
        frame(&mut before, f);
    }
    after.restore_image(&k_after.unwrap()).unwrap();
    for f in 15..40 {
        frame(&mut after, f);
    }
    // The hazard NEW-7 names: the raw image bytes may differ although the physics is the same.
    let raw = |w: &mut PhysicsWorld| w.save_image().unwrap().world_bytes().to_vec();
    let s_raw = raw(&mut straight);
    let differ = [raw(&mut before) != s_raw, raw(&mut after) != s_raw];
    println!("RESULT NEW-7 raw image bytes differ from the straight world's: before-destroy keyframe {}, after-destroy keyframe {}", differ[0], differ[1]);
    for (name, w) in [("keyframe before the destroy", &before), ("keyframe after the destroy", &after)] {
        assert_eq!(w.state_hash().unwrap(), straight.state_hash().unwrap(), "{name}: state_hash");
        assert_eq!(w.check_hash().unwrap(), straight.check_hash().unwrap(), "{name}: check_hash");
    }
    println!("RESULT NEW-7 via pe_physics::check_hash: equal at frame 40 for both restore paths ({})", straight.check_hash().unwrap());
}

/// Repeated rollbacks across a destroy (as a replica re-simulating late entries does): keyframes
/// every 4 frames, and at frames 12, 14, 16 ... 30 a restore to the newest keyframe at least 3
/// frames back, then re-simulation. The check hash must equal the straight world's at frame 40.
#[test]
fn check_hash_after_repeated_rollbacks_across_a_destroy() {
    let mut straight = pile();
    for f in 0..40 {
        frame(&mut straight, f);
    }
    let mut w = pile();
    let mut ring: std::collections::BTreeMap<u64, pe_physics::WorldImage> = Default::default();
    let mut f = 0u64;
    let mut rollbacks = 0;
    while f < 40 {
        if f % 4 == 0 && !ring.contains_key(&f) {
            ring.insert(f, w.save_image().unwrap());
        }
        frame(&mut w, f);
        f += 1;
        if (12..=30).contains(&f) && f % 2 == 0 && rollbacks < 10 {
            let (&k, img) = ring.range(..=f - 3).next_back().unwrap();
            w.restore_image(img).unwrap();
            ring.retain(|&g, _| g <= k);
            f = k;
            rollbacks += 1;
        }
    }
    let (a, b) = (w.check_hash().unwrap(), straight.check_hash().unwrap());
    println!("RESULT repeated rollbacks ({rollbacks}) across a destroy: state_hash equal {}, check_hash equal {}", w.state_hash().unwrap() == straight.state_hash().unwrap(), a == b);
    assert_eq!(w.state_hash().unwrap(), straight.state_hash().unwrap());
    assert_eq!(a, b);
}

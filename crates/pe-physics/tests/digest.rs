//! Per-body digests (docs/spec/replica.md 5.1), and what restores a physics state exactly (6.2): the whole
//! canonical transfer does; rebuilding a world body by body from poses and velocities does not.

mod common;

use common::*;
use pe_physics::{ObjectId, PhysicsConfig, PhysicsWorld};

/// The pile, stepped with the scripted inputs up to `frames`.
fn pile_at(frames: u64) -> Game {
    let mut g = Game::pile(PhysicsConfig::engine());
    g.step_scripted(&scripted_inputs(), frames);
    g
}

#[test]
fn equal_worlds_have_equal_digests() {
    let a = pile_at(200);
    let b = pile_at(200);
    let da = a.pw.body_digests().unwrap();
    assert_eq!(da.len(), 257, "the ground and 256 pile bodies");
    assert_eq!(da, b.pw.body_digests().unwrap());
    // A world restored from a's transfer has a's digests too.
    let transfer = a.pw.save_transfer().unwrap();
    let c = PhysicsWorld::from_transfer(PhysicsConfig::engine(), &transfer, &a.pw.check_hash().unwrap()).unwrap();
    assert_eq!(da, c.body_digests().unwrap());
}

#[test]
fn one_changed_body_changes_its_own_digest_only() {
    let a = pile_at(200);
    let mut b = pile_at(200);
    // An awake body, so changing its velocity wakes no island.
    let k: ObjectId = *a.objects.iter().find(|&&id| a.pw.is_awake(id).unwrap()).expect("an awake body at frame 200");
    let (v, w) = b.pw.velocity(k).unwrap();
    b.pw.set_velocity(k, [f32::from_bits(v[0].to_bits() ^ 1), v[1], v[2]], w).unwrap();
    let (da, db) = (a.pw.body_digests().unwrap(), b.pw.body_digests().unwrap());
    let differing: Vec<ObjectId> = da.iter().zip(&db).filter(|(x, y)| x != y).map(|(x, _)| x.0).collect();
    assert_eq!(differing, vec![k], "one bit of one body's velocity shows in that body's digest alone");
    assert_ne!(a.pw.check_hash().unwrap(), b.pw.check_hash().unwrap(), "and in the full check hash");
}

/// The control behind replica.md 5.2 and 6.2, and @diverge-eng's test at the physics level: a world restored from
/// the whole transfer steps on exactly like the original for 600 frames; a fresh world given every body's exact
/// pose and velocities (all that patching rows can give) does not.
#[test]
fn transfer_import_is_exact_and_per_body_import_is_not() {
    let inputs = scripted_inputs();
    let mut a = pile_at(200);
    let transfer = a.pw.save_transfer().unwrap();
    let pw = PhysicsWorld::from_transfer(PhysicsConfig::engine(), &transfer, &a.pw.check_hash().unwrap()).unwrap();
    let mut b = Game { pw, objects: a.objects.clone(), frame: a.frame, claims: a.claims.clone(), seed: SEED };
    let (pw, objects) = pile_world(PhysicsConfig::engine(), 8, 4);
    let mut c = Game { pw, objects, frame: a.frame, claims: a.claims.clone(), seed: SEED };
    for &id in &a.objects {
        let (v, w) = a.pw.velocity(id).unwrap();
        c.pw.set_state(id, a.pw.pose(id).unwrap(), v, w).unwrap();
    }
    let mut per_body_differs_at = None;
    while a.frame < 800 {
        let f = a.frame;
        let now: Vec<Interaction> = inputs.iter().copied().filter(|i| i.frame == f).collect();
        a.advance(&now);
        b.advance(&now);
        c.advance(&now);
        let h = a.pw.box3d_hash().unwrap();
        assert_eq!(h, b.pw.box3d_hash().unwrap(), "transfer import, frame {f}");
        if f % 100 == 99 {
            assert_eq!(
                a.pw.check_hash().unwrap(),
                b.pw.check_hash().unwrap(),
                "transfer import, check hash, frame {f}"
            );
            assert_eq!(
                a.pw.body_digests().unwrap(),
                b.pw.body_digests().unwrap(),
                "transfer import, digests, frame {f}"
            );
        }
        if per_body_differs_at.is_none() && h != c.pw.box3d_hash().unwrap() {
            per_body_differs_at = Some(f);
        }
    }
    println!(
        "RESULT import: the transfer import stayed exact for 600 frames; the per-body import first differed at frame {per_body_differs_at:?}"
    );
    assert!(
        per_body_differs_at.is_some(),
        "the per-body import stayed equal for 600 frames: the control does not bite"
    );
}

//! The wrapper's own contract: refusals change nothing, images round-trip, joints and grabs
//! survive images, and poses reach the sim store.

use std::sync::Arc;

use pe_physics::{
    BodyDesc, BodyKind, Geometry, GeometryBlobs, JointDesc, ObjectId, PhysicsConfig, PhysicsError, PhysicsWorld,
    Push, ShapeDesc, WorldImage,
};
use pe_sim::{Registry, SimStore, Transform};

fn ball(y: f32) -> BodyDesc {
    BodyDesc::new(BodyKind::Dynamic, [0.0, y, 0.0]).with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.5 }))
}

fn ground() -> BodyDesc {
    BodyDesc::new(BodyKind::Static, [0.0, -1.0, 0.0]).with_shape(ShapeDesc::new(Geometry::Box { half_extents: [20.0, 1.0, 20.0] }))
}

fn id(n: u32) -> ObjectId {
    ObjectId::new(1, n)
}

#[test]
fn refusals_change_nothing() {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine());
    pw.add_body(id(1), &ball(2.0)).unwrap();
    let before = pw.state_hash();

    assert_eq!(pw.add_body(id(1), &ball(3.0)), Err(PhysicsError::AlreadyBound(id(1))));
    assert_eq!(pw.add_body(ObjectId::NONE, &ball(3.0)), Err(PhysicsError::NoneObject));
    let mut nan = ball(3.0);
    nan.position[1] = f32::NAN;
    assert!(matches!(pw.add_body(id(2), &nan), Err(PhysicsError::InvalidValue(_))));
    let mut bad_rot = ball(3.0);
    bad_rot.rotation = [0.0, 0.0, 0.0, 2.0];
    assert!(matches!(pw.add_body(id(2), &bad_rot), Err(PhysicsError::InvalidValue(_))));
    let bad_radius = BodyDesc::new(BodyKind::Dynamic, [0.0; 3])
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: -1.0 }));
    assert!(matches!(pw.add_body(id(2), &bad_radius), Err(PhysicsError::InvalidValue(_))));
    let flat = BodyDesc::new(BodyKind::Dynamic, [0.0; 3])
        .with_shape(ShapeDesc::new(Geometry::Hull { points: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 1.0, 0.0]] }));
    assert_eq!(pw.add_body(id(2), &flat), Err(PhysicsError::BadHull));
    assert!(matches!(pw.push(id(1), Push::Impulse([f32::INFINITY, 0.0, 0.0])), Err(PhysicsError::InvalidValue(_))));
    assert_eq!(pw.push(id(9), Push::Impulse([0.0; 3])), Err(PhysicsError::NotBound(id(9))));
    assert_eq!(pw.remove_body(id(9)), Err(PhysicsError::NotBound(id(9))));

    assert_eq!(pw.state_hash(), before, "a refused call changed the world");
    assert_eq!(pw.body_count(), 1);
}

#[test]
fn hull_bodies_and_pushes() {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine());
    pw.add_body(id(1), &ground()).unwrap();
    let tetra = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.3, 0.3, 0.3]];
    pw.add_body(id(2), &BodyDesc::new(BodyKind::Dynamic, [0.0, 2.0, 0.0]).with_shape(ShapeDesc::new(Geometry::Hull { points: tetra })))
        .unwrap();
    pw.push(id(2), Push::ImpulseAt { impulse: [1.0, 0.0, 0.0], point: [0.0, 2.5, 0.0] }).unwrap();
    pw.push(id(2), Push::Torque([0.0, 1.0, 0.0])).unwrap();
    pw.push(id(2), Push::Force([0.0, 20.0, 0.0])).unwrap();
    pw.step(1.0 / 60.0);
    let (v, w) = pw.velocity(id(2)).unwrap();
    assert!(v[0] > 0.0 && w.iter().any(|c| *c != 0.0));
    assert_eq!(pw.objects().collect::<Vec<_>>(), vec![id(1), id(2)]);
}

/// Images restore in place and into a fresh world, through their byte form, with joints and a
/// grab; corrupt bytes are refused.
#[test]
fn images_round_trip_with_joints() {
    let run = |pw: &mut PhysicsWorld, frames: usize| -> Vec<u64> {
        (0..frames)
            .map(|_| {
                pw.step(1.0 / 60.0);
                pw.state_hash()
            })
            .collect()
    };
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine().with_workers(2));
    pw.add_body(id(1), &ground()).unwrap();
    for n in 2..6 {
        pw.add_body(id(n), &ball(n as f32)).unwrap();
    }
    pw.add_joint(&JointDesc::Spherical { a: id(2), b: Some(id(3)), anchor_a: [0.0, 0.5, 0.0], anchor_b: [0.0, -0.5, 0.0] }).unwrap();
    pw.add_joint(&JointDesc::Distance { a: id(4), b: None, anchor_a: [0.0; 3], anchor_b: [0.0, 6.0, 0.0], length: 2.0 }).unwrap();
    let grab = pw.add_joint(&JointDesc::Grab { a: None, b: id(5), max_force: 500.0, max_torque: 50.0 }).unwrap();
    pw.set_grab_velocity(grab, [1.0, 0.0, 0.0], [0.0; 3]).unwrap();
    run(&mut pw, 30);

    let image = pw.save_image();
    let bytes = image.to_bytes();
    let geometry = pw.geometry().to_bytes();
    let reference = run(&mut pw, 60);

    pw.restore_image(&WorldImage::from_bytes(&bytes).unwrap()).unwrap();
    assert_eq!(run(&mut pw, 60), reference, "in place");

    let geometry = GeometryBlobs::from_bytes(&geometry).unwrap();
    let mut fresh = PhysicsWorld::from_image(PhysicsConfig::engine(), &geometry, &WorldImage::from_bytes(&bytes).unwrap()).unwrap();
    assert_eq!(fresh.joint_count(), 3);
    assert_eq!(run(&mut fresh, 60), reference, "fresh world");

    // Removing a body removes its joints; the joint keys keep counting.
    fresh.remove_body(id(3)).unwrap();
    assert_eq!(fresh.joint_count(), 2);
    let k = fresh.add_joint(&JointDesc::Weld { a: id(2), b: Some(id(4)), anchor_a: [0.0; 3], anchor_b: [0.0; 3] }).unwrap();
    assert_eq!(k.0, 4);

    // Corrupt input.
    assert!(WorldImage::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    let mut extended = bytes.clone();
    extended.push(0);
    assert!(WorldImage::from_bytes(&extended).is_err());
    let mut wrong_magic = bytes.clone();
    wrong_magic[0] ^= 1;
    assert!(WorldImage::from_bytes(&wrong_magic).is_err());
    let mut broken_world = bytes.clone();
    broken_world[12 + 20] ^= 0xFF;
    if let Ok(img) = WorldImage::from_bytes(&broken_world) {
        let mut target = PhysicsWorld::new(PhysicsConfig::engine());
        let _ = target.restore_image(&img); // Box3D refuses or restores; it must not panic.
    }
}

/// Body poses reach root objects' Transforms, offset by the world's origin chunk and rebased;
/// objects under a transformed parent are left to their parent.
#[test]
fn poses_reach_the_sim_store() {
    let mut store = SimStore::new(Arc::new(Registry::new()), 7);
    let root = store.create(1).unwrap();
    let parent = store.create(1).unwrap();
    let child = store.create(1).unwrap();
    store.insert(root, Transform::IDENTITY.with_scale([2.0; 3])).unwrap();
    store.insert(parent, Transform::IDENTITY).unwrap();
    store.insert(child, Transform::local([0.0, 1.0, 0.0])).unwrap();
    store.set_parent(child, Some(parent), None).unwrap();

    let mut pw = PhysicsWorld::new(PhysicsConfig::engine());
    pw.add_body(root, &ball(3.0)).unwrap();
    pw.add_body(child, &ball(5.0)).unwrap();
    for _ in 0..10 {
        pw.step(1.0 / 60.0);
    }
    let origin = [2, 0, -1];
    assert_eq!(pw.write_root_transforms(&mut store, origin).unwrap(), 1);
    let t = *store.get::<Transform>(root).unwrap();
    let pose = pw.pose(root).unwrap();
    let wt = store.world_transform(root).unwrap();
    let expect = Transform { chunk: origin, pos: pose.position, rot: pose.rotation, scale: [2.0; 3], _pad: 0 }.rebased();
    assert_eq!(t, expect);
    assert_eq!(t.scale, [2.0; 3], "scale kept");
    assert!(wt.pos.local.iter().all(|c| c.is_finite()));
    assert_eq!(*store.get::<Transform>(child).unwrap(), Transform::local([0.0, 1.0, 0.0]), "child untouched");
    store.check_values().unwrap();
}

#[test]
fn worker_count_does_not_change_the_hash() {
    let build = |workers| {
        let mut pw = PhysicsWorld::new(PhysicsConfig::engine().with_workers(workers));
        pw.add_body(id(1), &ground()).unwrap();
        for n in 0..300u32 {
            let x = (n % 10) as f32 - 5.0;
            let z = ((n / 10) % 10) as f32 - 5.0;
            let mut desc = ball(1.0 + (n / 100) as f32 * 1.1).with_velocity([x * 0.01, 0.0, z * 0.01], [0.0; 3]);
            desc.position = [x * 1.1, desc.position[1], z * 1.1];
            pw.add_body(id(2 + n), &desc).unwrap();
        }
        pw
    };
    let mut hashes = Vec::new();
    for workers in [1, 2, 5] {
        let mut pw = build(workers);
        for f in 0..120 {
            if f == 40 {
                pw.set_workers(8 - workers);
            }
            pw.step(1.0 / 60.0);
        }
        hashes.push(pw.state_hash());
    }
    assert!(hashes.windows(2).all(|w| w[0] == w[1]), "{hashes:x?}");
}

//! The wrapper's own contract: refusals change nothing, images and transfers round-trip,
//! hostile transfers and images are refused, joints and grabs survive images, and poses reach
//! the sim store.

use std::sync::Arc;

use pe_physics::{
    BodyDesc, BodyKind, CheckHash, Geometry, GrabSpring, JointDesc, JointKey, JointKind, ObjectId, PhysicsConfig,
    PhysicsError,
    PhysicsWorld, Pose, Push, ShapeDesc, Transfer, WorldImage, WriteBack,
};
use pe_sim::{Registry, SimStore, Transform};

const DT: f32 = 1.0 / 60.0;

fn ball(y: f64) -> BodyDesc {
    BodyDesc::new(BodyKind::Dynamic, [0.0, y, 0.0]).with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.5 }))
}

fn ground() -> BodyDesc {
    BodyDesc::new(BodyKind::Static, [0.0, -1.0, 0.0]).with_shape(ShapeDesc::new(Geometry::Box { half_extents: [20.0, 1.0, 20.0] }))
}

fn id(n: u32) -> ObjectId {
    ObjectId::new(1, n)
}

fn world(config: PhysicsConfig) -> PhysicsWorld {
    PhysicsWorld::new(config).unwrap()
}

fn run(pw: &mut PhysicsWorld, frames: usize) -> Vec<u64> {
    (0..frames)
        .map(|_| {
            pw.step(DT).unwrap();
            pw.state_hash().unwrap()
        })
        .collect()
}

fn invalid<T>(r: Result<T, PhysicsError>) -> bool {
    matches!(r, Err(PhysicsError::InvalidValue(_)))
}

#[test]
fn refusals_change_nothing() {
    let mut pw = world(PhysicsConfig::engine());
    pw.add_body(id(1), &ball(2.0)).unwrap();
    let before = pw.state_hash().unwrap();

    assert_eq!(pw.add_body(id(1), &ball(3.0)), Err(PhysicsError::AlreadyBound(id(1))));
    assert_eq!(pw.add_body(ObjectId::NONE, &ball(3.0)), Err(PhysicsError::NoneObject));
    let mut nan = ball(3.0);
    nan.position[1] = f64::NAN;
    assert!(invalid(pw.add_body(id(2), &nan)));
    let mut far = ball(3.0);
    far.position[0] = 1.0e38;
    assert!(invalid(pw.add_body(id(2), &far)), "position beyond MAX_POSITION");
    let mut bad_rot = ball(3.0);
    bad_rot.rotation = [0.0, 0.0, 0.0, 2.0];
    assert!(invalid(pw.add_body(id(2), &bad_rot)));
    let mut fast = ball(3.0);
    fast.linear_velocity = [1.0e30, 0.0, 0.0];
    assert!(invalid(pw.add_body(id(2), &fast)));
    let sphere = |radius| BodyDesc::new(BodyKind::Dynamic, [0.0; 3]).with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius }));
    assert!(invalid(pw.add_body(id(2), &sphere(-1.0))));
    assert!(invalid(pw.add_body(id(2), &sphere(1.0e30))), "radius beyond MAX_SIZE");
    assert!(invalid(pw.add_body(id(2), &BodyDesc { shapes: vec![ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.5 }).with_density(1.0e30)], ..ball(3.0) })));
    let flat = BodyDesc::new(BodyKind::Dynamic, [0.0; 3])
        .with_shape(ShapeDesc::new(Geometry::Hull { points: vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 1.0, 0.0]] }));
    assert_eq!(pw.add_body(id(2), &flat), Err(PhysicsError::BadHull));
    assert!(invalid(pw.push(id(1), Push::Impulse([f32::INFINITY, 0.0, 0.0]))));
    assert!(invalid(pw.push(id(1), Push::Force([1.0e30, 0.0, 0.0]))));
    assert!(invalid(pw.push(id(1), Push::ImpulseAt { impulse: [1.0, 0.0, 0.0], point: [1.0e7, 0.0, 0.0] })));
    assert_eq!(pw.push(id(9), Push::Impulse([0.0; 3])), Err(PhysicsError::NotBound(id(9))));
    assert_eq!(pw.remove_body(id(9)), Err(PhysicsError::NotBound(id(9))));
    // Joints from a body to itself, and joints to unknown bodies, create nothing (not even the
    // world anchor).
    let same = JointDesc::Spherical { a: id(1), b: Some(id(1)), anchor_a: [0.0; 3], anchor_b: [0.0; 3] };
    assert_eq!(pw.add_joint(JointKey(1), &same), Err(PhysicsError::SameBody));
    let grab_self = JointDesc::Grab { a: Some(id(1)), b: id(1), anchor_a: [0.0; 3], anchor_b: [0.0; 3], max_force: 1.0, max_torque: 1.0, spring: None };
    assert_eq!(pw.add_joint(JointKey(1), &grab_self), Err(PhysicsError::SameBody));
    let to_unknown = JointDesc::Weld { a: id(9), b: None, anchor_a: [0.0; 3], anchor_b: [0.0; 3] };
    assert_eq!(pw.add_joint(JointKey(1), &to_unknown), Err(PhysicsError::NotBound(id(9))));
    let anchor_far = JointDesc::Weld { a: id(1), b: None, anchor_a: [1.0e30, 0.0, 0.0], anchor_b: [0.0; 3] };
    assert!(invalid(pw.add_joint(JointKey(1), &anchor_far)));
    assert_eq!(pw.raw().bodies.len(), 1, "no world anchor was created");
    for dt in [0.0, -DT, f32::NAN, 2.0] {
        assert!(invalid(pw.step(dt)), "step({dt})");
    }

    assert_eq!(pw.state_hash().unwrap(), before, "a refused call changed the world");
    assert_eq!(pw.body_count(), 1);

    let mut bad = PhysicsConfig::engine();
    bad.gravity = [0.0, f32::NAN, 0.0];
    assert!(invalid(PhysicsWorld::new(bad)));
    assert_eq!(world(PhysicsConfig::engine().with_workers(100)).config().workers, 32, "worker count clamped");
}

/// A rotation slightly off unit length is normalized before it reaches Box3D (which
/// debug-asserts unit length to about 2e-6).
#[test]
fn rotations_are_normalized() {
    let mut pw = world(PhysicsConfig::engine());
    let s = 0.5f32.sqrt() * 1.004; // 0.8% long
    pw.add_body(id(1), &ball(2.0).with_rotation([0.0, s, 0.0, s])).unwrap();
    let q = pw.pose(id(1)).unwrap().rotation;
    let n2: f32 = q.iter().map(|c| c * c).sum();
    assert!((n2 - 1.0).abs() < 1.0e-6, "{n2}");
    run(&mut pw, 5);

    // A rotation Box3D already takes as unit length keeps its bits (P2-3 arrivals), including
    // one whose length is not 1.0 even in f32, so that dividing by it would change the bits.
    let s = std::f32::consts::FRAC_1_SQRT_2;
    let q = (1..40)
        .map(|k| [0.0, s, 0.0, s].map(|c| c * (1.0 + k as f32 * 1.0e-7)))
        .find(|q| {
            let n2: f32 = q.iter().map(|c| c * c).sum();
            (n2 - 1.0).abs() < 20.0 * f32::EPSILON && n2.sqrt() != 1.0
        })
        .expect("a rotation Box3D takes as unit whose length is not 1.0");
    pw.add_body(id(2), &ball(5.0).with_rotation(q)).unwrap();
    assert_eq!(pw.pose(id(2)).unwrap().rotation.map(f32::to_bits), q.map(f32::to_bits), "bits kept");
    assert!(invalid(pw.add_body(id(3), &ball(8.0).with_rotation([0.0, 0.0, 0.0, 1.03]))), "beyond 1%");
}

#[test]
fn hull_bodies_and_pushes() {
    let mut pw = world(PhysicsConfig::engine());
    pw.add_body(id(1), &ground()).unwrap();
    let tetra = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [0.3, 0.3, 0.3]];
    pw.add_body(id(2), &BodyDesc::new(BodyKind::Dynamic, [0.0, 2.0, 0.0]).with_shape(ShapeDesc::new(Geometry::Hull { points: tetra })))
        .unwrap();
    pw.push(id(2), Push::ImpulseAt { impulse: [1.0, 0.0, 0.0], point: [0.0, 2.5, 0.0] }).unwrap();
    pw.push(id(2), Push::Torque([0.0, 1.0, 0.0])).unwrap();
    pw.push(id(2), Push::Force([0.0, 20.0, 0.0])).unwrap();
    pw.step(DT).unwrap();
    let (v, w) = pw.velocity(id(2)).unwrap();
    assert!(v[0] > 0.0 && w.iter().any(|c| *c != 0.0));
    assert_eq!(pw.objects().collect::<Vec<_>>(), vec![id(1), id(2)]);
}

/// A scene with every joint kind, stepped a little.
fn jointed() -> PhysicsWorld {
    let mut pw = world(PhysicsConfig::engine().with_workers(2));
    pw.add_body(id(1), &ground()).unwrap();
    for n in 2..6 {
        pw.add_body(id(n), &ball(n as f64)).unwrap();
    }
    pw.add_joint(JointKey(10), &JointDesc::Spherical { a: id(2), b: Some(id(3)), anchor_a: [0.0, 0.5, 0.0], anchor_b: [0.0, -0.5, 0.0] }).unwrap();
    pw.add_joint(JointKey(20), &JointDesc::Distance { a: id(4), b: None, anchor_a: [0.0; 3], anchor_b: [0.0, 6.0, 0.0], length: 2.0 }).unwrap();
    let grab = JointDesc::Grab { a: None, b: id(5), anchor_a: [0.0; 3], anchor_b: [0.0; 3], max_force: 500.0, max_torque: 50.0, spring: None };
    pw.add_joint(JointKey(30), &grab).unwrap();
    pw.set_grab_velocity(JointKey(30), [1.0, 0.0, 0.0], [0.0; 3]).unwrap();
    run(&mut pw, 30);
    pw
}

/// Images restore in place through their byte form; transfers restore into a fresh world;
/// joint keys are the caller's and belong to the state.
#[test]
fn images_and_transfers_round_trip_with_joints() {
    let mut pw = jointed();
    let image = pw.save_image().unwrap();
    let bytes = image.to_bytes();
    let transfer = pw.save_transfer().unwrap();
    let confirmed = pw.check_hash().unwrap();
    assert_eq!(transfer.check_hash(), confirmed);
    let reference = run(&mut pw, 60);

    // In place. A joint added after the save is gone after the restore: its key names nothing.
    pw.add_joint(JointKey(40), &JointDesc::Weld { a: id(2), b: Some(id(4)), anchor_a: [0.0; 3], anchor_b: [0.0; 3] }).unwrap();
    pw.restore_image(&WorldImage::from_bytes(&bytes).unwrap()).unwrap();
    assert_eq!(pw.joint_kind(JointKey(40)), None);
    assert_eq!(pw.remove_joint(JointKey(40)), Err(PhysicsError::UnknownJoint(40)));
    assert_eq!(run(&mut pw, 60), reference, "in place");

    // Into a fresh world, at another worker count.
    let mut fresh = PhysicsWorld::from_transfer(PhysicsConfig::engine().with_workers(3), &Transfer::from_bytes(&transfer.to_bytes()).unwrap(), &confirmed).unwrap();
    assert_eq!(fresh.joint_count(), 3);
    assert_eq!(fresh.joint_kind(JointKey(30)), Some(JointKind::Grab));
    assert_eq!(fresh.config().workers, 3);
    assert_eq!(run(&mut fresh, 60), reference, "fresh world");

    // Removing a body removes its joints; keys are the caller's, refused if in use.
    fresh.remove_body(id(3)).unwrap();
    assert_eq!(fresh.joint_count(), 2);
    let weld = JointDesc::Weld { a: id(2), b: Some(id(4)), anchor_a: [0.0; 3], anchor_b: [0.0; 3] };
    assert_eq!(fresh.add_joint(JointKey(20), &weld), Err(PhysicsError::JointExists(20)));
    fresh.add_joint(JointKey(10), &weld).unwrap();
    assert_eq!(fresh.joint_kind(JointKey(10)), Some(JointKind::Weld));
}

/// The check hash depends on the state only: not on the save history (the geometry registry)
/// or on the joint creation order of equal states.
#[test]
fn check_hash_is_canonical() {
    let mut a = jointed();
    let b = jointed();
    for _ in 0..5 {
        a.save_image().unwrap(); // a's registry and save history differ from b's
    }
    assert_eq!(a.check_hash().unwrap(), b.check_hash().unwrap());
    assert_eq!(a.save_transfer().unwrap().to_bytes(), b.save_transfer().unwrap().to_bytes());
    let mut c = jointed();
    c.push(id(2), Push::Impulse([0.0, 1.0e-3, 0.0])).unwrap();
    c.step(DT).unwrap();
    let mut d = jointed();
    d.step(DT).unwrap();
    assert_ne!(c.check_hash().unwrap(), d.check_hash().unwrap(), "a different state has a different hash");
}

/// Grabs: only grab keys take grab velocities, a sleeping object wakes, and a spring keeps a
/// held object at the hand.
#[test]
fn grabs() {
    let mut pw = jointed();
    assert_eq!(pw.set_grab_velocity(JointKey(10), [0.0; 3], [0.0; 3]), Err(PhysicsError::NotAGrab(10)));
    assert_eq!(pw.set_grab_velocity(JointKey(99), [0.0; 3], [0.0; 3]), Err(PhysicsError::UnknownJoint(99)));
    assert!(invalid(pw.set_grab_velocity(JointKey(30), [f32::NAN, 0.0, 0.0], [0.0; 3])));

    // Wake: a ball at rest on the ground falls asleep, then a grab velocity wakes it.
    let mut pw = world(PhysicsConfig::engine());
    pw.add_body(id(1), &ground()).unwrap();
    pw.add_body(id(2), &ball(0.5)).unwrap();
    let grab = JointDesc::Grab { a: None, b: id(2), anchor_a: [0.0, 0.5, 0.0], anchor_b: [0.0; 3], max_force: 100.0, max_torque: 10.0, spring: None };
    pw.add_joint(JointKey(1), &grab).unwrap();
    let mut frames = 0;
    while pw.is_awake(id(2)).unwrap() {
        pw.step(DT).unwrap();
        frames += 1;
        assert!(frames < 600, "never slept");
    }
    pw.set_grab_velocity(JointKey(1), [0.5, 0.0, 0.0], [0.0; 3]).unwrap();
    assert!(pw.is_awake(id(2)).unwrap(), "grab velocity wakes the held body");

    // Spring: a kinematic hand moves at 1 m/s; the held ball (0.52 kg, under gravity) stays
    // near it.
    let mut pw = world(PhysicsConfig::engine());
    let hand = BodyDesc::new(BodyKind::Kinematic, [0.0, 3.0, 0.0])
        .with_velocity([1.0, 0.0, 0.0], [0.0; 3])
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.1 }));
    pw.add_body(id(1), &hand).unwrap();
    let light = BodyDesc::new(BodyKind::Dynamic, [0.0, 2.0, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.5 }).with_density(1.0));
    pw.add_body(id(2), &light).unwrap();
    let spring = GrabSpring { hertz: 5.0, damping_ratio: 1.0, max_force: 1000.0, max_torque: 100.0 };
    let grab = JointDesc::Grab { a: Some(id(1)), b: id(2), anchor_a: [0.0, -1.0, 0.0], anchor_b: [0.0; 3], max_force: 100.0, max_torque: 10.0, spring: Some(spring) };
    pw.add_joint(JointKey(1), &grab).unwrap();
    run(&mut pw, 180);
    let (h, b) = (pw.pose(id(1)).unwrap().position, pw.pose(id(2)).unwrap().position);
    let gap = ((h[0] - b[0]).powi(2) + (h[1] - 1.0 - b[1]).powi(2) + (h[2] - b[2]).powi(2)).sqrt();
    println!("RESULT grab spring gap after 3 s: {gap:.4} m (hand at x = {:.3})", h[0]);
    assert!(h[0] > 2.9 && gap < 0.1, "hand {h:?}, ball {b:?}, gap {gap}");
}

/// Images from elsewhere: refused before anything is written when their settings differ, and
/// a world poisoned by a failed restore refuses everything until a good image is restored.
#[test]
fn foreign_and_corrupt_images() {
    let mut pw = jointed();
    let good = pw.save_image().unwrap();
    let before = pw.state_hash().unwrap();

    // An upstream (no contact recycling) world's image does not fit an engine world.
    let mut upstream = world(PhysicsConfig::upstream());
    upstream.add_body(id(1), &ground()).unwrap();
    let foreign = upstream.save_image().unwrap();
    assert!(matches!(pw.restore_image(&foreign), Err(PhysicsError::BadImage(_))));
    assert!(!pw.is_poisoned(), "settings are checked before anything is written");
    assert_eq!(pw.state_hash().unwrap(), before);

    // Framing.
    let bytes = good.to_bytes();
    assert!(WorldImage::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    let mut extended = bytes.clone();
    extended.push(0);
    assert!(WorldImage::from_bytes(&extended).is_err());
    let mut wrong_magic = bytes.clone();
    wrong_magic[0] ^= 1;
    assert!(WorldImage::from_bytes(&wrong_magic).is_err());

    // Corrupt world bytes past the settings until Box3D refuses them: the world is poisoned.
    let mut poisoned = false;
    for at in (200..bytes.len() - 100).step_by(4) {
        let mut broken = bytes.clone();
        broken[12 + at..12 + at + 4].copy_from_slice(&i32::MAX.to_le_bytes());
        let Ok(img) = WorldImage::from_bytes(&broken) else { continue };
        if pw.restore_image(&img).is_err() {
            poisoned = true;
            assert!(pw.is_poisoned());
            assert_eq!(pw.step(DT), Err(PhysicsError::Poisoned));
            assert_eq!(pw.push(id(2), Push::Impulse([0.0; 3])), Err(PhysicsError::Poisoned));
            assert_eq!(pw.pose(id(2)), Err(PhysicsError::Poisoned));
            // Nothing of a half-written world leaves it (review P22R-4).
            assert_eq!(pw.save_image(), Err(PhysicsError::Poisoned));
            assert_eq!(pw.save_transfer(), Err(PhysicsError::Poisoned));
            assert_eq!(pw.check_hash(), Err(PhysicsError::Poisoned));
            assert_eq!(pw.state_hash(), Err(PhysicsError::Poisoned));
            assert_eq!(pw.box3d_hash(), Err(PhysicsError::Poisoned));
            break;
        }
        pw.restore_image(&good).unwrap();
    }
    assert!(poisoned, "no corruption was refused; pick other offsets");
    pw.restore_image(&good).unwrap();
    assert!(!pw.is_poisoned());
    assert_eq!(pw.state_hash().unwrap(), before);
    assert_eq!(run(&mut pw, 10), run(&mut jointed(), 10), "a good restore recovers the world");
}

/// Hostile transfers: refused unless the check hash is the confirmed one, and table invariants
/// are checked when the bytes are read.
#[test]
fn hostile_transfers() {
    let pw = jointed();
    let transfer = pw.save_transfer().unwrap();
    let confirmed = transfer.check_hash();
    let bytes = transfer.to_bytes();

    // Any changed byte changes the hash, so the joiner refuses it before Box3D reads it.
    for at in [16, bytes.len() / 2, bytes.len() - 20] {
        let mut b = bytes.clone();
        b[at] ^= 0x40;
        if let Ok(t) = Transfer::from_bytes(&b) {
            assert!(matches!(PhysicsWorld::from_transfer(PhysicsConfig::engine(), &t, &confirmed), Err(PhysicsError::BadImage(_))));
        }
    }
    // Confirmed, but for another configuration: the settings are refused.
    assert!(matches!(PhysicsWorld::from_transfer(PhysicsConfig::upstream(), &transfer, &confirmed), Err(PhysicsError::BadImage(_))));

    // Tables: the image part ends with the bodies (16 bytes each: object id, Box3D id), the
    // joints (17 bytes each) and the anchor (1 + 8 bytes). Give the second object the first's
    // Box3D id.
    let joints = 4 + 3 * 17 + 9;
    let bodies_end = bytes.len() - joints;
    let body = |n: usize| bodies_end - 16 * (5 - n);
    let mut dup = bytes.clone();
    let first: Vec<u8> = dup[body(0) + 8..body(0) + 16].to_vec();
    dup[body(1) + 8..body(1) + 16].copy_from_slice(&first);
    assert!(matches!(Transfer::from_bytes(&dup), Err(PhysicsError::BadImage(_))), "duplicate Box3D body id");
    let mut swapped = bytes.clone();
    let (a, b) = (swapped[body(0)..body(0) + 16].to_vec(), swapped[body(1)..body(1) + 16].to_vec());
    swapped[body(0)..body(0) + 16].copy_from_slice(&b);
    swapped[body(1)..body(1) + 16].copy_from_slice(&a);
    assert!(matches!(Transfer::from_bytes(&swapped), Err(PhysicsError::BadImage(_))), "unsorted objects");
    let mut kind = bytes.clone();
    kind[bodies_end + 4 + 16] = 9;
    assert!(matches!(Transfer::from_bytes(&kind), Err(PhysicsError::BadImage(_))), "unknown joint kind");
    let mut count = bytes.clone();
    count[bodies_end..bodies_end + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(Transfer::from_bytes(&count), Err(PhysicsError::BadImage(_))), "joint count beyond the end");
}

/// Body poses reach the sim store: roots offset by the origin chunk and rebased; objects under a
/// frame placed in it, bound frames first; scale kept; a refused pose leaves only its own
/// transform unchanged.
#[test]
fn poses_reach_the_sim_store() {
    let mut store = SimStore::new(Arc::new(Registry::new()), 7);
    let root = store.create(1).unwrap();
    let frame = store.create(1).unwrap();
    let child = store.create(1).unwrap();
    let grandchild = store.create(1).unwrap();
    store.insert(root, Transform::IDENTITY.with_scale([2.0; 3])).unwrap();
    store.insert(frame, Transform::IDENTITY.with_scale([0.5; 3])).unwrap();
    store.insert(child, Transform::local([0.0, 1.0, 0.0])).unwrap();
    store.insert(grandchild, Transform::local([0.0, 1.0, 0.0]).with_scale([3.0; 3])).unwrap();
    store.set_parent(child, Some(frame), None).unwrap();
    store.set_parent(grandchild, Some(child), None).unwrap();

    let mut pw = world(PhysicsConfig::engine());
    for (n, o) in [root, frame, child, grandchild].into_iter().enumerate() {
        pw.add_body(o, &ball(3.0 + 2.0 * n as f64).with_velocity([0.3 * n as f32, 0.0, 0.1], [0.0, 1.0, 0.0])).unwrap();
    }
    run(&mut pw, 10);
    let origin = [2, 0, -1];
    assert_eq!(pw.write_root_transforms(&mut store, origin).unwrap(), WriteBack { written: 4, refused: vec![] });
    let t = *store.get::<Transform>(root).unwrap();
    let pose = pw.pose(root).unwrap();
    assert_eq!(t, Transform { chunk: origin, pos: pose.position, rot: pose.rotation, scale: [2.0; 3], _pad: 0 }.rebased());
    for o in [frame, child, grandchild] {
        let w = store.world_transform(o).unwrap();
        let p = pw.pose(o).unwrap();
        let want = pe_core::coords::WorldPos::new(origin, p.position);
        let d = w.pos.sub(&want);
        assert!(d.iter().all(|c| c.abs() < 1.0e-5), "{o:?} is {d:?} from its body");
        let dot: f32 = w.rot.iter().zip(&p.rotation).map(|(a, b)| a * b).sum();
        assert!(dot.abs() > 1.0 - 1.0e-5, "{o:?} rotation");
    }
    assert_eq!(store.get::<Transform>(grandchild).unwrap().scale, [3.0; 3], "local scale kept");
    store.check_values().unwrap();

    // A body whose rebased chunk is beyond the sim store's limit keeps its transform and is
    // reported; the other body is still written (review P22R-7).
    let mut store = SimStore::new(Arc::new(Registry::new()), 7);
    let a = store.create(1).unwrap();
    let b = store.create(1).unwrap();
    store.insert(a, Transform::IDENTITY).unwrap();
    store.insert(b, Transform::IDENTITY).unwrap();
    let mut pw = world(PhysicsConfig::engine());
    pw.add_body(a, &BodyDesc { position: [-3.0, 2.0, 0.0], ..ball(0.0) }).unwrap();
    pw.add_body(b, &BodyDesc { position: [1100.0, 2.0, 0.0], ..ball(0.0) }).unwrap();
    pw.step(DT).unwrap();
    let edge = [pe_sim::transform::ROOT_CHUNK_LIMIT, 0, 0];
    assert_eq!(pw.write_root_transforms(&mut store, edge).unwrap(), WriteBack { written: 1, refused: vec![b] });
    let pose = pw.pose(a).unwrap();
    let want = Transform { chunk: edge, pos: pose.position, rot: pose.rotation, scale: [1.0; 3], _pad: 0 }.rebased();
    assert_eq!(*store.get::<Transform>(a).unwrap(), want, "the representable pose is written");
    assert_eq!(*store.get::<Transform>(b).unwrap(), Transform::IDENTITY, "the refused one is unchanged");
    store.check_values().unwrap();
}

#[test]
fn worker_count_does_not_change_the_hash() {
    let build = |workers| {
        let mut pw = world(PhysicsConfig::engine().with_workers(workers));
        pw.add_body(id(1), &ground()).unwrap();
        for n in 0..300u32 {
            let x = (n % 10) as f32 - 5.0;
            let z = ((n / 10) % 10) as f32 - 5.0;
            let mut desc = ball(1.0 + (n / 100) as f64 * 1.1).with_velocity([x * 0.01, 0.0, z * 0.01], [0.0; 3]);
            desc.position = [(x * 1.1) as f64, desc.position[1], (z * 1.1) as f64];
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
            pw.step(DT).unwrap();
        }
        hashes.push((pw.state_hash().unwrap(), pw.check_hash().unwrap()));
    }
    assert!(hashes.windows(2).all(|w| w[0] == w[1]), "{hashes:x?}");
}

/// Where the image part of a transfer starts: after the header and the geometry entries.
fn geometry_end(bytes: &[u8]) -> (usize, usize) {
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    let mut at = 12;
    for _ in 0..u32_at(8) {
        at += 5 + u32_at(at + 1);
    }
    (u32_at(8), at)
}

/// Defence in depth behind the cryptographic check hash (review P22R-1, P22R-3): a transfer
/// confirmed for its own hash, as if the confirmation were forged, is still refused if a byte
/// of it goes unread or its tables disagree with Box3D, and nothing panics.
#[test]
fn forged_confirmations_still_refused() {
    let pw = jointed();
    let transfer = pw.save_transfer().unwrap();
    let bytes = transfer.to_bytes();
    let (blobs, image_at) = geometry_end(&bytes);
    assert!(blobs > 0, "the scene has hull geometry");
    let forged = |b: &[u8]| {
        let t = Transfer::from_bytes(b).expect("well framed");
        PhysicsWorld::from_transfer(PhysicsConfig::engine(), &t, &t.check_hash()).map(|_| ())
    };
    assert_eq!(forged(&bytes), Ok(()), "the honest transfer");

    // An unused geometry blob: a copy of the first, appended to the list.
    let first_len = u32::from_le_bytes(bytes[13..17].try_into().unwrap()) as usize;
    let mut extra = bytes.clone();
    extra[8..12].copy_from_slice(&(blobs as u32 + 1).to_le_bytes());
    let blob = bytes[12..17 + first_len].to_vec();
    extra.splice(image_at..image_at, blob);
    assert!(matches!(forged(&extra), Err(PhysicsError::BadImage(_))), "unused geometry");

    // A byte after Box3D's world bytes, inside the image's world section.
    let world_len_at = image_at + 8;
    let world_len = u32::from_le_bytes(bytes[world_len_at..world_len_at + 4].try_into().unwrap()) as usize;
    let mut tail = bytes.clone();
    tail[world_len_at..world_len_at + 4].copy_from_slice(&(world_len as u32 + 1).to_le_bytes());
    tail.insert(world_len_at + 4 + world_len, 0);
    assert!(matches!(forged(&tail), Err(PhysicsError::BadImage(_))), "unread world byte");

    // The grab (key 30, the third joint) recorded as a weld: set_grab_velocity on it would
    // panic in Box3D, so the tables are refused. And the spherical joint recorded as a grab.
    let kind_at = bytes.len() - 9 - 17 + 16;
    assert_eq!(bytes[kind_at], JointKind::Grab as u8);
    let mut weld = bytes.clone();
    weld[kind_at] = JointKind::Weld as u8;
    assert!(matches!(forged(&weld), Err(PhysicsError::BadImage(_))), "grab recorded as a weld");
    let mut spherical = bytes.clone();
    spherical[kind_at - 17 - 17] = JointKind::Grab as u8;
    assert!(matches!(forged(&spherical), Err(PhysicsError::BadImage(_))), "spherical recorded as a grab");

    // The same through a local image (feature raw): refused, and the world recovers.
    let mut pw = jointed();
    let good = pw.save_image().unwrap();
    let mut image = good.to_bytes();
    let at = image.len() - 9 - 17 + 16;
    image[at] = JointKind::Weld as u8;
    assert!(matches!(pw.restore_image(&WorldImage::from_bytes(&image).unwrap()), Err(PhysicsError::BadImage(_))));
    pw.restore_image(&good).unwrap();
    pw.set_grab_velocity(JointKey(30), [0.0, 1.0, 0.0], [0.0; 3]).unwrap();
}

/// A peer that rolled back in place and re-simulated has the check hash (and per-frame hash)
/// of one that ran straight through, though its geometry registry and save history differ
/// (review P22R-8). The rollback target is frame 10 of a shared history; after it, the
/// mispredicted branch destroys a shared body (undone by the rollback), and the true inputs
/// destroy another (re-simulated, review P22S-4).
#[test]
fn rolled_back_peer_has_the_straight_check_hash() {
    let tetra = |s: f32| Geometry::Hull { points: vec![[0.0; 3], [s, 0.0, 0.0], [0.0, s, 0.0], [0.0, 0.0, s]] };
    let inputs = |pw: &mut PhysicsWorld, f: usize| {
        if f.is_multiple_of(7) {
            pw.push(id(3), Push::Impulse([0.5, 2.0, 0.0])).unwrap();
        }
        if f == 12 {
            pw.add_body(id(8), &BodyDesc::new(BodyKind::Dynamic, [1.0, 4.0, 0.0]).with_shape(ShapeDesc::new(tetra(0.6)))).unwrap();
        }
        if f == 25 {
            // A shared body with a joint, destroyed after the rollback target.
            pw.remove_body(id(4)).unwrap();
        }
    };
    let mut straight = jointed();
    let mut rolled = jointed();
    let mut ring = Vec::new();
    for f in 0..10 {
        inputs(&mut straight, f);
        inputs(&mut rolled, f);
        straight.step(DT).unwrap();
        rolled.step(DT).unwrap();
        ring.push(rolled.save_image().unwrap());
    }
    let target = ring.last().unwrap().clone();
    // A mispredicted branch from frame 10: other pushes, a body (new hull geometry) created and
    // destroyed, the shared grabbed body destroyed, and a save every frame.
    for f in 10..30 {
        rolled.push(id(2), Push::Impulse([0.0, 3.0, 1.0])).unwrap();
        if f == 15 {
            rolled.add_body(id(9), &BodyDesc::new(BodyKind::Dynamic, [-1.0, 3.0, 0.0]).with_shape(ShapeDesc::new(tetra(0.9)))).unwrap();
            rolled.remove_body(id(5)).unwrap();
        }
        if f == 25 {
            rolled.remove_body(id(9)).unwrap();
        }
        rolled.step(DT).unwrap();
        ring.push(rolled.save_image().unwrap());
    }
    rolled.restore_image(&target).unwrap();
    assert!(rolled.has_body(id(5)) && rolled.joint_kind(JointKey(30)) == Some(JointKind::Grab), "the destroy is undone");
    for f in 10..70 {
        inputs(&mut straight, f);
        inputs(&mut rolled, f);
        straight.step(DT).unwrap();
        rolled.step(DT).unwrap();
        assert_eq!(rolled.state_hash().unwrap(), straight.state_hash().unwrap(), "frame {f}");
        if f % 10 == 9 {
            rolled.save_image().unwrap();
            assert_eq!(rolled.check_hash().unwrap(), straight.check_hash().unwrap(), "frame {f}");
        }
    }
    assert!(!rolled.has_body(id(4)) && rolled.joint_kind(JointKey(20)).is_none(), "the destroy is re-simulated");
    println!("RESULT rollback check_hash after 70 frames {}", straight.check_hash().unwrap());
}

/// Geometry blobs in a transfer must be the ones its shapes use, each once. Confirmed for its
/// own hash (as if forged), a transfer with one blob replaced by a copy of the other is
/// refused. Swapped blobs are a different, consistent state (each shape gets the other's
/// geometry): only the confirmation tells it apart, and it does.
#[test]
fn transfer_geometry_order_and_duplicates_refused() {
    let mut pw = world(PhysicsConfig::engine());
    pw.add_body(id(1), &ground()).unwrap();
    let tetra = Geometry::Hull { points: vec![[0.0; 3], [0.7, 0.0, 0.0], [0.0, 0.7, 0.0], [0.0, 0.0, 0.7]] };
    pw.add_body(id(2), &BodyDesc::new(BodyKind::Dynamic, [0.0, 2.0, 0.0]).with_shape(ShapeDesc::new(tetra))).unwrap();
    run(&mut pw, 5);
    let bytes = pw.save_transfer().unwrap().to_bytes();
    let (blobs, image_at) = geometry_end(&bytes);
    assert_eq!(blobs, 2, "the ground box and the tetrahedron");
    let len_at = |at: usize| u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
    let first = bytes[12..17 + len_at(12)].to_vec();
    let second = bytes[12 + first.len()..image_at].to_vec();
    assert_eq!(second.len(), 5 + len_at(12 + first.len()), "two entries fill the geometry list");
    let forged = |b: &[u8]| {
        let t = Transfer::from_bytes(b).expect("well framed");
        PhysicsWorld::from_transfer(PhysicsConfig::engine(), &t, &t.check_hash()).map(|_| ())
    };
    assert_eq!(forged(&bytes), Ok(()), "the honest transfer");
    let with_blobs = |a: &[u8], b: &[u8]| [&bytes[..12], a, b, &bytes[image_at..]].concat();
    let swapped = Transfer::from_bytes(&with_blobs(&second, &first)).unwrap();
    let honest = Transfer::from_bytes(&bytes).unwrap().check_hash();
    assert_ne!(swapped.check_hash(), honest);
    assert!(matches!(PhysicsWorld::from_transfer(PhysicsConfig::engine(), &swapped, &honest), Err(PhysicsError::BadImage(_))), "swapped blobs, honest confirmation");
    let mut other = PhysicsWorld::from_transfer(PhysicsConfig::engine(), &swapped, &swapped.check_hash()).expect("a consistent state");
    assert_ne!(other.check_hash().unwrap(), pw.check_hash().unwrap());
    assert_ne!(run(&mut other, 30), run(&mut pw, 30), "a different state: it moves differently");
    assert!(matches!(forged(&with_blobs(&first, &first)), Err(PhysicsError::BadImage(_))), "first blob twice");
    assert!(matches!(forged(&with_blobs(&second, &second)), Err(PhysicsError::BadImage(_))), "second blob twice");
}

/// Hull point and shape counts are bounded (review P22R-6).
#[test]
fn hull_and_shape_counts_bounded() {
    use pe_physics::limits::{MAX_HULL_POINTS, MAX_SHAPES_PER_BODY};
    let mut pw = world(PhysicsConfig::engine());
    // A box's corners, then points inside it (a hull this many points could span would have
    // more edges than Box3D allows; the cap is on the input).
    let points = |n: usize| {
        (0..n)
            .map(|i| match i {
                0..8 => [(i & 1) as f32, ((i >> 1) & 1) as f32, ((i >> 2) & 1) as f32],
                _ => [0.5 + 0.4 * (i as f32 * 0.37).sin(), 0.5 + 0.4 * (i as f32 * 0.71).cos(), (i % 97) as f32 / 97.0 * 0.8 + 0.1],
            })
            .collect::<Vec<_>>()
    };
    let hull = |n| BodyDesc::new(BodyKind::Dynamic, [0.0, 2.0, 0.0]).with_shape(ShapeDesc::new(Geometry::Hull { points: points(n) }));
    pw.add_body(id(1), &hull(MAX_HULL_POINTS)).unwrap();
    assert_eq!(pw.add_body(id(2), &hull(MAX_HULL_POINTS + 1)), Err(PhysicsError::BadHull));
    let sphere = || ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.1 });
    let mut many = BodyDesc::new(BodyKind::Dynamic, [3.0, 2.0, 0.0]);
    for _ in 0..MAX_SHAPES_PER_BODY {
        many = many.with_shape(sphere());
    }
    pw.add_body(id(3), &many).unwrap();
    assert!(invalid(pw.add_body(id(4), &many.clone().with_shape(sphere()))));
    assert_eq!(pw.body_count(), 2);
}

fn bits(p: Pose, v: ([f32; 3], [f32; 3])) -> Vec<u64> {
    let f32s = p.rotation.iter().chain(&v.0).chain(&v.1).map(|c| c.to_bits() as u64);
    p.position.iter().map(|c| c.to_bits()).chain(f32s).collect()
}

/// Carried state (P2-3, review SET-1/SET-2): the toucher's pose and velocities, applied with
/// `set_state` by every peer (the toucher included) at the same frame, give identical worlds
/// afterwards: off-centre bodies, a source asleep on the ground, one settling towards sleep
/// (sleep timer running), and one rolling in contact. Applied by the receiver only, an
/// off-centre body diverges on the next step, although its pose bits are equal.
#[test]
fn carried_state_lands_exactly() {
    let off = |y: f64| {
        BodyDesc::new(BodyKind::Dynamic, [0.0, y, 0.0])
            .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.37, 0.05, -0.11], radius: 0.3 }))
            .with_shape(ShapeDesc::new(Geometry::Sphere { center: [-0.2, 0.0, 0.0], radius: 0.2 }))
    };
    let cases = [
        ("off-centre, spinning", off(3.0).with_velocity([0.7, 0.0, -0.2], [0.3, 4.1, 1.7]), off(9.0), 37),
        ("asleep on the ground", ball(0.5), ball(9.0), 300),
        ("settling, sleep timer running", ball(0.5), ball(9.0), 20),
        ("rolling in contact", ball(0.6).with_velocity([1.5, 0.0, 0.0], [0.0, 0.0, -3.0]), ball(9.0), 20),
    ];
    for both_sides in [true, false] {
        for (label, src, dst, frames) in &cases {
            let mut a = world(PhysicsConfig::engine());
            let mut b = world(PhysicsConfig::engine());
            for pw in [&mut a, &mut b] {
                pw.add_body(id(1), &ground()).unwrap();
            }
            a.add_body(id(2), src).unwrap();
            b.add_body(id(2), dst).unwrap();
            run(&mut a, *frames);
            let (pose, vel) = (a.pose(id(2)).unwrap(), a.velocity(id(2)).unwrap());
            if both_sides {
                a.set_state(id(2), pose, vel.0, vel.1).unwrap();
            }
            b.set_state(id(2), pose, vel.0, vel.1).unwrap();
            assert_eq!(bits(b.pose(id(2)).unwrap(), b.velocity(id(2)).unwrap()), bits(pose, vel), "exact bits, {label}");
            assert!(b.is_awake(id(2)).unwrap(), "carried state is awake, {label}");
            let (ra, rb) = (run(&mut a, 120), run(&mut b, 120));
            let first = ra.iter().zip(&rb).position(|(x, y)| x != y);
            println!("RESULT carried state, {label}, both sides {both_sides}: first differing frame of 120 {first:?}");
            if both_sides {
                assert_eq!(first, None, "{label}");
                assert_eq!(a.is_awake(id(2)).unwrap(), b.is_awake(id(2)).unwrap(), "{label}");
            } else if label.starts_with("off-centre") {
                assert_eq!(first, Some(0), "receiver only: the derived state differs (SET-1)");
            }
        }
    }

    // Refusals change nothing; each check runs before anything is written.
    let mut b = world(PhysicsConfig::engine());
    b.add_body(id(1), &ground()).unwrap();
    b.add_body(id(2), &ball(2.0)).unwrap();
    run(&mut b, 3);
    let before = b.state_hash().unwrap();
    let pose = b.pose(id(2)).unwrap();
    let ok = ([0.1, 0.0, 0.0], [0.0; 3]);
    assert!(invalid(b.set_state(id(2), Pose { rotation: [0.0, 0.0, 0.0, 1.001], ..pose }, ok.0, ok.1)), "not unit: refused, not renormalized");
    assert!(invalid(b.set_state(id(2), Pose { rotation: [f32::NAN, 0.0, 0.0, 1.0], ..pose }, ok.0, ok.1)));
    assert!(invalid(b.set_state(id(2), Pose { position: [f64::NAN, 0.0, 0.0], ..pose }, ok.0, ok.1)));
    assert!(invalid(b.set_state(id(2), Pose { position: [2.0e6, 0.0, 0.0], ..pose }, ok.0, ok.1)));
    assert!(invalid(b.set_state(id(2), pose, [1.0e5, 0.0, 0.0], ok.1)));
    assert!(invalid(b.set_state(id(2), pose, ok.0, [f32::NAN, 0.0, 0.0])), "the pose is not written either");
    assert!(invalid(b.set_pose(id(2), Pose { rotation: [0.0, 0.0, 0.0, 1.001], ..pose })));
    assert!(invalid(b.set_velocity(id(2), [0.0; 3], [f32::NAN, 0.0, 0.0])), "the linear part is not written either");
    assert!(invalid(b.set_state(id(1), pose, ok.0, ok.1)), "static body");
    assert!(invalid(b.set_pose(id(1), pose)), "static body");
    assert!(invalid(b.set_velocity(id(1), [1.0, 0.0, 0.0], [0.0; 3])), "static body");
    assert_eq!(b.set_state(id(7), pose, ok.0, ok.1), Err(PhysicsError::NotBound(id(7))));
    assert_eq!(b.state_hash().unwrap(), before, "a refused call changed the world");
}

/// set_target (hands): a kinematic body reaches its target pose after dt.
#[test]
fn targets_drive_hands() {
    let mut pw = world(PhysicsConfig::engine());
    pw.add_body(id(1), &ground()).unwrap();
    let hand = BodyDesc::new(BodyKind::Kinematic, [0.0, 1.0, 0.0]).with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.1 }));
    pw.add_body(id(2), &hand).unwrap();
    // 30 degrees about y: Box3D turns a body by at most 45 degrees per step.
    let (s, c) = 15f32.to_radians().sin_cos();
    let target = Pose { position: [0.5, 1.2, -0.3], rotation: [0.0, s, 0.0, c] };
    pw.set_target(id(2), target, DT, true).unwrap();
    pw.step(DT).unwrap();
    let got = pw.pose(id(2)).unwrap();
    let d = got.position.iter().zip(&target.position).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
    let dot: f32 = got.rotation.iter().zip(&target.rotation).map(|(a, b)| a * b).sum();
    assert!(d < 1.0e-5 && dot.abs() > 1.0 - 1.0e-5, "hand at {got:?}, target {target:?}");

    let before = pw.state_hash().unwrap();
    for dt in [0.0, -1.0, 1.0e-4, 2.0, f32::NAN] {
        assert!(invalid(pw.set_target(id(2), target, dt, true)), "dt {dt}");
    }
    assert!(invalid(pw.set_target(id(2), Pose { rotation: [0.0, 0.0, 0.0, 0.5], ..target }, DT, true)));
    assert!(invalid(pw.set_target(id(2), Pose { position: [0.0, f64::INFINITY, 0.0], ..target }, DT, true)));
    assert!(invalid(pw.set_target(id(1), target, DT, true)), "static body");
    assert_eq!(pw.state_hash().unwrap(), before);
}

/// After a destroy, a world that never restored saves the same transfer as one restored from
/// it, so `from_transfer` accepts it and their check hashes agree (world-chunks' r14 case: a
/// sphere resting in a settled pile leaves through a portal; the freed slot kept its type).
#[test]
fn transfer_canonical_after_destroy() {
    let mut pw = world(PhysicsConfig::engine());
    pw.add_body(id(1), &ground()).unwrap();
    for n in 0..12u32 {
        let (x, z) = ((n % 4) as f64 * 1.1 - 1.6, (n / 4) as f64 * 1.1 - 1.1);
        pw.add_body(id(2 + n), &BodyDesc { position: [x, 0.5, z], ..ball(0.0) }).unwrap();
    }
    run(&mut pw, 120);
    pw.remove_body(id(7)).unwrap();
    for frames in [0, 1, 30] {
        run(&mut pw, frames);
        let transfer = pw.save_transfer().unwrap();
        let confirmed = transfer.check_hash();
        let joiner = PhysicsWorld::from_transfer(PhysicsConfig::engine(), &transfer, &confirmed);
        assert!(joiner.is_ok(), "{frames} frames after the destroy: {:?}", joiner.err());
        assert_eq!(joiner.unwrap().check_hash().unwrap(), confirmed);
    }
}

/// A body with a hull no other body has, so each one adds a registry entry.
fn unique_hull(n: u32, x: f64) -> BodyDesc {
    let s = 0.3 + 0.001 * n as f32;
    let points = vec![[0.0; 3], [s, 0.0, 0.0], [0.0, s, 0.0], [0.0, 0.0, s], [s * 0.3, s * 0.4, s * 0.5]];
    BodyDesc::new(BodyKind::Dynamic, [x, 3.0, 0.0]).with_shape(ShapeDesc::new(Geometry::Hull { points }))
}

/// Hull churn: frame `f`'s inputs. A unique hull body appears every 4 frames and the one from
/// 16 frames earlier goes; a ball gets a push every 9 frames.
fn churn(pw: &mut PhysicsWorld, f: u32) {
    if f.is_multiple_of(4) {
        pw.add_body(id(1000 + f), &unique_hull(f, (f % 7) as f64 - 3.0)).unwrap();
        if f >= 16 {
            pw.remove_body(id(1000 + f - 16)).unwrap();
        }
    }
    if f % 9 == 4 {
        pw.push(id(2), Push::Impulse([0.3, 1.0, 0.0])).unwrap();
    }
}

fn churn_world() -> PhysicsWorld {
    let mut pw = world(PhysicsConfig::engine());
    pw.add_body(id(1), &ground()).unwrap();
    pw.add_body(id(2), &ball(1.0)).unwrap();
    pw
}

/// Compaction is exact: every kept image restores to the same state after it as before, the
/// live world is unchanged, the registry shrinks to what the kept images and the live world
/// use, and later saves and restores work.
#[test]
fn compact_images_is_exact() {
    let mut pw = churn_world();
    let mut ring = Vec::new();
    for f in 0..120 {
        churn(&mut pw, f);
        pw.step(DT).unwrap();
        ring.push(pw.save_image().unwrap());
    }
    let mut kept: Vec<WorldImage> = ring.split_off(100);
    let hashes = |pw: &mut PhysicsWorld, images: &[WorldImage]| -> Vec<(u64, CheckHash)> {
        let live = pw.save_image().unwrap();
        let out = images.iter().map(|i| {
            pw.restore_image(i).unwrap();
            (pw.state_hash().unwrap(), pw.check_hash().unwrap())
        });
        let out = out.collect();
        pw.restore_image(&live).unwrap();
        out
    };
    let before = hashes(&mut pw, &kept);
    let live_before = (pw.state_hash().unwrap(), pw.check_hash().unwrap());
    let len_before = pw.registry_len();
    pw.compact_images(&mut kept.iter_mut().collect::<Vec<_>>()).unwrap();
    let len_after = pw.registry_len();
    println!("RESULT compact registry {len_before} -> {len_after} blobs (20 kept images)");
    assert!(len_after < len_before / 2, "{len_before} -> {len_after}");
    assert_eq!((pw.state_hash().unwrap(), pw.check_hash().unwrap()), live_before, "live world unchanged");
    assert_eq!(hashes(&mut pw, &kept), before, "each kept image restores as before");
    // The world goes on, saving into the new registry and rolling back with the new images.
    let reference = run(&mut pw, 20);
    pw.restore_image(&kept[19]).unwrap();
    run(&mut pw, 0);
    assert_eq!(pw.check_hash().unwrap(), live_before.1);
    assert_eq!(run(&mut pw, 20), reference);
}

/// A refused kept image (another configuration's) changes nothing: the images, the registry
/// and the live world stay as they were, and the world is not poisoned.
#[test]
fn compact_images_refusal_changes_nothing() {
    let mut pw = churn_world();
    for f in 0..40 {
        churn(&mut pw, f);
        pw.step(DT).unwrap();
    }
    let mut mine = pw.save_image().unwrap();
    let mut upstream = world(PhysicsConfig::upstream());
    upstream.add_body(id(1), &ground()).unwrap();
    let mut foreign = upstream.save_image().unwrap();
    let (a, b, len, live) = (mine.clone(), foreign.clone(), pw.registry_len(), pw.check_hash().unwrap());
    assert!(matches!(pw.compact_images(&mut [&mut mine, &mut foreign]), Err(PhysicsError::BadImage(_))));
    assert_eq!((mine, foreign, pw.registry_len()), (a, b, len));
    assert!(!pw.is_poisoned());
    assert_eq!(pw.check_hash().unwrap(), live);
}

/// Compaction is invisible to the simulation (P22-12 condition 1): two peers with the same
/// inputs but different save cadences, rollbacks at different frames (a late input reaches
/// them 3 and 7 frames late) and compaction at different frames, through hull churn, keep
/// `state_hash` equal every frame and `check_hash` equal every 10 frames.
#[test]
fn compaction_is_invisible_to_the_simulation() {
    struct Peer {
        pw: PhysicsWorld,
        /// (frame, the state at the start of that frame)
        ring: Vec<(u32, WorldImage)>,
        save_every: u32,
        compact_every: u32,
        late_by: u32,
        max_len: usize,
    }
    // A late input: a push at frames 11k + 5, applied only once the peer has heard of it.
    let late = |f: u32| f % 11 == 5;
    let step = |p: &mut Peer, f: u32, heard_up_to: u32| {
        if f.is_multiple_of(p.save_every) {
            let image = p.pw.save_image().unwrap();
            p.ring.push((f, image));
        }
        churn(&mut p.pw, f);
        if late(f) && f <= heard_up_to {
            p.pw.push(id(2), Push::Impulse([0.0, 2.0, 0.4])).unwrap();
        }
        p.pw.step(DT).unwrap();
    };
    let mut peers = [3u32, 7].map(|late_by| Peer {
        pw: churn_world(),
        ring: Vec::new(),
        save_every: if late_by == 3 { 1 } else { 3 },
        compact_every: if late_by == 3 { 20 } else { 33 },
        late_by,
        max_len: 0,
    });
    let (mut compactions, mut rollbacks) = ([0; 2], [0; 2]);
    for f in 0..300u32 {
        let mut hashes = Vec::new();
        for (k, p) in peers.iter_mut().enumerate() {
            // Hear of the late input of frame f - late_by: roll back to before it and re-simulate.
            if f >= p.late_by && late(f - p.late_by) {
                let r = f - p.late_by;
                let (from, image) = p.ring.iter().rev().find(|(g, _)| *g <= r).cloned().expect("a kept image at or before the input");
                p.pw.restore_image(&image).unwrap();
                rollbacks[k] += 1;
                p.ring.retain(|(g, _)| *g <= from);
                p.ring.pop();
                for g in from..f {
                    step(p, g, f);
                }
            }
            step(p, f, f);
            p.ring.retain(|(g, _)| *g + 30 > f);
            p.max_len = p.max_len.max(p.pw.registry_len());
            if f > 0 && f.is_multiple_of(p.compact_every) {
                let mut kept: Vec<&mut WorldImage> = p.ring.iter_mut().map(|(_, i)| i).collect();
                p.pw.compact_images(&mut kept).unwrap();
                compactions[k] += 1;
            }
            hashes.push(p.pw.state_hash().unwrap());
        }
        assert_eq!(hashes[0], hashes[1], "state_hash at frame {f}");
        if f % 10 == 9 {
            assert_eq!(peers[0].pw.check_hash().unwrap(), peers[1].pw.check_hash().unwrap(), "check_hash at frame {f}");
        }
    }
    // 75 unique hulls were created; uncompacted, the registry would hold every one of them.
    for p in &peers {
        println!("RESULT compaction peer late by {}: registry at most {} blobs (75 unique hulls made), now {}", p.late_by, p.max_len, p.pw.registry_len());
        assert!(p.max_len < 40, "the registry stays bounded: {}", p.max_len);
    }
    assert!(compactions.iter().all(|&n| n >= 9), "{compactions:?}");
    assert!(rollbacks.iter().all(|&n| n >= 25), "{rollbacks:?}");
}

/// Images not passed to `compact_images` are refused afterwards, with nothing written: their
/// geometry indices name other blobs in the new registry (review CMP-1). So is an image from
/// another world with the same settings, whose indices never meant anything here.
#[test]
fn stale_and_foreign_images_refused() {
    let mut pw = churn_world();
    let mut ring = Vec::new();
    for f in 0..120 {
        churn(&mut pw, f);
        pw.step(DT).unwrap();
        ring.push(pw.save_image().unwrap());
    }
    let mut kept = ring.split_off(100);
    pw.compact_images(&mut kept.iter_mut().collect::<Vec<_>>()).unwrap();
    let live = (pw.state_hash().unwrap(), pw.check_hash().unwrap());
    let refused = ring.iter().filter(|image| matches!(pw.restore_image(image), Err(PhysicsError::BadImage(_)))).count();
    println!("RESULT stale images after compaction: {refused} of {} refused", ring.len());
    assert_eq!(refused, ring.len());
    assert!(!pw.is_poisoned(), "refused before anything is written");
    assert_eq!((pw.state_hash().unwrap(), pw.check_hash().unwrap()), live, "the world is unchanged");
    // Stale images cannot be compacted in either: nothing changes.
    let len = pw.registry_len();
    assert!(matches!(pw.compact_images(&mut [&mut ring[99]]), Err(PhysicsError::BadImage(_))));
    assert_eq!((pw.registry_len(), pw.check_hash().unwrap()), (len, live.1));
    // Another world, same settings and history.
    let mut other = churn_world();
    for f in 0..120 {
        churn(&mut other, f);
        other.step(DT).unwrap();
    }
    let theirs = other.save_image().unwrap();
    assert!(matches!(pw.restore_image(&theirs), Err(PhysicsError::BadImage(_))));
    assert!(!pw.is_poisoned());
    // The kept images still restore.
    pw.restore_image(&kept[19]).unwrap();
    assert_eq!(pw.check_hash().unwrap(), live.1);
}

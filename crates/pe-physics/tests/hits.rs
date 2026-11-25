//! Hit events for impact sounds ([`PhysicsWorld::hits`]): every shape reports them on every machine, and reading them
//! changes nothing (@item:primordial-engine/collision-hit-events-from-pe-physics-through-pe-r).

use pe_physics::{BodyDesc, BodyKind, Geometry, Hit, ObjectId, PhysicsConfig, PhysicsWorld, ShapeDesc};

const DT: f32 = 1.0 / 60.0;

fn id(n: u32) -> ObjectId {
    ObjectId::from_u128((1u128 << 32) | n as u128)
}

/// A static floor (top at y = 0) and, above it, a ball and a two-shape body whose second shape is lowest.
fn scene() -> PhysicsWorld {
    let mut pw = PhysicsWorld::new(PhysicsConfig::engine()).unwrap();
    let floor = BodyDesc::new(BodyKind::Static, [0.0, -0.5, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [10.0, 0.5, 10.0] }));
    pw.add_body(id(1), &floor).unwrap();
    let ball = BodyDesc::new(BodyKind::Dynamic, [0.0, 2.0, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0; 3], radius: 0.1 }));
    pw.add_body(id(2), &ball).unwrap();
    let pair = BodyDesc::new(BodyKind::Dynamic, [3.0, 1.0, 0.0])
        .with_shape(ShapeDesc::new(Geometry::Sphere { center: [0.0, 0.3, 0.0], radius: 0.1 }))
        .with_shape(ShapeDesc::new(Geometry::Box { half_extents: [0.1, 0.1, 0.1] }));
    pw.add_body(id(3), &pair).unwrap();
    pw
}

/// Every step's hits for `frames` frames.
fn run(pw: &mut PhysicsWorld, frames: usize) -> Vec<Vec<Hit>> {
    (0..frames)
        .map(|_| {
            pw.step(DT).unwrap();
            pw.hits()
        })
        .collect()
}

fn involves(h: &Hit, object: ObjectId) -> Option<(u32, f32)> {
    if h.a == object {
        Some((h.shape_a, -h.normal[1]))
    } else if h.b == object {
        Some((h.shape_b, h.normal[1]))
    } else {
        None
    }
}

#[test]
fn a_falling_ball_reports_one_hard_landing_then_none_at_rest() {
    let mut pw = scene();
    let steps = run(&mut pw, 120);
    let landings: Vec<(usize, &Hit)> = steps
        .iter()
        .enumerate()
        .flat_map(|(f, hits)| hits.iter().map(move |h| (f, h)))
        .filter(|(_, h)| involves(h, id(2)).is_some())
        .collect();
    assert!(!landings.is_empty(), "no hit for the ball");
    let (frame, first) = landings[0];
    // Dropped from 1.9 m above the floor: about sqrt(2 g h) = 6.2 m/s at the floor.
    assert!((first.speed - 6.2).abs() < 0.6, "approach speed {}", first.speed);
    assert!(first.a == id(1) || first.b == id(1), "the floor is the other body: {first:?}");
    let (shape, up) = involves(first, id(2)).unwrap();
    assert_eq!(shape, 0);
    assert!(up > 0.9, "the floor pushes the ball up: {first:?}");
    // Box3D answers a speculative contact up to a step's travel (6.2 m/s / 60 Hz, 10 cm) before the touch.
    assert!(first.point[1].abs() < 0.12, "at the floor's top: {first:?}");
    // At rest the approach is below the threshold: no more hits for it.
    assert!(steps[frame + 30..].iter().all(|hits| hits.iter().all(|h| involves(h, id(2)).is_none())), "hits at rest");
}

#[test]
fn a_hit_names_the_shape_that_landed() {
    let mut pw = scene();
    let steps = run(&mut pw, 120);
    let first = steps.iter().flatten().find(|h| involves(h, id(3)).is_some()).expect("a hit for the pair");
    // The sphere (shape 0) sits 0.3 m above the box (shape 1), so the box lands first.
    assert_eq!(involves(first, id(3)).unwrap().0, 1, "{first:?}");
}

#[test]
fn reading_hits_changes_nothing_and_two_runs_hear_the_same() {
    let (mut read, mut quiet) = (scene(), scene());
    let mut heard = Vec::new();
    for _ in 0..120 {
        read.step(DT).unwrap();
        quiet.step(DT).unwrap();
        heard.push(read.hits());
        assert_eq!(read.check_hash().unwrap(), quiet.check_hash().unwrap());
    }
    assert!(heard.iter().any(|h| !h.is_empty()));
    let mut again = scene();
    assert_eq!(run(&mut again, 120), heard, "the same hits on a second run");
}

#[test]
fn a_restored_image_hears_nothing_until_it_steps_then_the_same_hits() {
    let mut pw = scene();
    run(&mut pw, 10);
    let image = pw.save_image().unwrap();
    let ahead = run(&mut pw, 80);
    pw.restore_image(&image).unwrap();
    assert!(pw.hits().is_empty(), "a restore keeps no hits from the replaced state");
    assert_eq!(run(&mut pw, 80), ahead, "a rollback hears the same hits");
}

//! Primordial Engine fork: world images from untrusted peers.
//!
//! A joining peer restores a world image another peer sent. Every count the reader takes from
//! the image is checked against the bytes left before it sizes an allocation or a loop, so a
//! crafted count (`i32::MAX`) cannot make `reserve` abort the process. The settings in an
//! image are checked too: an image cannot set what `World::new` would clamp or refuse.

use crate::body::create_body;
use crate::geometry::Sphere;
use crate::hull::make_box_hull;
use crate::joint::create_spherical_joint;
use crate::math_functions::{Vec3, VEC3_ZERO};
use crate::recording::{
    deserialize_into_shell, image_world_settings, serialize_world, world_settings, GeometryRegistry,
    RecBuffer,
};
use crate::recycle_scale::ContactRecycleScaling;
use crate::shape::{create_hull_shape, create_sphere_shape};
use crate::types::{default_body_def, default_shape_def, default_spherical_joint_def, default_world_def, BodyType, WorldDef};
use crate::world::World;

/// A small world with every image section populated: static and dynamic bodies, hulls and
/// spheres, contacts, islands, a joint and a sensor.
pub(crate) fn scene(def: &WorldDef) -> World {
    let mut world = World::new(def);
    let ground = create_body(&mut world, &default_body_def());
    create_hull_shape(&mut world, ground, &default_shape_def(), &make_box_hull(10.0, 1.0, 10.0).base);
    let mut sensor = default_shape_def();
    sensor.is_sensor = true;
    sensor.enable_sensor_events = true;
    create_sphere_shape(&mut world, ground, &sensor, &Sphere { center: Vec3 { x: 0.0, y: 2.0, z: 0.0 }, radius: 1.5 });
    let mut bodies = Vec::new();
    for i in 0..4 {
        let mut body_def = default_body_def();
        body_def.type_ = BodyType::Dynamic;
        body_def.position.x = (1.1 * i as f32) as _;
        body_def.position.y = (1.5 + 0.2 * i as f32) as _;
        let body = create_body(&mut world, &body_def);
        let mut shape = default_shape_def();
        shape.enable_sensor_events = true;
        if i % 2 == 0 {
            create_sphere_shape(&mut world, body, &shape, &Sphere { center: VEC3_ZERO, radius: 0.5 });
        } else {
            create_hull_shape(&mut world, body, &shape, &make_box_hull(0.4, 0.4, 0.4).base);
        }
        bodies.push(body);
    }
    let mut joint = default_spherical_joint_def();
    joint.base.body_id_a = bodies[0];
    joint.base.body_id_b = bodies[1];
    create_spherical_joint(&mut world, &joint);
    for _ in 0..30 {
        world.step(1.0 / 60.0, 4);
    }
    world
}

fn image(world: &World) -> (Vec<u8>, GeometryRegistry) {
    let mut registry = GeometryRegistry::new();
    let mut buf = RecBuffer::new();
    serialize_world(world, &mut buf, &mut registry);
    (buf.data, registry)
}

/// Overwrite every aligned word of a real image with hostile values: the reader must return
/// (true or false) without panicking and without a huge allocation, which would abort.
#[test]
fn hostile_counts_never_panic_or_abort() {
    let world = scene(&default_world_def());
    let (bytes, registry) = image(&world);
    let mut shell = World::new(&default_world_def());
    let mut accepted = 0;
    let mut checked = 0;
    for offset in (16..bytes.len().saturating_sub(4)).step_by(4) {
        for value in [i32::MAX, i32::MAX / 16, 0x0100_0000, -1] {
            let mut bad = bytes.clone();
            bad[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            let mut slots = registry.to_slots();
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                deserialize_into_shell(&bad, &mut shell, &mut slots)
            }));
            match ok {
                Ok(true) => accepted += 1,
                Ok(false) => {}
                Err(_) => panic!("restore panicked with {value:#x} at image byte {offset}"),
            }
            checked += 1;
        }
    }
    // Most words are floats or ids the reader cannot judge; the counts are what matter here.
    assert!(checked > 1000, "image too small to be a useful test: {checked}");
    let _ = accepted;
    // The original image still restores into the reused shell.
    let mut slots = registry.to_slots();
    assert!(deserialize_into_shell(&bytes, &mut shell, &mut slots));
}

/// The settings bytes read from an image equal those of the world it was saved from, with
/// contact recycling on and off, and do not include the step state.
#[test]
fn image_settings_match_world_settings() {
    for scaling in [ContactRecycleScaling::disabled(), ContactRecycleScaling::enabled()] {
        let mut def = default_world_def();
        def.contact_recycle_scaling = scaling;
        let world = scene(&def);
        let (bytes, _) = image(&world);
        assert_eq!(image_world_settings(&bytes), Some(world_settings(&world)), "recycling {}", scaling.enabled);
        assert_eq!(world_settings(&world), world_settings(&World::new(&def)), "step state left out");
        let mut other = def;
        other.gravity.y = -9.0;
        assert_ne!(world_settings(&world), world_settings(&World::new(&other)));
    }
    assert_eq!(image_world_settings(&[0u8; 40]), None);
}

/// Settings World::new would clamp or refuse are refused in an image: restitution iterations
/// beyond the limit (a CPU denial of service), non-finite floats, an unknown combine.
#[test]
fn image_settings_out_of_range_refused() {
    let mut def = default_world_def();
    def.contact_recycle_scaling = ContactRecycleScaling::enabled();
    let world = scene(&def);
    let (bytes, registry) = image(&world);
    let restore = |b: &[u8]| {
        let mut slots = registry.to_slots();
        let mut shell = World::new(&def);
        deserialize_into_shell(b, &mut shell, &mut slots)
    };
    assert!(restore(&bytes));
    // Offsets in the config record (after the 16-byte header): gravity x at 0, restitution
    // iterations at 20, the recycling combine byte after the capacity, flags and three floats.
    let patch = |at: usize, v: &[u8]| {
        let mut b = bytes.clone();
        b[16 + at..16 + at + v.len()].copy_from_slice(v);
        b
    };
    assert!(!restore(&patch(20, &i32::MAX.to_le_bytes())), "restitution iterations i32::MAX");
    assert!(!restore(&patch(20, &(-1i32).to_le_bytes())), "negative restitution iterations");
    assert!(!restore(&patch(0, &f32::NAN.to_le_bytes())), "NaN gravity");
    assert!(!restore(&patch(36, &f32::INFINITY.to_le_bytes())), "infinite contact hertz");
    let combine = 48 + 28 + 20 + 1 + 12;
    assert_eq!(bytes[16 + combine], 1, "HarmonicMean combine byte located");
    assert!(!restore(&patch(combine, &[7])), "unknown combine");
}

/// An image must be consumed exactly: a byte appended to a real image is refused (review
/// P22R-1: no unread bytes ride along under a confirmed hash).
#[test]
fn trailing_bytes_refused() {
    let world = scene(&default_world_def());
    let (mut bytes, registry) = image(&world);
    let restore = |b: &[u8]| {
        let mut slots = registry.to_slots();
        deserialize_into_shell(b, &mut World::new(&default_world_def()), &mut slots)
    };
    assert!(restore(&bytes));
    bytes.push(0);
    assert!(!restore(&bytes));
}

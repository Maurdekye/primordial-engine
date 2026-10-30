//! Hit events for contacts in the overflow graph color.
//!
//! C's `b3StoreImpulses_Overflow` runs the Mesh store kernel, which also flags hit events, so an
//! overflow contact reports hits like any colored contact. The port used to flag only the graph
//! colors and dropped these events.

use crate::body::create_body;
use crate::constraint_graph::OVERFLOW_INDEX;
use crate::core::NULL_INDEX;
use crate::geometry::Sphere;
use crate::hull::make_box_hull;
use crate::math_functions::{Pos, Vec3, VEC3_ZERO};
use crate::shape::{create_hull_shape, create_sphere_shape};
use crate::types::{default_body_def, default_shape_def, default_world_def, BodyType};
use crate::world::World;

fn pos(x: f32, y: f32, z: f32) -> Pos {
    Pos {
        x: x as _,
        y: y as _,
        z: z as _,
    }
}

/// A dynamic hub plate carries more resting boxes than there are dynamic colors, so the hub
/// sits in every dynamic color and a ball landing on it is forced into the overflow color.
#[test]
fn overflow_contact_reports_hit_event() {
    let mut world = World::new(&default_world_def());

    // Ground
    {
        let mut body_def = default_body_def();
        body_def.position = pos(0.0, -1.0, 0.0);
        let ground_id = create_body(&mut world, &body_def);
        let box_hull = make_box_hull(20.0, 1.0, 20.0);
        create_hull_shape(&mut world, ground_id, &default_shape_def(), &box_hull.base);
    }

    // Dynamic hub plate lying on the ground
    {
        let mut body_def = default_body_def();
        body_def.type_ = BodyType::Dynamic;
        body_def.position = pos(0.0, 0.25, 0.0);
        let hub_id = create_body(&mut world, &body_def);
        let box_hull = make_box_hull(4.0, 0.25, 4.0);
        create_hull_shape(&mut world, hub_id, &default_shape_def(), &box_hull.base);
    }

    // 36 small boxes resting on the hub, away from the landing spot at the center
    let small_box = make_box_hull(0.2, 0.2, 0.2);
    for i in 0..6 {
        for j in 0..6 {
            let x = -3.5 + 1.4 * i as f32;
            let z = -3.5 + 1.4 * j as f32;
            if x.abs() < 1.0 && z.abs() < 1.0 {
                continue;
            }
            let mut body_def = default_body_def();
            body_def.type_ = BodyType::Dynamic;
            body_def.position = pos(x, 0.7, z);
            let body_id = create_body(&mut world, &body_def);
            create_hull_shape(&mut world, body_id, &default_shape_def(), &small_box.base);
        }
    }

    // Let the boxes settle so their hub contacts claim every dynamic color
    for _ in 0..30 {
        world.step(1.0 / 60.0, 4);
    }

    // Ball with hit events dropped onto the center of the hub
    let mut body_def = default_body_def();
    body_def.type_ = BodyType::Dynamic;
    body_def.position = pos(0.0, 2.0, 0.0);
    body_def.linear_velocity = Vec3::new(0.0, -4.0, 0.0);
    let ball_id = create_body(&mut world, &body_def);
    let mut shape_def = default_shape_def();
    shape_def.enable_hit_events = true;
    let sphere = Sphere {
        center: VEC3_ZERO,
        radius: 0.25,
    };
    let ball_shape_id = create_sphere_shape(&mut world, ball_id, &shape_def, &sphere);

    let mut hit_found = false;
    let mut in_overflow = false;
    for _ in 0..60 {
        world.step(1.0 / 60.0, 4);

        // The ball contact must be solved in the overflow color for this test to mean anything
        let ball_body = &world.bodies[(ball_id.index1 - 1) as usize];
        let mut contact_key = ball_body.head_contact_key;
        while contact_key != NULL_INDEX {
            let contact = &world.contacts[(contact_key >> 1) as usize];
            if contact.color_index == OVERFLOW_INDEX {
                in_overflow = true;
            }
            contact_key = contact.edges[(contact_key & 1) as usize].next_key;
        }

        for hit in &world.contact_hit_events {
            if hit.shape_id_a == ball_shape_id || hit.shape_id_b == ball_shape_id {
                hit_found = true;
            }
        }

        if hit_found {
            break;
        }
    }

    assert!(in_overflow, "ball contact never entered the overflow color");
    assert!(hit_found, "overflow contact did not report a hit event");
}

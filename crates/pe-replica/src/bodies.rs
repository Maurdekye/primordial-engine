//! Bodies: the world model's physical objects as pe-physics bodies (replica.md 2.1, 3.2).
//!
//! Everything here is a function of the world model's rows and the physics state, applied in id order, so every
//! replica makes the same calls in the same order. A refused physics call is a fault, counted and reported; it never
//! panics and never stops the frame (3.1).

use pe_physics::{BodyDesc, BodyKind as PhysicsKind, Geometry, PhysicsError, PhysicsWorld, Pose, Push, ShapeDesc};
use pe_seams::{ObjectId, Pos};

use crate::world::{BodyKind, BodyRow, Change, PushFx, ShapeRow, WorldModel};

const IDENTITY: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// A body's description from its row, placed relative to the domain's physics origin, at rest.
pub fn desc(row: &BodyRow, origin: Pos) -> BodyDesc {
    let kind = match row.kind {
        BodyKind::Static => PhysicsKind::Static,
        BodyKind::Kinematic => PhysicsKind::Kinematic,
        BodyKind::Dynamic => PhysicsKind::Dynamic,
    };
    let mut d = BodyDesc::new(kind, (row.pos - origin).0).with_rotation(row.rotation);
    d.linear_damping = row.linear_damping;
    d.angular_damping = row.angular_damping;
    d.gravity_scale = row.gravity_scale;
    d.bullet = row.bullet;
    d.enable_sleep = row.may_sleep;
    for s in &row.shapes {
        d = d.with_shape(shape(s, row));
    }
    d
}

fn shape(s: &ShapeRow, row: &BodyRow) -> ShapeDesc {
    let geometry = match *s {
        ShapeRow::Sphere { center, radius } => Geometry::Sphere { center, radius },
        ShapeRow::Capsule { center1, center2, radius } => Geometry::Capsule { center1, center2, radius },
        ShapeRow::Box { center, rotation, half_extents } if center == [0.0; 3] && rotation == IDENTITY => {
            Geometry::Box { half_extents }
        }
        // pe-physics's box is centred on its body, unturned. A compound child's box, offset or turned, becomes the
        // hull of its eight corners: the same solid, and the same on every replica.
        ShapeRow::Box { center, rotation, half_extents } => {
            Geometry::Hull { points: corners(center, rotation, half_extents) }
        }
    };
    let mut shape = ShapeDesc::new(geometry)
        .with_density(row.density)
        .with_friction(row.friction)
        .with_restitution(row.restitution);
    shape.rolling_resistance = row.rolling_resistance;
    shape
}

fn corners(center: [f32; 3], rotation: [f32; 4], half: [f32; 3]) -> Vec<[f32; 3]> {
    (0..8u8)
        .map(|i| {
            let sign = |bit: u8, h: f32| if i & bit == 0 { -h } else { h };
            let r = rotate(rotation, [sign(1, half[0]), sign(2, half[1]), sign(4, half[2])]);
            [center[0] + r[0], center[1] + r[1], center[2] + r[2]]
        })
        .collect()
}

/// `v` turned by the unit quaternion `q = [x, y, z, w]`: v + 2w (u x v) + 2 u x (u x v), with u = (x, y, z).
fn rotate(q: [f32; 4], v: [f32; 3]) -> [f32; 3] {
    let u = [q[0], q[1], q[2]];
    let t = cross(u, v).map(|c| 2.0 * c);
    let ut = cross(u, t);
    [v[0] + q[3] * t[0] + ut[0], v[1] + q[3] * t[1] + ut[1], v[2] + q[3] * t[2] + ut[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

/// Bring the physics in line with what an entry or a logic stage changed (3.2), in the given order (id order).
/// Returns how many physics calls were refused.
pub fn reconcile<W: WorldModel + ?Sized>(
    world: &W,
    physics: &mut PhysicsWorld,
    origin: Pos,
    changed: &[(ObjectId, Change)],
) -> u32 {
    let mut faults = 0;
    for &(id, change) in changed {
        let done = match change {
            Change::Created => create(world, physics, origin, id),
            Change::Removed => remove(physics, id),
            Change::Rebuilt => rebuild(world, physics, origin, id),
            Change::Teleported => teleport(world, physics, origin, id),
            Change::Velocity { linear, angular } if physics.has_body(id) => physics.set_velocity(id, linear, angular),
            Change::Velocity { .. } | Change::Reparented => Ok(()),
        };
        faults += u32::from(done.is_err());
    }
    faults
}

/// Created, or became physical: a body at its world transform, at rest.
fn create<W: WorldModel + ?Sized>(
    world: &W,
    physics: &mut PhysicsWorld,
    origin: Pos,
    id: ObjectId,
) -> Result<(), PhysicsError> {
    remove(physics, id)?;
    match world.body(id) {
        Some(row) => physics.add_body(id, &desc(&row, origin)),
        None => Ok(()),
    }
}

fn remove(physics: &mut PhysicsWorld, id: ObjectId) -> Result<(), PhysicsError> {
    if physics.has_body(id) { physics.remove_body(id) } else { Ok(()) }
}

/// Its shapes, body fields or scale changed: rebuilt, keeping the pose and velocities a moving body has in the
/// physics. A static body takes its row's pose, which is how an ancestor's move reaches it.
fn rebuild<W: WorldModel + ?Sized>(
    world: &W,
    physics: &mut PhysicsWorld,
    origin: Pos,
    id: ObjectId,
) -> Result<(), PhysicsError> {
    let Some(row) = world.body(id) else {
        return remove(physics, id);
    };
    if !physics.has_body(id) {
        return create(world, physics, origin, id);
    }
    let (pose, (linear, angular)) = (physics.pose(id)?, physics.velocity(id)?);
    physics.remove_body(id)?;
    let mut d = desc(&row, origin);
    if row.kind != BodyKind::Static {
        d.position = pose.position;
        d.rotation = pose.rotation;
        d.linear_velocity = linear;
        d.angular_velocity = angular;
    }
    physics.add_body(id, &d)
}

/// An edit set its transform: exactly the new pose, velocities kept, awake, contact history reset (`set_state`). A
/// static body cannot be moved in place, so it is rebuilt at the new pose.
fn teleport<W: WorldModel + ?Sized>(
    world: &W,
    physics: &mut PhysicsWorld,
    origin: Pos,
    id: ObjectId,
) -> Result<(), PhysicsError> {
    let Some(row) = world.body(id) else {
        return remove(physics, id);
    };
    if !physics.has_body(id) || row.kind == BodyKind::Static {
        return rebuild(world, physics, origin, id);
    }
    let (linear, angular) = physics.velocity(id)?;
    physics.set_state(id, Pose { position: (row.pos - origin).0, rotation: row.rotation }, linear, angular)
}

/// One push through pe-physics's bounded `push`. A push on an object that has no body does nothing.
pub fn push(physics: &mut PhysicsWorld, origin: Pos, p: &PushFx) -> Result<(), PhysicsError> {
    if !physics.has_body(p.object) {
        return Ok(());
    }
    let push = match p.at {
        None => Push::Impulse(p.impulse),
        Some(at) => Push::ImpulseAt { impulse: p.impulse, point: (at - origin).0 },
    };
    physics.push(p.object, push)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turned_box_becomes_the_hull_of_its_corners() {
        // A quarter turn about y maps x to -z.
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let c = corners([1.0, 0.0, 0.0], [0.0, s, 0.0, s], [2.0, 1.0, 0.5]);
        assert_eq!(c.len(), 8);
        let near = |a: [f32; 3], b: [f32; 3]| a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-5);
        // The corner (+2, +1, +0.5) turns to (+0.5, +1, -2), then moves by the centre.
        assert!(c.iter().any(|&p| near(p, [1.5, 1.0, -2.0])), "{c:?}");
        assert!(c.iter().any(|&p| near(p, [0.5, -1.0, 2.0])), "{c:?}");
    }
}

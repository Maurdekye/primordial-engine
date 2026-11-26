//! Engine-side descriptions of bodies and shapes. Plain data, validated before anything
//! reaches Box3D (4.3 rule 6: no NaN or infinity enters the simulation).

use box3d_rust::math_functions::{Quat, Vec3, is_normalized_quat};

use crate::error::PhysicsError;
use crate::mesh::Mesh;

/// Bounds on what the engine passes to Box3D. Box3D itself only debug-asserts its inputs, and a
/// finite but huge value (a radius of 1e30, a position of 1e38) overflows to infinity or NaN
/// inside it. These bounds are the engine's choice, far outside any planned scene.
pub mod limits {
    /// Largest coordinate of a body position or a world point, in metres. Physics worlds are
    /// placed near the bodies they hold (design 5). Box3D positions are `f64` in our build (D58's
    /// `double-precision`, the fork's default); its bounding boxes stay `f32`.
    pub const MAX_POSITION: f64 = 1.0e6;
    /// Smallest and largest radius, half extent, shape offset or hull coordinate, in metres.
    pub const MIN_SIZE: f32 = 1.0e-4;
    pub const MAX_SIZE: f32 = 1.0e4;
    /// Largest density, kg per cubic metre (osmium is 22,600).
    pub const MAX_DENSITY: f32 = 1.0e5;
    /// Largest friction, restitution or rolling resistance.
    pub const MAX_MATERIAL: f32 = 100.0;
    /// Largest linear (m/s) or angular (rad/s) speed.
    pub const MAX_SPEED: f32 = 1.0e4;
    /// Largest damping coefficient or gravity scale magnitude.
    pub const MAX_DAMPING: f32 = 1.0e3;
    /// Largest impulse (N s), force (N), torque (N m), or joint force or torque cap.
    pub const MAX_PUSH: f32 = 1.0e9;
    /// Largest gravity component, m/s^2.
    pub const MAX_GRAVITY: f32 = 1.0e4;
    /// Most Box3D sub-steps per tick: the rule gives 8 at the slowest supported rate, 30 Hz.
    pub const MAX_SUB_STEPS: i32 = 16;
    /// Most distance queries a sub-step may spend sweeping bullets ([`crate::PhysicsConfig::swept_budget`]).
    pub const MAX_SWEPT_BUDGET: i32 = 65_536;
    /// Most points a hull is built from (Box3D keeps at most 64 hull vertices), and most
    /// shapes on one body: building costs every peer CPU time (review P22R-6).
    pub const MAX_HULL_POINTS: usize = 1024;
    pub const MAX_SHAPES_PER_BODY: usize = 64;
    /// The largest simulation blob, a mesh's or a hull's, in bytes: 960 KiB, pe-world's carried-blob cap
    /// (world-model.md 3.4, `MAX_CARRIED_BLOB`). A mesh given as vertices and triangles is held to what such a blob
    /// holds.
    pub const MAX_SIM_BLOB_BYTES: usize = 960 * 1024;
    /// Most points a hull v1 blob holds (world-model.md 3.2).
    pub const MAX_HULL_BLOB_POINTS: usize = 64;
    /// Smallest magnitude of a mesh scale axis: Box3D's floor (`MIN_SCALE`), which it would raise a smaller axis to,
    /// so the mesh would collide and weigh as a solid 1% thick where it is drawn flat. Refused instead
    /// (@coordinator-opus, after @fable-reviewer; world-model.md 3.2, "the scale floor").
    pub const MIN_MESH_SCALE: f32 = box3d_rust::math_functions::MIN_SCALE;
}

use limits::*;

pub(crate) fn bounded(v: &[f32], max: f32, what: &'static str) -> Result<(), PhysicsError> {
    if v.iter().all(|c| c.is_finite() && c.abs() <= max) { Ok(()) } else { Err(PhysicsError::InvalidValue(what)) }
}

pub(crate) fn position(v: &[f64; 3], what: &'static str) -> Result<(), PhysicsError> {
    if v.iter().all(|c| c.is_finite() && c.abs() <= MAX_POSITION) {
        Ok(())
    } else {
        Err(PhysicsError::InvalidValue(what))
    }
}

/// `q` as Box3D will take it. Its bits are kept when Box3D's own check (`is_normalized_quat`,
/// |q|^2 within 20 epsilon of 1) passes, so a rotation read from a simulated body lands bit for
/// bit (P2-3 arrivals, like `set_pose`). Otherwise it is scaled to unit length if within 1% of
/// it (`sqrt` and division are correctly rounded, so every peer gets the same bits), and
/// refused beyond that.
pub(crate) fn unit_quat(q: [f32; 4], what: &'static str) -> Result<[f32; 4], PhysicsError> {
    if !q.iter().all(|c| c.is_finite()) {
        return Err(PhysicsError::InvalidValue(what));
    }
    let n2 = q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3];
    if (n2 - 1.0).abs() > 0.02 {
        return Err(PhysicsError::InvalidValue(what));
    }
    if is_normalized_quat(Quat { v: Vec3 { x: q[0], y: q[1], z: q[2] }, s: q[3] }) {
        return Ok(q);
    }
    let n = n2.sqrt();
    Ok([q[0] / n, q[1] / n, q[2] / n, q[3] / n])
}

/// How a body moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    /// Never moves; infinite mass.
    Static,
    /// Moved by its velocity only; pushes dynamic bodies, is not pushed.
    Kinematic,
    /// Moved by forces, contacts and joints.
    Dynamic,
}

/// A shape's geometry, in the body's frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Geometry {
    Sphere {
        center: [f32; 3],
        radius: f32,
    },
    Capsule {
        center1: [f32; 3],
        center2: [f32; 3],
        radius: f32,
    },
    /// A box centred on the body origin, by half extents.
    Box {
        half_extents: [f32; 3],
    },
    /// The convex hull of 4 to [`limits::MAX_HULL_POINTS`] points (at most 64 hull vertices
    /// are kept; a hull of triangles too big for Box3D's 128 edges is simplified to 44).
    Hull {
        points: Vec<[f32; 3]>,
    },
    /// A triangle mesh (world-model.md 3.2), built once ([`Mesh`]) and scaled per shape: Box3D multiplies every
    /// vertex by `scale`, axis by axis, in the body's frame. An axis under [`limits::MIN_MESH_SCALE`] in magnitude is
    /// refused. Each triangle collides on its front only (Box3D's mesh contact is one-sided), unless `both_sides`:
    /// then the shape has a back too, the same triangles reversed, as a second Box3D shape with no density, so the
    /// mass stays the mesh's own. A closed mesh weighs the solid it encloses at the shape's density, an open one
    /// nothing, and a dynamic body needs mass from its shapes together ([`PhysicsError::Massless`]). A mesh never
    /// collides with another mesh (Box3D has no mesh-against-mesh contact), and a body with a mesh shape is never
    /// swept as a bullet, only against.
    Mesh {
        mesh: Mesh,
        scale: [f32; 3],
        both_sides: bool,
    },
}

/// A shape: geometry plus surface and mass properties.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeDesc {
    pub geometry: Geometry,
    /// kg per cubic metre. Ignored for static and kinematic bodies.
    pub density: f32,
    pub friction: f32,
    pub restitution: f32,
    pub rolling_resistance: f32,
}

impl ShapeDesc {
    /// Box3D's default material (friction 0.6, restitution 0, rolling resistance 0) and
    /// density (1000 kg per cubic metre, water's: a 0.5 m radius ball weighs 524 kg).
    pub fn new(geometry: Geometry) -> ShapeDesc {
        let def = box3d_rust::types::default_shape_def();
        ShapeDesc {
            geometry,
            density: def.density,
            friction: def.base_material.friction,
            restitution: def.base_material.restitution,
            rolling_resistance: def.base_material.rolling_resistance,
        }
    }

    pub fn with_density(mut self, density: f32) -> ShapeDesc {
        self.density = density;
        self
    }

    pub fn with_friction(mut self, friction: f32) -> ShapeDesc {
        self.friction = friction;
        self
    }

    pub fn with_restitution(mut self, restitution: f32) -> ShapeDesc {
        self.restitution = restitution;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), PhysicsError> {
        let size = |v: f32, what| {
            if v.is_finite() && (MIN_SIZE..=MAX_SIZE).contains(&v) {
                Ok(())
            } else {
                Err(PhysicsError::InvalidValue(what))
            }
        };
        match &self.geometry {
            Geometry::Sphere { center, radius } => {
                bounded(center, MAX_SIZE, "sphere center")?;
                size(*radius, "sphere radius")?;
            }
            Geometry::Capsule { center1, center2, radius } => {
                bounded(center1, MAX_SIZE, "capsule center1")?;
                bounded(center2, MAX_SIZE, "capsule center2")?;
                size(*radius, "capsule radius")?;
            }
            Geometry::Box { half_extents } => {
                for &h in half_extents {
                    size(h, "box half extent")?;
                }
            }
            Geometry::Hull { points } => {
                if points.len() < 4 || points.len() > MAX_HULL_POINTS {
                    return Err(PhysicsError::BadHull);
                }
                for p in points {
                    bounded(p, MAX_SIZE, "hull point")?;
                }
            }
            Geometry::Mesh { mesh, scale, .. } => {
                if !scale.iter().all(|s| s.is_finite()) {
                    return Err(PhysicsError::InvalidValue("mesh scale"));
                }
                const UNDER: [&str; 3] = [
                    "mesh scale x is under 0.01 in magnitude, Box3D's floor on scale: rescale the mesh at import",
                    "mesh scale y is under 0.01 in magnitude, Box3D's floor on scale: rescale the mesh at import",
                    "mesh scale z is under 0.01 in magnitude, Box3D's floor on scale: rescale the mesh at import",
                ];
                for (s, why) in scale.iter().zip(UNDER) {
                    if s.abs() < MIN_MESH_SCALE {
                        return Err(PhysicsError::InvalidValue(why));
                    }
                }
                // Each vertex scaled, as Box3D scales it, within the size limit: the scaled corners of the box
                // around every vertex bound them all.
                let (lo, hi) = mesh.extent();
                let within =
                    (0..3).all(|k| (lo[k] * scale[k]).abs() <= MAX_SIZE && (hi[k] * scale[k]).abs() <= MAX_SIZE);
                if !within {
                    return Err(PhysicsError::InvalidValue("mesh vertex, scaled, beyond the size limit"));
                }
            }
        }
        let non_negative = |v: f32, max: f32, what| {
            if v.is_finite() && (0.0..=max).contains(&v) { Ok(()) } else { Err(PhysicsError::InvalidValue(what)) }
        };
        non_negative(self.density, MAX_DENSITY, "density")?;
        non_negative(self.friction, MAX_MATERIAL, "friction")?;
        non_negative(self.restitution, MAX_MATERIAL, "restitution")?;
        non_negative(self.rolling_resistance, MAX_MATERIAL, "rolling resistance")?;
        Ok(())
    }
}

/// A body: its kind, initial pose and velocity, and shapes (created in order).
#[derive(Clone, Debug, PartialEq)]
pub struct BodyDesc {
    pub kind: BodyKind,
    /// World position of the body origin, in the physics world's frame.
    pub position: [f64; 3],
    /// Unit quaternion `[x, y, z, w]`; within 1% of unit length, normalized before use.
    pub rotation: [f32; 4],
    pub linear_velocity: [f32; 3],
    pub angular_velocity: [f32; 3],
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub gravity_scale: f32,
    /// Swept in every sub-step against everything it collides with, moving or not, so no contact is skipped however
    /// fast either moves (world-model.md 3.2, box3d-fork D3). It takes effect when every shape is a sphere, capsule,
    /// box or hull; a body with a mesh shape is never swept itself (other bullets are swept against it). Off by
    /// default.
    pub bullet: bool,
    pub enable_sleep: bool,
    pub shapes: Vec<ShapeDesc>,
}

impl BodyDesc {
    /// A body of `kind` at `position` with Box3D's default settings and no shapes.
    pub fn new(kind: BodyKind, position: [f64; 3]) -> BodyDesc {
        let def = box3d_rust::types::default_body_def();
        BodyDesc {
            kind,
            position,
            rotation: [0.0, 0.0, 0.0, 1.0],
            linear_velocity: [0.0; 3],
            angular_velocity: [0.0; 3],
            linear_damping: def.linear_damping,
            angular_damping: def.angular_damping,
            gravity_scale: def.gravity_scale,
            bullet: false,
            enable_sleep: def.enable_sleep,
            shapes: Vec::new(),
        }
    }

    pub fn with_shape(mut self, shape: ShapeDesc) -> BodyDesc {
        self.shapes.push(shape);
        self
    }

    pub fn with_rotation(mut self, rotation: [f32; 4]) -> BodyDesc {
        self.rotation = rotation;
        self
    }

    pub fn with_velocity(mut self, linear: [f32; 3], angular: [f32; 3]) -> BodyDesc {
        self.linear_velocity = linear;
        self.angular_velocity = angular;
        self
    }

    /// Check every value and return the normalized rotation.
    pub(crate) fn validate(&self) -> Result<[f32; 4], PhysicsError> {
        position(&self.position, "body position")?;
        let rotation = unit_quat(self.rotation, "body rotation is not a unit quaternion")?;
        bounded(&self.linear_velocity, MAX_SPEED, "linear velocity")?;
        bounded(&self.angular_velocity, MAX_SPEED, "angular velocity")?;
        bounded(&[self.gravity_scale], MAX_DAMPING, "gravity scale")?;
        bounded(&[self.linear_damping, self.angular_damping], MAX_DAMPING, "damping")?;
        if self.linear_damping < 0.0 || self.angular_damping < 0.0 {
            return Err(PhysicsError::InvalidValue("negative damping"));
        }
        if self.shapes.len() > MAX_SHAPES_PER_BODY {
            return Err(PhysicsError::InvalidValue("too many shapes on one body"));
        }
        for shape in &self.shapes {
            shape.validate()?;
        }
        Ok(rotation)
    }
}

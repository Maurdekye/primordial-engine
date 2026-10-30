//! Engine-side descriptions of bodies and shapes. Plain data, validated before anything
//! reaches Box3D (4.3 rule 6: no NaN or infinity enters the simulation).

use crate::error::PhysicsError;

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
    Sphere { center: [f32; 3], radius: f32 },
    Capsule { center1: [f32; 3], center2: [f32; 3], radius: f32 },
    /// A box centred on the body origin, by half extents.
    Box { half_extents: [f32; 3] },
    /// The convex hull of these points (at most 64 hull vertices are kept).
    Hull { points: Vec<[f32; 3]> },
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
    /// Box3D's default material (friction 0.6, restitution 0, rolling resistance 0) with
    /// density 1.
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
        let positive = |v: f32, what| {
            if v.is_finite() && v > 0.0 { Ok(()) } else { Err(PhysicsError::InvalidValue(what)) }
        };
        let finite3 = |v: &[f32; 3], what| {
            if v.iter().all(|c| c.is_finite()) { Ok(()) } else { Err(PhysicsError::InvalidValue(what)) }
        };
        match &self.geometry {
            Geometry::Sphere { center, radius } => {
                finite3(center, "sphere center")?;
                positive(*radius, "sphere radius")?;
            }
            Geometry::Capsule { center1, center2, radius } => {
                finite3(center1, "capsule center1")?;
                finite3(center2, "capsule center2")?;
                positive(*radius, "capsule radius")?;
            }
            Geometry::Box { half_extents } => {
                for &h in half_extents {
                    positive(h, "box half extent")?;
                }
            }
            Geometry::Hull { points } => {
                if points.len() < 4 {
                    return Err(PhysicsError::BadHull);
                }
                for p in points {
                    finite3(p, "hull point")?;
                }
            }
        }
        let non_negative = |v: f32, what| {
            if v.is_finite() && v >= 0.0 { Ok(()) } else { Err(PhysicsError::InvalidValue(what)) }
        };
        non_negative(self.density, "density")?;
        non_negative(self.friction, "friction")?;
        non_negative(self.restitution, "restitution")?;
        non_negative(self.rolling_resistance, "rolling resistance")?;
        Ok(())
    }
}

/// A body: its kind, initial pose and velocity, and shapes (created in order).
#[derive(Clone, Debug, PartialEq)]
pub struct BodyDesc {
    pub kind: BodyKind,
    /// World position of the body origin, in the physics world's frame.
    pub position: [f32; 3],
    /// Unit quaternion `[x, y, z, w]`.
    pub rotation: [f32; 4],
    pub linear_velocity: [f32; 3],
    pub angular_velocity: [f32; 3],
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub gravity_scale: f32,
    /// Fast, small bodies get continuous collision against dynamic bodies too.
    pub bullet: bool,
    pub enable_sleep: bool,
    pub shapes: Vec<ShapeDesc>,
}

impl BodyDesc {
    /// A body of `kind` at `position` with Box3D's default settings and no shapes.
    pub fn new(kind: BodyKind, position: [f32; 3]) -> BodyDesc {
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

    pub(crate) fn validate(&self) -> Result<(), PhysicsError> {
        let finite = |v: &[f32], what| {
            if v.iter().all(|c| c.is_finite()) { Ok(()) } else { Err(PhysicsError::InvalidValue(what)) }
        };
        finite(&self.position, "body position")?;
        finite(&self.rotation, "body rotation")?;
        finite(&self.linear_velocity, "linear velocity")?;
        finite(&self.angular_velocity, "angular velocity")?;
        finite(&[self.linear_damping, self.angular_damping, self.gravity_scale], "damping or gravity scale")?;
        let n2: f32 = self.rotation.iter().map(|c| c * c).sum();
        if (n2 - 1.0).abs() > 0.02 {
            return Err(PhysicsError::InvalidValue("body rotation is not a unit quaternion"));
        }
        if self.linear_damping < 0.0 || self.angular_damping < 0.0 {
            return Err(PhysicsError::InvalidValue("negative damping"));
        }
        for shape in &self.shapes {
            shape.validate()?;
        }
        Ok(())
    }
}

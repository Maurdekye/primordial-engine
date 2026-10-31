use pe_core::ids::ObjectId;

/// Why a physics call was refused. A refused call changes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhysicsError {
    /// The object already has a body.
    AlreadyBound(ObjectId),
    /// The object has no body.
    NotBound(ObjectId),
    /// `ObjectId::NONE` cannot have a body.
    NoneObject,
    /// The joint key is unknown.
    UnknownJoint(u64),
    /// The joint key is already in use.
    JointExists(u64),
    /// The joint is not a grab.
    NotAGrab(u64),
    /// Both ends of a joint are the same body.
    SameBody,
    /// A value is NaN, infinite or out of range (4.3 rule 6). The text names it.
    InvalidValue(&'static str),
    /// A hull could not be built from the points given.
    BadHull,
    /// The world is stepping (only possible from a callback).
    Locked,
    /// A world image could not be read or does not fit this world.
    BadImage(&'static str),
    /// A restore failed after it had started overwriting the world. Only a successful
    /// [`crate::PhysicsWorld::restore_image`] makes the world usable again.
    Poisoned,
    /// Box3D's process-wide length units are not metres (a recording player is running).
    LengthUnits,
}

impl std::fmt::Display for PhysicsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PhysicsError::AlreadyBound(id) => write!(f, "object {id:?} already has a body"),
            PhysicsError::NotBound(id) => write!(f, "object {id:?} has no body"),
            PhysicsError::NoneObject => write!(f, "ObjectId::NONE cannot have a body"),
            PhysicsError::UnknownJoint(k) => write!(f, "unknown joint {k}"),
            PhysicsError::JointExists(k) => write!(f, "joint {k} already exists"),
            PhysicsError::NotAGrab(k) => write!(f, "joint {k} is not a grab"),
            PhysicsError::SameBody => write!(f, "both ends of the joint are the same body"),
            PhysicsError::InvalidValue(what) => write!(f, "invalid value: {what}"),
            PhysicsError::BadHull => write!(f, "could not build a convex hull from these points"),
            PhysicsError::Locked => write!(f, "the physics world is stepping"),
            PhysicsError::BadImage(why) => write!(f, "bad world image: {why}"),
            PhysicsError::Poisoned => write!(f, "a failed restore left the world unusable; restore a good image"),
            PhysicsError::LengthUnits => write!(f, "Box3D length units are not metres"),
        }
    }
}

impl std::error::Error for PhysicsError {}

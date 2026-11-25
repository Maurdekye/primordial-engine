use pe_core::ids::ObjectId;

/// Why a physics call was refused. A refused call changes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhysicsError {
    /// The object already has a body.
    AlreadyBound(ObjectId),
    /// The object has no body.
    NotBound(ObjectId),
    /// The object's body is not a player ([`crate::PhysicsWorld::move_player`]).
    NotAPlayer(ObjectId),
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
    /// A mesh could not be built from the vertices and triangles given ([`crate::Mesh::new`]). The text says why.
    BadMesh(&'static str),
    /// A mesh or hull blob could not be read (world-model.md 3.2's formats). The text says why.
    BadBlob(&'static str),
    /// A dynamic body with a mesh shape whose shapes together have no mass (world-model.md 3.2: pe-replica's
    /// `Unbuildable::Mass`): Box3D would give it no inverse mass, so it would answer no force. `dropped` counts the
    /// triangles Box3D left out under the minimum area in the meshes they opened (closed as given, open as built);
    /// 0 when no mesh was opened that way.
    Massless { dropped: u32 },
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
            PhysicsError::NotAPlayer(id) => write!(f, "object {id:?}'s body is not a player"),
            PhysicsError::NoneObject => write!(f, "ObjectId::NONE cannot have a body"),
            PhysicsError::UnknownJoint(k) => write!(f, "unknown joint {k}"),
            PhysicsError::JointExists(k) => write!(f, "joint {k} already exists"),
            PhysicsError::NotAGrab(k) => write!(f, "joint {k} is not a grab"),
            PhysicsError::SameBody => write!(f, "both ends of the joint are the same body"),
            PhysicsError::InvalidValue(what) => write!(f, "invalid value: {what}"),
            PhysicsError::BadHull => write!(f, "could not build a convex hull from these points"),
            PhysicsError::BadMesh(why) => write!(f, "could not build the mesh: {why}"),
            PhysicsError::BadBlob(why) => write!(f, "could not read the blob: {why}"),
            PhysicsError::Massless { dropped: 0 } => write!(
                f,
                "a dynamic body needs mass, and its shapes together have none: close the mesh, or add a primitive or \
                 a hull with a density"
            ),
            PhysicsError::Massless { dropped } => write!(
                f,
                "{dropped} of the mesh's triangles are under the minimum area and were left out; without them the \
                 mesh is open"
            ),
            PhysicsError::Locked => write!(f, "the physics world is stepping"),
            PhysicsError::BadImage(why) => write!(f, "bad world image: {why}"),
            PhysicsError::Poisoned => write!(f, "a failed restore left the world unusable; restore a good image"),
            PhysicsError::LengthUnits => write!(f, "Box3D length units are not metres"),
        }
    }
}

impl std::error::Error for PhysicsError {}

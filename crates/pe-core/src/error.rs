use crate::ids::ObjectId;

/// Errors shared by the sim crates. Every variant is deterministic: the same operation on the same state
/// fails the same way on every peer, so a refused entry is refused everywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The object does not exist (never created, or destroyed).
    UnknownObject(ObjectId),
    /// An object with this id already exists.
    DuplicateObject(ObjectId),
    /// The component id is not in the registry.
    UnknownComponent(u32),
    /// The object has no row in that component's column.
    MissingComponent { object: ObjectId, component: u32 },
    /// A float field holds NaN or an infinity (design 4.3 rule 6), or a bool field is not 0 or 1.
    InvalidValue { component: u32, object: ObjectId, field: String },
    /// The component is managed by the engine (the hierarchy's `Node`) and cannot be written directly.
    Reserved { component: u32 },
    /// A creator ran out of object counters (2^32 - 1 objects).
    CountersExhausted { creator: u32 },
    /// A component description is malformed: gaps, overlaps or a field outside the row.
    BadLayout(String),
    /// Raw bytes have the wrong length for the component.
    WrongSize { component: u32, expected: usize, got: usize },
    /// A plain edit would give `object` a frame while its root transform has a non-zero `chunk` (for
    /// example a root that `maintain_transforms` has rebased). Use the keep-world edit instead
    /// (`set_parent_keep_world` or `insert_transform_keep_world`), which recomputes the chunk.
    ChunkUnderFrame { object: ObjectId },
    /// A reparent would make an object its own ancestor.
    Cycle { child: ObjectId, parent: ObjectId },
    /// A snapshot was taken with a different component schema than the store has now.
    SchemaMismatch { component: u32 },
    /// Serialized state is truncated or malformed.
    Corrupt(&'static str),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}

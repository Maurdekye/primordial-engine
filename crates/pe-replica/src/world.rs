//! What pe-replica needs from pe-world: the calls of docs/spec/replica.md 8.1, proposed to @world-model under
//! pe-seams's ruling 9 (both crates sit behind S2, so they agree on these between them).
//!
//! Until pe-world is written, the replica runs against this trait, and its tests against [`crate::mock::MockWorld`].
//! When pe-world lands, `pe_world::World` implements it (or the trait gives way to pe-world's own API, if
//! @world-model reshapes it), and the row types below become pe-world's.

use pe_seams::{DomainId, Frame, Hash, ImportError, MemberKey, ObjectId, Outcome, Pos, SignedEntry, SimRate, Tree};

// ---- what an applied entry or a logic stage changed ----------------------------------------------------------------

/// What changed about one object's physics-relevant rows (replica.md 3.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Change {
    /// Created, or became physical (gained `Body` and `Shape`): a body is added at its world transform, at rest.
    Created,
    /// Deleted, or stopped being physical: its body goes, with its joints.
    Removed,
    /// Its `Shape` or `Body` fields, or its world scale, changed: the body is rebuilt, keeping pose and velocities.
    Rebuilt,
    /// An edit set its `Transform` (a teleport): exactly the new pose, velocities kept, awake, history reset.
    Teleported,
    /// An edit op set its velocity.
    Velocity { linear: [f32; 3], angular: [f32; 3] },
    /// Reparented: its world pose is kept, and nothing changes in the physics, unless it is static and an ancestor
    /// moved it (then `Rebuilt`).
    Reparented,
}

/// An impulse that an entry or a logic stage applies before the step (world-model.md 8).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PushFx {
    pub object: ObjectId,
    /// Newton seconds, in the world frame.
    pub impulse: [f32; 3],
    /// Where it applies, in the world frame; `None` at the centre of mass.
    pub at: Option<Pos>,
}

/// The digest an interaction carried (replica.md 3.4): a check compared once its frame is final, never written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CarriedFx {
    pub object: ObjectId,
    pub digest: [u8; 32],
}

/// The physics effects of one applied entry, or of one logic stage's change set. Empty for a refused entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effects {
    /// In id order, at most one per object, except `Rebuilt` then `Teleported` for a body whose shapes or fields
    /// and whose world pose an entry both changed: a rebuild keeps a moving body's physics pose.
    pub changed: Vec<(ObjectId, Change)>,
    pub pushes: Vec<PushFx>,
    pub carried: Option<CarriedFx>,
}

impl Effects {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.pushes.is_empty() && self.carried.is_none()
    }
}

// ---- what the physics reads ----------------------------------------------------------------------------------------

/// A body's kind (world-model.md 3.2, `Body`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    Static,
    Kinematic,
    Dynamic,
}

/// One shape of a body, in the body's frame, already scaled by the object's world scale (replica.md 2.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShapeRow {
    Sphere { center: [f32; 3], radius: f32 },
    Capsule { center1: [f32; 3], center2: [f32; 3], radius: f32 },
    Box { center: [f32; 3], rotation: [f32; 4], half_extents: [f32; 3] },
}

/// One shape of a body and its density: pe-world keeps a density on each `Shape` row (world-model.md 3.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapePart {
    pub shape: ShapeRow,
    /// Kg per cubic metre (replica.md 2.1: mass becomes density).
    pub density: f32,
}

/// The physics view of one physical object: its `Body`, its shapes (its own `Shape`, then its compound children's in
/// id order) and its world transform.
#[derive(Clone, Debug, PartialEq)]
pub struct BodyRow {
    pub kind: BodyKind,
    pub pos: Pos,
    pub rotation: [f32; 4],
    pub shapes: Vec<ShapePart>,
    pub friction: f32,
    pub restitution: f32,
    pub rolling_resistance: f32,
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub gravity_scale: f32,
    pub bullet: bool,
    pub may_sleep: bool,
    pub grabbable: bool,
}

/// A hand's row (world-model.md 8): what its owner's newest input set.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HandRow {
    pub hand: ObjectId,
    pub owner: MemberKey,
    pub target: Pos,
    pub rotation: [f32; 4],
    pub grip: f32,
    pub engaged: bool,
    /// The frames the newest input covers: from its stamp, `span` frames.
    pub stamp: Frame,
    pub span: u32,
    /// The stamp of the input before it, if any: the stream rule of replica.md 3.3.
    pub previous: Option<(Frame, u32)>,
}

/// A claim (world-model.md 7.2): `hand` holds `object` since frame `since`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClaimRow {
    pub object: ObjectId,
    pub hand: ObjectId,
    pub member: MemberKey,
    pub since: Frame,
}

/// The world settings the physics takes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysicsSettings {
    pub rate: SimRate,
    pub gravity: [f32; 3],
    /// The domain's physics origin (world-model.md 4.2): bodies are simulated at `pos - origin`.
    pub origin: Pos,
}

// ---- what the replica writes ---------------------------------------------------------------------------------------

/// A bound body's `BodyState` row at a cut frame (replica.md 2.3): derived, hashed, never read by the step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyStateRow {
    pub linear: [f32; 3],
    pub angular: [f32; 3],
    pub awake: bool,
    pub sleep_time: f32,
    pub digest: [u8; 32],
}

// ---- the trait -----------------------------------------------------------------------------------------------------

/// pe-world, as the replica drives it (replica.md 3 and 8.1). Every call is deterministic in the state and its
/// arguments.
pub trait WorldModel: Send {
    /// A saved state, for keyframes and held roots: copy-on-write (world-model.md 5.2), so cheap to take every few
    /// frames and to keep.
    type Saved: Clone + Send + Sync;

    fn domain(&self) -> DomainId;

    // the step

    /// Stage 1, once per entry in agreed order: the outcome, and for an applied entry its physics effects.
    fn apply(&mut self, frame: Frame, entry: &SignedEntry) -> (Outcome, Effects);
    /// Claims and cooldowns expire at `frame + 1` (world-model.md 7.2).
    fn end_frame(&mut self, frame: Frame);

    // what the physics reads

    /// An object's physics view, or `None` if it is not physical.
    fn body(&self, id: ObjectId) -> Option<BodyRow>;
    /// Every physical object, in id order: for building a physics world from scratch.
    fn physical(&self) -> Vec<ObjectId>;
    /// Every `Hand` row, in id order.
    fn hands(&self) -> Vec<HandRow>;
    /// The claims, in the held objects' id order.
    fn claims(&self) -> Vec<ClaimRow>;
    fn settings(&self) -> PhysicsSettings;

    // what the replica writes

    /// Step 7's write-back (replica.md 3), once per frame: every body that moved, in id order, with its world pose.
    /// pe-world converts each against the frame's final world poses, parents first, so the result does not depend on
    /// the order (@world-model, m65729e24d52c). Returns the bodies whose pose is out of range (world-model.md 4.3), in
    /// id order: nothing is written for them, and the rest apply.
    fn write_poses(&mut self, poses: &[(ObjectId, Pos, [f32; 4])]) -> Vec<ObjectId>;
    /// Repairs out-of-limit transforms that trusted code wrote, and returns how many: nonzero is a fault (3.1).
    fn maintain_transforms(&mut self) -> u32;
    /// At a cut frame, before the root: every bound body's `BodyState`, and the domain's `physics_global`. It replaces
    /// every `BodyState` row: a body missing from `rows` has none afterwards. The root of a `save()` taken right after
    /// it, once restored, is the root taken then (rung 1 checks `Cv` that way).
    fn write_physics(&mut self, rows: &[(ObjectId, BodyStateRow)], global: Hash);

    // state

    /// The root of the current state; at a cut frame, after `write_physics`.
    fn root(&self) -> Hash;
    fn save(&self) -> Self::Saved;
    fn restore(&mut self, saved: &Self::Saved);
    /// The S2 tree calls on a saved root.
    fn tree<'a>(&'a self, saved: &'a Self::Saved) -> Box<dyn Tree + 'a>;
    /// The state with no objects, no rows and empty world tables. Not genesis, which would bring the registry and the
    /// settings along. It is only ever an import base, for an import from empty (a joiner, or a replica that answered
    /// `Damaged` at the cut), never a state to step from (@world-model, m5442e8e58e81).
    fn empty(&self) -> Self::Saved;
    /// Rung 2's and a joiner's world-model part: `saved` patched from records, each checked against
    /// `import.target_root` before anything is applied (`BadProof`), with every leaf in `import.absent` removed. The
    /// patched state is the state at the cut's frame. It refuses a bound body that comes without the physics image
    /// (`NeedsPhysicsImage`), and an image whose `global` is not the cut's `physics_global` record (`PhysicsCheck`;
    /// @world-model, m5442e8e58e81).
    fn import(&self, saved: &Self::Saved, import: &pe_seams::CutImport) -> Result<Self::Saved, ImportError>;
}

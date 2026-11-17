//! What pe-replica needs from pe-world: the calls of docs/spec/replica.md 8.1, proposed to @world-model under
//! pe-seams's ruling 9 (both crates sit behind S2, so they agree on these between them).
//!
//! [`crate::peworld::PeWorld`] implements it over pe-world's `World`, and [`crate::mock::MockWorld`] is a small
//! world model for tests and the harness.

use std::any::Any;
use std::sync::Arc;

use pe_seams::{
    Activation, DomainId, Frame, Hash, ImportError, MemberKey, ObjectClass, ObjectId, Outcome, Pos, SignedEntry,
    SimRate, Stage, Tree, ViewFlags, ViewObject,
};
use pe_world::script::AssetId;

use crate::logic::{ScriptEvent, ScriptRunner};

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

/// The physics effects of one applied entry, or of one logic stage's change set, and what its record carries
/// besides the outcome. Empty for a refused entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effects {
    /// In id order, at most one per object, except `Rebuilt` then `Teleported` for a body whose shapes or fields
    /// and whose world pose an entry both changed: a rebuild keeps a moving body's physics pose.
    pub changed: Vec<(ObjectId, Change)>,
    pub pushes: Vec<PushFx>,
    pub carried: Option<CarriedFx>,
}

/// What one logic stage did, for the replica ([`WorldModel::logic_stage`]; logic.md 9.2a).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogicFx {
    /// The physics effects of what the scripts created, removed, reparented and moved.
    pub effects: Effects,
    /// The events the scripts emitted, in order, each with its object's world position after the stage.
    pub events: Vec<(ScriptEvent, Pos)>,
    /// Local counts, not world state: host-stack faults and floating-point resets (N11).
    pub host_faults: u32,
    pub fp_resets: u32,
    /// Scripts that did not run on this machine (a build fault here: [`crate::logic::StageOutcome::missing`]).
    pub missing: Vec<AssetId>,
}

impl Effects {
    /// No physics effects.
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

/// A hand's radius in metres: the sphere a hand without a `Shape` of its own is drawn as (S5), and the body it will
/// push with once the replica steps hand inputs. pe-session's `HAND_R` (archive/crates/pe-session/src/session/
/// sim.rs:29), until avatars give hands a look of their own.
pub const HAND_RADIUS: f32 = 0.09;

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
    /// When its body is physical (D18b, per member): [`crate::hands::hand_mode`].
    pub mode: u8,
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

// ---- the order within a frame --------------------------------------------------------------------------------------

/// Where an entry applies within its frame (world-model.md 8.1; @coordinator-opus's apply-order ruling,
/// mf12426b9ee0a): every `First` entry before every `Second` one, and within a phase in agreed order. There are
/// exactly two phases: another is a seam and spec change. pe-world's `input_phase` is 0 for a hand input (`First`) and
/// 1 for anything else (`Second`), so a grab's reach is measured after its frame's hand movement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// Applies first: pe-world's hand inputs. The default, so a model without phases applies in agreed order.
    #[default]
    First,
    /// Applies after every `First` entry of its frame.
    Second,
}

/// A frame's entries in apply order: a stable sort by `phase`, so within a phase they keep their agreed order, and
/// none is dropped. It is the only path by which anything applies a frame's entries: the replica's step (live, in a
/// re-simulation, in rung 1's replay and in the re-step for a root at a cut) and the harness's oracle. Void entries are
/// removed before it. Hashes (the cut, the chains, the change record) stay in agreed order, and only the state root
/// reflects this one: an author's own entries in one frame can apply out of their sequence order.
pub fn apply_order<'a>(
    entries: impl IntoIterator<Item = &'a SignedEntry>,
    phase: impl Fn(&SignedEntry) -> Phase,
) -> Vec<&'a SignedEntry> {
    let mut order: Vec<&SignedEntry> = entries.into_iter().collect();
    // Stable: `sort_unstable_by_key` could reorder the entries of one phase.
    order.sort_by_key(|e| phase(e));
    order
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

    /// The entry's phase within its frame ([`apply_order`]). It reads only the entry, never the state. It is part of
    /// the determinism contract, so changing it is a world-format change. The default leaves every entry `First`, so a
    /// model without phases applies its frames in agreed order.
    fn phase(_entry: &SignedEntry) -> Phase
    where
        Self: Sized,
    {
        Phase::First
    }
    /// Stage 1, once per entry in apply order ([`apply_order`]): the outcome, and for an applied entry its physics
    /// effects.
    fn apply(&mut self, frame: Frame, entry: &SignedEntry) -> (Outcome, Effects);
    /// Claims and cooldowns expire at `frame + 1` (world-model.md 7.2). Returns what became of code commits at this
    /// frame's end (S5 v1.8's `FrameView.activations`; logic.md 6.2a), in asset order: activated, voided, or
    /// activated as the local member's prediction (option (i)).
    fn end_frame(&mut self, frame: Frame) -> Vec<Activation>;

    // the logic stages (crate::logic)

    /// Whether the world has scripts: a row in its modules table (logic.md 6.1). A replica without a runner refuses
    /// to step it. A model without scripts, the default, has none.
    fn scripted(&self) -> bool {
        false
    }
    /// One logic stage of `frame` (`stage` is `LogicPrePhysics` or `LogicPostPhysics`), run by `runner` over the
    /// scripts the modules table lists (crate::logic): what they changed, as physics effects, and what they emitted.
    /// A model without scripts, the default, does nothing.
    fn logic_stage(&mut self, _frame: Frame, _stage: Stage, _runner: &mut dyn ScriptRunner) -> LogicFx {
        LogicFx::default()
    }

    // the author's prediction of its own code commit (logic.md 6.2a's option (i))

    /// The local member ([`crate::ReplicaConfig::local`]), whose own code commits the model predicts. Called once, when
    /// the replica is made. The default, a model without scripts, ignores it.
    fn set_local(&mut self, _local: Option<MemberKey>) {}
    /// Where a prediction that just ended started, `c`: the replica steps again from there under the rule, at once.
    /// Taken: `None` until the next prediction ends. The default never predicts.
    fn take_replay_from(&mut self) -> Option<Frame> {
        None
    }
    /// The frames from which this machine may not vote while it predicts (S2 v1.7's `vote_holds`): `c + 1` while a
    /// prediction stands or its replay is pending. The default holds none.
    fn vote_holds(&self) -> Vec<Frame> {
        Vec::new()
    }
    /// Whether the replica is stepping off its live history (`root_at_cut`'s re-step from a first difference): the
    /// model's machine state must not change there, so no prediction starts or runs in such a step. The default has
    /// no such state.
    fn set_scratch(&mut self, _scratch: bool) {}

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

    // what the frame view draws (S5)

    /// What the frame view draws, once per publication (S5): every object with something to draw, in id order, at
    /// its world pose and scale, with its shape, appearance, light, bounding radius, hand owner, holder and edit
    /// version. The model leaves the velocities zero and the flags empty: the replica fills the velocities from its
    /// physics and marks the local member's hands `OWN_HAND` ([`crate::ReplicaConfig::local`]). The default draws
    /// each physical body at its pose with its bounding radius and nothing else, which is all a model without
    /// visuals (the mock) can say.
    fn view_objects(&self) -> Vec<ViewObject> {
        self.physical()
            .into_iter()
            .filter_map(|id| {
                let row = self.body(id)?;
                Some(ViewObject {
                    id,
                    class: ObjectClass::World,
                    pos: row.pos,
                    rotation: row.rotation,
                    scale: [1.0; 3],
                    linear_velocity: [0.0; 3],
                    angular_velocity: [0.0; 3],
                    shape: None,
                    appearance: None,
                    light: None,
                    radius: crate::replica::bounding_radius(&row.shapes),
                    hand_of: None,
                    held_by: None,
                    flags: ViewFlags::default(),
                    version: None,
                })
            })
            .collect()
    }

    /// The model's read-only snapshot of its live state, for [`pe_seams::Replica::world_snapshot`]: `final_frame` is
    /// the frame of the replica's last agreed cut. `None` by default, for a model without one (the mock).
    fn snapshot(&self, _final_frame: Frame) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }

    /// A saved state's read-only snapshot, for [`pe_seams::Replica::agreed_snapshot`]. `None` by default.
    fn snapshot_of(_saved: &Self::Saved, _final_frame: Frame) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }

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
    /// patched state is the state at the cut's frame. It refuses a bound body that comes in a record, or that
    /// `absent` removes, without the physics image (`NeedsPhysicsImage`): either changes the physics. It refuses an
    /// image whose `global` is not the cut's `physics_global` record (`PhysicsCheck`; @world-model, m5442e8e58e81).
    fn import(&self, saved: &Self::Saved, import: &pe_seams::CutImport) -> Result<Self::Saved, ImportError>;
}

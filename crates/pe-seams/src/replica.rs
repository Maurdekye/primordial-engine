//! Seam S2, the replica: one domain's deterministic state machine with its rollback loop (replica.md 1, 8).
//!
//! **Who does what** (agreed by WS-state, WS-world and WS-diverge; v0 settles the names):
//! - **pe-replica** implements [`Replica`]. It owns the order of work inside a frame ([`Stage`]), keyframes, save
//!   and restore, the rollback loop, the physics part of the root, repair rung 1, [`Replica::physics_image`], the
//!   receiver of a repair import, and the frame view (S5).
//! - **pe-world** owns the store, the edit and conflict rules, permission checks (as outcomes, inside the step), the
//!   hash tree and the tree calls of [`Tree`] (`node`, `export`, `object_hashes`, `verify`). pe-replica reaches them
//!   through [`Replica::tree`], on a root it holds.
//! - **Keyframes stay inside the replica.** pe-agree and pe-diverge never handle them: they see roots, trees,
//!   images, imports and outcomes. Nobody outside restores an arbitrary state.
//! - **The driver** (pe-app's simulation thread, and the harness) runs: deliver messages to S3, `submit` local
//!   entries, `tick`, [`Replica::advance`] to the present, give S3 the roots it asks for
//!   ([`Replica::root_at_cut`]), and pass the source's final cut ([`EntrySource::final_cut`]) to
//!   [`Replica::finalize`] each time it changes, and S3's events to pe-diverge. That loop is one
//!   function, which WS-state writes in pe-replica (`drive`), so the harness tests the loop that ships
//!   (@harness-eng's DESIGN.md 3.2).
//!
//! **Kept state** (the user's ruling that every limit is a duration): the floor (the newest final cut), my last
//! agreed cut `Cv` if older, and the roots of the final cuts within `T_keep` (1.5 s by default, at least the last 2
//! cuts), which are served to peers for repair (divergence.md 11.1, 14); also my root at a cut I diverged at, until
//! its repair ends, and the root I voted with at each cut not yet final. Merge bases are kept apart (M2).
//!
//! **Provisional in v0:** [`Reason`]'s variants (@world-model fills in its fields as pe-world is written);
//! [`TreePath`], [`Node`] and [`Record`]'s proof shape (WS-world with WS-diverge); [`PhysicsImage`] as one blob
//! (chunking by hash comes with the Bulk lane). **Not yet in the trait:** the M2 calls for the merge and background
//! replay (a fork at a frame, per-group restore and step, the per-frame footprint); they come with v1.

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;

use crate::agree::{Cut, CutRef, FinalCut};
use crate::entry::{DomainId, SignedEntry};
use crate::ids::{EntryRef, Frame, Hash, MemberKey, ObjectId};
use crate::time::HostTime;
use crate::view::FrameView;

// ---- outcomes ------------------------------------------------------------------------------------------------------

/// A row's or an object's version: the entry that last wrote it, and the op's index in that entry
/// (world-model.md 7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub entry: EntryRef,
    pub op: u32,
}

/// A field of a component, as a refusal names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FieldPath {
    /// The component id (128 bits, world-model.md 10).
    pub component: u128,
    pub field: u32,
}

/// What every machine computes for an entry at its place in the agreed order (world-model.md 7.7). Outcomes are a
/// function of the state and the entry; they are not hashed, the state they produce is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Applied {
        /// The objects it created, in op order.
        created: Vec<ObjectId>,
        /// The versions it overwrote (a plain write after another's).
        overwrote: Vec<Version>,
        notes: Vec<Note>,
        /// For an edit: the edit payload, in pe-world's codec, that undoes it (one-step revert, @api-eng's O4).
        /// WorldApi submits it as a new transaction with preconditions on the versions this one set.
        inverse: Option<Bytes>,
    },
    Refused {
        /// The index of the op that failed, if one did.
        op: Option<u32>,
        reason: Reason,
    },
}

/// Something an applied entry did that its author may want to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    /// A child it created or reparented under an object deleted earlier in the order became a root
    /// (world-model.md 7.5).
    Orphaned { object: ObjectId, deleted_by: EntryRef },
    /// It deleted an object that was already deleted.
    AlreadyDeleted { object: ObjectId, by: EntryRef },
}

/// Why an entry was refused: pe-world's reasons, checked in the order of world-model.md 7.6. A refused entry
/// changes nothing, the same way on every machine. Provisional: @world-model owns the list.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reason {
    /// The payload does not decode canonically, or breaks a limit (4,096 ops, 1 MiB).
    Malformed,
    /// The author lacks the permission.
    NoPermission,
    /// An op would break a protection.
    Protected { owner: MemberKey },
    /// A precondition named an older version; `winner` wrote the current one.
    Stale { precondition: u32, current: Version, winner: EntryRef },
    /// An op on, or a component attached to, an object deleted earlier in the order (mfc2bbd9d28ad).
    Deleted { object: ObjectId, by: EntryRef },
    /// No such object, and no tombstone.
    Unknown(ObjectId),
    /// A create of an id that already exists.
    Exists(ObjectId),
    /// A row of the wrong size, a value out of its kind, an unregistered component, or a field edits may not write.
    Invalid { field: Option<FieldPath> },
    /// A position past the supported range (world-model.md 4.3).
    OutOfRange,
    /// A reparent that would make a cycle.
    Cycle,
    /// The object is claimed by another hand; the first grab in the order wins (R5).
    Held { by: MemberKey },
    /// A grab beyond the hand's reach.
    OutOfReach,
    /// An input that moves another member's hand.
    NotYourHand,
    /// A transaction's new-object indices overlap.
    NewIndexOverlap,
}

// ---- the frame -----------------------------------------------------------------------------------------------------

/// The stages of one frame, in their fixed order (@iter-bench's N1, agreed with @vdw-physics; replica.md 3,
/// logic.md 3.5). Every replica runs exactly this order; it is part of the domain's history function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// The frame's entries, in agreed order: pe-world applies each, and the replica acts on its physics effects
    /// before the next entry.
    Entries,
    /// Logic systems bound before the physics.
    LogicPrePhysics,
    /// The physics step, with the drives before it and the write-back after it.
    Physics,
    /// Logic systems bound after the physics.
    LogicPostPhysics,
    /// The frame's one-shot effects are collected; at a cut frame the root is taken.
    Events,
}

impl Stage {
    pub const ORDER: [Stage; 5] =
        [Stage::Entries, Stage::LogicPrePhysics, Stage::Physics, Stage::LogicPostPhysics, Stage::Events];
}

/// A group of state that can be saved, restored and stepped alone (ruled 2026-11-10 20:05Z). M1 uses `Domain` only.
/// A group containing any bound body acts as `Domain`, because physics restores and steps only the whole domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupSel {
    Domain,
    Objects(BTreeSet<ObjectId>),
}

/// Where a replica's entries come from: S3 in the engine ([`crate::Agreement`] extends it), a mock or the harness
/// in tests.
pub trait EntrySource {
    /// The newest final cut whose entries this member holds in full, once there is one: the replica's floor follows
    /// it, and nothing at or below its frame changes again. It can lag the line's newest final cut
    /// ([`crate::AgreeEvent::CutFinal`]): a member that was not among a cut's voters may learn the cut is final while
    /// some of its entries are still on their way. Those entries then arrive as changes below the line's final frame
    /// ([`EntrySource::take_changed_from`]), and the replica re-simulates them, down to its own floor.
    fn final_cut(&self) -> Option<&FinalCut>;
    /// The entries to simulate at `frame`, in agreed order: final ones, then the predicted rest. Entries voided by a
    /// final cut are absent.
    fn entries_at(&self, frame: Frame) -> &[SignedEntry];
    /// The earliest frame whose entries changed since the last call, if any.
    fn take_changed_from(&mut self) -> Option<Frame>;
    /// When an entry arrived on this machine, if it came from the network: for "play a new effect only if its cause
    /// arrived recently" (divergence.md D6).
    fn arrived_at(&self, _entry: &EntryRef) -> Option<HostTime> {
        None
    }
}

/// What one [`Replica::advance`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Advance {
    /// The frame it rolled back to, if entries had changed below the present.
    pub rolled_back_to: Option<Frame>,
    /// Frames simulated again below the previous present.
    pub resimulated: u64,
    /// New frames stepped.
    pub stepped: u64,
}

/// What [`Replica::finalize`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finalized {
    /// My root at the cut is the quorum's: the cut is my new floor and my `Cv`.
    Agreed,
    /// My root differs: I diverged at this cut, and rung 1 is due.
    Diverged { mine: Hash },
    /// I do not hold every entry of the cut yet (the d1 rule): nothing compared, and the floor stays. It does not
    /// happen when the driver passes [`EntrySource::final_cut`], which waits for the entries; a driver that passes a
    /// cut early calls again once the source reports it.
    NotReady,
}

/// Rung 1's result (divergence.md 4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rung1 {
    /// Replayed from `Cv` to the cut, and my root there now equals the quorum's. The cut is my new `Cv` and floor,
    /// and the replica has replayed on to the present itself, so no frame after the cut still comes from the wrong
    /// state.
    Cured,
    /// The replay gives the same wrong root again: the fault is not a transient one.
    Persisted { root: Hash },
    /// The saved state at `Cv` failed its full rehash (records or physics): go to rung 2 at once.
    Damaged,
    /// `Cv`'s state is no longer kept.
    NotKept,
}

/// Why [`Replica::root_at_cut`] has no root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootError {
    /// The cut's frame is below my floor, or the state there is not kept.
    NotKept,
    /// I do not hold every entry the cut includes (the d1 rule): I may neither vote nor compare.
    MissingEntries,
    /// The cut's frame is past what I have simulated.
    NotYetSimulated,
}

// ---- the tree ------------------------------------------------------------------------------------------------------

/// A path from a root to a node: one child key per level (a nibble in the objects trie, or a table's index).
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TreePath(pub Vec<u8>);

/// What a leaf is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LeafKey {
    Object(ObjectId),
    /// A table of the tables root, by its index (world-model.md 6.2).
    Table(u8),
    /// A domain's `physics_global`.
    Physics(DomainId),
}

/// One node of a state tree, as [`Tree::node`] returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// The present children's (key, hash) pairs, in key order.
    Inner(Vec<(u8, Hash)>),
    Leaf {
        key: LeafKey,
        hash: Hash,
    },
}

/// A leaf's record with its proof, as [`Tree::export`] returns it: the receiver checks every record against the
/// quorum-agreed root before applying anything, never against the sender's claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub key: LeafKey,
    pub path: TreePath,
    /// The record's canonical bytes (pe-world's).
    pub bytes: Bytes,
    /// The inner nodes from the root down to the leaf's parent.
    pub proof: Vec<Node>,
}

/// A full rehash disagreed with a cached hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    pub path: TreePath,
    pub cached: Hash,
    pub computed: Hash,
}

/// Read access to one root this replica holds: pe-world's tree calls (world-model.md 6.5).
pub trait Tree {
    fn root(&self) -> Hash;
    fn frame(&self) -> Frame;
    /// A node's children, or a leaf; `None` if there is no node at that path.
    fn node(&self, path: &TreePath) -> Option<Node>;
    /// The records of these leaves (objects and tables), with their proofs. A leaf absent here is absent from the
    /// result; a verified parent node proves its absence. A `LeafKey::Physics` key gives no record: the physics travels
    /// as the whole image ([`Replica::physics_image`]).
    fn export(&self, keys: &[LeafKey]) -> Vec<Record>;
    /// Each object's leaf hash, which covers its body's digest: for the fault bundle (divergence.md, `replay-bundle`).
    fn object_hashes(&self, ids: &[ObjectId]) -> Vec<(ObjectId, Option<Hash>)>;
    /// Rehash from the rows and tables alone, ignoring every cached hash (@diverge-eng's R-H3).
    fn verify(&self) -> Result<(), Mismatch>;
}

/// One domain's physics at a root: its canonical transfer, checked against the root's `physics_global` before use
/// (replica.md 6.2). Rung 2 and a joiner restore bound bodies only this way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicsImage {
    pub domain: DomainId,
    pub frame: Frame,
    /// The image's check hash: what the root's `physics_global` must equal.
    pub global: Hash,
    pub bytes: Bytes,
}

/// What rung 2 or a joiner imports at a final cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutImport {
    pub cut: CutRef,
    /// The root the quorum signed at that cut: every record is checked against it.
    pub agreed_root: Hash,
    pub records: Vec<Record>,
    /// Leaves to remove, each proved absent by a verified parent node.
    pub absent: Vec<LeafKey>,
    /// The whole physics image, when bound bodies are in the import.
    pub physics: Option<PhysicsImage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportError {
    /// The cut's state is no longer kept.
    NotKept,
    /// A bound body can be restored only with the whole physics image: the fork offers no exact per-body setter
    /// (@vdw-physics; world-model.md 6.5).
    NeedsPhysicsImage(ObjectId),
    /// The record at this index does not check against the agreed root.
    BadProof(usize),
    /// The physics image's check hash is not the cut's `physics_global`.
    PhysicsCheck,
    /// Everything checked, but the patched root is not the agreed one: the import was incomplete.
    RootMismatch { got: Hash },
}

// ---- the trait -----------------------------------------------------------------------------------------------------

/// S2. Object-safe, so the harness can wrap it (`Recorder<dyn Replica>`); a constructor from a world config and a
/// seed is pe-replica's, and deterministic.
pub trait Replica: Send {
    fn domain(&self) -> DomainId;

    /// The next frame to step: every frame below it is simulated.
    fn frame(&self) -> Frame;

    /// Re-simulate from the earliest changed frame, never below the replica's own floor (the last cut it finalized,
    /// which can be older than the line's newest final cut), then step new frames up to `present`, exclusive. Never
    /// waits for the network: an entry not held yet is simply absent. Never aborts on input.
    fn advance(&mut self, present: Frame, source: &mut dyn EntrySource) -> Advance;

    /// An entry's outcome at the frame it was last simulated.
    fn outcome(&self, entry: &EntryRef) -> Option<(Frame, Outcome)>;

    /// My root at the cut's frame with exactly the cut's entries in effect (divergence.md 3.1): what I vote with,
    /// and what I compare once the cut is final. If my prediction holds other entries below the cut's frame, the
    /// replica computes the root without them; its live state is unchanged.
    fn root_at_cut(&mut self, cut: &Cut, source: &mut dyn EntrySource) -> Result<Hash, RootError>;

    /// The source's final cut changed: compare my root at it with the quorum's. Agreed: the cut becomes the floor
    /// and `Cv`, and older state is released beyond what is kept. Diverged: the floor moves, `Cv` stays, my own root
    /// at the cut stays held (rung 2 diffs against it, through [`Replica::tree`]) until the repair ends, and rung 1
    /// is due.
    fn finalize(&mut self, cut: &FinalCut, source: &mut dyn EntrySource) -> Finalized;

    /// Repair rung 1 (divergence.md 4.1): restore `Cv`, verify it from its records and from the restored physics
    /// itself, replay to the cut with the final entries, and compare. Uses no network.
    fn replay_from_last_agreed(&mut self, to: &FinalCut, source: &mut dyn EntrySource) -> Rung1;

    /// A root this replica holds, for pe-world's tree calls: the floor, `Cv`, the final cuts within `T_keep`, my own
    /// root at a cut I diverged at until its repair ends, and the root I voted with at each cut not yet final, until
    /// that cut is final or superseded (a root split's reference serves it, divergence.md 5).
    fn tree(&self, root: &Hash) -> Option<Box<dyn Tree + '_>>;

    /// The canonical physics transfer of a held root: rung 2's and a joiner's donor side.
    fn physics_image(&self, root: &Hash) -> Option<PhysicsImage>;

    /// Rung 2's and a joiner's receiver: replace the group's state at the cut by the imported parts, each checked
    /// against the agreed root first, then replay to the present. Returns the root at the cut afterwards; when it is
    /// the agreed root, the cut is my new `Cv`. `group` is `Domain` in M1.
    fn import_at_cut(
        &mut self,
        group: &GroupSel,
        import: CutImport,
        source: &mut dyn EntrySource,
    ) -> Result<Hash, ImportError>;

    /// The newest published frame view (S5): published once per frame, after the replica reached the present, never
    /// during a re-simulation.
    fn view(&self) -> Arc<FrameView>;
}

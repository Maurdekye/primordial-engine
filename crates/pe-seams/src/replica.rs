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
//! a later `finalize` agrees, and my current root at each cut not yet final that I voted on. Merge bases are kept
//! apart (M2).
//!
//! **Only `finalize` moves `Cv` and the floor** (v1, after @design-reviewer's High on pe-diverge ff37b4fd). Repairs
//! change the live state only, so a root split's repair at a cut not yet final gives nothing up if that cut is
//! superseded.
//!
//! **The tree** (v1, with @world-model, world-model.md 6.2 r12, and @diverge-eng): upper levels by name, a domain's
//! objects trie by nibble prefix ([`TreePath`]), and [`Node::hash`], the one definition of every node's hash.
//!
//! **Provisional:** [`Reason`]'s variants (@world-model fills in its fields as pe-world is written); [`PhysicsImage`]
//! as one blob (chunking by hash comes with the Bulk lane). **Not yet in the trait:** the M2 calls for the merge and
//! background replay (a fork at a frame, per-group restore and step, the per-frame footprint).

use core::time::Duration;
use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;

use crate::agree::{Cut, FinalCut};
use crate::entry::{DomainId, SignedEntry};
use crate::ids::{EntryRef, Frame, Hash, Hasher, MemberKey, ObjectId};
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
    /// Op `op` changed nothing: it removed a component instance that was already gone (world-model.md 7.6).
    NoEffect { op: u32 },
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
    /// An op would put an object deeper than the world's depth limit (world-model.md 3.3).
    TooDeep { object: ObjectId, limit: u32 },
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
    /// ([`EntrySource::take_changed_from`]), and the replica re-simulates them, down to its own floor. S3 reports a
    /// cut here only once this member holds every entry it names, void ones included ([`crate::agree`],
    /// "Dissemination before finality" and "No cut without its entries").
    fn final_cut(&self) -> Option<&FinalCut>;
    /// The entries to simulate at `frame`, in agreed order: final ones, then the predicted rest. Entries voided by a
    /// final cut are absent, and so are pending ones: stamped below the final cut's frame, and in no final cut yet
    /// (S3 v1.1).
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
    /// Whether it reached `present` within its budget ([`Replica::set_budget`]). When false it resumes on the next
    /// call, and [`Replica::view`] keeps the last view published at the present.
    pub caught_up: bool,
}

/// What [`Replica::finalize`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finalized {
    /// My root at the cut is the quorum's: the cut is my new floor and my `Cv`.
    Agreed,
    /// My root differs: I diverged at this cut. The floor moves to it (its entries are final), `Cv` stays, and a
    /// repair is due. After the repair, the driver calls [`Replica::finalize`] again for the same cut.
    Diverged { mine: Hash },
    /// I have not yet re-simulated to the cut's frame within my budget: nothing compared, and the floor stays. The
    /// driver calls again on a later tick. (Missing entries are not a case: the driver passes
    /// [`EntrySource::final_cut`], which S3 reports only once this member holds every entry the cut names.)
    NotReady,
}

/// Why [`Replica::root_at_cut`] or [`Replica::replay_to`] has no root.
///
/// There is no "missing entries" case: the d1 rule is S3's alone ([`crate::Agreement::wants_roots`] and
/// [`EntrySource::final_cut`] hand over only cuts whose entries the member holds). The replica cannot check it, since a
/// cut may cover void entries that [`EntrySource::entries_at`] rightly leaves out (@coordinator-opus, m8126544ad530).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootError {
    /// The state needed is no longer kept: the cut's frame is below my floor, or (for a replay) `Cv`'s state is gone.
    NotKept,
    /// The cut's frame is past what I have simulated.
    NotYetSimulated,
    /// The saved state at `Cv` failed its full rehash, from its records or from the restored physics itself: rung 2
    /// is due at once.
    Damaged,
    /// The work did not fit in this call's budget ([`Replica::set_budget`]) and is under way: call again with the
    /// same cut on a later tick.
    Pending,
}

// ---- the tree ------------------------------------------------------------------------------------------------------

/// Up to 32 nibbles of an object id, most significant first: a prefix in a domain's objects trie, so prefix order is
/// id order. Each nibble is below 16.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Nibbles(Vec<u8>);

impl Nibbles {
    /// An object id has 32 nibbles.
    pub const MAX: usize = 32;

    /// `None` if a nibble is 16 or more, or there are more than 32.
    pub fn new(nibbles: Vec<u8>) -> Option<Nibbles> {
        (nibbles.len() <= Self::MAX && nibbles.iter().all(|&n| n < 16)).then_some(Nibbles(nibbles))
    }

    /// All 32 nibbles of an id.
    pub fn of_id(id: ObjectId) -> Nibbles {
        let v = id.to_u128();
        Nibbles((0..32).map(|i| ((v >> (124 - 4 * i)) & 0xf) as u8).collect())
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn starts_with(&self, prefix: &Nibbles) -> bool {
        self.0.starts_with(&prefix.0)
    }

    /// `self` followed by one more nibble; `None` past 32 nibbles or for a nibble of 16 or more.
    pub fn child(&self, nibble: u8) -> Option<Nibbles> {
        let mut v = self.0.clone();
        v.push(nibble);
        Nibbles::new(v)
    }

    /// Two nibbles per byte, the first in the high half; when the count is odd, the last byte's low half is zero.
    pub fn packed(&self) -> Vec<u8> {
        self.0.chunks(2).map(|c| (c[0] << 4) | c.get(1).copied().unwrap_or(0)).collect()
    }
}

/// Where a node is in a state tree (world-model.md 6.2, r12). The upper levels are named; below a domain's root,
/// the objects trie is addressed by nibble prefix, so two tries of different shapes are compared prefix by prefix.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TreePath {
    /// The world root: its children are the tables root and each domain's root.
    World,
    /// The tables root: its children are the tables, each a leaf.
    Tables,
    /// A domain's root: its children are its objects trie and its `physics_global`.
    Domain(DomainId),
    /// The node of a domain's objects trie that covers this prefix: the shallowest node whose own prefix starts with
    /// it, passing through path-compressed segments, or `Node::Empty` if no object id there starts with it.
    Objects { domain: DomainId, prefix: Nibbles },
}

/// What a leaf is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LeafKey {
    Object(ObjectId),
    /// A table of the tables root, by its index (world-model.md 6.2).
    Table(u8),
    /// A domain's `physics_global`.
    Physics(DomainId),
}

/// One node of a state tree, as [`Tree::node`] returns it. [`Node::hash`] is the one definition of every node's
/// hash: pe-world builds its tree with it, and pe-diverge checks each node a peer sends against the hash its parent
/// named, starting from the agreed root, so a lying source is caught at its first lie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// The world root.
    World { frame: Frame, config: Hash, tables: Hash, domains: Vec<(DomainId, Hash)> },
    /// The tables root: each table's sorted-map hash, in table order.
    Tables(Vec<Hash>),
    /// A domain's root.
    Domain { domain: DomainId, objects: Hash, physics: Hash },
    /// An inner node of an objects trie: its full prefix (its depth is the prefix's length; under a path-compressed
    /// segment it is longer than the prefix asked for) and its present children by next nibble, in nibble order.
    Inner { prefix: Nibbles, children: Vec<(u8, Hash)> },
    /// An object's leaf: its id and its object leaf hash (pe-world's hash of the object's record).
    Leaf { id: ObjectId, object_hash: Hash },
    /// A subtree with no objects.
    Empty,
}

impl Node {
    /// The node's hash (world-model.md 6.2; the trie formulas are @world-model's, m02fcb0bd43e8):
    /// - `World`: `BLAKE3-derive("pe.root") ‖ frame u64 ‖ config ‖ tables ‖ the count of domains u64 ‖ each
    ///   domain's root, in domain-id order`;
    /// - `Tables`: `BLAKE3-derive("pe.tables") ‖ the count u64 ‖ each table's hash, in table order`;
    /// - `Domain`: `BLAKE3-derive("pe.domain") ‖ domain id u64 ‖ objects root ‖ physics_global`;
    /// - `Inner`: `BLAKE3-derive("pe.node") ‖ depth u8 ‖ the prefix packed two nibbles per byte (ceil(depth / 2)
    ///   bytes, the low half zero when the depth is odd) ‖ the child mask u16 ‖ the present children's hashes, in
    ///   nibble order`;
    /// - `Leaf`: `BLAKE3-derive("pe.leaf") ‖ id as u128 ‖ object leaf hash`;
    /// - `Empty`: `BLAKE3-derive("pe.empty")` of no bytes.
    ///
    /// Integers are little-endian. `None` when the node is malformed, so a peer's bad node is a fault, never a panic:
    /// domains not in strictly increasing id order; an inner node at depth 32, with no children, or with a child
    /// nibble of 16 or more or out of strictly increasing order.
    pub fn hash(&self) -> Option<Hash> {
        let h = match self {
            Node::World { frame, config, tables, domains } => {
                if domains.windows(2).any(|w| w[0].0 >= w[1].0) {
                    return None;
                }
                let mut h = Hasher::new("pe.root");
                h.write_u64(frame.0).write_hash(config).write_hash(tables).write_u64(domains.len() as u64);
                for (_, root) in domains {
                    h.write_hash(root);
                }
                h.finish()
            }
            Node::Tables(tables) => {
                let mut h = Hasher::new("pe.tables");
                h.write_u64(tables.len() as u64);
                for t in tables {
                    h.write_hash(t);
                }
                h.finish()
            }
            Node::Domain { domain, objects, physics } => {
                let mut h = Hasher::new("pe.domain");
                h.write_u64(domain.0).write_hash(objects).write_hash(physics);
                h.finish()
            }
            Node::Inner { prefix, children } => {
                if prefix.len() >= Nibbles::MAX
                    || children.is_empty()
                    || children.iter().any(|&(n, _)| n >= 16)
                    || children.windows(2).any(|w| w[0].0 >= w[1].0)
                {
                    return None;
                }
                let mask = children.iter().fold(0u16, |m, &(n, _)| m | (1 << n));
                let mut h = Hasher::new("pe.node");
                h.write_u8(prefix.len() as u8).write(&prefix.packed()).write(&mask.to_le_bytes());
                for (_, child) in children {
                    h.write_hash(child);
                }
                h.finish()
            }
            Node::Leaf { id, object_hash } => {
                let mut h = Hasher::new("pe.leaf");
                h.write_u128(id.to_u128()).write_hash(object_hash);
                h.finish()
            }
            Node::Empty => Hash::of("pe.empty", &[]),
        };
        Some(h)
    }
}

/// A leaf's record with its proof, as [`Tree::export`] returns it: the receiver checks every record against the
/// target root before applying anything, never against the sender's claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub key: LeafKey,
    /// The record's canonical bytes (pe-world's).
    pub bytes: Bytes,
    /// The nodes from the root down to the leaf's parent; the leaf itself is rebuilt from `key` and `bytes`.
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
    /// The node at, or for an objects prefix covering, the path; `None` only for a domain this root does not have.
    fn node(&self, path: &TreePath) -> Option<Node>;
    /// The records of these leaves (objects and tables), with their proofs. A leaf absent here is absent from the
    /// result; a verified node proves its absence. A `LeafKey::Physics` key gives no record: the physics travels as
    /// the whole image ([`Replica::physics_image`]).
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

/// What rung 2 or a joiner imports at a cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutImport {
    /// The cut, with its entries: the imported state is the state at its frame with exactly those entries.
    pub cut: Cut,
    /// The root every record is checked against: a final cut's quorum root, or a root split's reference root.
    pub target_root: Hash,
    pub records: Vec<Record>,
    /// Leaves to remove, each proved absent by a verified node.
    pub absent: Vec<LeafKey>,
    /// The whole physics image, when bound bodies are in the import.
    pub physics: Option<PhysicsImage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportError {
    /// The state at the cut is no longer kept.
    NotKept,
    /// A bound body can be restored only with the whole physics image: the fork offers no exact per-body setter
    /// (@vdw-physics; world-model.md 6.5).
    NeedsPhysicsImage(ObjectId),
    /// The record at this index does not check against the target root.
    BadProof(usize),
    /// The physics image's check hash is not the target root's `physics_global`.
    PhysicsCheck,
    /// Everything checked, but the patched root is not the target: the import was incomplete.
    RootMismatch { got: Hash },
    /// The re-simulation to the present did not fit in this call's budget and is under way: call again with the same
    /// import on a later tick.
    Pending,
}

// ---- the trait -----------------------------------------------------------------------------------------------------

/// S2. Object-safe, so the harness can wrap it (`Recorder<dyn Replica>`); a constructor from a world config and a
/// seed is pe-replica's, and deterministic.
///
/// **Only [`Replica::finalize`] moves `Cv` and the floor.** A repair ([`Replica::replay_to`],
/// [`Replica::import_at_cut`]) changes the live state only, so it may run at a cut that is not final, as in a root
/// split; if that cut is then superseded, nothing below it was given up. When the repaired cut is final, the driver
/// calls `finalize` for it again, which moves `Cv` once my root matches.
///
/// **Bounded work per call** (v1.1; @design-reviewer on docs/spec/app.md, mf5074c5ab2fd). A deep rollback, or a
/// repair's replay of up to `W_fg`, would otherwise stall the simulation thread, and agreement with it, for hundreds
/// of milliseconds. So the replica re-simulates at most its budget per call ([`Replica::set_budget`], simulated time,
/// so the harness stays deterministic) and resumes on the next call. While a replay or an import is pending, `advance`
/// continues it first. Until the replica reaches the present again, [`Replica::view`] keeps the last view it published
/// there, so the display draws (and extrapolates) that one. The results never depend on how the work was split.
pub trait Replica: Send {
    fn domain(&self) -> DomainId;

    /// The next frame to step: every frame below it is simulated.
    fn frame(&self) -> Frame;

    /// The most simulated time one call may re-simulate or step: a local setting, never world state, which pe-app may
    /// adjust from the measured step cost. It changes how fast the replica catches up, never what it computes. It is
    /// never below two frames at the world's rate: a smaller value is raised to that floor, because real time adds a
    /// frame per call and a budget of one frame would never catch up (@design-reviewer's Low on 549f4ca4).
    fn set_budget(&mut self, simulated: Duration);

    /// Re-simulate from the earliest changed frame, never below the replica's own floor (the last cut it finalized,
    /// which can be older than the line's newest final cut), then step new frames up to `present`, exclusive, within
    /// the budget ([`Advance::caught_up`]). Never waits for the network: an entry not held yet is simply absent. Never
    /// aborts on input.
    fn advance(&mut self, present: Frame, source: &mut dyn EntrySource) -> Advance;

    /// An entry's outcome at the frame it was last simulated.
    fn outcome(&self, entry: &EntryRef) -> Option<(Frame, Outcome)>;

    /// My root at the cut's frame with exactly the cut's entries in effect (divergence.md 3.1): what I vote with,
    /// and what I compare once the cut is final. If my prediction holds other entries below the cut's frame, the
    /// replica computes the root without them; its live state is unchanged.
    fn root_at_cut(&mut self, cut: &Cut, source: &mut dyn EntrySource) -> Result<Hash, RootError>;

    /// The source's final cut changed, or a repair at that cut ended: compare my root at it with the quorum's. The
    /// only call that moves `Cv` and the floor. Agreed: the cut becomes the floor and `Cv`, and older state is
    /// released beyond what is kept. Diverged: the floor moves, `Cv` stays, and my own root at the cut stays held
    /// (rung 2 diffs against it, through [`Replica::tree`]) until a later call agrees.
    fn finalize(&mut self, cut: &FinalCut, source: &mut dyn EntrySource) -> Finalized;

    /// Repair rung 1 (divergence.md 4.1), at a final cut or at a cut not final yet: restore `Cv`, verify it from its
    /// records and from the restored physics itself, replay to the cut with exactly its entries, and take my root
    /// there; then re-simulate on to the present with the source's entries, which becomes the live state whatever
    /// the root. Returns my root at the cut. Moves neither `Cv` nor the floor, and uses no network.
    fn replay_to(&mut self, cut: &Cut, source: &mut dyn EntrySource) -> Result<Hash, RootError>;

    /// A root this replica holds, for pe-world's tree calls: the floor, `Cv`, the final cuts within `T_keep`, my own
    /// root at a cut I diverged at until a later `finalize` agrees, and my current root at each cut not yet final
    /// that I voted on, until that cut is final or superseded (a root split's reference serves it, divergence.md 5).
    fn tree(&self, root: &Hash) -> Option<Box<dyn Tree + '_>>;

    /// The canonical physics transfer of a held root: rung 2's and a joiner's donor side.
    fn physics_image(&self, root: &Hash) -> Option<PhysicsImage>;

    /// Rung 2's and a joiner's receiver: replace the group's state at the cut by the imported parts, each checked
    /// against the target root before anything is applied, then re-simulate on to the present. Returns my root at
    /// the cut afterwards. Moves neither `Cv` nor the floor. `group` is `Domain` in M1.
    fn import_at_cut(
        &mut self,
        group: &GroupSel,
        import: CutImport,
        source: &mut dyn EntrySource,
    ) -> Result<Hash, ImportError>;

    /// The newest published frame view (S5): published once per frame, after the replica reached the present, never
    /// during a re-simulation or while catching up within the budget.
    fn view(&self) -> Arc<FrameView>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> Hash {
        Hash([b; 32])
    }

    #[test]
    fn nibbles_follow_the_id_from_its_top() {
        let id = ObjectId::from_u128(0xabc0_0000_0000_0000_0000_0000_0000_0001);
        let n = Nibbles::of_id(id);
        assert_eq!(n.len(), 32);
        assert_eq!(&n.as_slice()[..3], &[0xa, 0xb, 0xc]);
        assert_eq!(n.as_slice()[31], 1);
        let p = Nibbles::new(vec![0xa, 0xb]).unwrap();
        assert!(n.starts_with(&p));
        assert_eq!(p.child(0xc).unwrap().packed(), vec![0xab, 0xc0]);
        assert_eq!(Nibbles::new(vec![16]), None);
        assert_eq!(n.child(0), None);
    }

    #[test]
    fn the_inner_hash_binds_depth_prefix_mask_and_children() {
        let prefix = Nibbles::new(vec![0x3, 0x7, 0x1]).unwrap();
        let node = Node::Inner { prefix: prefix.clone(), children: vec![(0x2, h(1)), (0xf, h(2))] };
        let mut expect = Hasher::new("pe.node");
        expect.write_u8(3).write(&[0x37, 0x10]).write(&0x8004u16.to_le_bytes()).write_hash(&h(1)).write_hash(&h(2));
        assert_eq!(node.hash(), Some(expect.finish()));
        // The same children under another prefix of the same depth hash differently: a subtree cannot be relabelled.
        let moved = Node::Inner {
            prefix: Nibbles::new(vec![0x3, 0x7, 0x2]).unwrap(),
            children: vec![(0x2, h(1)), (0xf, h(2))],
        };
        assert_ne!(moved.hash(), node.hash());
    }

    #[test]
    fn leaf_empty_and_upper_hashes() {
        let id = ObjectId::from_u128(5);
        let mut expect = Hasher::new("pe.leaf");
        expect.write_u128(5).write_hash(&h(9));
        assert_eq!(Node::Leaf { id, object_hash: h(9) }.hash(), Some(expect.finish()));
        assert_eq!(Node::Empty.hash(), Some(Hash::of("pe.empty", &[])));
        let world = Node::World { frame: Frame(9), config: h(1), tables: h(2), domains: vec![(DomainId(0), h(3))] };
        let mut expect = Hasher::new("pe.root");
        expect.write_u64(9).write_hash(&h(1)).write_hash(&h(2)).write_u64(1).write_hash(&h(3));
        assert_eq!(world.hash(), Some(expect.finish()));
        let domain = Node::Domain { domain: DomainId(4), objects: h(5), physics: h(6) };
        let mut expect = Hasher::new("pe.domain");
        expect.write_u64(4).write_hash(&h(5)).write_hash(&h(6));
        assert_eq!(domain.hash(), Some(expect.finish()));
    }

    #[test]
    fn malformed_nodes_have_no_hash() {
        let p = Nibbles::new(vec![1]).unwrap();
        assert_eq!(Node::Inner { prefix: p.clone(), children: vec![] }.hash(), None);
        assert_eq!(Node::Inner { prefix: p.clone(), children: vec![(16, h(1))] }.hash(), None);
        assert_eq!(Node::Inner { prefix: p.clone(), children: vec![(3, h(1)), (3, h(2))] }.hash(), None);
        assert_eq!(Node::Inner { prefix: p, children: vec![(4, h(1)), (3, h(2))] }.hash(), None);
        let deep = Nibbles::of_id(ObjectId::from_u128(1));
        assert_eq!(Node::Inner { prefix: deep, children: vec![(0, h(1))] }.hash(), None);
        let unsorted = Node::World {
            frame: Frame(0),
            config: h(0),
            tables: h(0),
            domains: vec![(DomainId(2), h(1)), (DomainId(1), h(2))],
        };
        assert_eq!(unsorted.hash(), None);
    }
}

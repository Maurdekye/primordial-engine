//! Applying an edit entry (world-model.md 7): the one edit path. The ordered step, the author's prediction and a dry
//! run all call [`World::apply_edit`], so they cannot disagree (7.6).
//!
//! The checks run in 7.6's order: decoding, the actor, `EDIT`, the preconditions against the state before the
//! transaction, then each op in order on a copy-on-write working state, which replaces the world only if every op
//! applies. Protections (7.8) and holds (7.2) are checked at each op, against the state just before it, since an op
//! may name an object an earlier op of the same transaction created. The reported reason is the first failing op's.

use std::collections::BTreeMap;
use std::sync::Arc;

use pe_core::{EntryRef, Frame, Hash, MemberKey, ObjectId};
use pe_seams::{Class, FieldPath, Note, Outcome, Reason, SignedEntry, Version};
use pe_sim::{ComponentId, Node, RowVersion, Transform};

use crate::components::{Light, Protection, SimSize, ids};
use crate::edit::{
    Actor, AssetTarget, MAX_NEW_OBJECTS, OWN_CHECK, Op, Precondition, Row, Target, Transaction, content_hash, row_tag,
};
use crate::script::{
    AssetId, BuildFailure, MAX_REKEY_ROWS, MAX_SOURCE_BYTES, ModuleRow, PendingCommit, RekeyReport, VoidKind,
    asset_created, attest_window, check_source,
};
use crate::world::{DEPTH_LIMIT_MAX, GENESIS, TOMBSTONE_SECONDS, Tombstone, World, WorldError, depth, perm};

/// What an applied entry touched, for the replica's physics effects (9.2) and the caches. Empty for a refusal.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Footprint {
    /// Created, in op order.
    pub created: Vec<ObjectId>,
    /// Deleted (every object of each deleted subtree), in op order.
    pub removed: Vec<ObjectId>,
    /// Rows written or removed by edits, in op order (an object and component may repeat).
    pub rows: Vec<(ObjectId, ComponentId)>,
    /// Reparented, in op order, each with whether its world pose was kept (`keep_world`, or forced by an orphaning,
    /// 7.5); if not, its local transform was kept and its world pose moved.
    pub reparented: Vec<(ObjectId, bool)>,
    /// Pushes an input applied (8.1), with their author.
    pub pushes: Vec<Push>,
    /// Hands whose target and `Hand` row an input wrote (8.1).
    pub hands: Vec<ObjectId>,
    /// Player bodies whose `Player` row a `Move` input wrote (8.2).
    pub players: Vec<ObjectId>,
    /// Throws an input applied (8.1's throw), with their author: each ended its author's claim, in op order.
    pub throws: Vec<Thrown>,
    /// Objects an input's `Grab` began holding: a new claim, not a renewal of the author's own (8.1). With pushes and
    /// throws, what the author's own check after an interaction watches (replica.md 3.4).
    pub grabs: Vec<ObjectId>,
    /// Objects whose subtree the edit made inert or live: roots whose `Name` it wrote or removed (a template folder's
    /// name, [`crate::templates`]) and objects that gained or lost a `Folder` row, in id order: every body under each
    /// is affected, as under a reparent.
    pub regrouped: Vec<ObjectId>,
    /// What a script commit's re-key did (7.9): the objects, the row count and the fields zeroed or dropped. Always
    /// `None` for an entry since logic.md 6.2a: the re-key happens at activation, and [`World::end_frame`] reports it.
    pub rekey: Option<RekeyReport>,
}

/// An applied throw (8.1): the velocity its author's hand gave the object as it let go, in the world frame, `linear`
/// at its centre of mass in m/s and `angular` in rad/s. pe-world checks only that the author held the object and that
/// the values are finite; pe-replica caps both and applies them once the hold's joint is gone, if the object was with
/// the hand (8.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thrown {
    pub object: ObjectId,
    pub linear: [f32; 3],
    pub angular: [f32; 3],
    pub author: MemberKey,
}

/// An applied push (8.1): an impulse in newton seconds, in the world frame, at `at` or the centre of mass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Push {
    pub object: ObjectId,
    pub impulse: [f32; 3],
    pub at: Option<pe_core::Pos>,
    pub author: MemberKey,
}

/// Where a transaction that is not signed yet would come from (a dry run, api.md 9.2): its author and the
/// coordinates its new ids and row tags derive from (2.1, 7.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Draft {
    pub author: MemberKey,
    pub line: pe_core::LineId,
    pub chain: pe_core::ChainId,
    pub seq: u64,
}

impl Draft {
    /// The stand-in entry id a dry run's versions name: `BLAKE3("pe.draft" ‖ line ‖ author ‖ chain ‖ seq ‖ payload)`.
    /// It is never a real entry's id.
    pub fn entry(&self, payload: &[u8]) -> EntryRef {
        let mut h = pe_core::commit::Hasher::new("pe.draft");
        h.write_hash(&self.line.0).write(&self.author.0).write_hash(&self.chain.0).write_u64(self.seq);
        h.write_len_prefixed(payload);
        EntryRef(h.finish())
    }
}

/// The facts of an entry the edit path uses.
struct Source {
    entry: EntryRef,
    author: MemberKey,
    line: pe_core::LineId,
    chain: pe_core::ChainId,
    seq: u64,
}

/// A target after resolution.
enum Res {
    Live(ObjectId),
    Gone { id: ObjectId, by: EntryRef },
    Root,
}

/// The entry's facts every op needs, and what the transaction has done so far.
struct Cx {
    entry: EntryRef,
    author: MemberKey,
    line: pe_core::LineId,
    chain: pe_core::ChainId,
    seq: u64,
    frame: Frame,
    /// The transaction's `New` indices and the ids they took.
    new: BTreeMap<u32, ObjectId>,
    created: Vec<ObjectId>,
    overwrote: Vec<Version>,
    notes: Vec<Note>,
    fp: Footprint,
    /// The inverse's ops, in the order the changes were made (reversed at the end), and its `Version`
    /// preconditions: the last version this transaction set on each row it wrote (7.7).
    undo: Vec<Op>,
    undo_pre: BTreeMap<(ObjectId, ComponentId), Version>,
    /// The transaction's one `CommitScript`, if any: its asset and version hash, for `Register`'s check (7.9).
    script: Option<(AssetId, Hash)>,
    /// The descriptions its `Register` registered.
    registered: Option<Vec<pe_sim::ComponentDesc>>,
}

impl Cx {
    /// The instance tag of a row op `i` adds (7.6).
    fn tag(&self, i: u32) -> [u8; 16] {
        row_tag(self.line, self.author, self.chain, self.seq, i).to_le_bytes()
    }

    /// Objects that are gone after this op can carry no precondition in the inverse.
    fn forget(&mut self, gone: &[ObjectId]) {
        self.undo_pre.retain(|(id, _), _| !gone.contains(id));
    }
}

type Refusal = (Option<u32>, Reason);

fn invalid() -> Reason {
    Reason::Invalid { field: None }
}

fn invalid_field(c: ComponentId, field: u32) -> Reason {
    Reason::Invalid { field: Some(FieldPath { component: c.0, field }) }
}

/// The transaction would create more than [`MAX_NEW_OBJECTS`].
fn too_large() -> Reason {
    Reason::TooLarge { limit: MAX_NEW_OBJECTS as u32 }
}

/// Refuses the op if the transaction would then have created more than [`MAX_NEW_OBJECTS`].
fn within_cap(cx: &Cx, more: usize) -> Result<(), Reason> {
    if cx.created.len().saturating_add(more) > MAX_NEW_OBJECTS { Err(too_large()) } else { Ok(()) }
}

fn too_deep(object: ObjectId, limit: u32) -> Reason {
    Reason::TooDeep { object, limit }
}

/// Op `i` changed nothing (7.6): a `RemoveRow` of an instance already gone.
fn no_effect(cx: &mut Cx, i: u32) {
    cx.notes.push(Note::NoEffect { op: i });
}

/// Whether an undo op writes or removes row `c` of `id`.
fn row_undo(op: &Op, id: ObjectId, c: ComponentId) -> bool {
    match op {
        Op::SetRow { id: Target::Existing(x), row } => *x == id && row.component == c,
        Op::RemoveRow { id: Target::Existing(x), component, .. } => *x == id && *component == c,
        _ => false,
    }
}

fn version(entry: EntryRef, op: u32) -> Version {
    Version { entry, op }
}

fn from_row_version(v: RowVersion) -> Version {
    Version { entry: EntryRef(Hash(v.entry)), op: v.op }
}

/// Components no edit may write (7.6 step 4): the hierarchy, derived physics state and hands (inputs).
fn edit_writable(c: ComponentId) -> bool {
    c != ComponentId::NODE && c != ids::BODY_STATE && c != ids::HAND && c != ids::PLAYER
}

/// A hand's own rows an edit can't write or remove, though another object's it may: the ones that place it or make
/// it a body ([`held_rows`]: `Transform`, `Body`, `Shape`), so neither an edit nor physics ever moves a hand. Hands
/// are logic's alone, made by `admit`, moved by their member's input and removed by the leave (7.5; @coordinator-opus, widened after @chunks-reviewer's Medium).
fn hand_only(c: ComponentId) -> bool {
    held_rows(c)
}

/// What [`World::end_frame`] did to scripts (logic.md 6.2a), in asset order, each commit named by its asset and its
/// version (the commit's entry and op): the commits activated, with their re-key's report, and the commits voided,
/// with why. The report is `None` when no re-key was needed, so no object changed: a script's first version (no
/// rows to move) or a same-source commit (a toolchain or SDK change, the same components). A void's [`Blocked`] is
/// `Some` only for `RekeyBlockedAtActivation`, naming the objects as the commit-time refusal does (@engine-architect). pe-replica publishes them as S5's `FrameView.activations` (@coordinator-opus).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameEnd {
    pub activated: Vec<(AssetId, Version, Option<RekeyReport>)>,
    pub voided: Vec<Voided>,
}

/// What activating one pending commit gave (logic.md 6.2a): its re-key's report (`None` when nothing re-keyed), or
/// why it is voided instead and, for a blocked re-key, what blocked it.
pub type ActivationResult = Result<Option<RekeyReport>, (VoidKind, Option<Blocked>)>;

/// One pending commit [`World::end_frame`] voided: its asset and version, why, the objects that blocked it (only for
/// `RekeyBlockedAtActivation`), and who could not build it and why, in key order (Q1 (c)'s notice to the author; S5's
/// `Voided.failed`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Voided {
    pub asset: AssetId,
    pub version: Version,
    pub kind: VoidKind,
    pub blocked: Option<Blocked>,
    pub failed: Vec<(MemberKey, BuildFailure)>,
}

/// The objects that block a re-key (7.9), in id order: the first [`REKEY_BLOCKED_NAMED`], and how many more. One
/// helper for both reports of a block, `Reason::RekeyBlocked` at the commit and a `RekeyBlockedAtActivation` void,
/// so the cap and the order can't drift apart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Blocked {
    pub objects: Vec<ObjectId>,
    pub more: u32,
}

impl Blocked {
    /// `blocked` must be in id order, as [`RekeyReport::blocked`] is.
    pub fn of(blocked: &[ObjectId]) -> Self {
        let named = blocked.len().min(REKEY_BLOCKED_NAMED);
        let more = u32::try_from(blocked.len() - named).unwrap_or(u32::MAX);
        Blocked { objects: blocked[..named].to_vec(), more }
    }
}

/// `RekeyBlocked` for the objects `blocked` (in id order): the first [`REKEY_BLOCKED_NAMED`], and how many more.
fn rekey_blocked(blocked: &[ObjectId]) -> Reason {
    let Blocked { objects, more } = Blocked::of(blocked);
    Reason::RekeyBlocked { objects, more }
}

/// How many blocking objects a `RekeyBlocked` refusal names (7.9; @coordinator-opus).
pub const REKEY_BLOCKED_NAMED: usize = 16;

/// Rows a holder feels: refused while the object is held (7.2).
fn held_rows(c: ComponentId) -> bool {
    c == ComponentId::TRANSFORM || c == ids::BODY || c == ids::SHAPE
}

impl World {
    /// Applies one edit entry at `frame` (its place in the agreed order). Total: any entry gives an outcome, and a
    /// refused one changes nothing.
    pub fn apply_edit(&mut self, frame: Frame, entry: &SignedEntry) -> (Outcome, Footprint) {
        match self.try_apply(frame, entry) {
            Ok((outcome, fp)) => (outcome, fp),
            Err((op, reason)) => (Outcome::Refused { op, reason }, Footprint::default()),
        }
    }

    /// What [`World::apply_edit`] would give at the current state, changing nothing.
    pub fn dry_run(&self, frame: Frame, entry: &SignedEntry) -> Outcome {
        self.clone().apply_edit(frame, entry).0
    }

    /// What [`World::apply_edit`] would give `tx` from `draft`'s author and coordinates at the current state, before
    /// it is signed, changing nothing. The same path as the live one: the transaction is encoded and decoded first,
    /// so a limit refuses it as `Malformed` exactly as live. The entry id it would have is unknown, so versions in
    /// the outcome (`overwrote`, the inverse's preconditions) name a stand-in, [`Draft::entry`]; created ids and row
    /// tags are exact when `chain` and `seq` are the ones the signer will use.
    pub fn dry_run_draft(&self, frame: Frame, draft: &Draft, tx: &Transaction) -> Outcome {
        self.dry_run_draft_state(frame, draft, tx).0
    }

    /// [`World::dry_run_draft`], with the scratch world after it: unchanged if the draft is refused. Its versions
    /// name the draft's stand-in entry id.
    pub fn dry_run_draft_state(&self, frame: Frame, draft: &Draft, tx: &Transaction) -> (Outcome, World) {
        let payload = tx.encode();
        let src = Source {
            entry: draft.entry(&payload),
            author: draft.author,
            line: draft.line,
            chain: draft.chain,
            seq: draft.seq,
        };
        let mut w = self.clone();
        let outcome = match Transaction::decode(&payload) {
            None => Outcome::Refused { op: None, reason: Reason::Malformed },
            Some(tx) => match w.apply_tx(frame, &src, &tx) {
                Ok((outcome, _)) => outcome,
                Err((op, reason)) => Outcome::Refused { op, reason },
            },
        };
        (outcome, w)
    }

    fn try_apply(&mut self, frame: Frame, entry: &SignedEntry) -> Result<(Outcome, Footprint), Refusal> {
        let e = &entry.entry;
        if e.class != Class::Edit {
            return Err((None, Reason::Malformed));
        }
        let (tx, origin) = crate::branch::decode_edit(entry).ok_or((None, Reason::Malformed))?;
        // A re-issued edit (F3's n = 2 branch, [`crate::branch`]): its outermost reference must name a sealed original
        // of the branch this world continues from, by the same author, with exactly the bytes it re-issues. Its ids,
        // row tags and new asset ids derive from its innermost original's line, chain and seq, so they are the ids
        // the original made, and its versions name that original (@diverge-eng's M1), so a later re-issued edit's
        // `Version` and `ObjectVersion` preconditions, read on the parent line, hold.
        let reissue = crate::branch::split_reissue(&e.payload);
        if let Some((r, inner)) = &reissue
            && !self.branch.admits(e.author, r, inner)
        {
            return Err((None, invalid()));
        }
        let src =
            Source { entry: origin.entry, author: e.author, line: origin.line, chain: origin.chain, seq: origin.seq };
        let applied = self.apply_tx(frame, &src, &tx)?;
        // Each original is re-issued once: an applied re-issue takes it out of the record. A refused one changes
        // nothing, so the original may be re-issued again.
        if let Some((r, _)) = reissue {
            Arc::make_mut(&mut self.branch).sealed.remove(&(r.chain, r.seq));
        }
        Ok(applied)
    }

    /// The one edit path after decoding, for a signed entry and a draft alike.
    fn apply_tx(&mut self, frame: Frame, src: &Source, tx: &Transaction) -> Result<(Outcome, Footprint), Refusal> {
        if let Actor::Agent { principal, requested_by, .. } = &tx.actor
            && (*principal != src.author || requested_by.is_some())
        {
            return Err((None, invalid()));
        }
        // An attestation or a failed build (logic.md 6.2a) stands alone and needs no `EDIT`, only a voter's place on
        // the pending commit (or, for a failure, a member's on the current version), which its op checks.
        let attest = tx.ops.iter().any(|o| matches!(o, Op::AttestBuild { .. } | Op::BuildFailed { .. }));
        if attest && (tx.ops.len() != 1 || !tx.preconditions.is_empty()) {
            return Err((None, invalid()));
        }
        if !attest && !self.permits(src.author, perm::EDIT) {
            return Err((None, Reason::NoPermission));
        }
        for (i, p) in tx.preconditions.iter().enumerate() {
            self.check(i as u32, p).map_err(|r| (None, r))?;
        }
        // At most one `CommitScript`, and a `Register` only before it (7.9).
        let commit = tx.ops.iter().position(|o| matches!(o, Op::CommitScript { .. }));
        let register = tx.ops.iter().position(|o| matches!(o, Op::Register { .. }));
        let commits = tx.ops.iter().filter(|o| matches!(o, Op::CommitScript { .. })).count();
        let registers = tx.ops.iter().filter(|o| matches!(o, Op::Register { .. })).count();
        if commits > 1 || registers > 1 || register.is_some_and(|r| commit.is_none_or(|c| r > c)) {
            return Err((None, invalid()));
        }
        // The asset it commits to: an existing one, or a new one's id derived from this entry (7.9).
        let script = commit.and_then(|c| match &tx.ops[c] {
            Op::CommitScript { script, source, .. } => {
                let asset = match *script {
                    AssetTarget::Existing(a) => a,
                    AssetTarget::New(k) => asset_created(src.line, src.author, src.chain, src.seq, k),
                };
                Some((asset, Hash(source.hash)))
            }
            _ => None,
        });
        // The commit's source before any op, so its `Register` is judged against a valid version (logic.md 4.1a;
        // @coordinator-opus): its size from the length alone, then its form, on the bytes the entry
        // carries. A malformed one is refused, and the old version keeps running.
        if let Some(c) = commit
            && let Op::CommitScript { source, archive, .. } = &tx.ops[c]
        {
            let c = c as u32;
            if source.len > u64::from(MAX_SOURCE_BYTES) {
                return Err((Some(c), Reason::TooLarge { limit: MAX_SOURCE_BYTES }));
            }
            check_source(archive).map_err(|_| (Some(c), invalid()))?;
        }
        let mut cx = Cx {
            entry: src.entry,
            author: src.author,
            line: src.line,
            chain: src.chain,
            seq: src.seq,
            frame,
            new: BTreeMap::new(),
            created: Vec::new(),
            overwrote: Vec::new(),
            notes: Vec::new(),
            fp: Footprint::default(),
            undo: Vec::new(),
            undo_pre: BTreeMap::new(),
            script,
            registered: None,
        };
        let mut work = self.clone();
        for (i, op) in tx.ops.iter().enumerate() {
            let i = i as u32;
            work.op(&mut cx, i, op).map_err(|r| (Some(i), r))?;
        }
        // The inline blobs against what the ops introduced, with the registry after them (3.4): each one introduced,
        // and each introduced one given, except in world logic's own drafts (genesis), which no machine receives.
        crate::inline::check(work.store.registry(), tx, &tx.inline, !src.author.is_logic()).map_err(|r| (None, r))?;
        // The record ceiling (6.6, R_max; @coordinator-opus): no edit leaves a record past
        // `MAX_RECORD_BYTES`, whether an object's (a row it wrote, or one it created) or the tombstones table's (one
        // that grows it, as the shape limit refuses only edits that add shapes: an undelete that shrinks a table
        // already past it, through a path outside edits, is never refused). Other tables grow with members and
        // scripts, each under its own limits.
        let limit = crate::tree::record_limit();
        let over = |n: usize| n > limit;
        let mut touched: Vec<ObjectId> =
            cx.fp.rows.iter().map(|&(id, _)| id).chain(cx.fp.created.iter().copied()).collect();
        touched.sort();
        touched.dedup();
        if touched.iter().any(|&id| crate::tree::record_len(&work.store, id).is_some_and(over))
            || (!Arc::ptr_eq(&self.tombstones, &work.tombstones) && {
                let now = crate::tree::tombstones_record_len(&work);
                // The table before is summed only when the table after is over: rare, and it keeps an edit that
                // swaps a small tombstone for a large one (an undelete and a delete) from slipping by on the count.
                over(now) && now > crate::tree::tombstones_record_len(self)
            })
        {
            return Err((None, Reason::TooLarge { limit: u32::try_from(limit).unwrap_or(u32::MAX) }));
        }
        // The world's limits (7.1): only an admin changes them (as governance), and never to 0 or past the ceiling, so
        // neither a guest nor a typo can wedge the world (@world-model). World logic sets them at
        // genesis.
        if work.store.registry().get(ids::LIMITS).is_some()
            && work.store.column_raw(ids::LIMITS) != self.store.column_raw(ids::LIMITS)
        {
            if !src.author.is_logic() && !self.permits(src.author, perm::ADMIN) {
                return Err((None, Reason::NoPermission));
            }
            let bad =
                |l: &crate::components::Limits| l.max_shapes == 0 || l.max_shapes > crate::components::SHAPES_CEILING;
            if work.store.iter::<crate::components::Limits>().any(|(_, l)| bad(l)) {
                return Err((None, invalid()));
            }
        }
        // The world's shape limit (7.1; 7.6 step 5), on the state after every op: never past `shape_limit` by an edit
        // that adds shapes. An edit that adds none (a delete, a colour, a move) passes even past it.
        // The limit is the world's after the edit (`shape_limit`), so one edit may raise it and add shapes together.
        let (had, count, max) = (self.store.count(ids::SHAPE), work.store.count(ids::SHAPE), work.shape_limit());
        if count > max && count > had {
            let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
            return Err((None, Reason::TooManyShapes { limit: n(max), count: n(count) }));
        }
        // Templates (`crate::templates`): an edit that makes a held object inert or live is refused, as an edit of a
        // held object's parent is (7.2); the first such claim in object order names the holder.
        for (&held, c) in self.claims.iter() {
            if work.store.exists(held) && self.inert(held) != work.inert(held) {
                return Err((None, Reason::Held { by: c.member }));
            }
        }
        // A root renamed to or from a template folder's name, or an object that gained or lost its `Folder` row: its
        // subtree's bodies are affected (a reparent's are already, through its subtree).
        let mut regrouped: Vec<ObjectId> = cx
            .fp
            .rows
            .iter()
            .filter(|&&(id, c)| {
                work.store.exists(id) && ((c == ids::NAME && work.store.parent(id).is_none()) || c == ids::FOLDER)
            })
            .map(|&(id, _)| id)
            .filter(|&id| self.marks_inert(id) != work.marks_inert(id))
            .collect();
        regrouped.sort();
        regrouped.dedup();
        cx.fp.regrouped = regrouped;
        *self = work;
        // An applied edit's inline blobs go into the machine's store, on every machine that applies it (3.4). A
        // refused one stores nothing.
        for b in &tx.inline {
            self.blobs().put(b);
        }
        // An applied commit's source goes into the machine's store, on every machine that applies it, so each can
        // build the version (5.3; @coordinator-opus). A refused edit stores nothing; what a rollback,
        // or a dry run on the author's machine, leaves behind is store garbage, pruned with the store.
        if let Some(c) = commit
            && let Op::CommitScript { archive, .. } = &tx.ops[c]
        {
            self.blobs().put(archive);
        }
        let inverse = Transaction {
            actor: tx.actor.clone(),
            label: None,
            preconditions: cx
                .undo_pre
                .iter()
                .map(|(&(id, component), &is)| Precondition::Version { id, component, instance: 0, is })
                .collect(),
            ops: cx.undo.into_iter().rev().collect(),
            blobs: Vec::new(),
            inline: Vec::new(),
        };
        // The inverse carries the bytes of the inline-class blobs its ops restore, from this machine's store (3.4).
        let mut inverse = inverse;
        let mut restored: Vec<Vec<u8>> = crate::inline::introduced(self.store.registry(), &inverse)
            .iter()
            .filter_map(|r| self.blobs().get(r).map(|b| b.to_vec()))
            .collect();
        restored.sort_by_key(|b| crate::components::BlobRef::of(b).hash);
        inverse.inline = restored;
        let outcome = Outcome::Applied {
            created: cx.created,
            overwrote: cx.overwrote,
            notes: cx.notes,
            inverse: Some(bytes::Bytes::from(inverse.encode())),
        };
        Ok((outcome, cx.fp))
    }

    // ---- preconditions -------------------------------------------------------------------------------------------

    /// Why `id` cannot be used: deleted (with a live tombstone) or unknown.
    fn missing(&self, id: ObjectId) -> Reason {
        match self.tombstones.get(&id) {
            Some(t) => Reason::Deleted { object: id, by: t.by },
            None => Reason::Unknown(id),
        }
    }

    fn check(&self, i: u32, p: &Precondition) -> Result<(), Reason> {
        let stale = |current: Version| Reason::Stale { precondition: i, current, winner: current.entry };
        match *p {
            Precondition::Exists(id) => {
                if !self.store.exists(id) {
                    return Err(self.missing(id));
                }
            }
            Precondition::Absent(id) => {
                if self.store.exists(id) {
                    return Err(Reason::Exists(id));
                }
            }
            Precondition::ObjectVersion { id, is } => {
                if !self.store.exists(id) {
                    return Err(self.missing(id));
                }
                let current = node_version(&self.store.node(id));
                if current != is {
                    return Err(stale(current));
                }
            }
            Precondition::Version { id, component, instance, is } => {
                if !self.store.exists(id) {
                    return Err(self.missing(id));
                }
                let current = self.current_row_version(id, component, instance);
                if current != is {
                    return Err(stale(current));
                }
            }
            Precondition::Content { id, component, instance, hash } => {
                if !self.store.exists(id) {
                    return Err(self.missing(id));
                }
                let same =
                    instance == 0 && self.store.get_raw(component, id).is_some_and(|row| content_hash(row) == hash);
                if !same {
                    return Err(stale(self.current_row_version(id, component, instance)));
                }
            }
        }
        Ok(())
    }

    /// A row's edit version; [`GENESIS`] if the row does not exist (or the instance is not 0, which M1 never has).
    fn current_row_version(&self, id: ObjectId, c: ComponentId, instance: u16) -> Version {
        if instance != 0 {
            return GENESIS;
        }
        self.store.row_version(c, id).map_or(GENESIS, from_row_version)
    }

    // ---- ops -----------------------------------------------------------------------------------------------------

    fn resolve(&self, cx: &Cx, t: Target) -> Result<Res, Reason> {
        let id = match t {
            Target::Root => return Ok(Res::Root),
            Target::Existing(id) => id,
            Target::New(k) => *cx.new.get(&k).ok_or_else(invalid)?,
        };
        if self.store.exists(id) {
            return Ok(Res::Live(id));
        }
        match self.tombstones.get(&id) {
            Some(t) => Ok(Res::Gone { id, by: t.by }),
            None => Err(Reason::Unknown(id)),
        }
    }

    /// A target that must be a live object (`SetRow`, `SetField`, `RemoveRow`, the moved object of `Reparent`).
    fn live(&self, cx: &Cx, t: Target) -> Result<ObjectId, Reason> {
        match self.resolve(cx, t)? {
            Res::Live(id) => Ok(id),
            Res::Gone { id, by } => Err(Reason::Deleted { object: id, by }),
            Res::Root => Err(invalid()),
        }
    }

    fn op(&mut self, cx: &mut Cx, i: u32, op: &Op) -> Result<(), Reason> {
        match op {
            Op::Create { new, parent, index, rows } => self.op_create(cx, i, *new, *parent, *index, rows),
            Op::Delete { id } => self.op_delete(cx, i, *id),
            Op::SetRow { id, row } => {
                let id = self.live(cx, *id)?;
                self.write_row(cx, i, id, row.component, row.instance, &row.bytes, false)
            }
            Op::SetField { id, component, instance, field, value } => {
                let id = self.live(cx, *id)?;
                let row = self.field_row(id, *component, *instance, *field, value)?;
                self.write_row(cx, i, id, *component, *instance, &row, false)
            }
            Op::RemoveRow { id, component, instance, tag } => {
                let id = self.live(cx, *id)?;
                self.op_remove_row(cx, i, id, *component, *instance, *tag)
            }
            Op::Reparent { id, parent, index, keep_world } => {
                let id = self.live(cx, *id)?;
                self.op_reparent(cx, i, id, *parent, *index, *keep_world)
            }
            Op::Copy { source, new, parent, index, size } => self.op_copy(cx, i, *source, *new, *parent, *index, *size),
            Op::Undelete { id } => self.op_undelete(cx, i, *id),
            Op::Register { components } => {
                let (asset, v) = cx.script.ok_or_else(invalid)?;
                self.register_script(&asset, &v, components)?;
                cx.registered = Some(components.clone());
                Ok(())
            }
            Op::CommitScript { script, base, source, toolchain, sdk, author_module, activate_at, .. } => {
                let row = ModuleRow {
                    version: version(cx.entry, i),
                    source: *source,
                    toolchain: *toolchain,
                    sdk: *sdk,
                    author_module: *author_module,
                    components: Vec::new(),
                    activate_at: Frame(*activate_at),
                    faulted: None,
                };
                // A new asset only by `New(k)`; `Existing` names one the world knows (current or pending), never a
                // creation (@coordinator-opus).
                let (asset, _) = cx.script.ok_or_else(invalid)?;
                if let AssetTarget::Existing(a) = script
                    && !self.modules.contains_key(a)
                    && !self.pending.contains_key(a)
                {
                    return Err(invalid());
                }
                self.op_commit(cx, i, asset, *base, row)
            }
            Op::AttestBuild { script, version, module } => self.op_attest(cx, *script, *version, *module),
            Op::BuildFailed { script, version, reason, module, .. } => {
                self.op_build_failed(cx, *script, *version, *reason, *module)
            }
        }
    }

    /// `AttestBuild` (logic.md 6.2a; 7.9): counts the author's attestation on the asset's pending commit. `Stale` if
    /// no commit is pending at that version (none, another V, activated or voided); `Invalid` for a non-voter, a
    /// voter's second attestation, or a module other than the registered `author_module`. A refused one changes
    /// nothing, and its signed entry stays in the log as the evidence. It supersedes the voter's own `BuildFailed`.
    fn op_attest(&mut self, cx: &mut Cx, script: AssetId, v: Hash, module: Hash) -> Result<(), Reason> {
        let stale = |current: Version| Reason::Stale { precondition: OWN_CHECK, current, winner: current.entry };
        let current = self.modules.get(&script).map_or(Version { entry: EntryRef(Hash::ZERO), op: 0 }, |m| m.version);
        let p = match self.pending.get(&script) {
            Some(p) if p.void.is_none() && p.v() == v => p,
            Some(p) if p.void.is_none() => return Err(stale(p.version)),
            _ => return Err(stale(current)),
        };
        let counted = p.attested.iter().any(|(k, _)| *k == cx.author);
        if !p.voters.contains(&cx.author) || counted || module != p.author_module {
            return Err(invalid());
        }
        let p = Arc::make_mut(&mut self.pending).get_mut(&script).expect("present");
        p.failed.retain(|(k, _, _)| *k != cx.author);
        let at = p.attested.partition_point(|(k, _)| k.0 < cx.author.0);
        p.attested.insert(at, (cx.author, cx.frame));
        Ok(())
    }

    /// `BuildFailed` (logic.md 6.2a item 2; 7.9): a voter's statement that its build of version `v` failed, after its
    /// local retries. On the pending commit at `v` it is counted, one per voter, against the quorum (the shared void
    /// rule at this frame's end); `Invalid` for a non-voter, a second failure, or a failure after the voter's counted
    /// attestation. For the asset's current version it is a record only, the log's statement that this member split
    /// off (6.2 step 3), and changes nothing; `Invalid` from a non-member. Otherwise `Stale`, as for `AttestBuild`:
    /// nothing changes a void. The hashes ride in the entry as evidence; the world keeps the reason. The evidence must
    /// agree with itself: `module` is present exactly for a `Mismatch`, the hash it built, which is not the one it
    /// failed to match; anything else is `Invalid` (@chunks-reviewer's L1).
    fn op_build_failed(
        &mut self,
        cx: &mut Cx,
        script: AssetId,
        v: Hash,
        why: BuildFailure,
        module: Option<Hash>,
    ) -> Result<(), Reason> {
        let stale = |current: Version| Reason::Stale { precondition: OWN_CHECK, current, winner: current.entry };
        if module.is_some() != (why == BuildFailure::Mismatch) {
            return Err(invalid());
        }
        let row = self.modules.get(&script);
        let matched = |against: Hash| module == Some(against);
        let current = row.map_or(Version { entry: EntryRef(Hash::ZERO), op: 0 }, |m| m.version);
        match self.pending.get(&script) {
            Some(p) if p.void.is_none() && p.v() == v => {
                let refused = !p.voters.contains(&cx.author) || p.has_attested(&cx.author) || p.has_failed(&cx.author);
                if refused || matched(p.author_module) {
                    return Err(invalid());
                }
                let p = Arc::make_mut(&mut self.pending).get_mut(&script).expect("present");
                let at = p.failed.partition_point(|(k, _, _)| k.0 < cx.author.0);
                p.failed.insert(at, (cx.author, cx.frame, why));
                Ok(())
            }
            _ if row.is_some_and(|m| m.v() == v) => {
                let m = row.expect("checked");
                if self.members.contains_key(&cx.author) && !matched(m.author_module) { Ok(()) } else { Err(invalid()) }
            }
            Some(p) if p.void.is_none() => Err(stale(p.version)),
            _ => Err(stale(current)),
        }
    }

    /// Activations and voids at the end of `frame` (logic.md 6.2a), in asset order: a pending commit a quorum has
    /// attested activates (its re-key, re-checked, and its row); otherwise one whose deadline is `frame` is voided.
    /// Activation is tested first, so a quorum completed at the deadline activates.
    fn end_scripts(&mut self, frame: Frame) -> FrameEnd {
        let mut out = FrameEnd::default();
        let due: Vec<AssetId> = self
            .pending
            .iter()
            .filter(|(_, p)| {
                p.void.is_none()
                    && (p.attested_by_quorum()
                        || p.quorum_unreachable(|k| self.members.contains_key(k))
                        || frame >= p.deadline)
            })
            .map(|(a, _)| *a)
            .collect();
        for a in due {
            let p = self.pending[&a].clone();
            // Activation first, then the shared void rule, then the deadline (logic.md 6.2a).
            let done = if p.attested_by_quorum() {
                self.activate(a, &p, frame)
            } else if p.quorum_unreachable(|k| self.members.contains_key(k)) {
                Err((VoidKind::QuorumUnreachable, None))
            } else {
                Err((VoidKind::Unattested, None))
            };
            match done {
                Ok(report) => {
                    Arc::make_mut(&mut self.pending).remove(&a);
                    out.activated.push((a, p.version, report));
                }
                Err((kind, blocked)) => {
                    Arc::make_mut(&mut self.pending).get_mut(&a).expect("present").void = Some((frame, kind));
                    out.voided.push(Voided { asset: a, version: p.version, kind, blocked, failed: p.failures() });
                }
            }
        }
        out
    }

    /// Makes `p` asset `a`'s current version at the end of `frame` (`F`): the re-key of 7.9, judged again now for
    /// its author, then the row, which runs from `frame + 1`. All or nothing: an author who no longer holds `EDIT`,
    /// or a re-key that is now blocked or too large, voids the commit instead; a block names its objects.
    fn activate(&mut self, a: AssetId, p: &PendingCommit, frame: Frame) -> ActivationResult {
        // An author whose `EDIT` was revoked since the commit: their pending code stops too.
        if !self.permits(p.author, perm::EDIT) {
            return Err((VoidKind::AuthorRevokedAtActivation, None));
        }
        let mut report = None;
        if let Some(m) = self.modules.get(&a).cloned()
            && m.v() != p.v()
        {
            let reg = self.store.registry();
            let new: Option<Vec<_>> = p.components.iter().map(|c| reg.get(*c).cloned()).collect();
            // Unreachable: the commit registered every one of them, and registered ids are never removed.
            debug_assert!(new.is_some(), "a pending commit's components are registered");
            let new = new.ok_or((VoidKind::RekeyBlockedAtActivation, None))?;
            let (r, moves) = self.rekey_plan(&m.components, &new, p.author);
            if r.rows > MAX_REKEY_ROWS {
                return Err((VoidKind::RekeyTooLarge, None));
            }
            if !r.blocked.is_empty() {
                return Err((VoidKind::RekeyBlockedAtActivation, Some(Blocked::of(&r.blocked))));
            }
            // Unreachable once the plan has passed: its moves name rows it just read. Mapped to a void, not a panic,
            // but debug builds flag it so a future failure is not taken for a block.
            let mut w = self.clone();
            let moved = w.apply_rekey(moves, p.version.entry, p.version.op);
            debug_assert!(moved.is_ok(), "a planned re-key applies: {moved:?}");
            moved.map_err(|_| (VoidKind::RekeyBlockedAtActivation, None))?;
            *self = w;
            report = Some(r);
        }
        let row = ModuleRow {
            version: p.version,
            source: p.source,
            toolchain: p.toolchain,
            sdk: p.sdk,
            author_module: p.author_module,
            components: p.components.clone(),
            activate_at: Frame(frame.0 + 1),
            faulted: None,
        };
        Arc::make_mut(&mut self.modules).insert(a, row);
        Ok(report)
    }

    /// `CommitScript` (7.9, logic.md 6.2a): the stamp, the base, the version's components and the re-key's limits,
    /// then the asset's pending commit, which re-keys nothing yet. The voters are the governance members now, and the
    /// commit is its author's attestation if the author is one.
    fn op_commit(
        &mut self,
        cx: &mut Cx,
        _i: u32,
        script: AssetId,
        base: Option<Version>,
        new: ModuleRow,
    ) -> Result<(), Reason> {
        if new.activate_at != cx.frame {
            return Err(invalid());
        }
        // One pending commit per asset: the next waits for its activation or void (6.2a item 6).
        if let Some(p) = self.pending.get(&script).filter(|p| p.void.is_none()) {
            return Err(Reason::Stale { precondition: OWN_CHECK, current: p.version, winner: p.version.entry });
        }
        let row = self.modules.get(&script).cloned();
        let current = row.as_ref().map_or(Version { entry: EntryRef(Hash::ZERO), op: 0 }, |m| m.version);
        let stale = Reason::Stale { precondition: OWN_CHECK, current, winner: current.entry };
        match (&row, base) {
            (None, None) => {}
            (Some(m), Some(b)) if m.version == b => {}
            _ => return Err(stale),
        }
        let components = match &row {
            // The same source (a toolchain or SDK change): the same components, no re-key at activation.
            Some(m) if m.v() == new.v() => m.components.clone(),
            _ => {
                let reg = cx.registered.clone().ok_or_else(invalid)?;
                let mut ids: Vec<ComponentId> = reg.iter().map(|d| d.id()).collect();
                ids.sort();
                // The re-key's limits now, so the author hears at once; they are judged again at `F`.
                if let Some(m) = &row {
                    let (report, _) = self.rekey_plan(&m.components, &reg, cx.author);
                    if report.rows > MAX_REKEY_ROWS {
                        return Err(Reason::TooLarge { limit: MAX_REKEY_ROWS });
                    }
                    // The first 16 blocking objects in id order, and how many more (S2 v1.6).
                    if !report.blocked.is_empty() {
                        return Err(rekey_blocked(&report.blocked));
                    }
                }
                ids
            }
        };
        let voters: Vec<MemberKey> = self.members.keys().copied().collect();
        let attested = if voters.contains(&cx.author) { vec![(cx.author, cx.frame)] } else { Vec::new() };
        let deadline = Frame(cx.frame.0.saturating_add(attest_window(new.source.len, self.rate.hz())));
        let pending = PendingCommit {
            version: new.version,
            author: cx.author,
            source: new.source,
            toolchain: new.toolchain,
            sdk: new.sdk,
            author_module: new.author_module,
            components,
            commit_frame: cx.frame,
            deadline,
            voters,
            attested,
            failed: Vec::new(),
            void: None,
        };
        Arc::make_mut(&mut self.pending).insert(script, pending);
        Ok(())
    }

    fn op_create(
        &mut self,
        cx: &mut Cx,
        i: u32,
        new: u32,
        parent: Target,
        index: Option<u32>,
        rows: &[Row],
    ) -> Result<(), Reason> {
        if cx.new.contains_key(&new) {
            return Err(Reason::NewIndexOverlap);
        }
        within_cap(cx, 1)?;
        let id = ObjectId::created(cx.line, cx.author, cx.chain, cx.seq, new);
        if self.store.exists(id) || self.tombstones.contains_key(&id) {
            return Err(Reason::Exists(id));
        }
        // Delete first, add-on second: the add-on becomes a root where its parent last was (7.5).
        let (parent, orphan_base) = match self.resolve(cx, parent)? {
            Res::Root => (None, None),
            Res::Live(p) => (Some(p), None),
            Res::Gone { id: p, by } => {
                cx.notes.push(Note::Orphaned { object: id, deleted_by: by });
                (None, Some(self.tombstones.get(&p).and_then(|t| t.world)))
            }
        };
        self.not_into_body(parent)?;
        let v = version(cx.entry, i);
        self.create_at(id, parent, index.map(|x| x as usize), cx.author, v).map_err(|e| match e {
            WorldError::TooDeep { object, limit } => too_deep(object, limit),
            WorldError::Duplicate(id) => Reason::Exists(id),
            WorldError::UnknownObject(id) => Reason::Unknown(id),
            WorldError::Store(_) | WorldError::NotAMember(_) => invalid(),
        })?;
        cx.new.insert(new, id);
        for r in rows {
            if r.component == ComponentId::TRANSFORM
                && let Some(base) = orphan_base
            {
                // The op's transform is local to the deleted parent: compose it onto the parent's last place.
                let t: Transform = read_pod(&r.bytes).ok_or_else(|| invalid_field(r.component, 0))?;
                let bytes = match base {
                    Some(w) => {
                        let placed = w.compose(&t).ok_or(Reason::OutOfRange)?;
                        bytemuck::bytes_of(&placed.local_in(None)).to_vec()
                    }
                    None => r.bytes.clone(),
                };
                self.write_row(cx, i, id, r.component, r.instance, &bytes, true)?;
            } else {
                self.write_row(cx, i, id, r.component, r.instance, &r.bytes, true)?;
            }
        }
        // Every World object has a simulation size (3.5): 1 m times the creator's scale, which is 1 in M1.
        if !self.store.has::<SimSize>(id) {
            self.store.insert(id, SimSize { metres: 1.0 }).map_err(|_| invalid())?;
        }
        if let Some(p) = parent {
            self.touch(p, v, true);
        }
        // Creation set the versions; rows written above moved the object version to this same op.
        self.touch(id, v, true);
        cx.created.push(id);
        cx.fp.created.push(id);
        cx.undo.push(Op::Delete { id: Target::Existing(id) });
        Ok(())
    }

    fn op_delete(&mut self, cx: &mut Cx, i: u32, target: Target) -> Result<(), Reason> {
        let o = match self.resolve(cx, target)? {
            Res::Root => return Err(invalid()),
            Res::Gone { id, by } => {
                cx.notes.push(Note::AlreadyDeleted { object: id, by });
                return Ok(());
            }
            Res::Live(o) => o,
        };
        let doomed = self.store.subtree(o);
        // Hands are made and removed only by logic (`admit`, `leave`), so a subtree holding one is no edit's to delete,
        // as it is no edit's to copy (7.5; @coordinator-opus).
        if let Some(c) = doomed.iter().find_map(|&d| self.placed_by_logic(d)) {
            return Err(invalid_field(c, u32::MAX));
        }
        // A hold by anyone but the author refuses the delete (7.2). The author's own holds end with it, so a player
        // can delete what they hold without letting go first (@coordinator-opus, option (b)).
        for d in &doomed {
            if let Some(c) = self.claims.get(d)
                && c.member != cx.author
            {
                return Err(Reason::Held { by: c.member });
            }
        }
        let parent = self.store.parent(o);
        let expires =
            Frame(cx.frame.0.saturating_add(self.rate.frames(std::time::Duration::from_secs(TOMBSTONE_SECONDS))));
        let mut stones = Vec::with_capacity(doomed.len());
        // `doomed` is depth-first with children in order, so counting per parent gives each child's place.
        let mut places: BTreeMap<ObjectId, u32> = BTreeMap::new();
        for &d in &doomed {
            let up = self.store.parent(d);
            let place = match up {
                Some(p) if d == o => self.store.children(p).position(|c| c == o).unwrap_or(0) as u32,
                Some(p) => {
                    let n = places.entry(p).or_insert(0);
                    *n += 1;
                    *n - 1
                }
                None => 0,
            };
            stones.push((d, tombstone_of(&self.store, d, cx.entry, cx.frame, expires, up, place)));
        }
        self.store.destroy(o).map_err(|_| invalid())?;
        crate::per_player::bury(&mut self.per_player, &mut stones);
        let t = Arc::make_mut(&mut self.tombstones);
        for (d, s) in stones {
            t.insert(d, s);
        }
        // Holds on the deleted objects (all the author's, by the check above), and holds by hands that went with the
        // subtree, end with them.
        let gone = |o: &ObjectId, c: &crate::world::Claim| doomed.contains(o) || doomed.contains(&c.hand);
        if self.claims.iter().any(|(o, c)| gone(o, c)) {
            Arc::make_mut(&mut self.claims).retain(|o, c| !gone(o, c));
        }
        if let Some(p) = parent {
            self.touch(p, version(cx.entry, i), true);
        }
        cx.forget(&doomed);
        cx.undo.push(Op::Undelete { id: o });
        cx.fp.removed.extend_from_slice(&doomed);
        Ok(())
    }

    /// `Copy` (7.1): the subtree, rows and all, as new objects with derived ids, under `parent`. A protected object
    /// (or a subtree containing one) may be copied only by its owner (7.8); hands are never copied (8).
    #[allow(clippy::too_many_arguments)]
    fn op_copy(
        &mut self,
        cx: &mut Cx,
        i: u32,
        source: Target,
        new: u32,
        parent: Target,
        index: Option<u32>,
        size: u32,
    ) -> Result<(), Reason> {
        let src = self.live(cx, source)?;
        if size == 0 {
            // A subtree always has its root: a size of 0 is a malformed op, not a stale one.
            return Err(invalid());
        }
        // The cap on the size the author declares, before walking the subtree; the walk then confirms it.
        within_cap(cx, size as usize)?;
        let subtree = self.store.subtree(src);
        if subtree.len() != size as usize {
            // The subtree changed since the author read it. No single edit is the winner (any object in it may have
            // gained or lost children), so `current` and `winner` are zero, as for an absent row (7.6).
            return Err(Reason::Stale { precondition: OWN_CHECK, current: GENESIS, winner: GENESIS.entry });
        }
        let end = u32::try_from(subtree.len()).ok().and_then(|n| new.checked_add(n)).ok_or(Reason::NewIndexOverlap)?;
        if cx.new.range(new..end).next().is_some() {
            return Err(Reason::NewIndexOverlap);
        }
        if let Some(owner) = self.protected_by(src)
            && owner != cx.author
        {
            return Err(Reason::Protected { owner });
        }
        for &x in &subtree {
            if let Some(p) = self.store.get::<Protection>(x)
                && p.owner != cx.author
            {
                return Err(Reason::Protected { owner: p.owner });
            }
            if let Some(c) = self.placed_by_logic(x) {
                return Err(invalid_field(c, u32::MAX));
            }
        }
        let (parent, orphan_base) = match self.resolve(cx, parent)? {
            Res::Root => (None, None),
            Res::Live(p) => (Some(p), None),
            Res::Gone { id: p, by } => {
                let root = ObjectId::created(cx.line, cx.author, cx.chain, cx.seq, new);
                cx.notes.push(Note::Orphaned { object: root, deleted_by: by });
                (None, Some(self.tombstones.get(&p).and_then(|t| t.world)))
            }
        };
        self.not_into_body(parent)?;
        if let Some(p) = parent
            && depth(&self.store, p) + 1 + self.height(src) > self.depth_limit
        {
            return Err(too_deep(src, self.depth_limit));
        }
        let v = version(cx.entry, i);
        let rv = RowVersion { entry: v.entry.0.0, op: v.op, tag: cx.tag(i), _pad: 0 };
        let registry = self.store.registry().clone();
        let mut map: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
        for (j, &x) in subtree.iter().enumerate() {
            let k = new + j as u32;
            let id = ObjectId::created(cx.line, cx.author, cx.chain, cx.seq, k);
            if self.store.exists(id) || self.tombstones.contains_key(&id) {
                return Err(Reason::Exists(id));
            }
            let (p, at) = if j == 0 {
                (parent, index.map(|x| x as usize))
            } else {
                (self.store.parent(x).and_then(|q| map.get(&q).copied()), None)
            };
            self.create_at(id, p, at, cx.author, v).map_err(|_| invalid())?;
            for desc in registry.iter().filter(|d| d.id() != ComponentId::NODE && d.id() != ids::BODY_STATE) {
                let c = desc.id();
                let Some(row) = self.store.get_raw(c, x).map(<[u8]>::to_vec) else { continue };
                let row = match (j, orphan_base) {
                    (0, Some(Some(w))) if c == ComponentId::TRANSFORM => {
                        let t: Transform = read_pod(&row).ok_or_else(invalid)?;
                        let placed = w.compose(&t).ok_or(Reason::OutOfRange)?;
                        bytemuck::bytes_of(&placed.local_in(None)).to_vec()
                    }
                    _ => row,
                };
                self.store.insert_raw(c, id, &row).map_err(|e| self.store_reason(c, e))?;
                self.store.set_row_version(c, id, rv);
            }
            map.insert(x, id);
            cx.new.insert(k, id);
            cx.created.push(id);
            cx.fp.created.push(id);
        }
        if let Some(p) = parent {
            self.touch(p, v, true);
        }
        cx.undo.push(Op::Delete { id: Target::Existing(map[&src]) });
        Ok(())
    }

    /// `Undelete` (7.5): the deleted subtree back from its tombstones, with the same ids, rows and row versions, under
    /// its old parent at its old place (or as a root, orphaned, if that parent is gone too). Each restored object's
    /// object and structural versions move to this op.
    fn op_undelete(&mut self, cx: &mut Cx, i: u32, id: ObjectId) -> Result<(), Reason> {
        if self.store.exists(id) {
            return Err(Reason::Exists(id));
        }
        let root = self.tombstones.get(&id).cloned().ok_or(Reason::Unknown(id))?;
        // The subtree, parents first, children in their old order: the same delete's tombstones below `id`, through
        // their recorded parents and places, never through sibling links, which may name an object undeleted since
        // (@design-reviewer's Medium 1). Bounded by the number of tombstones, so corrupt records cannot loop.
        let limit = self.tombstones.len();
        let mut kids: BTreeMap<ObjectId, Vec<(u32, ObjectId)>> = BTreeMap::new();
        for (&x, t) in self.tombstones.iter() {
            if x != id
                && t.by == root.by
                && let Some(p) = t.parent
            {
                kids.entry(p).or_default().push((t.place, x));
            }
        }
        let mut order = Vec::new();
        let mut stack = vec![id];
        while let Some(x) = stack.pop() {
            if order.len() > limit {
                return Err(invalid());
            }
            order.push(x);
            if let Some(v) = kids.get_mut(&x) {
                v.sort_unstable();
                stack.extend(v.iter().rev().map(|&(_, c)| c));
            }
        }
        let parent = root.parent.filter(|&p| self.store.exists(p));
        let orphaned = root.parent.is_some() && parent.is_none();
        if orphaned {
            let by = root.parent.and_then(|p| self.tombstones.get(&p)).map_or(root.by, |t| t.by);
            cx.notes.push(Note::Orphaned { object: id, deleted_by: by });
        }
        let index = parent.map(|p| {
            let prev = root.node.prev_sibling;
            self.store.children(p).position(|c| c == prev).map_or(0, |k| k + 1)
        });
        if let Some(p) = parent {
            let height = order.iter().map(|x| self.stone_depth(id, *x)).max().unwrap_or(0);
            if depth(&self.store, p) + 1 + height > self.depth_limit {
                return Err(too_deep(id, self.depth_limit));
            }
        }
        let v = version(cx.entry, i);
        for &x in &order {
            let t = self.tombstones[&x].clone();
            let (p, at) = if x == id { (parent, index) } else { (t.parent, None) };
            self.create_at(x, p, at, t.node.creator, v).map_err(|_| invalid())?;
            let mut n = self.store.node(x);
            n.born_on = t.node.born_on;
            self.store.set_node_record(x, &n);
            for (c, bytes, rv) in &t.rows {
                let bytes = match root.world {
                    Some(w) if x == id && orphaned && *c == ComponentId::TRANSFORM => {
                        bytemuck::bytes_of(&w.local_in(None)).to_vec()
                    }
                    _ => bytes.clone(),
                };
                self.store.insert_raw(*c, x, &bytes).map_err(|e| self.store_reason(*c, e))?;
                self.store.set_row_version(*c, x, *rv);
            }
            // Its per-player field modes come back with it; its published values don't (logic.md 7.4).
            crate::per_player::unbury(&mut self.per_player, &t.modes);
        }
        let stones = Arc::make_mut(&mut self.tombstones);
        for x in &order {
            stones.remove(x);
        }
        if let Some(p) = parent {
            self.touch(p, v, true);
        }
        cx.fp.created.extend_from_slice(&order);
        cx.undo.push(Op::Delete { id: Target::Existing(id) });
        Ok(())
    }

    /// How many levels `x` sits below `top` in the tombstones' recorded parents.
    fn stone_depth(&self, top: ObjectId, x: ObjectId) -> u32 {
        let mut d = 0;
        let mut at = x;
        while at != top && d <= DEPTH_LIMIT_MAX {
            match self.tombstones.get(&at).and_then(|t| t.parent) {
                Some(p) => at = p,
                None => break,
            }
            d += 1;
        }
        d
    }

    /// The author's prediction of its own pending commit (logic.md 6.2a's option (i); @vdw-physics):
    /// activates `asset`'s pending commit at the end of `frame` as `end_frame` would with a quorum, without the
    /// quorum: the same re-key, its limits and the author's `EDIT` judged now, and the row, which runs from
    /// `frame + 1`. Call it right after `end_frame(frame)` on the author's own replica, for a history that is only a
    /// prediction: **never on a history whose roots are voted on**, since there the quorum decides (the replica holds
    /// its votes from `c + 1` and replays under the rule when an `AttestBuild`, a `BuildFailed`, a leave or `D`
    /// arrives).
    ///
    /// `None` if no commit is pending at `asset` (none, or voided). `Some(Ok(report))` activated it and removed the
    /// pending record, as a real activation does; the report is `end_frame`'s. `Some(Err(why))` changed nothing and
    /// leaves the record pending: a failed prediction is not a void, and the replica simply keeps the old code.
    pub fn activate_for_prediction(&mut self, asset: AssetId, frame: Frame) -> Option<ActivationResult> {
        let p = self.pending.get(&asset).filter(|p| p.void.is_none())?.clone();
        let done = self.activate(asset, &p, frame);
        if done.is_ok() {
            Arc::make_mut(&mut self.pending).remove(&asset);
        }
        Some(done)
    }

    /// Ends a frame (7.2, 7.5, 8.1): expires claims and cooldowns, writes each pending hand pack's pose for
    /// `frame + 1`, and drops the tombstones whose expiry frame has passed.
    ///
    /// First the scripts (logic.md 6.2a): each pending commit a quorum has attested activates, re-keying its rows,
    /// and runs from `frame + 1`; one at its deadline without a quorum is voided. What happened is returned.
    pub fn end_frame(&mut self, frame: Frame) -> FrameEnd {
        let end = self.end_scripts(frame);
        self.end_frame_inputs(frame);
        self.end_frame_players(frame);
        if self.tombstones.values().any(|t| t.expires <= frame) {
            Arc::make_mut(&mut self.tombstones).retain(|_, t| t.expires > frame);
        }
        end
    }

    /// The whole row with one field replaced.
    fn field_row(
        &self,
        id: ObjectId,
        c: ComponentId,
        instance: u16,
        field: u16,
        value: &[u8],
    ) -> Result<Vec<u8>, Reason> {
        let desc = self.store.registry().get(c).ok_or_else(invalid)?;
        let f = desc.fields().get(field as usize).ok_or_else(|| invalid_field(c, field as u32))?;
        let len = (f.kind.size() as usize).saturating_mul(f.count as usize);
        if f.name.starts_with('_') || value.len() != len || instance != 0 {
            return Err(invalid_field(c, field as u32));
        }
        let mut row = self.store.get_raw(c, id).ok_or_else(|| invalid_field(c, field as u32))?.to_vec();
        let at = f.offset as usize;
        row.get_mut(at..at + len).ok_or_else(|| invalid_field(c, field as u32))?.copy_from_slice(value);
        Ok(row)
    }

    /// The protection rules for writing or removing a `Protection` row on `id` (7.8): only the object's creator may
    /// add one, naming themselves; nobody else may change or remove one; none may go inside another's subtree.
    fn check_protection_write(
        &self,
        author: MemberKey,
        id: ObjectId,
        new_owner: Option<MemberKey>,
    ) -> Result<(), Reason> {
        if let Some(owner) = self.protected_by(id)
            && owner != author
        {
            return Err(Reason::Protected { owner });
        }
        let creator = self.store.node(id).creator;
        if let Some(o) = new_owner
            && (creator != author || o != author)
        {
            return Err(Reason::Protected { owner: creator });
        }
        Ok(())
    }

    /// Refuses an edit that would move hand or player body `id` or write one of its [`hand_only`] rows, as `Invalid`
    /// naming `Hand` or `Player` (7.5, 8.2): a hand is moved only by its member's input, a body only by the mover. A
    /// subtree that merely holds a hand may still move.
    fn not_a_hand(&self, id: ObjectId) -> Result<(), Reason> {
        match self.placed_by_logic(id) {
            Some(c) => Err(invalid_field(c, u32::MAX)),
            None => Ok(()),
        }
    }

    /// `Hand` or `Player` when `id` is a hand, a player body or one of a body's other children (its drawn shape):
    /// objects only world logic makes, places and removes (7.5, 8.2), so no edit moves, deletes or copies them. A
    /// body's children are all logic's, because no edit puts anything directly under a body ([`Self::not_into_body`]).
    pub(crate) fn placed_by_logic(&self, id: ObjectId) -> Option<ComponentId> {
        if let Some(c) = [ids::HAND, ids::PLAYER].into_iter().find(|&c| self.store.get_raw(c, id).is_some()) {
            return Some(c);
        }
        let parent = self.store.parent(id)?;
        self.store.get_raw(ids::PLAYER, parent).map(|_| ids::PLAYER)
    }

    /// Refuses an edit that would put an object directly under a player body, as `Invalid` naming `Player` (8.2): a
    /// body's children are its hands and its drawn shape, all world logic's. Under a hand is allowed, as before.
    fn not_into_body(&self, parent: Option<ObjectId>) -> Result<(), Reason> {
        match parent {
            Some(p) if self.store.get_raw(ids::PLAYER, p).is_some() => Err(invalid_field(ids::PLAYER, u32::MAX)),
            _ => Ok(()),
        }
    }

    /// The owner of the nearest `Protection` at or above `id`. Iterative and bounded (3.3).
    pub(crate) fn protected_by(&self, id: ObjectId) -> Option<MemberKey> {
        let mut at = id;
        for _ in 0..=DEPTH_LIMIT_MAX {
            if let Some(p) = self.store.get::<Protection>(at) {
                return Some(p.owner);
            }
            at = self.store.parent(at)?;
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn write_row(
        &mut self,
        cx: &mut Cx,
        i: u32,
        id: ObjectId,
        c: ComponentId,
        instance: u16,
        bytes: &[u8],
        creating: bool,
    ) -> Result<(), Reason> {
        if instance != 0 || !edit_writable(c) || self.store.registry().get(c).is_none() {
            return Err(invalid_field(c, u32::MAX));
        }
        if hand_only(c) {
            self.not_a_hand(id)?;
        }
        // A superseded version's components take no writes, and no hand or player body takes a tag (7.9, 8.2), so no
        // script may destroy one.
        self.superseded(c)?;
        if self.is_tag(c) && self.placed_by_logic(id).is_some() {
            return Err(invalid_field(c, u32::MAX));
        }
        if held_rows(c)
            && let Some(claim) = self.claims.get(&id)
        {
            return Err(Reason::Held { by: claim.member });
        }
        if c == ids::PROTECTION {
            let p: Protection = read_pod(bytes).ok_or_else(|| invalid_field(c, 0))?;
            self.check_protection_write(cx.author, id, Some(p.owner))?;
        }
        if c == ids::LIGHT {
            let l: Light = read_pod(bytes).ok_or_else(|| invalid_field(c, 0))?;
            if let Some(field) = l.invalid_angle() {
                return Err(invalid_field(c, field));
            }
        }
        let before = self.store.row_version(c, id);
        let prior = self.store.get_raw(c, id).map(<[u8]>::to_vec);
        // An overwrite keeps the instance's tag; an addition gives it this op's.
        let tag = before.map_or(cx.tag(i), |v| v.tag);
        self.store.insert_raw(c, id, bytes).map_err(|e| self.store_reason(c, e))?;
        if !creating {
            cx.undo.push(match prior {
                Some(bytes) => Op::SetRow { id: Target::Existing(id), row: Row { component: c, instance: 0, bytes } },
                None => {
                    Op::RemoveRow { id: Target::Existing(id), component: c, instance: 0, tag: u128::from_le_bytes(tag) }
                }
            });
            cx.undo_pre.insert((id, c), version(cx.entry, i));
        }
        if !creating
            && let Some(v) = before
            && v.edited()
        {
            cx.overwrote.push(from_row_version(v));
        }
        let v = version(cx.entry, i);
        self.store.set_row_version(c, id, RowVersion { entry: v.entry.0.0, op: v.op, tag, _pad: 0 });
        self.touch(id, v, false);
        cx.fp.rows.push((id, c));
        Ok(())
    }

    fn op_remove_row(
        &mut self,
        cx: &mut Cx,
        i: u32,
        id: ObjectId,
        c: ComponentId,
        instance: u16,
        tag: u128,
    ) -> Result<(), Reason> {
        if instance != 0 || !edit_writable(c) || c == ids::SIM_SIZE || self.store.registry().get(c).is_none() {
            return Err(invalid_field(c, u32::MAX));
        }
        if hand_only(c) {
            self.not_a_hand(id)?;
        }
        // The instance it names is already gone: no row, or a row added again since with another tag (7.6). There is
        // nothing to remove, so nothing to hold or protect either, and two removals in a merge need no rule of their
        // own.
        let Some(before) = self.store.row_version(c, id).filter(|v| v.tag() == tag) else {
            no_effect(cx, i);
            return Ok(());
        };
        if held_rows(c)
            && let Some(claim) = self.claims.get(&id)
        {
            return Err(Reason::Held { by: claim.member });
        }
        if c == ids::PROTECTION {
            self.check_protection_write(cx.author, id, None)?;
        }
        let added_here = cx.undo.iter().rposition(|op| {
            matches!(op, Op::RemoveRow { id: Target::Existing(x), component, tag: t, .. }
                if *x == id && *component == c && *t == before.tag())
        });
        if let Some(at) = added_here {
            // This transaction added the row and now removes it: nothing it did to the row since needs undoing.
            let tail: Vec<Op> = cx.undo.split_off(at).into_iter().filter(|op| !row_undo(op, id, c)).collect();
            cx.undo.extend(tail);
        } else if let Some(bytes) = self.store.get_raw(c, id).map(<[u8]>::to_vec) {
            cx.undo.push(Op::SetRow { id: Target::Existing(id), row: Row { component: c, instance: 0, bytes } });
        }
        // The inverse re-adds the row only while it is still absent, so a revert never overwrites a row someone added
        // since (@design-reviewer's Medium 2).
        if cx.undo.iter().any(|op| row_undo(op, id, c)) {
            cx.undo_pre.insert((id, c), GENESIS);
        } else {
            cx.undo_pre.remove(&(id, c));
        }
        self.store.remove_raw(c, id).map_err(|e| self.store_reason(c, e))?;
        if before.edited() {
            cx.overwrote.push(from_row_version(before));
        }
        self.touch(id, version(cx.entry, i), false);
        cx.fp.rows.push((id, c));
        Ok(())
    }

    fn op_reparent(
        &mut self,
        cx: &mut Cx,
        i: u32,
        id: ObjectId,
        parent: Target,
        index: Option<u32>,
        keep_world: bool,
    ) -> Result<(), Reason> {
        self.not_a_hand(id)?;
        if let Some(claim) = self.claims.get(&id) {
            return Err(Reason::Held { by: claim.member });
        }
        let (parent, keep_world) = match self.resolve(cx, parent)? {
            Res::Root => (None, keep_world),
            Res::Live(p) => (Some(p), keep_world),
            Res::Gone { by, .. } => {
                // A reparent onto a deleted object orphans: it keeps its own world pose (7.5).
                cx.notes.push(Note::Orphaned { object: id, deleted_by: by });
                (None, true)
            }
        };
        self.not_into_body(parent)?;
        if let Some(p) = parent {
            if self.store.is_ancestor_or_self(id, p) {
                return Err(Reason::Cycle);
            }
            if depth(&self.store, p) + 1 + self.height(id) > self.depth_limit {
                return Err(too_deep(id, self.depth_limit));
            }
        }
        let old = self.store.parent(id);
        let old_index = old.and_then(|p| self.store.children(p).position(|c| c == id)).map(|x| x as u32);
        cx.undo.push(Op::Reparent {
            id: Target::Existing(id),
            parent: old.map_or(Target::Root, Target::Existing),
            index: old_index,
            keep_world,
        });
        let index = index.map(|x| x as usize);
        let r = if keep_world {
            self.store.set_parent_keep_world(id, parent, index)
        } else {
            self.store.set_parent(id, parent, index)
        };
        r.map_err(|e| self.store_reason(ComponentId::TRANSFORM, e))?;
        let v = version(cx.entry, i);
        self.touch(id, v, true);
        for p in [old, parent].into_iter().flatten() {
            self.touch(p, v, true);
        }
        cx.fp.reparented.push((id, keep_world));
        Ok(())
    }

    /// How many levels the deepest descendant of `id` is below it. Iterative.
    fn height(&self, id: ObjectId) -> u32 {
        let mut max = 0;
        let mut stack = vec![(id, 0u32)];
        while let Some((x, d)) = stack.pop() {
            max = max.max(d);
            if d <= DEPTH_LIMIT_MAX {
                stack.extend(self.store.children(x).map(|c| (c, d + 1)));
            }
        }
        max
    }

    /// Moves an object's version to `v`, and its structural version too if `structural` (7.3).
    pub(crate) fn touch(&mut self, id: ObjectId, v: Version, structural: bool) {
        let mut n = self.store.node(id);
        n.version_entry = v.entry.0.0;
        n.version_op = v.op;
        if structural {
            n.structural_entry = v.entry.0.0;
            n.structural_op = v.op;
        }
        self.store.set_node_record(id, &n);
    }

    fn store_reason(&self, c: ComponentId, e: pe_core::Error) -> Reason {
        match e {
            pe_core::Error::OutOfRange { .. } => Reason::OutOfRange,
            pe_core::Error::Cycle { .. } => Reason::Cycle,
            pe_core::Error::InvalidValue { field, .. } => {
                let idx = self
                    .store
                    .registry()
                    .get(c)
                    .and_then(|d| d.fields().iter().position(|f| f.name == field))
                    .map_or(u32::MAX, |p| p as u32);
                invalid_field(c, idx)
            }
            _ => invalid_field(c, u32::MAX),
        }
    }
}

/// A tombstone for `d` as `store` holds it (7.5): its rows, versions, place and last world transform.
pub(crate) fn tombstone_of(
    store: &pe_sim::SimStore,
    d: ObjectId,
    by: EntryRef,
    frame: Frame,
    expires: Frame,
    parent: Option<ObjectId>,
    place: u32,
) -> Tombstone {
    let rows = store
        .registry()
        .iter()
        .filter(|desc| desc.id() != ComponentId::NODE)
        .filter_map(|desc| {
            let row = store.get_raw(desc.id(), d)?;
            Some((desc.id(), row.to_vec(), store.row_version(desc.id(), d).unwrap_or_default()))
        })
        .collect();
    let modes = Vec::new();
    Tombstone { by, frame, expires, parent, place, world: store.world_transform(d), node: store.node(d), rows, modes }
}

fn node_version(n: &Node) -> Version {
    Version { entry: EntryRef(Hash(n.version_entry)), op: n.version_op }
}

fn read_pod<T: bytemuck::Pod>(b: &[u8]) -> Option<T> {
    (b.len() == size_of::<T>()).then(|| bytemuck::pod_read_unaligned(b))
}

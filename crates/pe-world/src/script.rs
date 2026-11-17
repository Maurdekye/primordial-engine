//! Scripts in the world (world-model.md 7.9, 9.2a; slice D): the tag, component ids from the version hash, the modules
//! table, `Register` and `CommitScript` with re-keying, the engine components a script may name, and the logic stages'
//! hooks (`World::logic_stage`, `World::after_logic`). pe-world never depends on pe-logic: the runner reaches the
//! world through plain types and closures.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pe_core::commit::Hasher;
use pe_core::{EntryRef, Frame, Hash, MemberKey, ObjectId};
use pe_seams::{Reason, Version};
use pe_sim::{ComponentDesc, ComponentId, FieldDesc, RowVersion, SimStore};

use crate::apply::Footprint;
use crate::components::{BlobRef, Body, Light, SimSize, ids};
use crate::world::{TOMBSTONE_SECONDS, World};

pub use crate::source::{
    MAX_PATH, MAX_SEGMENT, MAX_SEGMENTS, MAX_SOURCE_BYTES, SOURCE_MAGIC, SourceError, check_source, pack_source,
    path_ok, source_files,
};

/// A script's stable asset id (logic.md 6.1), the modules table's key.
pub type AssetId = [u8; 32];

/// The name every script version's tag carries (7.9). An SDK name is a Rust identifier and has no `.`.
pub const TAG_NAME: &str = "pe.tag";

/// The most rows one commit's re-key moves (7.9) [I, to measure: the frame-time spike of moving them in one step].
pub const MAX_REKEY_ROWS: u32 = 32_768;

/// Script component ids start here: everything below is engine space (3.2).
pub const SCRIPT_ID_FLOOR: u128 = 1 << 32;

/// One script asset's modules row (5.4, 6.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleRow {
    /// The commit that wrote it.
    pub version: Version,
    /// The source archive (logic.md 4.1a); its hash is the version hash V.
    pub source: BlobRef,
    pub toolchain: Hash,
    pub sdk: Hash,
    pub author_module: Hash,
    /// The version's component ids, strictly ascending, its tag among them.
    pub components: Vec<ComponentId>,
    pub activate_at: Frame,
    /// The frame and kind (pe-logic's `Fault::code()`, 1 to 13) of the fault that stopped it, until its next commit.
    pub faulted: Option<(Frame, u8)>,
}

impl ModuleRow {
    /// The version hash V.
    pub fn v(&self) -> Hash {
        Hash(self.source.hash)
    }

    /// 6.6's value bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.version.entry.0.0);
        b.extend_from_slice(&self.version.op.to_le_bytes());
        b.extend_from_slice(&self.source.hash);
        b.extend_from_slice(&self.source.len.to_le_bytes());
        b.extend_from_slice(&self.toolchain.0);
        b.extend_from_slice(&self.sdk.0);
        b.extend_from_slice(&self.author_module.0);
        b.extend_from_slice(&(self.components.len() as u32).to_le_bytes());
        for c in &self.components {
            b.extend_from_slice(&c.0.to_le_bytes());
        }
        b.extend_from_slice(&self.activate_at.0.to_le_bytes());
        match self.faulted {
            None => b.push(0),
            Some((f, k)) => {
                b.push(1);
                b.extend_from_slice(&f.0.to_le_bytes());
                b.push(k);
            }
        }
        b
    }

    /// Strict decoding of 6.6's value bytes.
    pub fn from_bytes(b: &[u8]) -> Option<ModuleRow> {
        let mut r = Rd(b);
        let entry = EntryRef(Hash(r.arr()?));
        let op = u32::from_le_bytes(r.arr()?);
        let source = BlobRef { hash: r.arr()?, len: u64::from_le_bytes(r.arr()?) };
        let toolchain = Hash(r.arr()?);
        let sdk = Hash(r.arr()?);
        let author_module = Hash(r.arr()?);
        let n = u32::from_le_bytes(r.arr()?) as usize;
        if n > r.0.len() / 16 {
            return None;
        }
        let mut components = Vec::with_capacity(n);
        for _ in 0..n {
            let c = ComponentId(u128::from_le_bytes(r.arr()?));
            if components.last().is_some_and(|l: &ComponentId| *l >= c) {
                return None;
            }
            components.push(c);
        }
        let activate_at = Frame(u64::from_le_bytes(r.arr()?));
        let faulted = match r.arr::<1>()?[0] {
            0 => None,
            1 => {
                let f = Frame(u64::from_le_bytes(r.arr()?));
                let k = r.arr::<1>()?[0];
                if !(1..=13).contains(&k) {
                    return None;
                }
                Some((f, k))
            }
            _ => return None,
        };
        if !r.0.is_empty() {
            return None;
        }
        Some(ModuleRow {
            version: Version { entry, op },
            source,
            toolchain,
            sdk,
            author_module,
            components,
            activate_at,
            faulted,
        })
    }
}

/// Divergence.md 4.2's wait for one module, its fixed parts, in milliseconds (logic.md 6.2a item 4; @iter-bench): the foreground wait `W_fg` and the build time `T_build`.
pub const W_FG_MS: u64 = 2_000;
pub const T_BUILD_MS: u64 = 20_000;
/// The thin links' rate `R_thin`, in bytes per second.
pub const R_THIN: u64 = 1_000;

/// How many frames after its commit frame `c` a pending commit is voided if no quorum has attested it (logic.md
/// 6.2a item 4): `ceil(2 · T_module · hz)`, with `T_module = W_fg + len / R_thin + T_build` from the source's length,
/// all in integer milliseconds. The lamp's 1,213 bytes give 2,786 frames at 60 Hz (46.4 s); the 256 KiB cap gives
/// 34,098 (568.3 s).
pub fn attest_window(source_len: u64, hz: u32) -> u64 {
    let t_module_ms = W_FG_MS + source_len.saturating_mul(1_000) / R_THIN + T_BUILD_MS;
    (2 * t_module_ms * u64::from(hz)).div_ceil(1_000)
}

/// The quorum over `n` voters (agreement.md): `t = floor((n - 1) / 3)`, `q = floor((n + t) / 2) + 1`.
pub fn quorum(n: usize) -> usize {
    let t = n.saturating_sub(1) / 3;
    (n + t) / 2 + 1
}

/// Why a pending commit was voided (logic.md 6.2a), recorded with the frame for the author.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoidKind {
    /// No quorum attested it by its deadline.
    Unattested = 1,
    /// At `F` its re-key would lose data on an object its committer may not edit (7.9's loss rule).
    RekeyBlockedAtActivation = 2,
    /// At `F` its re-key would move more than [`MAX_REKEY_ROWS`] rows.
    RekeyTooLarge = 3,
    /// At `F` its author no longer holds `EDIT`: revoking someone's rights stops their pending code too
    /// (@coordinator-opus).
    AuthorRevokedAtActivation = 4,
    /// Its quorum became unreachable ([`PendingCommit::quorum_unreachable`], logic.md 6.2a item 4), for example
    /// after a leave: voided at that frame's end rather than at `D` (@fable-reviewer via @coordinator-opus).
    QuorumUnreachable = 5,
}

impl VoidKind {
    fn from_u8(b: u8) -> Option<VoidKind> {
        match b {
            1 => Some(VoidKind::Unattested),
            2 => Some(VoidKind::RekeyBlockedAtActivation),
            3 => Some(VoidKind::RekeyTooLarge),
            4 => Some(VoidKind::AuthorRevokedAtActivation),
            5 => Some(VoidKind::QuorumUnreachable),
            _ => None,
        }
    }
}

/// Why a voter's build of a pending version failed (logic.md 6.2a item 2), after its local retries (6.2b). Hashes
/// and this code only, never compiler output: the entry goes to every member and stays in the log, and a compiler's
/// message can carry local paths. The codes are consensus (the op's wire form and the hashed pending row), matching
/// S5's `BuildFailure` (@engine-architect, @world-model).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum BuildFailure {
    /// The source does not compile.
    Compile = 1,
    /// The build read something outside its archive and pe-sdk (6.2b's dep-info check).
    Refused = 2,
    /// The build went past the memory cap (6.2a item 5).
    Cap = 3,
    /// It built, but its module hash is not the registered `author_module`, after one rebuild from a clean folder.
    Mismatch = 4,
    /// A fault of this machine's (an I/O error, a crashed tool), after two retries.
    Local = 5,
}

impl BuildFailure {
    pub const ALL: [BuildFailure; 5] =
        [BuildFailure::Compile, BuildFailure::Refused, BuildFailure::Cap, BuildFailure::Mismatch, BuildFailure::Local];

    pub fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(b: u8) -> Option<BuildFailure> {
        BuildFailure::ALL.into_iter().find(|f| f.code() == b)
    }

    /// A reader's words for it (pe-api's history).
    pub fn name(self) -> &'static str {
        match self {
            BuildFailure::Compile => "compile error",
            BuildFailure::Refused => "refused input",
            BuildFailure::Cap => "memory cap",
            BuildFailure::Mismatch => "module mismatch",
            BuildFailure::Local => "local fault",
        }
    }
}

/// A script's pending commit (logic.md 6.2a; 7.9): the version a `CommitScript` wrote, which runs nowhere until a
/// quorum of the voters at its commit frame has attested its module. Kept, once voided, as the record of why, until
/// the asset's next commit replaces it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingCommit {
    /// The commit (its entry and op), which becomes the row's version at activation.
    pub version: Version,
    /// Who committed it: the re-key's loss rule is judged for them, at the commit and again at `F`.
    pub author: MemberKey,
    pub source: BlobRef,
    pub toolchain: Hash,
    pub sdk: Hash,
    pub author_module: Hash,
    /// The new version's component ids, strictly ascending, its tag among them.
    pub components: Vec<ComponentId>,
    /// `c`, the commit's frame.
    pub commit_frame: Frame,
    /// `D`: voided at `end_frame(D)` if not activated by then.
    pub deadline: Frame,
    /// The voters at `c` (the governance members then), in key order; the quorum is over them.
    pub voters: Vec<MemberKey>,
    /// The counted attestations, one per voter, in key order, each with its stamp. The author's own is the commit's.
    pub attested: Vec<(MemberKey, Frame)>,
    /// The counted `BuildFailed`s, one per voter, in key order, each with its stamp and reason; never a voter that has
    /// a counted attestation (a later success supersedes a failure while the commit is pending; a failure after a
    /// success is refused).
    pub failed: Vec<(MemberKey, Frame, BuildFailure)>,
    /// Set when voided: the frame and why. A voided commit no longer counts attestations.
    pub void: Option<(Frame, VoidKind)>,
}

impl PendingCommit {
    /// The version hash V it would bring.
    pub fn v(&self) -> Hash {
        Hash(self.source.hash)
    }

    /// Whether a quorum of its voters has attested it.
    pub fn attested_by_quorum(&self) -> bool {
        self.attested.len() >= quorum(self.voters.len())
    }

    /// The shared void rule for a leave and a failed build (logic.md 6.2a item 4): the quorum can no longer be
    /// reached, because the counted attestations plus the voters at `c` who are still members (`is_member`) and have
    /// neither attested nor failed fall short of `q`. The voters stay those at `c`, so an attestation given before a
    /// leave still counts.
    pub fn quorum_unreachable(&self, is_member: impl Fn(&MemberKey) -> bool) -> bool {
        let could = self.voters.iter().filter(|k| !self.has_attested(k) && !self.has_failed(k) && is_member(k)).count();
        self.attested.len() + could < quorum(self.voters.len())
    }

    pub fn has_attested(&self, k: &MemberKey) -> bool {
        self.attested.iter().any(|(a, _)| a == k)
    }

    pub fn has_failed(&self, k: &MemberKey) -> bool {
        self.failed.iter().any(|(a, _, _)| a == k)
    }

    /// Who could not build it and why, in key order: the author's notice on a void (Q1 (c), user ruling).
    pub fn failures(&self) -> Vec<(MemberKey, BuildFailure)> {
        self.failed.iter().map(|&(k, _, r)| (k, r)).collect()
    }

    fn write(&self, b: &mut Vec<u8>) {
        b.extend_from_slice(&self.version.entry.0.0);
        b.extend_from_slice(&self.version.op.to_le_bytes());
        b.extend_from_slice(&self.author.0);
        b.extend_from_slice(&self.source.hash);
        b.extend_from_slice(&self.source.len.to_le_bytes());
        b.extend_from_slice(&self.toolchain.0);
        b.extend_from_slice(&self.sdk.0);
        b.extend_from_slice(&self.author_module.0);
        b.extend_from_slice(&(self.components.len() as u32).to_le_bytes());
        for c in &self.components {
            b.extend_from_slice(&c.0.to_le_bytes());
        }
        b.extend_from_slice(&self.commit_frame.0.to_le_bytes());
        b.extend_from_slice(&self.deadline.0.to_le_bytes());
        b.extend_from_slice(&(self.voters.len() as u32).to_le_bytes());
        for k in &self.voters {
            b.extend_from_slice(&k.0);
        }
        b.extend_from_slice(&(self.attested.len() as u32).to_le_bytes());
        for (k, f) in &self.attested {
            b.extend_from_slice(&k.0);
            b.extend_from_slice(&f.0.to_le_bytes());
        }
        b.extend_from_slice(&(self.failed.len() as u32).to_le_bytes());
        for (k, f, r) in &self.failed {
            b.extend_from_slice(&k.0);
            b.extend_from_slice(&f.0.to_le_bytes());
            b.push(r.code());
        }
        match self.void {
            None => b.push(0),
            Some((f, k)) => {
                b.push(k as u8);
                b.extend_from_slice(&f.0.to_le_bytes());
            }
        }
    }

    fn read(r: &mut Rd) -> Option<PendingCommit> {
        let entry = EntryRef(Hash(r.arr()?));
        let op = u32::from_le_bytes(r.arr()?);
        let author = MemberKey(r.arr()?);
        let source = BlobRef { hash: r.arr()?, len: u64::from_le_bytes(r.arr()?) };
        let toolchain = Hash(r.arr()?);
        let sdk = Hash(r.arr()?);
        let author_module = Hash(r.arr()?);
        let n = u32::from_le_bytes(r.arr()?) as usize;
        if n > r.0.len() / 16 {
            return None;
        }
        let mut components = Vec::with_capacity(n);
        for _ in 0..n {
            let c = ComponentId(u128::from_le_bytes(r.arr()?));
            if components.last().is_some_and(|l: &ComponentId| *l >= c) {
                return None;
            }
            components.push(c);
        }
        let commit_frame = Frame(u64::from_le_bytes(r.arr()?));
        let deadline = Frame(u64::from_le_bytes(r.arr()?));
        let n = u32::from_le_bytes(r.arr()?) as usize;
        if n > r.0.len() / 32 {
            return None;
        }
        let mut voters: Vec<MemberKey> = Vec::with_capacity(n);
        for _ in 0..n {
            let k = MemberKey(r.arr()?);
            if voters.last().is_some_and(|l| l.0 >= k.0) {
                return None;
            }
            voters.push(k);
        }
        let n = u32::from_le_bytes(r.arr()?) as usize;
        if n > r.0.len() / 40 {
            return None;
        }
        let mut attested: Vec<(MemberKey, Frame)> = Vec::with_capacity(n);
        for _ in 0..n {
            let k = MemberKey(r.arr()?);
            let f = Frame(u64::from_le_bytes(r.arr()?));
            if attested.last().is_some_and(|(l, _)| l.0 >= k.0) || !voters.contains(&k) {
                return None;
            }
            attested.push((k, f));
        }
        let n = u32::from_le_bytes(r.arr()?) as usize;
        if n > r.0.len() / 41 {
            return None;
        }
        let mut failed: Vec<(MemberKey, Frame, BuildFailure)> = Vec::with_capacity(n);
        for _ in 0..n {
            let k = MemberKey(r.arr()?);
            let f = Frame(u64::from_le_bytes(r.arr()?));
            let why = BuildFailure::from_code(r.arr::<1>()?[0])?;
            let after = failed.last().is_none_or(|(l, _, _)| l.0 < k.0);
            if !after || !voters.contains(&k) || attested.iter().any(|(a, _)| *a == k) {
                return None;
            }
            failed.push((k, f, why));
        }
        let void = match r.arr::<1>()?[0] {
            0 => None,
            k => {
                let kind = VoidKind::from_u8(k)?;
                Some((Frame(u64::from_le_bytes(r.arr()?)), kind))
            }
        };
        Some(PendingCommit {
            version: Version { entry, op },
            author,
            source,
            toolchain,
            sdk,
            author_module,
            components,
            commit_frame,
            deadline,
            voters,
            attested,
            failed,
            void,
        })
    }
}

/// The modules table's value for one asset (6.6, logic.md 6.2a): its current row, its pending commit, or both.
/// `flags u8` (bit 0: current, bit 1: pending; at least one), then `u32 len ‖ current row` if present, then the
/// pending commit if present. Strict: anything else does not decode.
pub(crate) fn modules_value(current: Option<&ModuleRow>, pending: Option<&PendingCommit>) -> Vec<u8> {
    let mut b = vec![u8::from(current.is_some()) | (u8::from(pending.is_some()) << 1)];
    if let Some(m) = current {
        let row = m.to_bytes();
        b.extend_from_slice(&(row.len() as u32).to_le_bytes());
        b.extend_from_slice(&row);
    }
    if let Some(p) = pending {
        p.write(&mut b);
    }
    b
}

/// The inverse of [`modules_value`].
pub(crate) fn modules_value_from(b: &[u8]) -> Option<(Option<ModuleRow>, Option<PendingCommit>)> {
    let mut r = Rd(b);
    let flags = r.arr::<1>()?[0];
    if flags == 0 || flags > 3 {
        return None;
    }
    let current = if flags & 1 != 0 {
        let n = u32::from_le_bytes(r.arr()?) as usize;
        let (row, rest) = r.0.split_at_checked(n)?;
        r.0 = rest;
        Some(ModuleRow::from_bytes(row)?)
    } else {
        None
    };
    let pending = if flags & 2 != 0 { Some(PendingCommit::read(&mut r)?) } else { None };
    r.0.is_empty().then_some((current, pending))
}

struct Rd<'a>(&'a [u8]);

impl Rd<'_> {
    fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (h, t) = self.0.split_first_chunk::<N>()?;
        self.0 = t;
        Some(*h)
    }
}

/// The one id derivation for a script's component (3.2, 7.9): `u128::from_le_bytes(BLAKE3("pe.component" ‖ V ‖
/// lp(name) ‖ schema hash)[..16])`, where the schema hash is 6.6's hash of the description, which has no id in it.
/// `None` if the fields do not make a valid description. `Register`'s check and pe-replica's binding both call it.
pub fn script_component_id(v: &Hash, name: &str, fields: Vec<FieldDesc>) -> Option<ComponentId> {
    let d = ComponentDesc::new(ComponentId(0), name, fields).ok()?;
    let mut h = Hasher::new("pe.component");
    h.write_hash(v).write_len_prefixed(name.as_bytes()).write_hash(&crate::tree::schema_hash(&d));
    let id = u128::from_le_bytes(h.finish().0[..16].try_into().expect("16 bytes"));
    Some(ComponentId(id))
}

/// Version `v`'s tag (7.9): zero size, named [`TAG_NAME`].
pub fn tag_desc(v: &Hash) -> ComponentDesc {
    let id = script_component_id(v, TAG_NAME, Vec::new()).expect("an empty description is valid");
    ComponentDesc::new(id, TAG_NAME, Vec::new()).expect("an empty description is valid")
}

/// An engine component a script may name in its manifest (`pe.X`), and whether it may write it (3.2's table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineComponent {
    pub name: &'static str,
    pub id: ComponentId,
    pub scripts_may_write: bool,
}

/// The engine components a script may name (3.2): read any, write only those marked.
pub const ENGINE_COMPONENTS: [EngineComponent; 10] = [
    EngineComponent { name: "pe.Transform", id: ComponentId::TRANSFORM, scripts_may_write: true },
    EngineComponent { name: "pe.Light", id: ids::LIGHT, scripts_may_write: true },
    EngineComponent { name: "pe.Appearance", id: ids::APPEARANCE, scripts_may_write: true },
    EngineComponent { name: "pe.Name", id: ids::NAME, scripts_may_write: false },
    EngineComponent { name: "pe.Shape", id: ids::SHAPE, scripts_may_write: false },
    EngineComponent { name: "pe.Body", id: ids::BODY, scripts_may_write: false },
    EngineComponent { name: "pe.BodyState", id: ids::BODY_STATE, scripts_may_write: false },
    EngineComponent { name: "pe.Hand", id: ids::HAND, scripts_may_write: false },
    EngineComponent { name: "pe.SimSize", id: ids::SIM_SIZE, scripts_may_write: false },
    EngineComponent { name: "pe.Protection", id: ids::PROTECTION, scripts_may_write: false },
];

/// What a logic stage's runner reported (9.2a), in plain types.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogicReport {
    /// Objects created, in order, each with its spawner.
    pub created: Vec<(ObjectId, ObjectId)>,
    /// Every object the surviving destroys removed, in order.
    pub removed: Vec<ObjectId>,
    /// Untagged objects beneath a destroyed one, made roots in place.
    pub orphaned: Vec<ObjectId>,
    /// Rows written, in order.
    pub written: Vec<(ComponentId, ObjectId)>,
    /// Faulting assets with their fault's code, in order.
    pub faults: Vec<(AssetId, u8)>,
}

/// What a re-key does or would do (7.9): the objects it moves rows on, the row count, the losses and the objects
/// whose losses block the commit. `objects` and `blocked` are in id order, each object once, live and tombstoned
/// objects alike (a tombstone's rows are re-keyed too).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RekeyReport {
    pub objects: Vec<ObjectId>,
    pub rows: u32,
    pub losses: Vec<Loss>,
    pub blocked: Vec<ObjectId>,
}

/// One kind of data a re-key loses: a component's field zeroed or dropped, or a whole component dropped.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Loss {
    pub component: String,
    /// `None`: the whole component.
    pub field: Option<String>,
    pub zeroed: bool,
}

/// One row a re-key moves, live or held in a tombstone.
pub(crate) struct Move {
    object: ObjectId,
    from: ComponentId,
    to: Option<(ComponentId, Vec<u8>)>,
    /// The row is in the object's tombstone (7.5), not the store: `Undelete` must bring back the new version's rows.
    stone: bool,
}

/// Maps `row` of `old` onto `new` (7.9): a field with the same name, kind, count and unit copies; anything else starts
/// at zero. Returns the new row and the losses: a non-zero field zeroed or dropped. Padding is never a loss.
fn map_row(old: &ComponentDesc, new: &ComponentDesc, row: &[u8]) -> (Vec<u8>, Vec<Loss>) {
    let mut out = vec![0u8; new.size()];
    let mut losses = Vec::new();
    for f in old.fields().iter().filter(|f| !f.name.starts_with('_')) {
        let at = f.offset as usize;
        let len = f.kind.size() as usize * f.count as usize;
        let bytes = &row[at..at + len];
        let same = new.fields().iter().find(|g| g.name == f.name);
        match same {
            Some(g) if g.kind == f.kind && g.count == f.count && g.unit == f.unit => {
                out[g.offset as usize..g.offset as usize + len].copy_from_slice(bytes);
            }
            Some(_) | None if bytes.iter().all(|&b| b == 0) => {}
            Some(_) => losses.push(Loss { component: old.name().into(), field: Some(f.name.clone()), zeroed: true }),
            None => losses.push(Loss { component: old.name().into(), field: Some(f.name.clone()), zeroed: false }),
        }
    }
    (out, losses)
}

impl World {
    /// The modules table (5.4): each script asset's row.
    pub fn modules(&self) -> &BTreeMap<AssetId, ModuleRow> {
        &self.modules
    }

    /// The modules table's pending commits (logic.md 6.2a): versions waiting for a quorum's attestations, and the
    /// voided ones, kept as their record until the asset's next commit. None of them runs.
    pub fn pending(&self) -> &BTreeMap<AssetId, PendingCommit> {
        &self.pending
    }

    /// The asset that registered script component `c`, if it is one.
    pub fn script_asset(&self, c: ComponentId) -> Option<AssetId> {
        self.script_ids.get(&c).copied()
    }

    /// Whether `c` is a script's tag.
    pub(crate) fn is_tag(&self, c: ComponentId) -> bool {
        self.script_ids.contains_key(&c) && self.store.registry().get(c).is_some_and(|d| d.name() == TAG_NAME)
    }

    /// `Stale` if `c` is a script component whose asset's current version does not list it (7.9): an author who
    /// resolved names before a commit they had not received.
    pub(crate) fn superseded(&self, c: ComponentId) -> Result<(), Reason> {
        let Some(a) = self.script_ids.get(&c) else { return Ok(()) };
        match self.modules.get(a) {
            Some(m) if m.components.contains(&c) => Ok(()),
            m => {
                let current = m.map_or(Version { entry: EntryRef(Hash::ZERO), op: 0 }, |m| m.version);
                Err(Reason::Stale { precondition: crate::edit::OWN_CHECK, current, winner: current.entry })
            }
        }
    }

    /// Registers `components` for asset `asset` at version `v` (7.9's `Register`): every id derived from `v`, none
    /// in engine space, exactly one tag; the same description and asset again is a no-op, anything else under a
    /// registered id is a collision. `Invalid` otherwise, with nothing changed.
    pub(crate) fn register_script(
        &mut self,
        asset: &AssetId,
        v: &Hash,
        components: &[ComponentDesc],
    ) -> Result<(), Reason> {
        let invalid = || Reason::Invalid { field: None };
        if components.iter().filter(|d| d.name() == TAG_NAME).count() != 1 {
            return Err(invalid());
        }
        for d in components {
            let want = script_component_id(v, d.name(), d.fields().to_vec()).ok_or_else(invalid)?;
            // No `.` outside the tag, so no script component can pose as an engine one (`pe.X`, @chunks-reviewer's
            // code Low 1).
            let posing = d.name() != TAG_NAME && d.name().contains('.');
            if d.id() != want || d.id().0 < SCRIPT_ID_FLOOR || posing || (d.name() == TAG_NAME && d.size() != 0) {
                return Err(invalid());
            }
            match (self.script_ids.get(&d.id()), self.store.registry().get(d.id())) {
                (Some(a), Some(have)) if a == asset && have == d => {}
                (None, None) => {}
                _ => return Err(invalid()),
            }
        }
        for d in components {
            self.store.register(d.clone()).map_err(|_| invalid())?;
            Arc::make_mut(&mut self.script_ids).insert(d.id(), *asset);
        }
        Ok(())
    }

    /// What re-keying `old` (the base version's ids) onto `new` (the new version's descriptions) would do for
    /// `committer` (7.9), and the moves. Pure: a dry run (pe-api) calls it to show the cost before the commit.
    pub(crate) fn rekey_plan(
        &self,
        old: &[ComponentId],
        new: &[ComponentDesc],
        committer: MemberKey,
    ) -> (RekeyReport, Vec<Move>) {
        let mut report = RekeyReport::default();
        let mut moves = Vec::new();
        let mut objects = BTreeSet::new();
        let mut blocked = BTreeSet::new();
        let mut losses = BTreeSet::new();
        let reg = self.store.registry();
        let map = |od: &ComponentDesc, row: &[u8]| match new.iter().find(|d| d.name() == od.name()) {
            Some(nd) => {
                let (b, l) = map_row(od, nd, row);
                (Some((nd.id(), b)), l)
            }
            None if row.iter().all(|&b| b == 0) => (None, Vec::new()),
            None => (None, vec![Loss { component: od.name().into(), field: None, zeroed: false }]),
        };
        for &o in old {
            let Some(od) = reg.get(o) else { continue };
            let (ids, bytes) = self.store.column_raw(o);
            for (k, &obj) in ids.iter().enumerate() {
                let (to, lost) = map(od, &bytes[k * od.size()..(k + 1) * od.size()]);
                if !lost.is_empty() && !self.committer_may_edit(committer, obj) {
                    blocked.insert(obj);
                }
                losses.extend(lost);
                objects.insert(obj);
                report.rows = report.rows.saturating_add(1);
                moves.push(Move { object: obj, from: o, to, stone: false });
            }
        }
        // Tombstones hold rows too (@chunks-reviewer's code M1): left on the old ids, an `Undelete` would bring back
        // rows no system binds and no later commit re-keys. A loss there is judged by the protection the object had.
        for (&obj, stone) in self.tombstones.iter() {
            for (c, row, _) in &stone.rows {
                let Some(od) = old.contains(c).then(|| reg.get(*c)).flatten() else { continue };
                let (to, lost) = map(od, row);
                if !lost.is_empty() && self.stone_protected_by(obj).is_some_and(|o| o != committer) {
                    blocked.insert(obj);
                }
                losses.extend(lost);
                objects.insert(obj);
                report.rows = report.rows.saturating_add(1);
                moves.push(Move { object: obj, from: *c, to, stone: true });
            }
        }
        report.objects = objects.into_iter().collect();
        report.blocked = blocked.into_iter().collect();
        report.losses = losses.into_iter().collect();
        (report, moves)
    }

    /// Whether `who` could edit `obj` for 7.9's loss rule: not held by another member, and not protected (7.8) by
    /// another owner.
    fn committer_may_edit(&self, who: MemberKey, obj: ObjectId) -> bool {
        self.claims.get(&obj).is_none_or(|c| c.member == who) && self.protected_by(obj).is_none_or(|o| o == who)
    }

    /// The owner whose protection (7.8) covered a deleted object: its tombstone's own `Protection` row, else its
    /// nearest ancestor's, live or tombstoned.
    fn stone_protected_by(&self, obj: ObjectId) -> Option<MemberKey> {
        let mut at = obj;
        for _ in 0..=crate::world::DEPTH_LIMIT_MAX {
            let Some(stone) = self.tombstones.get(&at) else { return self.protected_by(at) };
            if let Some((_, row, _)) = stone.rows.iter().find(|(c, _, _)| *c == ids::PROTECTION) {
                return Some(MemberKey(row[..32].try_into().ok()?));
            }
            at = stone.parent?;
        }
        None
    }

    /// Applies `moves` as op `op` of entry `entry`: each moved row takes the commit as its row version and keeps its
    /// instance tag. Live objects are touched; a tombstone's rows are rewritten in place, in component-id order.
    pub(crate) fn apply_rekey(&mut self, moves: Vec<Move>, entry: EntryRef, op: u32) -> Result<(), Reason> {
        let v = RowVersion { entry: entry.0.0, op, tag: [0; 16], _pad: 0 };
        for m in moves {
            if m.stone {
                let stone =
                    Arc::make_mut(&mut self.tombstones).get_mut(&m.object).ok_or(Reason::Invalid { field: None })?;
                let k = stone.rows.iter().position(|(c, _, _)| *c == m.from).ok_or(Reason::Invalid { field: None })?;
                let (_, _, rv) = stone.rows.remove(k);
                if let Some((to, bytes)) = m.to {
                    stone.rows.push((to, bytes, RowVersion { tag: rv.tag, ..v }));
                    stone.rows.sort_by_key(|(c, _, _)| *c);
                }
                continue;
            }
            let tag = self.store.row_version(m.from, m.object).map_or([0; 16], |r| r.tag);
            self.store.remove_raw(m.from, m.object).map_err(|_| Reason::Invalid { field: None })?;
            if let Some((to, bytes)) = m.to {
                self.store.insert_raw(to, m.object, &bytes).map_err(|_| Reason::Invalid { field: None })?;
                self.store.set_row_version(to, m.object, RowVersion { tag, ..v });
            }
            self.touch(m.object, Version { entry, op }, false);
        }
        Ok(())
    }

    /// The per-row edit checks for a row a script writes to an engine component (7.9), with no author: the
    /// component must be marked writable, a hand carries no script's writes, a held object's pose is the holder's,
    /// and the row must be valid as an edit's would be. pe-replica passes the returned closure to the runner, which
    /// faults the call on an `Err`. The sets it needs are taken now, so it borrows nothing of the world.
    pub fn script_row_checker(&self) -> impl Fn(ComponentId, ObjectId, &[u8]) -> Result<(), String> + use<> {
        let held: BTreeSet<ObjectId> = self.claims.keys().copied().collect();
        let hands: BTreeSet<ObjectId> = self.store.column_raw(ids::HAND).0.iter().copied().collect();
        move |c, obj, bytes| {
            let e = ENGINE_COMPONENTS.iter().find(|e| e.id == c).ok_or("not an engine component a script names")?;
            if !e.scripts_may_write {
                return Err(format!("{} is not written by scripts", e.name));
            }
            if hands.contains(&obj) {
                return Err("a hand takes no script's writes".into());
            }
            if c == ComponentId::TRANSFORM && held.contains(&obj) {
                return Err("held".into());
            }
            if c == ids::LIGHT {
                let l: Light = bytemuck::try_pod_read_unaligned(bytes).map_err(|_| "Light size")?;
                if let Some(f) = l.invalid_angle() {
                    return Err(format!("Light field {f}"));
                }
            }
            Ok(())
        }
    }

    /// Runs one logic stage (9.2a): keeps the stage-start state, then calls `f` with the store, the guard (objects a
    /// script may not remove: held ones), the faulted assets and the row check. pe-replica builds pe-logic's
    /// `StageCtx` from them and returns the runner's report through `f`.
    pub fn logic_stage<R>(
        &mut self,
        f: impl FnOnce(
            &mut SimStore,
            &dyn Fn(ObjectId) -> bool,
            &BTreeSet<AssetId>,
            &dyn Fn(ComponentId, ObjectId, &[u8]) -> Result<(), String>,
        ) -> R,
    ) -> R {
        self.stage_start = Some(self.store.clone());
        let held: BTreeSet<ObjectId> = self.claims.keys().copied().collect();
        let guard = move |o: ObjectId| held.contains(&o);
        let faulted: BTreeSet<AssetId> =
            self.modules.iter().filter(|(_, m)| m.faulted.is_some()).map(|(a, _)| *a).collect();
        let check = self.script_row_checker();
        f(&mut self.store, &guard, &faulted, &check)
    }

    /// Records what a logic stage did (9.2a): tombstones for the removed objects from the stage-start state, `SimSize`
    /// for the created ones, the faulted marks, and the physics effects for pe-replica.
    pub fn after_logic(&mut self, frame: Frame, report: &LogicReport) -> Footprint {
        // Without `logic_stage` first there is no stage-start state and the removed objects would leave no tombstones:
        // a wiring slip, loud in debug builds (@chunks-reviewer's code Low 2).
        debug_assert!(self.stage_start.is_some(), "after_logic without logic_stage");
        let start = self.stage_start.take().unwrap_or_else(|| self.store.clone());
        let mut fp = Footprint::default();
        let expires =
            Frame(frame.0.saturating_add(self.rate.frames(std::time::Duration::from_secs(TOMBSTONE_SECONDS))));
        let removed: BTreeSet<ObjectId> = report.removed.iter().copied().collect();
        let mut places: BTreeMap<ObjectId, u32> = BTreeMap::new();
        let mut stones = Vec::new();
        for &d in &report.removed {
            // An object created and removed in this stage has no stage-start record and leaves no tombstone.
            if !start.exists(d) {
                continue;
            }
            let up = start.parent(d);
            let place = match up {
                Some(p) if !removed.contains(&p) => start.children(p).position(|c| c == d).unwrap_or(0) as u32,
                Some(p) => {
                    let n = places.entry(p).or_insert(0);
                    *n += 1;
                    *n - 1
                }
                None => 0,
            };
            stones.push((d, crate::apply::tombstone_of(&start, d, EntryRef(Hash::ZERO), frame, expires, up, place)));
        }
        if !stones.is_empty() {
            let t = Arc::make_mut(&mut self.tombstones);
            for (d, s) in stones {
                t.insert(d, s);
            }
        }
        if self.claims.keys().any(|o| removed.contains(o)) {
            Arc::make_mut(&mut self.claims).retain(|o, _| !removed.contains(o));
        }
        for &(id, spawner) in &report.created {
            if !self.store.exists(id) {
                continue;
            }
            let scale = self
                .store
                .world_transform(spawner)
                .map_or(1.0, |t| t.scale.iter().fold(0.0_f32, |m, s| m.max(s.abs())) as f64);
            let metres = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
            let _ = self.store.insert(id, SimSize { metres });
            fp.created.push(id);
        }
        for &(asset, code) in &report.faults {
            if let Some(m) = self.modules.get(&asset)
                && m.faulted.is_none()
                && (1..=13).contains(&code)
            {
                Arc::make_mut(&mut self.modules).get_mut(&asset).expect("present").faulted = Some((frame, code));
            }
        }
        fp.removed.extend(report.removed.iter().filter(|o| start.has::<Body>(**o)).copied());
        for &o in &report.orphaned {
            fp.reparented.push((o, true));
        }
        for &(c, o) in &report.written {
            if c == ComponentId::TRANSFORM && self.store.has::<Body>(o) && !fp.rows.contains(&(o, c)) {
                fp.rows.push((o, c));
            }
        }
        fp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> PendingCommit {
        let (a, b, c) = (MemberKey([1; 32]), MemberKey([2; 32]), MemberKey([3; 32]));
        PendingCommit {
            version: Version { entry: EntryRef(Hash([10; 32])), op: 1 },
            author: a,
            source: BlobRef { hash: [8; 32], len: 64 },
            toolchain: Hash([1; 32]),
            sdk: Hash([2; 32]),
            author_module: Hash([3; 32]),
            components: vec![ComponentId(1 << 100)],
            commit_frame: Frame(5),
            deadline: Frame(1205),
            voters: vec![a, b, c],
            attested: vec![(a, Frame(5))],
            failed: vec![(b, Frame(6), BuildFailure::Mismatch), (c, Frame(7), BuildFailure::Local)],
            void: Some((Frame(7), VoidKind::QuorumUnreachable)),
        }
    }

    /// The pending row's failures round-trip, and decoding is strict: out of order, a non-voter, a voter that also
    /// attested, or an unknown reason does not decode.
    #[test]
    fn failures_round_trip_and_decode_strictly() {
        let p = pending();
        assert_eq!(modules_value_from(&modules_value(None, Some(&p))), Some((None, Some(p.clone()))));
        let bad = |edit: &dyn Fn(&mut PendingCommit)| {
            let mut q = pending();
            edit(&mut q);
            modules_value_from(&modules_value(None, Some(&q)))
        };
        assert_eq!(bad(&|q| q.failed.reverse()), None, "out of order");
        assert_eq!(bad(&|q| q.failed[0].0 = MemberKey([9; 32])), None, "a non-voter");
        assert_eq!(bad(&|q| q.failed[0].0 = MemberKey([1; 32])), None, "the attester");
        let mut b = modules_value(None, Some(&p));
        let n = b.len();
        // The last failure's reason sits before the void (kind u8 + frame u64).
        b[n - 1 - 1 - 8] = 0;
        assert_eq!(modules_value_from(&b), None, "an unknown reason");
    }

    #[test]
    fn the_shared_void_rule_counts_neither_attested_nor_failed() {
        let mut p = pending();
        p.void = None;
        // q(3) = 2: A attested, B and C failed, so 1 + 0 < 2.
        assert!(p.quorum_unreachable(|_| true));
        p.failed.pop();
        assert!(!p.quorum_unreachable(|_| true), "C could still attest");
        assert!(p.quorum_unreachable(|k| *k != MemberKey([3; 32])), "unless C left");
    }
}

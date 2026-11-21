//! The edit payload (world-model.md 7.1) and its canonical codec.
//!
//! Every integer is little-endian; an `ObjectId` or `ComponentId` is its `u128`; options and bools are one byte, 0 or
//! 1; text is a `u16` length and UTF-8; byte strings are a `u32` length and the bytes; lists are a `u32` count and the
//! items. Decoding is total and strict: any other byte string (a bad tag, a flag of 2, a zero id, a length past the
//! end, trailing bytes, a limit broken) is refused, never panics, and each value has exactly one encoding, so
//! `encode(decode(b)) == b` for every accepted `b` (5.3).
//!
//! `Register` and `CommitScript` (7.9, slice D) carry a script version's component descriptions and its modules row.
//! Not in M1: `Local` or `Session` classes, which no edit creates (3.1).

use pe_core::commit::Hasher;
use pe_core::{ChainId, EntryRef, Frame, Hash, LineId, MemberKey, ObjectId};
use pe_seams::Version;
use pe_sim::{ComponentDesc, ComponentId, FieldDesc};

use crate::components::BlobRef;
use crate::script::BuildFailure;

/// The most ops a transaction may have (7.1).
pub const MAX_OPS: usize = 4096;
/// The most objects a transaction may create, counting each `Create` and every object of each `Copy`'s subtree (7.1;
/// @coordinator-opus). The one authoritative bound: pe-api uses it. Raise it only with a measured
/// reason. Over it, the transaction is refused with `TooLarge`.
pub const MAX_NEW_OBJECTS: usize = 65_536;
/// The most collision shapes (`Shape` rows, whatever object carries them) an edit may leave in the world (7.1). An edit
/// that would leave more, and more than the world had, is refused whole with `TooManyShapes`; one that adds none
/// always passes. Measured on the slowest M1 machine, the user's laptop (i7-8650U), with every ball of the pit awake:
/// stepping 520 shapes takes about half a frame period there (the M1 recording's stalls, 2026-11-20;
/// @coordinator-opus). A constant of this build, so the build id's source hash covers it: members of one
/// session all run the same sources (app.md 4). It moves only with a new measurement.
pub const MAX_SHAPES: usize = 520;
/// The `precondition` index a `Stale` refusal carries when an op's own check failed rather than a listed
/// precondition: a `Copy` whose source subtree no longer has the `size` its author saw (7.1).
pub const OWN_CHECK: u32 = u32::MAX;
/// The most bytes of row data a transaction may carry, apart from blobs (7.1).
pub const MAX_ROW_BYTES: usize = 1 << 20;
/// The longest encoded transaction: the entry's `MAX_PAYLOAD`, less room for [`crate::branch::MAX_REISSUE_DEPTH`]
/// re-issue prefixes, so any edit can be re-issued across that many branches (@diverge-eng's L1 on F3 L3). Longer
/// bytes do not decode (`Malformed`).
pub const MAX_EDIT_BYTES: usize =
    pe_seams::MAX_PAYLOAD - crate::branch::MAX_REISSUE_DEPTH * crate::branch::REISSUE_PREFIX;
/// The longest `label`, in bytes (7.1).
pub const MAX_LABEL: usize = 200;
/// The longest agent `harness` name, in bytes (7.1).
pub const MAX_HARNESS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    pub actor: Actor,
    pub label: Option<String>,
    pub preconditions: Vec<Precondition>,
    pub ops: Vec<Op>,
    pub blobs: Vec<BlobRef>,
    /// The bytes of the inline-class blobs the ops introduce (3.4, [`crate::inline`]), inside the signed entry: each
    /// 1 to 256 bytes, in ascending order of hash. Apply refuses a blob no op introduces, and over 2 KiB in all
    /// ([`crate::inline::MAX_INLINE_BYTES`]); pe-api refuses any edit over its own 1 KiB first.
    pub inline: Vec<Vec<u8>>,
}

/// Who produced the transaction (7.1). An honest client's statement: permissions come from the author alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Actor {
    Person,
    Agent { harness: String, principal: MemberKey, requested_by: Option<MemberKey> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Precondition {
    /// The row's edit version the author read.
    Version {
        id: ObjectId,
        component: ComponentId,
        instance: u16,
        is: Version,
    },
    /// The object version the author read: nothing about the object was edited since.
    ObjectVersion {
        id: ObjectId,
        is: Version,
    },
    Exists(ObjectId),
    Absent(ObjectId),
    /// `Hash::of("pe.content", row bytes)` of the row as the author read it; stale if any writer changed it.
    Content {
        id: ObjectId,
        component: ComponentId,
        instance: u16,
        hash: Hash,
    },
}

/// An object an op names: an existing one, the `k`-th this transaction creates, or the world's root (for a parent).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Existing(ObjectId),
    New(u32),
    Root,
}

/// A component row given whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub component: ComponentId,
    pub instance: u16,
    pub bytes: Vec<u8>,
}

/// The asset a `CommitScript` commits to (7.9; @coordinator-opus): an existing asset, or a new one
/// whose id is derived from the creating entry and `k`, its index among the entry's new assets
/// ([`crate::script::asset_created`]), as a new object's is. No op names a new asset's id itself, so a modified client
/// can't squat on another's pending id or reuse a destroyed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetTarget {
    Existing([u8; 32]),
    New(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Creates object `new` (its id derives from the entry and `new`, 2.1) under `parent`, at `index` among its
    /// children (`None`: last), with `rows`.
    Create { new: u32, parent: Target, index: Option<u32>, rows: Vec<Row> },
    /// Deletes the object and its subtree (7.5).
    Delete { id: Target },
    /// Adds or overwrites a row.
    SetRow { id: Target, row: Row },
    /// Sets one field, by its index in the component's description (fields sorted by offset), to `value`.
    SetField { id: Target, component: ComponentId, instance: u16, field: u16, value: Vec<u8> },
    /// Removes one component instance: the row at `component` and `instance`, if its instance tag is `tag`
    /// ([`row_tag`]). If that instance is already gone (no row, or a row added again since, with another tag), the op
    /// changes nothing and its outcome notes "no effect" (7.6), so a stale remove cannot take out a later row.
    RemoveRow { id: Target, component: ComponentId, instance: u16, tag: u128 },
    /// Moves `id` under `parent` at `index` (`None`: last). `keep_world` keeps its world pose; otherwise its local
    /// transform is kept (3.3).
    Reparent { id: Target, parent: Target, index: Option<u32>, keep_world: bool },
    /// Copies `source` and its subtree under `parent`. The copies take `New` indices `new .. new + size` in
    /// depth-first order, children in their order (7.1). `size` is the number of objects in the subtree as the author
    /// read it. If the subtree has another size now, the indices would name other objects, so the op is refused as
    /// `Stale` with `precondition` [`OWN_CHECK`] (@coordinator-opus).
    Copy { source: Target, new: u32, parent: Target, index: Option<u32>, size: u32 },
    /// Restores a deleted subtree from its tombstones, with the same ids (7.5).
    Undelete { id: ObjectId },
    /// Registers a script version's components, its tag included, with the transaction's one `CommitScript` (7.9).
    Register { components: Vec<ComponentDesc> },
    /// Commits a version of script `script` (an asset id): writes its modules row and re-keys the old version's rows
    /// (7.9). `base` is the version the author edited (`None`: a new script); `activate_at` must be the entry's stamp.
    /// `archive` is the version's source, a canonical archive (logic.md 4.1a), carried inside the entry so every
    /// machine that applies the commit (live, re-simulating, replaying) holds it (7.9); `source` is its reference,
    /// whose hash is the version V. Only `archive` is encoded: decoding derives `source` from it, so the two always
    /// agree on the wire. Build one with [`Op::commit_script`].
    CommitScript {
        script: AssetTarget,
        base: Option<Version>,
        source: BlobRef,
        archive: bytes::Bytes,
        toolchain: Hash,
        sdk: Hash,
        author_module: Hash,
        activate_at: u64,
    },
    /// A voter's attestation that it built version `version` (V) of script `script` into module `module`, passing
    /// every check of logic.md 6.2 step 3 (6.2a; 7.9). The only op of its transaction; it needs no `EDIT`, only to
    /// be a voter of the pending commit.
    AttestBuild { script: [u8; 32], version: Hash, module: Hash },
    /// A voter's statement that its build of version `version` (V) of script `script` failed, after its local
    /// retries (logic.md 6.2a item 2; 7.9): why, this machine's toolchain and SDK hashes, and the module hash a
    /// `Mismatch` built. Never compiler output. The only op of its transaction; it needs no `EDIT`.
    BuildFailed {
        script: [u8; 32],
        version: Hash,
        reason: BuildFailure,
        toolchain: Hash,
        sdk: Hash,
        module: Option<Hash>,
    },
}

impl Op {
    /// A `CommitScript` whose `source` is `archive`'s reference.
    pub fn commit_script(
        script: AssetTarget,
        base: Option<Version>,
        archive: bytes::Bytes,
        toolchain: Hash,
        sdk: Hash,
        author_module: Hash,
        activate_at: u64,
    ) -> Op {
        Op::CommitScript {
            script,
            base,
            source: BlobRef::of(&archive),
            archive,
            toolchain,
            sdk,
            author_module,
            activate_at,
        }
    }
}

/// The most components one `Register` may carry.
pub const MAX_REGISTER: usize = 256;

/// The instance tag of a row that op `op` of an entry adds (7.6): `BLAKE3("pe.row" ‖ line ‖ author ‖ chain ‖ seq ‖
/// op)[..16]`, from the same coordinates as [`ObjectId::created`], so an author can name a row its own transaction
/// adds. A row no edit added has tag 0. Comparisons are only within one object, component and instance, so a tag
/// need not be unique beyond that.
pub fn row_tag(line: LineId, author: MemberKey, chain: ChainId, seq: u64, op: u32) -> u128 {
    let mut h = Hasher::new("pe.row");
    h.write_hash(&line.0).write(&author.0).write_hash(&chain.0).write_u64(seq).write_u32(op);
    let h = h.finish().0;
    let mut b = [0; 16];
    b.copy_from_slice(&h[..16]);
    // Zero means "no edit added it", so an edit's tag is never zero.
    u128::from_le_bytes(b).max(1)
}

/// The context of [`Precondition::Content`] hashes.
pub const CONTENT_CONTEXT: &str = "pe.content";

/// The content hash of a row's bytes, as [`Precondition::Content`] names it.
pub fn content_hash(row: &[u8]) -> Hash {
    Hash::of(CONTENT_CONTEXT, row)
}

// ---- encoding ------------------------------------------------------------------------------------------------------

pub(crate) struct W(pub(crate) Vec<u8>);

impl W {
    pub(crate) fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub(crate) fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn u128(&mut self, v: u128) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn raw(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    pub(crate) fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }
    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.raw(b);
    }
    pub(crate) fn text(&mut self, s: &str) {
        self.u16(u16::try_from(s.len()).unwrap_or(u16::MAX));
        self.raw(s.as_bytes());
    }
    pub(crate) fn id(&mut self, id: ObjectId) {
        self.u128(id.to_u128());
    }
    pub(crate) fn opt_u32(&mut self, v: Option<u32>) {
        match v {
            None => self.u8(0),
            Some(x) => {
                self.u8(1);
                self.u32(x);
            }
        }
    }
    pub(crate) fn version(&mut self, v: &Version) {
        self.raw(&v.entry.0.0);
        self.u32(v.op);
    }
    pub(crate) fn target(&mut self, t: Target) {
        match t {
            Target::Existing(id) => {
                self.u8(0);
                self.id(id);
            }
            Target::New(k) => {
                self.u8(1);
                self.u32(k);
            }
            Target::Root => self.u8(2),
        }
    }
}

impl Transaction {
    /// Sets every `CommitScript`'s `activate_at` to `frame`, which must be the frame the entry is applied at (its
    /// stamp), or the commit is refused `Invalid` (7.9). The submitting machine calls it with the entry's stamp before
    /// encoding, since only it knows that frame (with stamp-ahead, the present plus a lead); a dry run calls it with
    /// its own frame (@engine-architect, agreed with @api-eng).
    pub fn set_activation(&mut self, frame: Frame) {
        for op in &mut self.ops {
            if let Op::CommitScript { activate_at, .. } = op {
                *activate_at = frame.0;
            }
        }
    }

    /// The canonical bytes. A transaction that breaks a limit encodes, but its bytes do not decode.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        match &self.actor {
            Actor::Person => w.u8(0),
            Actor::Agent { harness, principal, requested_by } => {
                w.u8(1);
                w.text(harness);
                w.raw(&principal.0);
                match requested_by {
                    None => w.u8(0),
                    Some(k) => {
                        w.u8(1);
                        w.raw(&k.0);
                    }
                }
            }
        }
        match &self.label {
            None => w.u8(0),
            Some(l) => {
                w.u8(1);
                w.text(l);
            }
        }
        w.len(self.preconditions.len());
        for p in &self.preconditions {
            match p {
                Precondition::Version { id, component, instance, is } => {
                    w.u8(0);
                    w.id(*id);
                    w.u128(component.0);
                    w.u16(*instance);
                    w.version(is);
                }
                Precondition::ObjectVersion { id, is } => {
                    w.u8(1);
                    w.id(*id);
                    w.version(is);
                }
                Precondition::Exists(id) => {
                    w.u8(2);
                    w.id(*id);
                }
                Precondition::Absent(id) => {
                    w.u8(3);
                    w.id(*id);
                }
                Precondition::Content { id, component, instance, hash } => {
                    w.u8(4);
                    w.id(*id);
                    w.u128(component.0);
                    w.u16(*instance);
                    w.raw(&hash.0);
                }
            }
        }
        w.len(self.ops.len());
        for op in &self.ops {
            match op {
                Op::Create { new, parent, index, rows } => {
                    w.u8(0);
                    w.u32(*new);
                    w.target(*parent);
                    w.opt_u32(*index);
                    w.len(rows.len());
                    for r in rows {
                        w.u128(r.component.0);
                        w.u16(r.instance);
                        w.bytes(&r.bytes);
                    }
                }
                Op::Delete { id } => {
                    w.u8(1);
                    w.target(*id);
                }
                Op::SetRow { id, row } => {
                    w.u8(2);
                    w.target(*id);
                    w.u128(row.component.0);
                    w.u16(row.instance);
                    w.bytes(&row.bytes);
                }
                Op::SetField { id, component, instance, field, value } => {
                    w.u8(3);
                    w.target(*id);
                    w.u128(component.0);
                    w.u16(*instance);
                    w.u16(*field);
                    w.bytes(value);
                }
                Op::RemoveRow { id, component, instance, tag } => {
                    w.u8(4);
                    w.target(*id);
                    w.u128(component.0);
                    w.u16(*instance);
                    w.u128(*tag);
                }
                Op::Reparent { id, parent, index, keep_world } => {
                    w.u8(5);
                    w.target(*id);
                    w.target(*parent);
                    w.opt_u32(*index);
                    w.u8(*keep_world as u8);
                }
                Op::Copy { source, new, parent, index, size } => {
                    w.u8(6);
                    w.target(*source);
                    w.u32(*new);
                    w.target(*parent);
                    w.opt_u32(*index);
                    w.u32(*size);
                }
                Op::Undelete { id } => {
                    w.u8(7);
                    w.id(*id);
                }
                Op::Register { components } => {
                    w.u8(8);
                    w.len(components.len());
                    for d in components {
                        w.u128(d.id().0);
                        w.text(d.name());
                        w.len(d.fields().len());
                        for f in d.fields() {
                            w.text(&f.name);
                            w.u8(crate::tree::field_byte(f));
                            w.u32(f.offset);
                            w.u32(f.count);
                            w.text(&f.unit);
                        }
                    }
                }
                Op::CommitScript { script, base, archive, toolchain, sdk, author_module, activate_at, .. } => {
                    w.u8(9);
                    match script {
                        AssetTarget::Existing(a) => {
                            w.u8(0);
                            w.raw(a);
                        }
                        AssetTarget::New(k) => {
                            w.u8(1);
                            w.raw(&k.to_le_bytes());
                        }
                    }
                    match base {
                        None => w.u8(0),
                        Some(v) => {
                            w.u8(1);
                            w.version(v);
                        }
                    }
                    // The archive, not its reference: the reference is derived on decoding (7.9).
                    w.len(archive.len());
                    w.raw(archive);
                    w.raw(&toolchain.0);
                    w.raw(&sdk.0);
                    w.raw(&author_module.0);
                    w.raw(&activate_at.to_le_bytes());
                }
                Op::AttestBuild { script, version, module } => {
                    w.u8(10);
                    w.raw(script);
                    w.raw(&version.0);
                    w.raw(&module.0);
                }
                Op::BuildFailed { script, version, reason, toolchain, sdk, module } => {
                    w.u8(11);
                    w.raw(script);
                    w.raw(&version.0);
                    w.u8(reason.code());
                    w.raw(&toolchain.0);
                    w.raw(&sdk.0);
                    match module {
                        None => w.u8(0),
                        Some(m) => {
                            w.u8(1);
                            w.raw(&m.0);
                        }
                    }
                }
            }
        }
        w.len(self.blobs.len());
        for b in &self.blobs {
            w.raw(&b.hash);
            w.raw(&b.len.to_le_bytes());
        }
        // The inline blobs last, and only when there are any (3.4), so a transaction without them encodes as it
        // always did: its entry, and the ids derived from it, are unchanged.
        if !self.inline.is_empty() {
            w.len(self.inline.len());
            for b in &self.inline {
                w.len(b.len());
                w.raw(b);
            }
        }
        w.0
    }

    /// Decodes canonical bytes. `None` for anything else (7.6 step 1: `Malformed`).
    pub fn decode(bytes: &[u8]) -> Option<Transaction> {
        if bytes.len() > MAX_EDIT_BYTES {
            return None;
        }
        let mut r = R { b: bytes, row_bytes: 0 };
        let tx = r.transaction()?;
        r.b.is_empty().then_some(tx)
    }
}

// ---- decoding ------------------------------------------------------------------------------------------------------

pub(crate) struct R<'a> {
    pub(crate) b: &'a [u8],
    pub(crate) row_bytes: usize,
}

impl<'a> R<'a> {
    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.b.len() {
            return None;
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Some(h)
    }
    pub(crate) fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
    pub(crate) fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    pub(crate) fn flag(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    pub(crate) fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.arr()?))
    }
    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.arr()?))
    }
    pub(crate) fn u128(&mut self) -> Option<u128> {
        Some(u128::from_le_bytes(self.arr()?))
    }
    pub(crate) fn id(&mut self) -> Option<ObjectId> {
        ObjectId::from_u128(self.u128()?).some()
    }
    pub(crate) fn component(&mut self) -> Option<ComponentId> {
        Some(ComponentId(self.u128()?))
    }
    pub(crate) fn key(&mut self) -> Option<MemberKey> {
        Some(MemberKey(self.arr()?))
    }
    /// A count of items each at least `min` bytes long, so a count can never ask for more than the bytes left.
    pub(crate) fn count(&mut self, min: usize, max: usize) -> Option<usize> {
        let n = self.u32()? as usize;
        (n <= max && n.checked_mul(min)? <= self.b.len()).then_some(n)
    }
    pub(crate) fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        self.row_bytes = self.row_bytes.checked_add(n)?;
        if self.row_bytes > MAX_ROW_BYTES {
            return None;
        }
        Some(self.take(n)?.to_vec())
    }
    pub(crate) fn text(&mut self, max: usize) -> Option<String> {
        let n = self.u16()? as usize;
        if n > max {
            return None;
        }
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
    pub(crate) fn opt_u32(&mut self) -> Option<Option<u32>> {
        Some(if self.flag()? { Some(self.u32()?) } else { None })
    }
    pub(crate) fn version(&mut self) -> Option<Version> {
        Some(Version { entry: EntryRef(Hash(self.arr()?)), op: self.u32()? })
    }
    pub(crate) fn target(&mut self) -> Option<Target> {
        Some(match self.u8()? {
            0 => Target::Existing(self.id()?),
            1 => Target::New(self.u32()?),
            2 => Target::Root,
            _ => return None,
        })
    }

    pub(crate) fn transaction(&mut self) -> Option<Transaction> {
        let actor = match self.u8()? {
            0 => Actor::Person,
            1 => {
                let harness = self.text(MAX_HARNESS)?;
                let principal = self.key()?;
                let requested_by = if self.flag()? { Some(self.key()?) } else { None };
                Actor::Agent { harness, principal, requested_by }
            }
            _ => return None,
        };
        let label = if self.flag()? { Some(self.text(MAX_LABEL)?) } else { None };
        let n = self.count(17, MAX_OPS)?;
        let mut preconditions = Vec::with_capacity(n);
        for _ in 0..n {
            preconditions.push(match self.u8()? {
                0 => Precondition::Version {
                    id: self.id()?,
                    component: self.component()?,
                    instance: self.u16()?,
                    is: self.version()?,
                },
                1 => Precondition::ObjectVersion { id: self.id()?, is: self.version()? },
                2 => Precondition::Exists(self.id()?),
                3 => Precondition::Absent(self.id()?),
                4 => Precondition::Content {
                    id: self.id()?,
                    component: self.component()?,
                    instance: self.u16()?,
                    hash: Hash(self.arr()?),
                },
                _ => return None,
            });
        }
        let n = self.count(2, MAX_OPS)?;
        let mut ops = Vec::with_capacity(n);
        for _ in 0..n {
            ops.push(match self.u8()? {
                0 => {
                    let new = self.u32()?;
                    let parent = self.target()?;
                    let index = self.opt_u32()?;
                    let m = self.count(22, MAX_OPS)?;
                    let mut rows = Vec::with_capacity(m);
                    for _ in 0..m {
                        rows.push(Row { component: self.component()?, instance: self.u16()?, bytes: self.bytes()? });
                    }
                    Op::Create { new, parent, index, rows }
                }
                1 => Op::Delete { id: self.target()? },
                2 => Op::SetRow {
                    id: self.target()?,
                    row: Row { component: self.component()?, instance: self.u16()?, bytes: self.bytes()? },
                },
                3 => Op::SetField {
                    id: self.target()?,
                    component: self.component()?,
                    instance: self.u16()?,
                    field: self.u16()?,
                    value: self.bytes()?,
                },
                4 => Op::RemoveRow {
                    id: self.target()?,
                    component: self.component()?,
                    instance: self.u16()?,
                    tag: self.u128()?,
                },
                5 => Op::Reparent {
                    id: self.target()?,
                    parent: self.target()?,
                    index: self.opt_u32()?,
                    keep_world: self.flag()?,
                },
                6 => Op::Copy {
                    source: self.target()?,
                    new: self.u32()?,
                    parent: self.target()?,
                    index: self.opt_u32()?,
                    size: self.u32()?,
                },
                7 => Op::Undelete { id: self.id()? },
                8 => {
                    let n = self.count(1, MAX_REGISTER)?;
                    let mut components = Vec::with_capacity(n);
                    for _ in 0..n {
                        let id = ComponentId(u128::from_le_bytes(self.arr()?));
                        let name = self.text(u16::MAX as usize)?;
                        let k = self.count(13, 4096)?;
                        let mut fields = Vec::with_capacity(k);
                        for _ in 0..k {
                            let fname = self.text(u16::MAX as usize)?;
                            let (kind, color) = crate::tree::field_of_byte(self.u8()?)?;
                            let offset = self.u32()?;
                            let count = self.u32()?;
                            let unit = self.text(u16::MAX as usize)?;
                            let mut f = FieldDesc::new(&fname, kind, offset as usize, count).with_unit(&unit);
                            f.color = color;
                            fields.push(f);
                        }
                        let d = ComponentDesc::new(id, &name, fields.clone()).ok()?;
                        // One encoding per value: the fields as given must be the description's own, in offset order.
                        if d.fields() != fields.as_slice() {
                            return None;
                        }
                        components.push(d);
                    }
                    Op::Register { components }
                }
                9 => {
                    let script = match self.u8()? {
                        0 => AssetTarget::Existing(self.arr()?),
                        1 => AssetTarget::New(u32::from_le_bytes(self.arr()?)),
                        _ => return None,
                    };
                    let base = if self.flag()? { Some(self.version()?) } else { None };
                    // Not row bytes: the payload's own bound limits it, and apply refuses it over
                    // `MAX_SOURCE_BYTES` as `TooLarge` (7.9).
                    let n = self.u32()? as usize;
                    let archive = bytes::Bytes::copy_from_slice(self.take(n)?);
                    Op::CommitScript {
                        script,
                        base,
                        source: BlobRef::of(&archive),
                        archive,
                        toolchain: Hash(self.arr()?),
                        sdk: Hash(self.arr()?),
                        author_module: Hash(self.arr()?),
                        activate_at: u64::from_le_bytes(self.arr()?),
                    }
                }
                10 => Op::AttestBuild { script: self.arr()?, version: Hash(self.arr()?), module: Hash(self.arr()?) },
                11 => Op::BuildFailed {
                    script: self.arr()?,
                    version: Hash(self.arr()?),
                    reason: BuildFailure::from_code(self.u8()?)?,
                    toolchain: Hash(self.arr()?),
                    sdk: Hash(self.arr()?),
                    module: if self.flag()? { Some(Hash(self.arr()?)) } else { None },
                },
                _ => return None,
            });
        }
        let n = self.count(40, MAX_OPS)?;
        let mut blobs = Vec::with_capacity(n);
        for _ in 0..n {
            blobs.push(BlobRef { hash: self.arr()?, len: u64::from_le_bytes(self.arr()?) });
        }
        // The inline blobs (3.4): absent when there are none, and otherwise one encoding per value, so at least one,
        // each 1 to 256 bytes, in ascending order of hash. The section is the payload's last.
        let present = !self.b.is_empty();
        let n = if present { self.count(5, crate::inline::MAX_INLINE_BYTES)? } else { 0 };
        if present && n == 0 {
            return None;
        }
        let mut inline = Vec::with_capacity(n);
        for _ in 0..n {
            let len = self.u32()? as usize;
            if len > crate::inline::MAX_INLINE_BLOB {
                return None;
            }
            inline.push(self.take(len)?.to_vec());
        }
        if !crate::inline::canonical(&inline) {
            return None;
        }
        Some(Transaction { actor, label, preconditions, ops, blobs, inline })
    }
}

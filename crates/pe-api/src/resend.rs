//! The re-send backstop's work on one transaction (api.md 9.4): where its `ObjectRef`s and fields sit, what it
//! names, rewriting it to the entries now carrying earlier changes, the later-word rule, the precondition move and
//! the re-point follow-up. Pure functions over pe-world's payload; the state machine is in [`crate::changes`].
//!
//! - **Slots** are read from the schema when `apply` submits the change, so preparing a re-send on the simulation
//!   thread needs no schema.
//! - **The map** takes every object an earlier change created, and every version it wrote, from the entry that
//!   carried it before to the entry carrying it now: ids derive from the chain, the seq and the index k (api.md 4.3),
//!   versions from the entry and the op.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use pe_core::{ChainId, EntryRef, LineId, MemberKey, ObjectId};
use pe_seams::Version;
use pe_sim::{ComponentDesc, ComponentId, FieldKind};
use pe_world::edit::{Op, Precondition, Row, Target, Transaction};

/// The one-minute bound: automatic re-sends stop this long after the change's first submission (api.md 9.4;
/// coordinator (a)).
pub const BOUND: Duration = Duration::from_secs(60);

/// The wait before re-send number `n` (1 is the first, at once): 0.5 s, doubling to 16 s, then every 16 s [I].
pub fn backoff(n: u32) -> Duration {
    if n <= 1 { Duration::ZERO } else { Duration::from_millis(500u64 << (n - 2).min(5)) }
}

/// `RefSlot::row` for a `SetField`'s value.
pub const VALUE: u32 = u32::MAX;

/// Where one `ObjectRef` element sits: op `op`, its `row`-th row (or [`VALUE`]), at byte `at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefSlot {
    pub op: u32,
    pub row: u32,
    pub at: u32,
    /// The field holding it, and the field's place in the slot's bytes (offset, length): a follow-up writes only
    /// that field (@design-reviewer's Low 1).
    pub field: u16,
    pub span: (u32, u32),
}

/// A slot's field and that field's (offset, length) in the slot's bytes.
type RefField = (u16, (u32, u32));

/// What pe-api reads of a transaction from the schema at submission.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Slots {
    pub refs: Vec<RefSlot>,
    /// Per op: a `SetField`'s place in its row (offset, length), to patch a later value into a whole row.
    pub field_at: Vec<Option<(u32, u32)>>,
}

/// The slots of `tx` under `schema`. A component the schema doesn't know has none.
pub fn slots<'a>(tx: &Transaction, schema: impl Iterator<Item = &'a ComponentDesc>) -> Slots {
    let descs: HashMap<ComponentId, &ComponentDesc> = schema.map(|d| (d.id(), d)).collect();
    let mut s = Slots { refs: vec![], field_at: vec![None; tx.ops.len()] };
    let row_refs = |s: &mut Slots, op: u32, ri: u32, row: &Row| {
        if let Some(d) = descs.get(&row.component) {
            for (fi, f) in d.fields().iter().enumerate().filter(|(_, f)| f.kind == FieldKind::ObjectRef) {
                for e in 0..f.count {
                    let (field, span) = (fi as u16, (f.offset, 16 * f.count));
                    s.refs.push(RefSlot { op, row: ri, at: f.offset + 16 * e, field, span });
                }
            }
        }
    };
    for (i, op) in tx.ops.iter().enumerate() {
        let i = i as u32;
        match op {
            Op::Create { rows, .. } => {
                for (ri, r) in rows.iter().enumerate() {
                    row_refs(&mut s, i, ri as u32, r);
                }
            }
            Op::SetRow { row, .. } => row_refs(&mut s, i, 0, row),
            Op::SetField { component, field, .. } => {
                if let Some(f) = descs.get(component).and_then(|d| d.fields().get(usize::from(*field))) {
                    s.field_at[i as usize] = Some((f.offset, f.kind.size() * f.count));
                    if f.kind == FieldKind::ObjectRef {
                        for e in 0..f.count {
                            s.refs.push(RefSlot {
                                op: i,
                                row: VALUE,
                                at: 16 * e,
                                field: *field,
                                span: (0, 16 * f.count),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    s
}

/// An `ObjectRef` in row bytes: `hi`, then `lo`, each little-endian (world-model.md 3.2).
fn id_at(bytes: &[u8], at: usize) -> Option<ObjectId> {
    let b = bytes.get(at..at.checked_add(16)?)?;
    let hi = u64::from_le_bytes(b[..8].try_into().ok()?);
    let lo = u64::from_le_bytes(b[8..].try_into().ok()?);
    Some(ObjectId::from_u128((u128::from(hi) << 64) | u128::from(lo)))
}

fn put_id(bytes: &mut [u8], at: usize, id: ObjectId) {
    let v = id.to_u128();
    bytes[at..at + 8].copy_from_slice(&((v >> 64) as u64).to_le_bytes());
    bytes[at + 8..at + 16].copy_from_slice(&(v as u64).to_le_bytes());
}

/// The bytes a slot names, read-only.
fn slot_ref<'a>(ops: &'a [Op], s: &RefSlot) -> Option<&'a [u8]> {
    match (ops.get(s.op as usize)?, s.row) {
        (Op::SetField { value, .. }, VALUE) => Some(value),
        (Op::Create { rows, .. }, r) => rows.get(r as usize).map(|r| &r.bytes[..]),
        (Op::SetRow { row, .. }, 0) => Some(&row.bytes),
        _ => None,
    }
}

/// The bytes a slot names, if the op has them.
fn slot_bytes<'a>(ops: &'a mut [Op], s: &RefSlot) -> Option<&'a mut Vec<u8>> {
    match (ops.get_mut(s.op as usize)?, s.row) {
        (Op::SetField { value, .. }, VALUE) => Some(value),
        (Op::Create { rows, .. }, r) => rows.get_mut(r as usize).map(|r| &mut r.bytes),
        (Op::SetRow { row, .. }, 0) => Some(&mut row.bytes),
        _ => None,
    }
}

/// The `New` indices a transaction creates: each `Create`'s, and a `Copy`'s block.
pub fn new_indices(tx: &Transaction) -> impl Iterator<Item = u32> + '_ {
    tx.ops.iter().flat_map(|op| match op {
        Op::Create { new, .. } => *new..new.saturating_add(1),
        Op::Copy { new, size, .. } => *new..new.saturating_add(*size),
        _ => 0..0,
    })
}

/// A script commit, a registration or an attestation: never re-sent in M1 (api.md 9.4).
pub fn is_code(tx: &Transaction) -> bool {
    tx.ops.iter().any(|op| {
        matches!(op, Op::Register { .. } | Op::CommitScript { .. } | Op::AttestBuild { .. } | Op::BuildFailed { .. })
    })
}

/// A rough size, for the memory cap on kept transactions.
pub fn bytes(tx: &Transaction) -> usize {
    let op = |op: &Op| match op {
        Op::Create { rows, .. } => rows.iter().map(|r| r.bytes.len() + 24).sum(),
        Op::SetRow { row, .. } => row.bytes.len(),
        Op::SetField { value, .. } => value.len(),
        Op::CommitScript { archive, .. } => archive.len(),
        Op::Register { components } => components.len() * 256,
        _ => 0,
    } + 48;
    tx.ops.iter().map(op).sum::<usize>() + tx.preconditions.len() * 80 + 128
}

/// One entry that carried a change: the original, or a re-send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Carrier {
    pub entry: EntryRef,
    pub chain: ChainId,
    pub seq: u64,
}

/// Objects and versions, from the entries that carried earlier changes to the entries carrying them now.
#[derive(Clone, Debug, Default)]
pub struct Map {
    pub ids: HashMap<ObjectId, ObjectId>,
    pub entries: HashMap<EntryRef, EntryRef>,
    /// For an entry whose ops sit at other places in the entry now carrying it (a re-send dropped some): each op's
    /// place there, `None` if it was dropped. Absent: the same places.
    pub ops: HashMap<EntryRef, Arc<[Option<u32>]>>,
}

impl Map {
    /// What `tx` created and wrote when `from` carried it is now `to`'s. `from_back`/`to_back`: each of that
    /// carrier's ops' place in `tx`, the original (`None` for the original itself); a version moves to the op's
    /// place in `to`, and one of an op `to` dropped stays as it was, never to exist (@design-reviewer's M1).
    pub fn carry(
        &mut self,
        (line, author, tx): (LineId, MemberKey, &Transaction),
        from: &Carrier,
        from_back: Option<&[u32]>,
        to: &Carrier,
        to_back: Option<&[u32]>,
    ) {
        if from.entry == to.entry {
            return;
        }
        self.entries.insert(from.entry, to.entry);
        let n = from_back.map_or(tx.ops.len(), |b| b.len());
        let place: Vec<Option<u32>> = (0..n)
            .map(|i| {
                let orig = from_back.map_or(i as u32, |b| b[i]);
                to_back.map_or(Some(orig), |b| b.iter().position(|&o| o == orig).map(|j| j as u32))
            })
            .collect();
        if place.iter().enumerate().any(|(i, j)| *j != Some(i as u32)) {
            self.ops.insert(from.entry, place.into());
        }
        for k in new_indices(tx) {
            self.ids.insert(
                ObjectId::created(line, author, from.chain, from.seq, k),
                ObjectId::created(line, author, to.chain, to.seq, k),
            );
        }
    }

    /// What `tx` created and wrote when `from` carried it, with nowhere to go yet (it waits to be re-sent): for
    /// finding what depends on it.
    pub fn name(&mut self, line: LineId, author: MemberKey, tx: &Transaction, from: &Carrier) {
        self.entries.insert(from.entry, from.entry);
        for k in new_indices(tx) {
            let id = ObjectId::created(line, author, from.chain, from.seq, k);
            self.ids.insert(id, id);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty() && self.entries.is_empty()
    }

    fn id(&self, o: ObjectId) -> ObjectId {
        self.ids.get(&o).copied().unwrap_or(o)
    }

    fn version(&self, v: Version) -> Version {
        let Some(&entry) = self.entries.get(&v.entry) else { return v };
        match self.ops.get(&v.entry).map(|p| p.get(v.op as usize).copied().flatten()) {
            None => Version { entry, op: v.op },
            Some(Some(op)) => Version { entry, op },
            // Its op was dropped: the author's own later change wrote there instead, so the version stays as it
            // was, and what names it is refused as stale (api.md 9.4).
            Some(None) => v,
        }
    }

    fn target(&self, t: Target) -> Target {
        match t {
            Target::Existing(o) => Target::Existing(self.id(o)),
            t => t,
        }
    }
}

/// How `tx` names what `m` maps: `Some(true)` in a target, a precondition, an `Undelete` or a script's base;
/// `Some(false)` only in `ObjectRef` fields; `None` not at all.
pub fn names(tx: &Transaction, slots: &Slots, m: &Map) -> Option<bool> {
    let id = |o: &ObjectId| m.ids.contains_key(o);
    let tgt = |t: &Target| matches!(t, Target::Existing(o) if id(o));
    let pre = tx.preconditions.iter().any(|p| match p {
        Precondition::Version { id: o, is, .. } | Precondition::ObjectVersion { id: o, is } => {
            id(o) || m.entries.contains_key(&is.entry)
        }
        Precondition::Exists(o) | Precondition::Absent(o) | Precondition::Content { id: o, .. } => id(o),
    });
    let op = tx.ops.iter().any(|op| match op {
        Op::Create { parent, .. } => tgt(parent),
        Op::Delete { id: t } | Op::SetRow { id: t, .. } | Op::SetField { id: t, .. } | Op::RemoveRow { id: t, .. } => {
            tgt(t)
        }
        Op::Reparent { id: t, parent, .. } => tgt(t) || tgt(parent),
        Op::Copy { source, parent, .. } => tgt(source) || tgt(parent),
        Op::Undelete { id: o } => id(o),
        Op::CommitScript { base, .. } => base.is_some_and(|b| m.entries.contains_key(&b.entry)),
        Op::Register { .. } | Op::AttestBuild { .. } | Op::BuildFailed { .. } => false,
    });
    if pre || op {
        return Some(true);
    }
    slots
        .refs
        .iter()
        .any(|s| slot_ref(&tx.ops, s).and_then(|b| id_at(b, s.at as usize)).is_some_and(|o| id(&o)))
        .then_some(false)
}

/// `tx` rewritten through `m`: targets, preconditions, `Undelete`s, a script's base and `ObjectRef` fields.
pub fn rewrite(tx: &Transaction, slots: &Slots, m: &Map) -> Transaction {
    let mut out = tx.clone();
    if m.is_empty() {
        return out;
    }
    for p in &mut out.preconditions {
        match p {
            Precondition::Version { id, is, .. } | Precondition::ObjectVersion { id, is } => {
                *id = m.id(*id);
                *is = m.version(*is);
            }
            Precondition::Exists(id) | Precondition::Absent(id) | Precondition::Content { id, .. } => *id = m.id(*id),
        }
    }
    for op in &mut out.ops {
        match op {
            Op::Create { parent, .. } => *parent = m.target(*parent),
            Op::Delete { id } | Op::SetRow { id, .. } | Op::SetField { id, .. } | Op::RemoveRow { id, .. } => {
                *id = m.target(*id)
            }
            Op::Reparent { id, parent, .. } => {
                *id = m.target(*id);
                *parent = m.target(*parent);
            }
            Op::Copy { source, parent, .. } => {
                *source = m.target(*source);
                *parent = m.target(*parent);
            }
            Op::Undelete { id } => *id = m.id(*id),
            Op::CommitScript { base, .. } => *base = base.map(|b| m.version(b)),
            Op::Register { .. } | Op::AttestBuild { .. } | Op::BuildFailed { .. } => {}
        }
    }
    for s in &slots.refs {
        if let Some(b) = slot_bytes(&mut out.ops, s)
            && let Some(o) = id_at(b, s.at as usize)
            && let Some(n) = m.ids.get(&o)
        {
            put_id(b, s.at as usize, *n);
        }
    }
    out
}

/// A row: its object, component and instance.
type RowKey = (ObjectId, ComponentId, u16);
/// A field's newest later value, and its place (offset, length) in the row if the schema gave one.
type LaterField = Option<(Vec<u8>, (u32, u32))>;

/// What this machine's later accepted API changes wrote, for the later-word rule (api.md 9.4; @design-reviewer's
/// M1). Added oldest first, so a field's newest value wins.
#[derive(Clone, Debug, Default)]
pub struct Later {
    /// Per row, each field set later: its newest value and its place in the row, if the schema gave one.
    fields: HashMap<RowKey, BTreeMap<u16, LaterField>>,
    rows: HashSet<(ObjectId, ComponentId, u16)>,
    removed: HashSet<(ObjectId, ComponentId, u16)>,
    gone: HashSet<ObjectId>,
    moved: HashSet<ObjectId>,
}

impl Later {
    pub fn add(&mut self, tx: &Transaction, slots: &Slots) {
        for (i, op) in tx.ops.iter().enumerate() {
            match op {
                Op::SetField { id: Target::Existing(o), component, instance, field, value } => {
                    let at = slots.field_at.get(i).copied().flatten();
                    self.fields
                        .entry((*o, *component, *instance))
                        .or_default()
                        .insert(*field, at.map(|at| (value.clone(), at)));
                }
                Op::SetRow { id: Target::Existing(o), row } => {
                    self.rows.insert((*o, row.component, row.instance));
                }
                Op::RemoveRow { id: Target::Existing(o), component, instance, .. } => {
                    self.removed.insert((*o, *component, *instance));
                }
                Op::Delete { id: Target::Existing(o) } => {
                    self.gone.insert(*o);
                }
                Op::Reparent { id: Target::Existing(o), .. } => {
                    self.moved.insert(*o);
                }
                _ => {}
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
            && self.rows.is_empty()
            && self.removed.is_empty()
            && self.gone.is_empty()
            && self.moved.is_empty()
    }

    fn row_taken(&self, k: &(ObjectId, ComponentId, u16)) -> bool {
        self.rows.contains(k) || self.removed.contains(k) || self.gone.contains(&k.0)
    }
}

/// `tx` without the ops `later` overwrote, and each kept op's index in `tx`; a row written whole keeps the later
/// field values patched in. `None` if no op is left. Creates and copies are never dropped, so `New` targets still
/// resolve.
pub fn supersede(tx: &Transaction, later: &Later) -> Option<(Transaction, Vec<u32>)> {
    if later.is_empty() {
        return Some((tx.clone(), (0..tx.ops.len() as u32).collect()));
    }
    let mut ops = Vec::with_capacity(tx.ops.len());
    let mut back = Vec::with_capacity(tx.ops.len());
    for (i, op) in tx.ops.iter().enumerate() {
        let keep = match op {
            Op::SetField { id: Target::Existing(o), component, instance, field, .. } => {
                let k = (*o, *component, *instance);
                let over = later.row_taken(&k) || later.fields.get(&k).is_some_and(|f| f.contains_key(field));
                (!over).then(|| op.clone())
            }
            Op::SetRow { id: Target::Existing(o), row } => {
                let k = (*o, row.component, row.instance);
                if later.row_taken(&k) {
                    None
                } else {
                    let mut row = row.clone();
                    for (at, len, v) in later
                        .fields
                        .get(&k)
                        .into_iter()
                        .flatten()
                        .filter_map(|(_, f)| f.as_ref().map(|(v, (at, len))| (*at as usize, *len as usize, v)))
                    {
                        if v.len() == len && at + len <= row.bytes.len() {
                            row.bytes[at..at + len].copy_from_slice(v);
                        }
                    }
                    Some(Op::SetRow { id: Target::Existing(*o), row })
                }
            }
            Op::RemoveRow { id: Target::Existing(o), component, instance, .. } => {
                let k = (*o, *component, *instance);
                (!(later.rows.contains(&k) || later.fields.contains_key(&k) || later.gone.contains(o)))
                    .then(|| op.clone())
            }
            Op::Reparent { id: Target::Existing(o), .. } => {
                (!(later.moved.contains(o) || later.gone.contains(o))).then(|| op.clone())
            }
            Op::Delete { id: Target::Existing(o) } => (!later.gone.contains(o)).then(|| op.clone()),
            op => Some(op.clone()),
        };
        if let Some(op) = keep {
            ops.push(op);
            back.push(i as u32);
        }
    }
    if ops.is_empty() {
        return None;
    }
    Some((Transaction { ops, ..tx.clone() }, back))
}

/// Moves each object precondition whose object's newest version (`current`) was written by one of `mine` (this
/// machine's later accepted API changes) to that version (api.md 9.4; @design-reviewer's M1′). Row preconditions
/// stay: pe-world's snapshot shows no row version [I].
pub fn move_preconditions(
    tx: &mut Transaction,
    current: impl Fn(ObjectId) -> Option<Version>,
    mine: &HashSet<EntryRef>,
) {
    for p in &mut tx.preconditions {
        if let Precondition::ObjectVersion { id, is } = p
            && let Some(v) = current(*id)
            && mine.contains(&v.entry)
        {
            *is = v;
        }
    }
}

/// The re-point follow-up for an accepted dependent (api.md 9.4): a plain write of just its ops that carry an
/// `ObjectRef` `m` maps, rewritten, on the objects as they now are (`created(k)`: the dependent's own `New` ids),
/// under its producer, through the later-word rule. `None` if nothing is left to re-point.
pub fn follow_up(
    tx: &Transaction,
    slots: &Slots,
    m: &Map,
    created: impl Fn(u32) -> ObjectId,
    later: &Later,
) -> Option<Transaction> {
    let mut rewritten = rewrite(tx, slots, &Map { ids: m.ids.clone(), ..Map::default() });
    // Per (op, row): the ObjectRef fields holding a mapped id.
    let mut mapped: BTreeMap<(u32, u32), BTreeSet<RefField>> = BTreeMap::new();
    for s in slots
        .refs
        .iter()
        .filter(|s| slot_ref(&tx.ops, s).and_then(|b| id_at(b, s.at as usize)).is_some_and(|o| m.ids.contains_key(&o)))
    {
        mapped.entry((s.op, s.row)).or_default().insert((s.field, s.span));
    }
    if mapped.is_empty() {
        return None;
    }
    let at = |t: Target| match t {
        Target::Existing(o) => Some(Target::Existing(o)),
        Target::New(k) => Some(Target::Existing(created(k))),
        Target::Root => None,
    };
    // Only the ObjectRef fields that moved are written, each as its own `SetField`, so the follow-up overwrites
    // nothing else of the row that another member set since.
    let fields = |ops: &mut Vec<Op>, id: Target, row: Row, at: &BTreeSet<RefField>| {
        for &(field, (off, len)) in at {
            if let Some(value) = row.bytes.get(off as usize..(off + len) as usize) {
                ops.push(Op::SetField {
                    id,
                    component: row.component,
                    instance: row.instance,
                    field,
                    value: value.to_vec(),
                });
            }
        }
    };
    let mut ops = vec![];
    for (i, op) in rewritten.ops.drain(..).enumerate() {
        let i = i as u32;
        match op {
            Op::SetField { id, component, instance, field, value } if mapped.contains_key(&(i, VALUE)) => {
                if let Some(id) = at(id) {
                    ops.push(Op::SetField { id, component, instance, field, value });
                }
            }
            Op::SetRow { id, row } => {
                if let (Some(id), Some(f)) = (at(id), mapped.get(&(i, 0))) {
                    fields(&mut ops, id, row, f);
                }
            }
            Op::Create { new, rows, .. } => {
                for (ri, row) in rows.into_iter().enumerate() {
                    if let Some(f) = mapped.get(&(i, ri as u32)) {
                        fields(&mut ops, Target::Existing(created(new)), row, f);
                    }
                }
            }
            _ => {}
        }
    }
    let label = format!("Re-point after a re-send: {}", tx.label.as_deref().unwrap_or("a change"));
    let t = Transaction {
        inline: vec![],
        actor: tx.actor.clone(),
        label: Some(label),
        preconditions: vec![],
        ops,
        blobs: vec![],
    };
    supersede(&t, later).map(|(t, _)| t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_core::Hash;
    use pe_world::edit::Actor;

    const LINE: LineId = LineId(Hash([7; 32]));
    const ME: MemberKey = MemberKey([1; 32]);
    const C: ComponentId = ComponentId(5);

    fn tx(ops: Vec<Op>, preconditions: Vec<Precondition>) -> Transaction {
        Transaction { inline: vec![], actor: Actor::Person, label: None, preconditions, ops, blobs: vec![] }
    }
    fn carrier(e: u8, seq: u64) -> Carrier {
        Carrier { entry: EntryRef(Hash([e; 32])), chain: ChainId(Hash([3; 32])), seq }
    }
    fn bytes_of(id: ObjectId) -> Vec<u8> {
        let mut b = vec![0; 16];
        put_id(&mut b, 0, id);
        b
    }

    #[test]
    fn rewrite_moves_ids_versions_and_object_refs_to_the_new_carrier() {
        let (old, new) = (carrier(1, 4), carrier(2, 9));
        let a = tx(vec![Op::Copy { source: Target::Root, new: 0, parent: Target::Root, index: None, size: 2 }], vec![]);
        let mut m = Map::default();
        m.carry((LINE, ME, &a), &old, None, &new, None);
        let x = |c: &Carrier, k| ObjectId::created(LINE, ME, c.chain, c.seq, k);
        let mut row = vec![0u8; 24];
        row[8..24].copy_from_slice(&bytes_of(x(&old, 1)));
        let b = tx(
            vec![
                Op::SetRow { id: Target::Existing(x(&old, 0)), row: Row { component: C, instance: 0, bytes: row } },
                Op::Reparent {
                    id: Target::Existing(x(&old, 1)),
                    parent: Target::New(0),
                    index: None,
                    keep_world: true,
                },
            ],
            vec![Precondition::ObjectVersion { id: x(&old, 0), is: Version { entry: old.entry, op: 0 } }],
        );
        let slots =
            Slots { refs: vec![RefSlot { op: 0, row: 0, at: 8, field: 1, span: (8, 16) }], field_at: vec![None, None] };
        assert_eq!(names(&b, &slots, &m), Some(true));
        let r = rewrite(&b, &slots, &m);
        let Op::SetRow { id, row } = &r.ops[0] else { panic!() };
        assert_eq!((*id, id_at(&row.bytes, 8)), (Target::Existing(x(&new, 0)), Some(x(&new, 1))));
        assert!(
            matches!(r.ops[1], Op::Reparent { id: Target::Existing(i), parent: Target::New(0), .. } if i == x(&new, 1))
        );
        assert_eq!(
            r.preconditions,
            vec![Precondition::ObjectVersion { id: x(&new, 0), is: Version { entry: new.entry, op: 0 } }]
        );
        // Only an ObjectRef names it: a weak dependency.
        let only_ref = tx(
            vec![Op::SetRow {
                id: Target::Root,
                row: Row {
                    component: C,
                    instance: 0,
                    bytes: {
                        let mut b = vec![0u8; 24];
                        b[8..24].copy_from_slice(&bytes_of(x(&old, 0)));
                        b
                    },
                },
            }],
            vec![],
        );
        assert_eq!(names(&only_ref, &slots, &m), Some(false));
        assert_eq!(names(&tx(vec![], vec![]), &slots, &m), None);
    }

    #[test]
    fn a_later_field_is_patched_into_a_whole_row_and_preconditions_move_only_to_the_authors_version() {
        let o = ObjectId::from_u128(42);
        let later_tx = tx(
            vec![Op::SetField { id: Target::Existing(o), component: C, instance: 0, field: 1, value: vec![7; 4] }],
            vec![],
        );
        let mut later = Later::default();
        later.add(&later_tx, &Slots { refs: vec![], field_at: vec![Some((4, 4))] });
        let row = Row { component: C, instance: 0, bytes: vec![1; 12] };
        let a = tx(
            vec![Op::SetRow { id: Target::Existing(o), row }],
            vec![Precondition::ObjectVersion { id: o, is: Version { entry: EntryRef(Hash([1; 32])), op: 0 } }],
        );
        let (mut t, back) = supersede(&a, &later).unwrap();
        let Op::SetRow { row, .. } = &t.ops[0] else { panic!() };
        assert_eq!((row.bytes.clone(), back), ([vec![1; 4], vec![7; 4], vec![1; 4]].concat(), vec![0]));
        let mine_v = Version { entry: EntryRef(Hash([9; 32])), op: 0 };
        let theirs = Version { entry: EntryRef(Hash([8; 32])), op: 0 };
        let mine: HashSet<EntryRef> = [mine_v.entry].into();
        let mut u = t.clone();
        move_preconditions(&mut u, |_| Some(theirs), &mine);
        assert_eq!(u.preconditions, a.preconditions, "another member's write still refuses it");
        move_preconditions(&mut t, |_| Some(mine_v), &mine);
        assert_eq!(t.preconditions, vec![Precondition::ObjectVersion { id: o, is: mine_v }]);
    }

    #[test]
    fn a_follow_up_writes_only_the_object_ref_fields_that_moved() {
        let (old, new) = (carrier(1, 4), carrier(2, 9));
        let a = tx(vec![Op::Create { new: 0, parent: Target::Root, index: None, rows: vec![] }], vec![]);
        let mut m = Map::default();
        m.carry((LINE, ME, &a), &old, None, &new, None);
        let x = |c: &Carrier| ObjectId::created(LINE, ME, c.chain, c.seq, 0);
        let lamp = ObjectId::from_u128(11);
        // A row: field 0 (8 bytes, not a reference), field 1 a reference to the re-sent object, field 2 another
        // reference that didn't move.
        let mut bytes = vec![7u8; 40];
        bytes[8..24].copy_from_slice(&bytes_of(x(&old)));
        bytes[24..40].copy_from_slice(&bytes_of(lamp));
        let d =
            tx(vec![Op::SetRow { id: Target::Existing(lamp), row: Row { component: C, instance: 2, bytes } }], vec![]);
        let slots = Slots {
            refs: vec![
                RefSlot { op: 0, row: 0, at: 8, field: 1, span: (8, 16) },
                RefSlot { op: 0, row: 0, at: 24, field: 2, span: (24, 16) },
            ],
            field_at: vec![None],
        };
        let f = follow_up(&d, &slots, &m, |_| lamp, &Later::default()).expect("a follow-up");
        assert_eq!(
            f.ops,
            vec![Op::SetField {
                id: Target::Existing(lamp),
                component: C,
                instance: 2,
                field: 1,
                value: bytes_of(x(&new))
            }]
        );
    }

    #[test]
    fn backoff_starts_at_once_and_settles_at_sixteen_seconds() {
        let ms: Vec<u128> = (1..=9).map(|n| backoff(n).as_millis()).collect();
        assert_eq!(ms, vec![0, 500, 1_000, 2_000, 4_000, 8_000, 16_000, 16_000, 16_000]);
    }
}

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

use crate::components::{Protection, SimSize, ids};
use crate::edit::{Actor, Op, Precondition, Row, Target, Transaction, content_hash, row_tag};
use crate::world::{DEPTH_LIMIT_MAX, GENESIS, TOMBSTONE_SECONDS, Tombstone, World, WorldError, depth, perm};

/// What an applied entry touched, for the replica's physics effects (9.2) and the caches. Empty for a refusal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Footprint {
    /// Created, in op order.
    pub created: Vec<ObjectId>,
    /// Deleted (every object of each deleted subtree), in op order.
    pub removed: Vec<ObjectId>,
    /// Rows written or removed by edits, in op order (an object and component may repeat).
    pub rows: Vec<(ObjectId, ComponentId)>,
    /// Reparented, in op order.
    pub reparented: Vec<ObjectId>,
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

fn too_deep(object: ObjectId, limit: u32) -> Reason {
    Reason::TooDeep { object, limit }
}

/// Op `i` changed nothing (7.6): a `RemoveRow` of an instance already gone. pe-seams has no `Note::NoEffect` yet
/// (asked of @engine-architect, m48a126a4d778); until it does, the op applies with no note.
fn no_effect(_cx: &mut Cx, _i: u32) {}

fn version(entry: EntryRef, op: u32) -> Version {
    Version { entry, op }
}

fn from_row_version(v: RowVersion) -> Version {
    Version { entry: EntryRef(Hash(v.entry)), op: v.op }
}

/// Components no edit may write (7.6 step 4): the hierarchy, derived physics state and hands (inputs).
fn edit_writable(c: ComponentId) -> bool {
    c != ComponentId::NODE && c != ids::BODY_STATE && c != ids::HAND
}

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

    fn try_apply(&mut self, frame: Frame, entry: &SignedEntry) -> Result<(Outcome, Footprint), Refusal> {
        let e = &entry.entry;
        if e.class != Class::Edit {
            return Err((None, Reason::Malformed));
        }
        let tx = Transaction::decode(&e.payload).ok_or((None, Reason::Malformed))?;
        if let Actor::Agent { principal, requested_by, .. } = &tx.actor
            && (*principal != e.author || requested_by.is_some())
        {
            return Err((None, invalid()));
        }
        if !self.permits(e.author, perm::EDIT) {
            return Err((None, Reason::NoPermission));
        }
        for (i, p) in tx.preconditions.iter().enumerate() {
            self.check(i as u32, p).map_err(|r| (None, r))?;
        }
        let mut cx = Cx {
            entry: entry.id(),
            author: e.author,
            line: e.line,
            chain: e.chain,
            seq: e.seq,
            frame,
            new: BTreeMap::new(),
            created: Vec::new(),
            overwrote: Vec::new(),
            notes: Vec::new(),
            fp: Footprint::default(),
            undo: Vec::new(),
            undo_pre: BTreeMap::new(),
        };
        let mut work = self.clone();
        for (i, op) in tx.ops.iter().enumerate() {
            let i = i as u32;
            work.op(&mut cx, i, op).map_err(|r| (Some(i), r))?;
        }
        *self = work;
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
        };
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
            Op::Copy { source, new, parent, index } => self.op_copy(cx, i, *source, *new, *parent, *index),
            Op::Undelete { id } => self.op_undelete(cx, i, *id),
        }
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
        let v = version(cx.entry, i);
        self.create_at(id, parent, index.map(|x| x as usize), cx.author, v).map_err(|e| match e {
            WorldError::TooDeep { object, limit } => too_deep(object, limit),
            WorldError::Duplicate(id) => Reason::Exists(id),
            WorldError::UnknownObject(id) => Reason::Unknown(id),
            WorldError::Store(_) => invalid(),
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
        // A hold by anyone but the author refuses the delete (7.2). The author's own holds end with it, so a player
        // can delete what they hold without letting go first (@coordinator-opus m6d5969e46cb1, option (b)).
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
        let registry = self.store.registry().clone();
        let mut stones = Vec::with_capacity(doomed.len());
        for &d in &doomed {
            let rows = registry
                .iter()
                .filter(|desc| desc.id() != ComponentId::NODE)
                .filter_map(|desc| {
                    let row = self.store.get_raw(desc.id(), d)?;
                    Some((desc.id(), row.to_vec(), self.store.row_version(desc.id(), d).unwrap_or_default()))
                })
                .collect();
            let stone = Tombstone {
                by: cx.entry,
                frame: cx.frame,
                expires,
                parent: self.store.parent(d),
                world: self.store.world_transform(d),
                node: self.store.node(d),
                rows,
            };
            stones.push((d, stone));
        }
        self.store.destroy(o).map_err(|_| invalid())?;
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
    fn op_copy(
        &mut self,
        cx: &mut Cx,
        i: u32,
        source: Target,
        new: u32,
        parent: Target,
        index: Option<u32>,
    ) -> Result<(), Reason> {
        let src = self.live(cx, source)?;
        let subtree = self.store.subtree(src);
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
            if self.store.get_raw(ids::HAND, x).is_some() {
                return Err(invalid_field(ids::HAND, u32::MAX));
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
        // The subtree, parents first, children in their old order, through the tombstones' own links. Bounded by
        // the number of tombstones, so corrupt links cannot loop.
        let limit = self.tombstones.len();
        let mut order = Vec::new();
        let mut stack = vec![id];
        while let Some(x) = stack.pop() {
            if order.len() > limit {
                return Err(invalid());
            }
            order.push(x);
            let mut kids = Vec::new();
            let mut c = self.tombstones[&x].node.first_child;
            while let Some(t) = c.some().and_then(|c| self.tombstones.get(&c)).filter(|t| t.by == root.by) {
                kids.push(c);
                if kids.len() > limit {
                    return Err(invalid());
                }
                c = t.node.next_sibling;
            }
            stack.extend(kids.into_iter().rev());
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

    /// Ends a frame (7.2, 7.5): drops the tombstones whose expiry frame has passed.
    pub fn end_frame(&mut self, frame: Frame) {
        if self.tombstones.values().any(|t| t.expires <= frame) {
            Arc::make_mut(&mut self.tombstones).retain(|_, t| t.expires > frame);
        }
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
        if held_rows(c)
            && let Some(claim) = self.claims.get(&id)
        {
            return Err(Reason::Held { by: claim.member });
        }
        if c == ids::PROTECTION {
            let p: Protection = read_pod(bytes).ok_or_else(|| invalid_field(c, 0))?;
            self.check_protection_write(cx.author, id, Some(p.owner))?;
        }
        let before = self.store.row_version(c, id);
        let prior = self.store.get_raw(c, id).map(<[u8]>::to_vec);
        // An overwrite keeps the instance's tag; an addition gives it this op's.
        let tag = before.map_or(cx.tag(i), |v| v.tag);
        self.store.insert_raw(c, id, bytes).map_err(|e| self.store_reason(c, e))?;
        if !creating {
            cx.undo.push(match prior {
                Some(bytes) => Op::SetRow { id: Target::Existing(id), row: Row { component: c, instance: 0, bytes } },
                None => Op::RemoveRow {
                    id: Target::Existing(id),
                    component: c,
                    instance: 0,
                    tag: u128::from_le_bytes(tag),
                },
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
        if let Some(bytes) = self.store.get_raw(c, id).map(<[u8]>::to_vec) {
            cx.undo.push(Op::SetRow { id: Target::Existing(id), row: Row { component: c, instance: 0, bytes } });
        }
        cx.undo_pre.remove(&(id, c));
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
        cx.fp.reparented.push(id);
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
    fn touch(&mut self, id: ObjectId, v: Version, structural: bool) {
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

fn node_version(n: &Node) -> Version {
    Version { entry: EntryRef(Hash(n.version_entry)), op: n.version_op }
}

fn read_pod<T: bytemuck::Pod>(b: &[u8]) -> Option<T> {
    (b.len() == size_of::<T>()).then(|| bytemuck::pod_read_unaligned(b))
}

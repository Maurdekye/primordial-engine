//! Resolving a transaction against its base snapshot (api.md 5.3, 9.2 step 2): what the author's machine does before
//! anything is signed, so the entry carries only explicit, deterministic edits.
//!
//! - Ids and paths resolve to one object (`not_found`, `ambiguous`); `$ref`s to the transaction's new objects, in
//!   pe-world's `New(k)` numbering (world-model.md 7.1: a `Copy` of an n-object subtree reserves n indices).
//! - A `where` selector expands into one op per object, with a required `max`; such ops are plain writes.
//! - Every op that changes an existing object gets an `ObjectVersion` precondition on it: its explicit `if_version`,
//!   else the version this connection last saw, else its version at the base (api.md 4.2); `"any"` gives none. An
//!   explicit version replaces a default; two different explicit versions for one object are refused. A parent an
//!   op attaches to (`create`, `reparent`, `duplicate`) gets none: pe-world's own rules decide (world-model.md 7.5
//!   orphaning, the cycle check, protections; coordinator m8e173fe643ad).
//! - Limits: at most 4,096 input ops and 4,096 ops once expanded (api.md 9.3), 65,536 new objects, and a budget of
//!   objects examined by `where` selectors, each refused as `too_large`. The op count here is of these resolved ops;
//!   pe-world counts its own ops (a `set` of k fields is k of them) and its check at apply is the authoritative one.
//!
//! Field values pass through as JSON: pe-app's adapter parses them against the schema and converts positions with
//! pe-world's own transform calls, so query and apply share one definition. It must convert each op against the
//! state as it stands at that op (pe-world's pure apply, op by op), not against the base: a frame may be a `$ref`
//! the base lacks, and an earlier reparent in the same change moves a parent frame. The same holds for the
//! root-position check after an in-change reparent.
//!
//! Not yet: `code` on an existing script. api.md 7 names a script by its asset id, with the script's source
//! version (`CommitScript.base`) as its precondition; the snapshot port has no script lookup yet, so such an `id`
//! is `not_found` today. That comes with the logic increment.

use std::collections::{BTreeMap, BTreeSet};

use pe_seams::world_api::{
    ComponentValues, Json, Keep, ObjRef, ObjectBrief, ObjectId, Op, Refusal, RefusalCode, Target, Transaction, Version,
};
use serde_json::json;

use crate::ports::Snapshot;
use crate::query::{Budget, EXAMINE_MAX, Named, resolve_in, select_in};

/// pe-world's per-transaction op limit (world-model.md 7.1).
pub const OPS_MAX: usize = 4096;
/// The most objects one change may create, counting every object a copy or a template brings (@chunks-reviewer's
/// M1). Checked before anything is reserved. pe-world owns this bound (coordinator m043d9e7a3f2b): it is to be
/// `pe_world::edit::MAX_NEW_OBJECTS`, beside `MAX_OPS`. Until that lands this copy must keep its value, 65,536;
/// then use pe-world's constant instead.
pub const NEW_MAX: u32 = 65_536;
/// pe-world's label limit, in bytes.
pub const LABEL_MAX: usize = 200;

/// An object an op acts on: one that exists, or the transaction's k-th new object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Obj {
    Existing(ObjectId),
    New(u32),
}

/// One explicit edit, one-to-one with pe-world's ops (the adapter maps them; a `Create` from a template becomes a
/// `Copy` plus its sets).
#[derive(Clone, Debug, PartialEq)]
pub enum ROp {
    Create {
        new: u32,
        parent: Obj,
        name: String,
        template: Option<ObjectId>,
        components: ComponentValues,
    },
    /// `Component.field` -> value.
    Set {
        target: Obj,
        fields: BTreeMap<String, Json>,
    },
    Add {
        target: Obj,
        component: String,
        fields: BTreeMap<String, Json>,
    },
    Remove {
        target: Obj,
        component: String,
    },
    Reparent {
        target: Obj,
        parent: Obj,
        index: Option<u32>,
        keep_world: bool,
    },
    Copy {
        source: Obj,
        new: u32,
        parent: Obj,
    },
    Delete {
        target: Obj,
    },
    Code {
        script: Obj,
        source: String,
        attach: Vec<Obj>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub label: String,
    pub ops: Vec<ROp>,
    /// `ObjectVersion` preconditions, one per object, in first-use order.
    pub preconditions: Vec<(ObjectId, Version)>,
    /// `$ref` -> its new index.
    pub refs: BTreeMap<String, u32>,
    /// How many new indices the transaction uses.
    pub new_count: u32,
    /// Objects written (api.md's `count`).
    pub count: u64,
}

fn refusal(code: RefusalCode, op: usize, message: impl Into<String>) -> Box<Refusal> {
    let mut f = whole(code, message);
    f.op = Some(op as u32);
    f
}

/// A refusal of the whole change, naming no op.
fn whole(code: RefusalCode, message: impl Into<String>) -> Box<Refusal> {
    Box::new(Refusal {
        code,
        message: message.into(),
        op: None,
        object: None,
        field: None,
        read: None,
        now: None,
        winner: None,
        next: None,
        details: None,
    })
}

fn too_much_examined(op: usize) -> Box<Refusal> {
    let mut f = refusal(
        RefusalCode::TooLarge,
        op,
        format!("The change's paths and selectors visit more than {EXAMINE_MAX} objects; narrow them (an id, under)."),
    );
    f.details = Some(json!({ "limit": EXAMINE_MAX }));
    f
}

fn too_many_new(op: usize) -> Box<Refusal> {
    let mut f = refusal(RefusalCode::TooLarge, op, format!("The change creates more than {NEW_MAX} objects."));
    f.details = Some(json!({ "limit": NEW_MAX }));
    f
}

struct Ctx<'a> {
    snap: &'a dyn Snapshot,
    seen: &'a dyn Fn(ObjectId) -> Option<Version>,
    refs: BTreeMap<String, u32>,
    /// The parent of each new object, for sizing a `Copy` of a new object's subtree.
    new_parent: Vec<Obj>,
    next_new: u32,
    /// Preconditions in first-use order, each marked explicit or default (M1).
    pre: Vec<(ObjectId, Version, bool)>,
    ops: Vec<ROp>,
    count: u64,
    /// New indices that are scripts, not objects (Low 7).
    scripts: BTreeSet<u32>,
    /// `where` results by selector, within this change (M3).
    selected: BTreeMap<String, Vec<ObjectId>>,
    /// Every object any walk of this change visits: selectors, paths in ids, parents, attachments, templates (M3).
    budget: Budget,
    /// Objects whose children an earlier op grew (create, copy into them).
    grown: BTreeSet<Obj>,
    /// Objects whose children an earlier op moved or removed (reparent in or out, delete). Moving or deleting the
    /// source itself doesn't change its size (a deleted source is refused by pe-world as gone).
    moved: BTreeSet<Obj>,
    /// Parents changed by an earlier reparent of an existing object.
    reparented: BTreeMap<ObjectId, Obj>,
}

impl Ctx<'_> {
    fn obj(&self, i: usize, r: &ObjRef) -> Result<Obj, Box<Refusal>> {
        if let Some(name) = r.0.strip_prefix('$') {
            let k = *self.refs.get(&r.0).ok_or_else(|| {
                refusal(
                    RefusalCode::InChange,
                    i,
                    format!("Op {i} refers to ${name}, which this change doesn't create before it."),
                )
            })?;
            if self.scripts.contains(&k) {
                return Err(refusal(
                    RefusalCode::Invalid,
                    i,
                    format!("${name} is a script this change creates, not an object; attach it with `code`."),
                ));
            }
            return Ok(Obj::New(k));
        }
        let named = resolve_in(self.snap, r, &self.budget);
        if self.budget.over() {
            return Err(too_much_examined(i));
        }
        match named {
            Named::One(id) => Ok(Obj::Existing(id)),
            Named::None => Err(refusal(RefusalCode::NotFound, i, format!("{:?} names no object.", r.0))),
            Named::Me => {
                Err(refusal(RefusalCode::Invalid, i, "\"me\" is a frame for positions, not an object you can edit."))
            }
            Named::Many(ids) => {
                let mut f =
                    refusal(RefusalCode::Ambiguous, i, format!("{:?} names {} objects; use an id.", r.0, ids.len()));
                f.details =
                    Some(json!({ "matches": ids.iter().take(10).map(|id| id.to_string()).collect::<Vec<_>>() }));
                Err(f)
            }
        }
    }

    /// Records the precondition for an existing object op `i` changes, named by id. An explicit version replaces a
    /// default one; a second, different explicit version for the same object is refused (design-reviewer M1).
    fn precondition(&mut self, i: usize, id: ObjectId, explicit: Option<&Version>) -> Result<(), Box<Refusal>> {
        let at = self.pre.iter().position(|(x, _, _)| *x == id);
        match explicit {
            Some(v) if v.is_any() => {}
            Some(v) => match at {
                Some(j) if self.pre[j].2 && self.pre[j].1 != *v => {
                    let mut f = refusal(
                        RefusalCode::Invalid,
                        i,
                        format!(
                            "Op {i} expects version {:?} of this object, but an earlier op expects {:?}.",
                            v.0, self.pre[j].1.0
                        ),
                    );
                    f.object = self.snap.info(id).map(|o| ObjectBrief { id, name: o.name });
                    return Err(f);
                }
                Some(j) => self.pre[j] = (id, v.clone(), true),
                None => self.pre.push((id, v.clone(), true)),
            },
            None => {
                if at.is_none()
                    && let Some(v) = (self.seen)(id).or_else(|| self.snap.info(id).map(|o| o.version))
                {
                    self.pre.push((id, v, false));
                }
            }
        }
        Ok(())
    }

    /// `where` results, memoised per selector within the change, against a budget of objects examined (M3).
    fn select(&mut self, i: usize, s: &pe_seams::world_api::Selector) -> Result<Vec<ObjectId>, Box<Refusal>> {
        // A selector always serialises; if one ever didn't, it is simply not memoised.
        let key = serde_json::to_string(s).ok();
        if let Some(ids) = key.as_ref().and_then(|k| self.selected.get(k)) {
            return Ok(ids.clone());
        }
        let ids = select_in(self.snap, s, &self.budget);
        if self.budget.over() {
            return Err(too_much_examined(i));
        }
        if let Some(k) = key {
            self.selected.insert(k, ids.clone());
        }
        Ok(ids)
    }

    /// The objects an op targets: one by `id` (with its precondition), or many by `where` (plain writes).
    fn targets(&mut self, i: usize, t: &Target, allow_where: bool) -> Result<Vec<Obj>, Box<Refusal>> {
        match (&t.id, &t.select) {
            (Some(_), Some(_)) => {
                Err(refusal(RefusalCode::Invalid, i, "An op names objects by `id` or by `where`, not both."))
            }
            (None, None) => Err(refusal(RefusalCode::Invalid, i, "The op names no object: give `id` or `where`.")),
            (Some(r), None) => {
                let o = self.obj(i, r)?;
                if let Obj::Existing(id) = o {
                    self.precondition(i, id, t.if_version.as_ref())?;
                }
                Ok(vec![o])
            }
            (None, Some(s)) => {
                if !allow_where {
                    return Err(refusal(RefusalCode::Invalid, i, "This op takes one object by `id`, not a `where`."));
                }
                let Some(max) = t.max else {
                    return Err(refusal(RefusalCode::Invalid, i, "`max` is required with `where`."));
                };
                let ids = self.select(i, s)?;
                if ids.len() > max as usize {
                    let mut f = refusal(
                        RefusalCode::TooMany,
                        i,
                        format!("The selector matched {} objects; `max` is {max}.", ids.len()),
                    );
                    f.details = Some(json!({ "count": ids.len(), "max": max }));
                    return Err(f);
                }
                Ok(ids.into_iter().map(Obj::Existing).collect())
            }
        }
    }

    fn new_ref(&mut self, i: usize, as_ref: Option<&String>, parent: Obj, size: u32) -> Result<u32, Box<Refusal>> {
        if size > NEW_MAX - self.next_new {
            return Err(too_many_new(i));
        }
        let k = self.next_new;
        if let Some(name) = as_ref {
            if !name.starts_with('$') || name.len() < 2 {
                return Err(refusal(
                    RefusalCode::Invalid,
                    i,
                    format!("`as` must be a name starting with $, not {name:?}."),
                ));
            }
            if self.refs.insert(name.clone(), k).is_some() {
                return Err(refusal(RefusalCode::InChange, i, format!("{name} is created twice in this change.")));
            }
        }
        self.new_parent.push(parent);
        for _ in 1..size {
            self.new_parent.push(Obj::New(k)); // the copied subtree's other members; parents within it are not tracked
        }
        self.next_new += size;
        Ok(k)
    }

    /// Objects in a subtree, for the indices a `Copy` reserves. Refuses `too_large` as soon as the count passes
    /// what is left of `NEW_MAX`, so a huge subtree is never walked in full.
    fn subtree_size(&self, i: usize, o: Obj) -> Result<u32, Box<Refusal>> {
        let room = NEW_MAX - self.next_new;
        let mut n: u32 = 1;
        match o {
            Obj::Existing(id) => {
                let mut stack = vec![id];
                while let Some(x) = stack.pop() {
                    let c = self.snap.children(x);
                    n = n.saturating_add(u32::try_from(c.len()).unwrap_or(u32::MAX));
                    if n > room {
                        return Err(too_many_new(i));
                    }
                    stack.extend(c);
                }
            }
            Obj::New(k) => {
                // A new object and the new objects created under it so far. A new object's parent always has a
                // lower index (it existed first), so one forward pass finds the subtree.
                let mut inside = vec![false; self.new_parent.len()];
                if let Some(x) = inside.get_mut(k as usize) {
                    *x = true;
                }
                for j in (k as usize + 1)..self.new_parent.len() {
                    if let Obj::New(p) = self.new_parent[j]
                        && (p as usize) < j
                        && inside[p as usize]
                    {
                        inside[j] = true;
                        n += 1;
                    }
                }
                if n > room {
                    return Err(too_many_new(i));
                }
            }
        }
        Ok(n)
    }

    /// An object's parent as it stands after the ops so far: a reparent's, else a new object's, else the base's.
    fn parent_of(&self, o: Obj) -> Option<Obj> {
        match o {
            Obj::Existing(id) => match self.reparented.get(&id) {
                Some(p) => Some(*p),
                None => self.snap.info(id).and_then(|i| i.parent).map(Obj::Existing),
            },
            Obj::New(k) => self.new_parent.get(k as usize).copied().filter(|p| *p != Obj::New(u32::MAX)),
        }
    }

    /// Whether `o` is `root` or under it, after the ops so far. Charged to the budget; a cycle stops the walk.
    fn inside(&self, o: Obj, root: Obj) -> bool {
        let mut seen = BTreeSet::new();
        let mut at = Some(o);
        while let Some(x) = at {
            if x == root {
                return true;
            }
            if !seen.insert(x) || !self.budget.charge(1) {
                return false;
            }
            at = self.parent_of(x);
        }
        false
    }

    /// A copy's size is counted from the base (an existing source) or from the new objects created under it (a new
    /// source). pe-world checks the size it gets (`Copy.size`) and refuses a mismatch as stale, which re-reading can't
    /// cure, so a duplicate whose source subtree an earlier op of this change grew, moved or emptied is refused here
    /// instead (design-reviewer, m513402c91b03). Creating or copying under a new source is counted, so only moves
    /// and deletes refuse one.
    fn check_copy_source(&self, i: usize, src: Obj) -> Result<(), Box<Refusal>> {
        let hit = match src {
            Obj::Existing(_) => self.grown.iter().chain(&self.moved).any(|t| self.inside(*t, src)),
            Obj::New(_) => self.moved.iter().any(|t| self.inside(*t, src)),
        };
        if self.budget.over() {
            return Err(too_much_examined(i));
        }
        if hit {
            let mut f = refusal(
                RefusalCode::Invalid,
                i,
                format!(
                    "Op {i} copies a subtree that an earlier op in this change adds to, moves or deletes from; copy it \
                     first, or in a change of its own."
                ),
            );
            if let Obj::Existing(id) = src {
                f.object = self.snap.info(id).map(|o| ObjectBrief { id, name: o.name });
            }
            return Err(f);
        }
        Ok(())
    }

    fn push(&mut self, i: usize, op: ROp) -> Result<(), Box<Refusal>> {
        self.ops.push(op);
        if self.ops.len() > OPS_MAX {
            let mut f =
                refusal(RefusalCode::TooLarge, i, format!("The change has more than {OPS_MAX} edits once expanded."));
            f.details = Some(json!({ "limit": OPS_MAX }));
            return Err(f);
        }
        Ok(())
    }
}

/// A template by name: a direct child of the root `Templates` folder with exactly that name (no paths or globs).
fn template_named(snap: &dyn Snapshot, i: usize, t: &str, b: &Budget) -> Result<ObjectId, Box<Refusal>> {
    let roots = snap.roots();
    if !b.charge(roots.len()) {
        return Err(too_much_examined(i));
    }
    let folders: Vec<ObjectId> =
        roots.into_iter().filter(|r| snap.info(*r).is_some_and(|o| o.name == "Templates")).collect();
    if folders.len() > 1 {
        return Err(refusal(
            RefusalCode::Ambiguous,
            i,
            format!("{} root objects are named \"Templates\".", folders.len()),
        ));
    }
    let folder = folders.first().copied();
    let children = folder.map(|f| snap.children(f)).unwrap_or_default();
    if !b.charge(children.len()) {
        return Err(too_much_examined(i));
    }
    let hits: Vec<ObjectId> = children.into_iter().filter(|c| snap.info(*c).is_some_and(|o| o.name == t)).collect();
    match hits.as_slice() {
        [one] => Ok(*one),
        [] => Err(refusal(RefusalCode::NotFound, i, format!("No template named {t:?}; world_schema lists them."))),
        many => Err(refusal(RefusalCode::Ambiguous, i, format!("{} templates are named {t:?}.", many.len()))),
    }
}

/// A root has no parent frame: a position written relative to `"parent"` on one is refused (api.md 4.4).
fn check_root_position(snap: &dyn Snapshot, i: usize, o: Obj, field: &str, v: &Json) -> Result<(), Box<Refusal>> {
    if field == "Transform.position"
        && v.get("relative_to").and_then(|r| r.as_str()) == Some("parent")
        && let Obj::Existing(id) = o
        && let Some(info) = snap.info(id)
        && info.parent.is_none()
    {
        let mut f = refusal(RefusalCode::Invalid, i, format!("\"{}\" has no parent; name a frame.", info.name));
        f.object = Some(ObjectBrief { id, name: info.name });
        f.field = Some(field.into());
        return Err(f);
    }
    Ok(())
}

/// Resolves `tx` against `snap`. `seen` gives this connection's last-seen version of an object (api.md 4.2).
pub fn resolve_tx(
    snap: &dyn Snapshot,
    tx: &Transaction,
    seen: &dyn Fn(ObjectId) -> Option<Version>,
) -> Result<Resolved, Box<Refusal>> {
    if tx.label.len() > LABEL_MAX {
        return Err(whole(RefusalCode::Invalid, format!("The label is longer than {LABEL_MAX} bytes.")));
    }
    if tx.ops.len() > OPS_MAX {
        let mut f = whole(RefusalCode::TooLarge, format!("The change has more than {OPS_MAX} ops."));
        f.details = Some(json!({ "limit": OPS_MAX }));
        return Err(f);
    }
    let mut c = Ctx {
        snap,
        seen,
        refs: BTreeMap::new(),
        new_parent: vec![],
        next_new: 0,
        pre: vec![],
        ops: vec![],
        count: 0,
        scripts: BTreeSet::new(),
        selected: BTreeMap::new(),
        budget: Budget::default(),
        grown: BTreeSet::new(),
        moved: BTreeSet::new(),
        reparented: BTreeMap::new(),
    };
    for (i, op) in tx.ops.iter().enumerate() {
        match op {
            Op::Create { as_ref, parent, name, template, components } => {
                // No precondition on the parent (M2 (a)): pe-world's rules decide.
                let parent = c.obj(i, parent)?;
                let template = match template {
                    None => None,
                    Some(t) => Some(template_named(snap, i, t, &c.budget)?),
                };
                let size = match template {
                    Some(t) => c.subtree_size(i, Obj::Existing(t))?,
                    None => 1,
                };
                let new = c.new_ref(i, as_ref.as_ref(), parent, size)?;
                c.grown.insert(parent);
                c.count += size as u64; // every object it makes, as for a duplicate
                c.push(i, ROp::Create { new, parent, name: name.clone(), template, components: components.clone() })?;
            }
            Op::Set { target, field, value, fields } => {
                let mut fs: BTreeMap<String, Json> = fields.clone().unwrap_or_default();
                match (field, value) {
                    (Some(f), v) => {
                        fs.insert(f.clone(), v.clone().unwrap_or(Json::Null));
                    }
                    (None, Some(_)) => return Err(refusal(RefusalCode::Invalid, i, "`value` needs a `field`.")),
                    (None, None) => {}
                }
                if fs.is_empty() {
                    return Err(refusal(RefusalCode::Invalid, i, "`set` needs `field` and `value`, or `fields`."));
                }
                for o in c.targets(i, target, true)? {
                    for (f, v) in &fs {
                        check_root_position(snap, i, o, f, v)?;
                    }
                    c.count += 1;
                    c.push(i, ROp::Set { target: o, fields: fs.clone() })?;
                }
            }
            Op::Add { target, component, fields } => {
                for o in c.targets(i, target, true)? {
                    c.count += 1;
                    c.push(i, ROp::Add { target: o, component: component.clone(), fields: fields.clone() })?;
                }
            }
            Op::Remove { target, component } => {
                for o in c.targets(i, target, true)? {
                    c.count += 1;
                    c.push(i, ROp::Remove { target: o, component: component.clone() })?;
                }
            }
            Op::Reparent { target, parent, index, keep } => {
                let t = c.targets(i, target, false)?[0];
                // The reparented object keeps its precondition (from `targets`); the new parent gets none (M2 (a)).
                let p = c.obj(i, parent)?;
                if let Some(old) = c.parent_of(t) {
                    c.moved.insert(old);
                }
                c.moved.insert(p);
                match t {
                    Obj::Existing(id) => {
                        c.reparented.insert(id, p);
                    }
                    Obj::New(k) => c.new_parent[k as usize] = p,
                }
                c.count += 1;
                c.push(i, ROp::Reparent { target: t, parent: p, index: *index, keep_world: *keep == Keep::WorldPose })?;
            }
            Op::Duplicate { target, as_ref, parent } => {
                let src = c.targets(i, target, false)?[0];
                let parent = match parent {
                    Some(p) => c.obj(i, p)?,
                    None => match src {
                        Obj::Existing(id) => match snap.info(id).and_then(|x| x.parent) {
                            Some(p) => Obj::Existing(p),
                            None => {
                                return Err(refusal(RefusalCode::Invalid, i, "A root's duplicate needs a `parent`."));
                            }
                        },
                        Obj::New(k) => c.new_parent[k as usize],
                    },
                };
                c.check_copy_source(i, src)?;
                let size = c.subtree_size(i, src)?;
                let new = c.new_ref(i, as_ref.as_ref(), parent, size)?;
                c.grown.insert(parent);
                c.count += size as u64;
                c.push(i, ROp::Copy { source: src, new, parent })?;
            }
            Op::Delete { target } => {
                for o in c.targets(i, target, true)? {
                    if let Some(old) = c.parent_of(o) {
                        c.moved.insert(old);
                    }
                    c.count += 1;
                    c.push(i, ROp::Delete { target: o })?;
                }
            }
            Op::Code { target, source, attach } => {
                let script = match (&target.id, &target.select) {
                    // A new script: a $ref this op introduces.
                    (Some(r), None) if r.0.starts_with('$') && !c.refs.contains_key(&r.0) => {
                        let k = c.new_ref(i, Some(&r.0), Obj::New(u32::MAX), 1)?;
                        c.scripts.insert(k);
                        Obj::New(k)
                    }
                    _ => c.targets(i, target, false)?[0],
                };
                let attach = attach.iter().map(|r| c.obj(i, r)).collect::<Result<Vec<_>, _>>()?;
                c.count += 1;
                c.push(i, ROp::Code { script, source: source.clone(), attach })?;
            }
        }
    }
    Ok(Resolved {
        label: tx.label.clone(),
        ops: c.ops,
        preconditions: c.pre.into_iter().map(|(id, v, _)| (id, v)).collect(),
        refs: c.refs,
        new_count: c.next_new,
        count: c.count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::mem::*;
    use pe_seams::world_api::{At, Selector};
    use serde_json::json;

    fn tx(ops: Json) -> Transaction {
        serde_json::from_value(json!({ "label": "t", "ops": ops })).unwrap()
    }

    fn none(_: ObjectId) -> Option<Version> {
        None
    }

    fn with_templates() -> Mem {
        let mut m = Mem::pit(3);
        m.add(50, "Templates", None, &[], [0.0; 3]);
        m.add(51, "box", Some(50), &["Transform", "Shape"], [0.0; 3]);
        m.add(52, "lamp", Some(50), &["Transform"], [0.0; 3]);
        m.add(53, "bulb", Some(52), &["Light"], [0.0; 3]);
        m
    }

    #[test]
    fn the_demo_script_resolves() {
        let m = with_templates();
        let r = resolve_tx(
            &m,
            &tx(json!([
                {"op": "create", "as": "$cube", "parent": "/Table", "name": "Red cube", "template": "box",
                 "components": {"Appearance": {"color": "#d22b2b"}}},
                {"op": "set", "where": {"has": "Ball"}, "max": 1000, "field": "Appearance.color", "value": "#2b5bd2"},
                {"op": "duplicate", "id": "$cube", "as": "$copy"},
                {"op": "set", "id": "$copy", "field": "Transform.position", "value": {"relative_to": "$cube", "offset": [0.3, 0, 0]}},
                {"op": "delete", "id": oid(2).to_string()}
            ])),
            &none,
        )
        .unwrap();
        assert_eq!(r.refs, BTreeMap::from([("$cube".to_string(), 0), ("$copy".to_string(), 1)]));
        assert_eq!(r.new_count, 2);
        assert_eq!(r.ops.len(), 1 + 3 + 1 + 1 + 1);
        assert!(matches!(r.ops[0], ROp::Create { new: 0, template: Some(_), parent: Obj::Existing(_), .. }));
        assert!(matches!(r.ops[4], ROp::Copy { source: Obj::New(0), new: 1, parent: Obj::Existing(_) }));
        // Preconditions: only the deleted lamp. The table is the create's parent (none, M2 (a)); the balls are plain
        // writes.
        assert_eq!(r.preconditions.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [oid(2)]);
        assert_eq!(r.count, 1 + 3 + 1 + 1 + 1);
    }

    #[test]
    fn default_versions_come_from_the_connection_then_the_base() {
        let m = Mem::pit(1);
        let seen = |id: ObjectId| (id == oid(1)).then(|| Version("v:seen".into()));
        let r = resolve_tx(
            &m,
            &tx(json!([
                {"op": "set", "id": "/Table", "field": "Shape.a", "value": 1},
                {"op": "set", "id": "/Ball 0", "field": "Ball.a", "value": 1},
                {"op": "delete", "id": "/Table/Lamp", "if_version": "any"}
            ])),
            &seen,
        );
        // "/Table/Lamp" is ambiguous: two lamps.
        let e = r.unwrap_err();
        assert_eq!((e.code, e.op), (RefusalCode::Ambiguous, Some(2)));
        assert_eq!(e.details.unwrap()["matches"].as_array().unwrap().len(), 2);
        let r = resolve_tx(
            &m,
            &tx(json!([
                {"op": "set", "id": "/Table", "field": "Shape.a", "value": 1},
                {"op": "set", "id": "/Ball 0", "field": "Ball.a", "value": 1},
                {"op": "delete", "id": oid(3).to_string(), "if_version": "any"},
                {"op": "delete", "id": oid(2).to_string(), "if_version": "v:explicit"}
            ])),
            &seen,
        )
        .unwrap();
        assert_eq!(
            r.preconditions,
            vec![
                (oid(1), Version("v:seen".into())),
                (oid(10), Version(format!("v:{:016x}", 10))),
                (oid(2), Version("v:explicit".into())),
            ]
        );
    }

    #[test]
    fn copies_reserve_their_subtree() {
        let m = with_templates();
        let r = resolve_tx(
            &m,
            &tx(json!([
                {"op": "create", "as": "$l", "parent": "/Table", "name": "Lamp 3", "template": "lamp"},
                {"op": "create", "as": "$after", "parent": "/Table", "name": "x"}
            ])),
            &none,
        )
        .unwrap();
        // The lamp template has two objects (lamp, bulb): indices 0 and 1; the next create is 2.
        assert_eq!(r.refs["$after"], 2);
        let r = resolve_tx(
            &m,
            &tx(json!([{"op": "duplicate", "id": "/Table", "as": "$t2", "parent": "/Templates"}, {"op": "create", "as": "$n", "parent": "$t2", "name": "y"}])),
            &none,
        )
        .unwrap();
        assert_eq!(r.refs["$n"], 4); // table + two lamps + shade = 4 indices
    }

    #[test]
    fn refusals_name_their_op() {
        let m = Mem::pit(5);
        let cases: Vec<(Json, RefusalCode, u32)> = vec![
            (json!([{"op": "set", "id": "$nope", "field": "A.b", "value": 1}]), RefusalCode::InChange, 0),
            (json!([{"op": "set", "id": "/Missing", "field": "A.b", "value": 1}]), RefusalCode::NotFound, 0),
            (json!([{"op": "set", "where": {"has": "Ball"}, "field": "A.b", "value": 1}]), RefusalCode::Invalid, 0),
            (
                json!([{"op": "set", "where": {"has": "Ball"}, "max": 4, "field": "A.b", "value": 1}]),
                RefusalCode::TooMany,
                0,
            ),
            (
                json!([{"op": "create", "as": "$a", "parent": "/Table", "name": "a"}, {"op": "create", "as": "$a", "parent": "/Table", "name": "b"}]),
                RefusalCode::InChange,
                1,
            ),
            (
                json!([{"op": "set", "id": "/Table", "field": "Transform.position", "value": {"relative_to": "parent", "offset": [0, 0, 0]}}]),
                RefusalCode::Invalid,
                0,
            ),
            (
                json!([{"op": "reparent", "where": {"has": "Ball"}, "max": 9, "parent": "/Table"}]),
                RefusalCode::Invalid,
                0,
            ),
            (json!([{"op": "set", "id": "me", "field": "A.b", "value": 1}]), RefusalCode::Invalid, 0),
            (json!([{"op": "create", "parent": "/Table", "name": "c", "template": "nope"}]), RefusalCode::NotFound, 0),
        ];
        for (ops, code, op) in cases {
            let e = resolve_tx(&m, &tx(ops.clone()), &none).unwrap_err();
            assert_eq!((e.code, e.op), (code, Some(op)), "{ops}: {}", e.message);
        }
        let e = resolve_tx(&m, &tx(json!([{"op": "set", "id": "/Table", "field": "Transform.position", "value": {"relative_to": "parent", "offset": [0, 0, 0]}}])), &none).unwrap_err();
        assert_eq!(e.message, "\"Table\" has no parent; name a frame.");
    }

    #[test]
    fn the_op_limit_counts_expanded_ops() {
        let mut m = Mem { frame: 1, objs: vec![] };
        for i in 0..200u8 {
            m.add(i, &format!("b{i}"), None, &["Ball"], [0.0; 3]);
        }
        let one = json!({"op": "set", "where": {"has": "Ball"}, "max": 1000, "field": "Ball.a", "value": 1});
        let ops: Vec<Json> = std::iter::repeat_n(one, 21).collect(); // 21 x 200 = 4,200 > 4,096
        let e = resolve_tx(&m, &tx(Json::Array(ops)), &none).unwrap_err();
        assert_eq!(e.code, RefusalCode::TooLarge);
        let _ = (At::default(), Selector::Has(String::new())); // types used by neighbours' tests
    }

    // Review M1: a copy reserves an index per object it brings, so the objects one change creates are capped and
    // checked before anything is reserved. Before the fix, these 4,096 copies of a 1,000-object subtree reserved
    // about 4 M indices and were accepted.
    #[test]
    fn copies_are_capped_by_the_objects_they_create() {
        let b = Big::wide(2, 999); // two roots of 1,000 objects each
        let src = bid(0).to_string();
        let to = bid(1_000).to_string();
        let one = json!({"op": "duplicate", "id": src, "parent": to});
        let t = std::time::Instant::now();
        let e = resolve_tx(&b, &tx(Json::Array(std::iter::repeat_n(one.clone(), 4_096).collect())), &none).unwrap_err();
        assert_eq!((e.code, e.op), (RefusalCode::TooLarge, Some(65)), "{}", e.message); // the 66th copy passes 65,536
        assert_eq!(e.details, Some(json!({ "limit": 65_536 })));
        assert!(t.elapsed().as_secs() < 10, "took {:?}", t.elapsed());
        // Within the cap it resolves, and a copy of a new copy counts the new subtree.
        let ok = resolve_tx(&b, &tx(Json::Array(std::iter::repeat_n(one, 65).collect())), &none).unwrap();
        assert_eq!((ok.new_count, ok.count), (65_000, 65_000));
        let chain = json!([
            {"op": "duplicate", "id": bid(0).to_string(), "parent": bid(1_000).to_string(), "as": "$a"},
            {"op": "duplicate", "id": "$a", "parent": bid(1_000).to_string()},
        ]);
        assert_eq!(resolve_tx(&b, &tx(chain), &none).unwrap().new_count, 2_000);
    }

    // design-reviewer M1: an explicit if_version wins over a default, and two different explicit ones are refused.
    #[test]
    fn explicit_versions_win_and_must_agree() {
        let m = Mem::pit(0);
        let t = oid(1).to_string();
        let seen = |_: ObjectId| Some(Version("v:seen".into()));
        let ops = json!([
            {"op": "set", "id": t, "field": "Shape.a", "value": 1},
            {"op": "delete", "id": t, "if_version": "v:explicit"},
        ]);
        let r = resolve_tx(&m, &tx(ops), &seen).unwrap();
        assert_eq!(r.preconditions, [(oid(1), Version("v:explicit".into()))]);
        let ops = json!([
            {"op": "set", "id": t, "field": "Shape.a", "value": 1, "if_version": "v:one"},
            {"op": "set", "id": t, "field": "Shape.a", "value": 2, "if_version": "v:two"},
        ]);
        let e = resolve_tx(&m, &tx(ops), &seen).unwrap_err();
        assert_eq!((e.code, e.op), (RefusalCode::Invalid, Some(1)), "{}", e.message);
        assert_eq!(e.object.map(|o| o.name).as_deref(), Some("Table"));
        let ops = json!([
            {"op": "set", "id": t, "field": "Shape.a", "value": 1, "if_version": "v:one"},
            {"op": "set", "id": t, "field": "Shape.a", "value": 2, "if_version": "any"},
            {"op": "set", "id": t, "field": "Shape.a", "value": 3, "if_version": "v:one"},
        ]);
        assert_eq!(resolve_tx(&m, &tx(ops), &seen).unwrap().preconditions, [(oid(1), Version("v:one".into()))]);
    }

    // M2 (a), coordinator m8e173fe643ad: a parent an op attaches to gets no precondition; the reparented object keeps
    // its own.
    #[test]
    fn a_parent_gets_no_precondition() {
        let m = Mem::pit(1);
        let ops = json!([
            {"op": "create", "parent": "/Table", "name": "Cup"},
            {"op": "reparent", "id": "/Ball 0", "parent": "/Table"},
            {"op": "duplicate", "id": "/Ball 0", "parent": "/Table"},
        ]);
        let r = resolve_tx(&m, &tx(ops), &none).unwrap();
        assert_eq!(r.preconditions, [(oid(10), Version(format!("v:{:016x}", 10)))]);
    }

    // design-reviewer M3: resolution cost is bounded: input ops up front, `where` results memoised per selector,
    // and a budget of objects examined.
    #[test]
    fn resolution_cost_is_bounded() {
        let b = Big::wide(1, 99_999);
        let t = std::time::Instant::now();
        let none_match = |n: u32| json!({"op": "delete", "where": {"has": format!("Nothing{n}")}, "max": 1});
        // 500 distinct selectors matching nothing: refused once the examined objects pass the budget.
        let e = resolve_tx(&b, &tx(Json::Array((0..500).map(none_match).collect())), &none).unwrap_err();
        assert_eq!((e.code, e.op), (RefusalCode::TooLarge, Some(10)), "{}", e.message);
        assert_eq!(e.details, Some(json!({ "limit": EXAMINE_MAX })));
        // The same selector repeated is examined once.
        let r = resolve_tx(&b, &tx(Json::Array((0..500).map(|_| none_match(0)).collect())), &none).unwrap();
        assert!(r.ops.is_empty());
        // More input ops than pe-world allows are refused before any work, naming no op.
        let many = Json::Array((0..4_097).map(|_| json!({"op": "delete", "id": bid(1).to_string()})).collect());
        let e = resolve_tx(&b, &tx(many), &none).unwrap_err();
        assert_eq!((e.code, e.op), (RefusalCode::TooLarge, None));
        assert!(t.elapsed().as_secs() < 10, "took {:?}", t.elapsed());
    }

    // design-reviewer M3 gap: paths in plain ids, parents and attach lists, and inside a selector, are charged to
    // the same budget.
    #[test]
    fn path_walks_in_a_change_are_charged() {
        let b = Big::wide(1, 99_999);
        let t = std::time::Instant::now();
        let globs: Vec<Json> = (0..200).map(|_| json!("/**/zzz")).collect();
        let e = resolve_tx(&b, &tx(json!([{"op": "delete", "where": {"ids": globs}, "max": 10}])), &none).unwrap_err();
        assert_eq!((e.code, e.op), (RefusalCode::TooLarge, Some(0)), "{}", e.message);
        // Plain ids by a glob that resolves to one object each: refused once the visits pass the budget.
        // About 200,000 visits each, so the sixth passes 1,000,000.
        let ops: Vec<Json> = (0..100).map(|_| json!({"op": "delete", "id": "/**/Root", "if_version": "any"})).collect();
        let e = resolve_tx(&b, &tx(Json::Array(ops)), &none).unwrap_err();
        assert_eq!((e.code, e.op), (RefusalCode::TooLarge, Some(5)), "{}", e.message);
        assert!(t.elapsed().as_secs() < 10, "took {:?}", t.elapsed());
    }

    // design-reviewer m513402c91b03: a duplicate whose source subtree an earlier op changed is refused up front,
    // since pe-world's size check would refuse it as stale on every retry.
    #[test]
    fn a_copy_of_a_subtree_changed_earlier_is_refused() {
        let m = Mem::pit(2);
        let refused = |ops: Json| {
            let e = resolve_tx(&m, &tx(ops.clone()), &none).unwrap_err();
            assert_eq!(
                (e.code, e.op),
                (RefusalCode::Invalid, Some(ops.as_array().unwrap().len() as u32 - 1)),
                "{ops}: {}",
                e.message
            );
        };
        // Existing source: something created, copied, deleted or moved inside it, or moved out of it.
        refused(
            json!([{"op": "create", "parent": "/Table", "name": "Cup"}, {"op": "duplicate", "id": "/Table", "parent": "/Ball 0"}]),
        );
        refused(
            json!([{"op": "create", "parent": oid(2).to_string(), "name": "x"}, {"op": "duplicate", "id": "/Table", "parent": "/Ball 0"}]),
        );
        refused(
            json!([{"op": "duplicate", "id": "/Ball 1", "parent": "/Table"}, {"op": "duplicate", "id": "/Table", "parent": "/Ball 0"}]),
        );
        refused(
            json!([{"op": "delete", "id": oid(4).to_string()}, {"op": "duplicate", "id": "/Table", "parent": "/Ball 0"}]),
        );
        refused(
            json!([{"op": "reparent", "id": "/Ball 1", "parent": oid(2).to_string()}, {"op": "duplicate", "id": "/Table", "parent": "/Ball 0"}]),
        );
        refused(
            json!([{"op": "reparent", "id": oid(4).to_string(), "parent": "/Ball 1"}, {"op": "duplicate", "id": "/Table", "parent": "/Ball 0"}]),
        );
        // New source: an object moved under it, or out of it.
        refused(
            json!([{"op": "create", "as": "$box", "parent": "/Table", "name": "Box"}, {"op": "reparent", "id": "/Ball 0", "parent": "$box"}, {"op": "duplicate", "id": "$box"}]),
        );
        refused(json!([
            {"op": "create", "as": "$box", "parent": "/Table", "name": "Box"},
            {"op": "create", "as": "$lid", "parent": "$box", "name": "Lid"},
            {"op": "reparent", "id": "$lid", "parent": "/Table"},
            {"op": "duplicate", "id": "$box"}
        ]));
        // Fine: changes elsewhere, a copy first, and objects created under a new source (counted).
        let ok = |ops: Json| resolve_tx(&m, &tx(ops), &none).unwrap();
        ok(
            json!([{"op": "create", "parent": "/Ball 0", "name": "x"}, {"op": "duplicate", "id": "/Table", "parent": "/Ball 1"}]),
        );
        ok(
            json!([{"op": "duplicate", "id": "/Table", "parent": "/Ball 1"}, {"op": "create", "parent": "/Table", "name": "Cup"}]),
        );
        ok(
            json!([{"op": "set", "id": oid(4).to_string(), "field": "Transform.a", "value": 1}, {"op": "duplicate", "id": "/Table", "parent": "/Ball 1"}]),
        );
        let r = ok(json!([
            {"op": "create", "as": "$box", "parent": "/Table", "name": "Box"},
            {"op": "create", "parent": "$box", "name": "Lid"},
            {"op": "duplicate", "id": "$box"}
        ]));
        assert!(matches!(r.ops[2], ROp::Copy { source: Obj::New(0), new: 2, .. }));
        assert_eq!(r.new_count, 4);
    }

    // Low 7 and chunks-reviewer L2: a new script's $ref is not an object.
    #[test]
    fn a_script_ref_is_not_an_object() {
        let m = Mem::pit(1);
        let code = json!({"op": "code", "id": "$spin", "source": "pub fn spin() {}"});
        for (op, field) in [
            (json!({"op": "duplicate", "id": "$spin"}), "duplicate"),
            (json!({"op": "set", "id": "$spin", "field": "Transform.a", "value": 1}), "set"),
            (json!({"op": "create", "parent": "$spin", "name": "x"}), "create"),
            (json!({"op": "code", "id": "$other", "source": "", "attach": ["$spin"]}), "attach"),
        ] {
            let e = resolve_tx(&m, &tx(json!([code, op])), &none).unwrap_err();
            assert_eq!((e.code, e.op), (RefusalCode::Invalid, Some(1)), "{field}: {}", e.message);
        }
    }

    // chunks-reviewer L1: a template is a direct child of /Templates by exact name; Low 6: a create counts the
    // objects it makes; Low 5: a label refusal names no op.
    #[test]
    fn templates_by_name_only() {
        let m = with_templates();
        for t in ["", "*", "**", "lamp/bulb", "bulb", "/lamp"] {
            let e =
                resolve_tx(&m, &tx(json!([{"op": "create", "parent": "/Table", "name": "x", "template": t}])), &none)
                    .unwrap_err();
            assert_eq!(e.code, RefusalCode::NotFound, "{t:?}");
        }
        let r =
            resolve_tx(&m, &tx(json!([{"op": "create", "parent": "/Table", "name": "x", "template": "lamp"}])), &none)
                .unwrap();
        assert_eq!((r.new_count, r.count), (2, 2));
        let mut two = with_templates();
        two.add(60, "Templates", None, &[], [0.0; 3]);
        let e =
            resolve_tx(&two, &tx(json!([{"op": "create", "parent": "/Table", "name": "x", "template": "box"}])), &none)
                .unwrap_err();
        assert_eq!(e.code, RefusalCode::Ambiguous);
        let long: Transaction = serde_json::from_value(json!({ "label": "x".repeat(201), "ops": [] })).unwrap();
        assert_eq!(resolve_tx(&m, &long, &none).unwrap_err().op, None);
    }

    #[test]
    fn a_new_script_and_its_attachments() {
        let m = Mem::pit(1);
        let r = resolve_tx(
            &m,
            &tx(json!([{"op": "code", "id": "$spin", "source": "pub fn spin() {}", "attach": ["/Ball 0"]}])),
            &none,
        )
        .unwrap();
        assert!(
            matches!(&r.ops[0], ROp::Code { script: Obj::New(0), attach, .. } if attach == &vec![Obj::Existing(oid(10))])
        );
    }
}

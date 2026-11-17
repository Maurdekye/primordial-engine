//! `watch` (api.md 5.4): the change stream's raw items, described for one caller on the caller's own thread.
//!
//! - A `Stepped` item that applied, or one a re-step undid (`retracted`, described from the step that applied), becomes
//!   a `ChangeItem`: who made it (member and engine-written producer, from the entry's own transaction), its label and
//!   frame, and what it created, deleted, reparented and set. Created ids come from pe-world's outcome; the rest from
//!   the transaction's ops. A refused step changed nothing and is not a change. A re-step with the same result is not
//!   reported again. A change a final cut dropped after it applied here is reported again, `retracted`.
//! - A caller's own changes (submitted under its principal, or named in `Filter::of`) also report their status in
//!   `mine`: `queued` with the entry id once agreement has it, `accepted` or `refused` per step, then `final`,
//!   `dropped` or a final refusal from the final cut. A refusal is described as `apply` describes it.
//! - `where` narrows every list to the objects it selects, in the newest snapshot [I: an object deleted before the
//!   call is no longer selectable by a path or a `has`, so a `where` names deleted objects by id].
//! - Versions are read from the snapshot of the item's frame while it is retained, else the newest one.

use std::collections::HashSet;
use std::sync::Arc;

use pe_seams::world_api::{
    ChangeItem, Filter, Frame, IdVersion, Json, MemberBrief, ObjRef, ObjectId, Outcome, Producer, ProducerKind,
    SetItem, Status, TxId, Version,
};
use pe_seams::{Outcome as WorldOutcome, StepRecord};
use pe_sim::ComponentId;
use pe_world::WorldSnapshot;
use pe_world::edit::{Actor, Op, Target, Transaction};

use crate::changes::Owner;
use crate::edit::WorldEdit;
use crate::names::Names;
use crate::outcome::map_refusal;
use crate::ports::Snapshot;
use crate::world::{WorldView, version_text};

/// The engine-written producer label of an entry's transaction (api.md 4.5).
pub fn producer(a: &Actor) -> Producer {
    match a {
        Actor::Person => Producer { kind: ProducerKind::Hand, harness: None, principal: None, requested_by: None },
        Actor::Agent { harness, requested_by, .. } => Producer {
            kind: ProducerKind::Agent,
            harness: Some(harness.clone()),
            principal: None,
            requested_by: requested_by.map(Into::into),
        },
    }
}

/// The API's name for a component on `obj`, or one of its fields (`"Transform.position"`), as `set` takes it there
/// (api.md 4.3): bare where that singles it out, else its full form. A script's tag, which an attach or detach
/// writes, is `pe.tag@a:<64 hex>` while its version is active.
fn field_name(snap: &WorldSnapshot, names: &Names, obj: ObjectId, c: ComponentId, field: Option<u16>) -> String {
    let Some(d) = snap.component_desc(c) else { return format!("#{:x}", c.0) };
    if d.name() == pe_world::script::TAG_NAME {
        return names
            .tags()
            .find(|(_, t)| **t == c)
            .map_or_else(|| d.name().to_string(), |(a, _)| format!("{}@{}", d.name(), crate::resolve::asset_text(a)));
    }
    let comp = names.shown_on(&names.attached(snap, obj.into()), c).unwrap_or_else(|| d.name().to_string());
    let Some(f) = field.and_then(|i| d.fields().get(usize::from(i))) else { return comp };
    format!("{comp}.{}", crate::schema::shown(d.name(), &f.name))
}

fn value(view: &WorldView, id: ObjectId, field: &str) -> Option<Json> {
    let (component, name) = field.split_once('.')?;
    // Query shows a rotation as rotation_deg and rotation_quat; `set` and watch name the stored quaternion `rot`.
    let key = if (component, name) == ("Transform", "rot") { "rotation_quat" } else { name };
    view.fields(id, component, &ObjRef("parent".into()))?.remove(key)
}

/// What one entry did, by object.
#[derive(Default)]
struct Touched {
    created: Vec<ObjectId>,
    deleted: Vec<ObjectId>,
    reparented: Vec<ObjectId>,
    set: Vec<(ObjectId, String)>,
}

fn touched(tx: Option<&Transaction>, outcome: &WorldOutcome, snap: &WorldSnapshot) -> Touched {
    let made: &[pe_core::ObjectId] = match outcome {
        WorldOutcome::Applied { created, .. } => created,
        WorldOutcome::Refused { .. } | WorldOutcome::Unknown { .. } | WorldOutcome::Dropped { .. } => &[],
    };
    let mut t = Touched { created: made.iter().map(|id| ObjectId::from(*id)).collect(), ..Default::default() };
    let names = Names::new(snap);
    let at = |x: &Target| match x {
        Target::Existing(id) => Some(ObjectId::from(*id)),
        Target::New(k) => made.get(*k as usize).map(|id| ObjectId::from(*id)),
        Target::Root => None,
    };
    for op in tx.map_or(&[][..], |tx| &tx.ops[..]) {
        match op {
            Op::Create { .. } | Op::Copy { .. } => {}
            Op::Undelete { id } => t.created.push((*id).into()),
            Op::Delete { id } => t.deleted.extend(at(id)),
            Op::Reparent { id, .. } => t.reparented.extend(at(id)),
            Op::SetField { id, component, field, .. } => {
                t.set.extend(at(id).map(|o| (o, field_name(snap, &names, o, *component, Some(*field)))))
            }
            Op::SetRow { id, row } => {
                t.set.extend(at(id).map(|o| (o, field_name(snap, &names, o, row.component, None))))
            }
            Op::RemoveRow { id, component, .. } => {
                t.set.extend(at(id).map(|o| (o, field_name(snap, &names, o, *component, None))))
            }
            // A script's ops create, delete and move nothing; an activation's re-key reaches watch through S5's
            // `FrameView.activations`, not an entry (@coordinator-opus).
            Op::Register { .. } | Op::CommitScript { .. } | Op::AttestBuild { .. } | Op::BuildFailed { .. } => {}
        }
    }
    t
}

/// Every object a change touched (created, deleted, reparented or set), for `history`'s object filter.
pub fn touched_ids(tx: Option<&Transaction>, outcome: &WorldOutcome) -> Vec<ObjectId> {
    let made: &[pe_core::ObjectId] = match outcome {
        WorldOutcome::Applied { created, .. } => created,
        WorldOutcome::Refused { .. } | WorldOutcome::Unknown { .. } | WorldOutcome::Dropped { .. } => &[],
    };
    let mut ids: Vec<ObjectId> = made.iter().map(|i| ObjectId::from(*i)).collect();
    let at = |x: &Target| match x {
        Target::Existing(id) => Some(ObjectId::from(*id)),
        Target::New(k) => made.get(*k as usize).map(|id| ObjectId::from(*id)),
        Target::Root => None,
    };
    for op in tx.map_or(&[][..], |tx| &tx.ops[..]) {
        ids.extend(match op {
            Op::Create { parent, .. } | Op::Copy { parent, .. } => at(parent),
            Op::Undelete { id } => Some((*id).into()),
            Op::Delete { id }
            | Op::Reparent { id, .. }
            | Op::SetField { id, .. }
            | Op::SetRow { id, .. }
            | Op::RemoveRow { id, .. } => at(id),
            Op::Register { .. } | Op::CommitScript { .. } | Op::AttestBuild { .. } | Op::BuildFailed { .. } => None,
        });
    }
    ids
}

fn id_version(snap: &WorldSnapshot, id: ObjectId) -> IdVersion {
    let version = snap.info(id.into()).map_or_else(|| Version(String::new()), |i| version_text(&i.version));
    IdVersion { id, version }
}

/// What a caller's `watch` needs from the world to describe an item.
pub struct Describe<'a> {
    pub filter: &'a Filter,
    /// `where`'s objects, if it has one.
    pub select: Option<&'a HashSet<ObjectId>>,
    /// The snapshot of the item's frame, if retained, else the newest.
    pub snap: Arc<WorldSnapshot>,
    pub me: Option<ObjectId>,
}

impl Describe<'_> {
    fn keep(&self, id: &ObjectId) -> bool {
        self.select.is_none_or(|s| s.contains(id))
    }

    /// The change a step made, for `changes`; `None` if it changed nothing this caller is watching.
    pub fn change(
        &self,
        step: &StepRecord,
        retracted: bool,
        tx: Option<&Transaction>,
        by: String,
    ) -> Option<ChangeItem> {
        if !retracted && !matches!(step.outcome, WorldOutcome::Applied { .. }) {
            return None;
        }
        let t = touched(tx, &step.outcome, &self.snap);
        let view = WorldView::new(self.snap.clone(), self.me);
        let created: Vec<IdVersion> =
            t.created.iter().filter(|i| self.keep(i)).map(|i| id_version(&self.snap, *i)).collect();
        let deleted: Vec<ObjectId> = t.deleted.into_iter().filter(|i| self.keep(i)).collect();
        let reparented: Vec<IdVersion> =
            t.reparented.iter().filter(|i| self.keep(i)).map(|i| id_version(&self.snap, *i)).collect();
        let set: Vec<SetItem> = t
            .set
            .into_iter()
            .filter(|(i, _)| self.keep(i))
            .map(|(id, field)| {
                let value =
                    (!retracted && self.filter.fields.contains(&field)).then(|| value(&view, id, &field)).flatten();
                SetItem { id, field, value }
            })
            .collect();
        if self.select.is_some() && created.is_empty() && deleted.is_empty() && reparented.is_empty() && set.is_empty()
        {
            return None;
        }
        Some(ChangeItem {
            change: step.entry.into(),
            by: MemberBrief { member: step.author.into(), name: by },
            producer: tx.map_or(
                Producer { kind: ProducerKind::Hand, harness: None, principal: None, requested_by: None },
                |tx| producer(&tx.actor),
            ),
            label: tx.and_then(|tx| tx.label.clone()),
            frame: step.frame.0,
            is_final: false,
            retracted,
            created,
            deleted,
            reparented,
            set,
        })
    }
}

/// What the re-send backstop adds to a change's outcome (api.md 9.4): the entry now carrying it, and why it is
/// `dropped` when that is more than a void: held, or replaced by the author's later changes.
pub fn backstop(o: &mut Outcome, resent_as: Option<TxId>, note: Option<crate::changes::DropNote>) {
    use crate::changes::DropNote;
    use pe_seams::world_api::{Refusal, RefusalCode};
    o.resent_as = resent_as;
    let r = |message: &str, next: Option<&str>| Refusal {
        code: RefusalCode::Dropped,
        message: message.into(),
        op: None,
        object: None,
        field: None,
        read: None,
        now: None,
        winner: None,
        next: next.map(Into::into),
        details: None,
    };
    match note {
        Some(DropNote::Held(b)) => {
            o.held = Some(crate::changes::handle(b));
            o.refusal = Some(r(
                "Arrived too late and wasn't applied; it is held, so you can apply it now or discard it.",
                Some("Call world_held with this handle: action \"apply\" submits it again, \"discard\" lets it go."),
            ));
        }
        Some(DropNote::Superseded) => {
            o.refusal = Some(r(
                "Arrived too late; your later changes had already replaced all of it, so it wasn't re-sent.",
                None,
            ));
        }
        None => {}
    }
}

/// A status report on one of the caller's own changes, for `mine`.
pub fn mine(
    change: TxId,
    (status, is_final): (Status, bool),
    step: Option<&StepRecord>,
    frame: Option<Frame>,
    owner: Option<&Owner>,
    tx: Option<std::sync::Arc<Transaction>>,
    snap: &WorldSnapshot,
) -> Outcome {
    let mut o = Outcome {
        dry_run: false,
        status,
        repaired_at: step.and_then(|s| match &s.outcome {
            WorldOutcome::Unknown { repaired_at } => Some(repaired_at.frame.0),
            _ => None,
        }),
        is_final,
        change: Some(change),
        frame: step.map(|s| s.frame.0).or(frame),
        final_frame: snap.final_frame().0,
        created: Default::default(),
        changed: vec![],
        count: 0,
        refusal: None,
        diff: None,
        resent_as: None,
        held: None,
        activation: None,
        scripts: Default::default(),
    };
    match step.map(|s| &s.outcome) {
        Some(WorldOutcome::Applied { created, .. }) if status != Status::Dropped => {
            for (name, k) in owner.map_or(&[][..], |o| &o.refs[..]) {
                if let Some(id) = created.get(*k as usize) {
                    o.created.insert(name.clone(), id_version(snap, (*id).into()));
                }
            }
        }
        Some(WorldOutcome::Refused { op, reason }) if status == Status::Refused => {
            if let Some(tx) = tx {
                let op_of = owner.map_or_else(Vec::new, |o| o.op_of.to_vec());
                // Copied only here, for a refusal's envelope.
                let tx = Transaction::clone(&tx);
                o.refusal = Some(map_refusal(snap, &WorldEdit { tx, blobs: vec![], op_of }, *op, reason));
            }
        }
        _ => {}
    }
    o
}

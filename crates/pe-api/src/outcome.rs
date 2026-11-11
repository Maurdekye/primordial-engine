//! A change's dry run on pe-world, and pe-world's outcomes as the API's (api.md 5.3.2, 5.3.3, 9.2 step 3).
//!
//! `dry_run` runs pe-world's one edit path (`WorldSnapshot::dry_run_state`) on the base snapshot, before anything
//! is signed, and reads each created object's id and version from the scratch world it returns: pe-world's own
//! `touch` result, no second copy of its rules. Ids derive from the draft's coordinates, which are a prediction
//! (another entry on the chain may go first), and versions name pe-world's stand-in draft entry, so both are
//! provisional, as the outcome's `dry_run` flag says; a live change's come from its receipt and the next snapshot. A refusal names the API op it came from (a `where` or a `set` of several fields is
//! many pe-world ops).
//!
//! Not yet: `Note::Orphaned` in the outcome (it needs `Outcome.orphaned` in pe-seams), the winner's frame, member and
//! producer in a stale refusal (they come from history, with the live path; the winning change's id is in
//! `details.winner_change` meanwhile), and the dry run's field diff.

use pe_core::Frame as CoreFrame;
use pe_seams::world_api::{IdVersion, MemberId, ObjectBrief, ObjectId, Outcome, Refusal, RefusalCode, Status, TxId};
use pe_seams::{Note, Reason};
use pe_world::edit::{MAX_OPS, MAX_ROW_BYTES, OWN_CHECK, Op, Precondition};
use pe_world::{Draft, WorldSnapshot};
use serde_json::json;

use crate::edit::WorldEdit;
use crate::resolve::Resolved;
use crate::world::version_text;

fn refusal(code: RefusalCode, op: Option<u32>, message: impl Into<String>) -> Refusal {
    Refusal {
        code,
        message: message.into(),
        op,
        object: None,
        field: None,
        read: None,
        now: None,
        winner: None,
        next: None,
        details: None,
    }
}

fn brief(snap: &WorldSnapshot, id: pe_core::ObjectId) -> Option<ObjectBrief> {
    snap.info(id).map(|i| ObjectBrief { id: id.into(), name: i.name.unwrap_or_default() })
}

/// pe-world's refusal of `e` as the API's, naming the API op.
pub fn map_refusal(snap: &WorldSnapshot, e: &WorldEdit, op: Option<u32>, reason: &Reason) -> Refusal {
    let api_op = op.and_then(|o| e.op_of.get(o as usize).copied());
    let mut f = match reason {
        Reason::Malformed => {
            let rows: usize =
                e.tx.ops
                    .iter()
                    .map(|o| match o {
                        Op::Create { rows, .. } => rows.iter().map(|r| r.bytes.len()).sum(),
                        Op::SetRow { row, .. } => row.bytes.len(),
                        Op::SetField { value, .. } => value.len(),
                        _ => 0,
                    })
                    .sum();
            if e.tx.ops.len() > MAX_OPS || rows > MAX_ROW_BYTES {
                let mut f = refusal(
                    RefusalCode::TooLarge,
                    None,
                    format!(
                        "The change is past the world's limits: at most {MAX_OPS} edits and {MAX_ROW_BYTES} bytes of values."
                    ),
                );
                f.details = Some(
                    json!({ "ops": e.tx.ops.len(), "ops_limit": MAX_OPS, "bytes": rows, "bytes_limit": MAX_ROW_BYTES }),
                );
                f
            } else {
                refusal(RefusalCode::Invalid, api_op, "The world could not read this change.")
            }
        }
        Reason::NoPermission => refusal(RefusalCode::NotPermitted, api_op, "You may not edit this world."),
        Reason::Protected { owner } => {
            let mut f = refusal(RefusalCode::Protected, api_op, "This is protected by its owner.");
            f.details = Some(json!({ "owner": MemberId::from(*owner).to_string() }));
            f
        }
        Reason::Stale { precondition, .. } if *precondition == OWN_CHECK => refusal(
            RefusalCode::StaleVersion,
            api_op,
            "The subtree you copied has changed since you read it; read it again.",
        ),
        Reason::Stale { precondition, current, winner } => {
            let mut f = refusal(RefusalCode::StaleVersion, api_op, "Someone changed this since you read it.");
            if let Some(Precondition::ObjectVersion { id, is }) = e.tx.preconditions.get(*precondition as usize) {
                f.object = brief(snap, *id);
                f.read = Some(version_text(is));
            }
            f.now = Some(version_text(current));
            f.details = Some(json!({ "winner_change": TxId::from(*winner).to_string() }));
            f
        }
        Reason::NewIndexOverlap => refusal(
            RefusalCode::StaleVersion,
            api_op,
            "Something you copied has changed since you read it; read it again.",
        ),
        Reason::Deleted { object, by } => {
            let mut f = refusal(RefusalCode::NotFound, api_op, "That object has been deleted.");
            f.object = Some(ObjectBrief { id: (*object).into(), name: String::new() });
            f.details = Some(json!({ "deleted_by": TxId::from(*by).to_string() }));
            f
        }
        Reason::Unknown(o) => {
            let mut f = refusal(RefusalCode::NotFound, api_op, "No such object.");
            f.object = Some(ObjectBrief { id: (*o).into(), name: String::new() });
            f
        }
        Reason::Exists(_) => refusal(RefusalCode::Invalid, api_op, "An object with that id already exists."),
        Reason::Invalid { .. } => refusal(RefusalCode::Invalid, api_op, "A value the world doesn't accept."),
        Reason::OutOfRange => refusal(RefusalCode::Invalid, api_op, "A position past the world's range."),
        Reason::Cycle => refusal(RefusalCode::Invalid, api_op, "That would put an object inside itself."),
        Reason::TooDeep { object, limit } => {
            let mut f =
                refusal(RefusalCode::Invalid, api_op, format!("That would nest objects more than {limit} deep."));
            f.object = brief(snap, *object);
            f.details = Some(json!({ "limit": limit }));
            f
        }
        Reason::TooLarge { limit } => {
            let mut f = refusal(RefusalCode::TooLarge, None, format!("The change creates more than {limit} objects."));
            f.details = Some(json!({ "limit": limit }));
            f
        }
        Reason::Held { by } => {
            let mut f = refusal(RefusalCode::Held, api_op, "Someone is holding it.");
            f.details = Some(json!({ "by": MemberId::from(*by).to_string() }));
            f
        }
        Reason::OutOfReach | Reason::NotYourHand => refusal(RefusalCode::NotPermitted, api_op, "Not yours to move."),
        // `Reason` is `#[non_exhaustive]` in pe-seams, so this crate must keep a wildcard; a new reason lands here
        // as `invalid` with its name in the message until it gets its own arm.
        _ => refusal(RefusalCode::Invalid, api_op, format!("The world refused this change ({reason:?}).")),
    };
    if f.object.is_none()
        && let Some(o) = op
        && let Some(id) = e.tx.ops.get(o as usize).and_then(op_object)
    {
        f.object = brief(snap, id);
    }
    f
}

/// The existing object a pe-world op acts on, if it names one.
fn op_object(op: &Op) -> Option<pe_core::ObjectId> {
    use pe_world::edit::Target;
    let t = match op {
        Op::Delete { id } | Op::SetRow { id, .. } | Op::SetField { id, .. } | Op::RemoveRow { id, .. } => id,
        Op::Reparent { id, .. } => id,
        Op::Copy { source, .. } => source,
        Op::Create { parent, .. } => parent,
        Op::Undelete { id } => return Some(*id),
    };
    if let Target::Existing(id) = t { Some(*id) } else { None }
}

/// The change's dry run on `snap`: pe-world's edit path on a scratch copy, as from `draft`, changing nothing.
pub fn dry_run(snap: &WorldSnapshot, r: &Resolved, e: &WorldEdit, draft: &Draft) -> Outcome {
    let base = Outcome {
        dry_run: true,
        status: Status::Accepted,
        is_final: false,
        change: None,
        frame: None,
        final_frame: snap.final_frame().0,
        created: Default::default(),
        changed: vec![],
        count: r.count,
        refusal: None,
        diff: None,
    };
    let (outcome, after) = snap.dry_run_state(CoreFrame(snap.frame().0), draft, &e.tx);
    match outcome {
        pe_seams::Outcome::Applied { notes, .. } => {
            let mut created = std::collections::BTreeMap::new();
            for (name, k) in &r.refs {
                let id = pe_core::ObjectId::created(draft.line, draft.author, draft.chain, draft.seq, *k);
                // A script's $ref is not an object; only objects the world now has are reported.
                if let Some(info) = after.info(id) {
                    created.insert(
                        name.clone(),
                        IdVersion { id: ObjectId::from(id), version: version_text(&info.version) },
                    );
                }
            }
            let _orphans: Vec<_> = notes.iter().filter(|n| matches!(n, Note::Orphaned { .. })).collect();
            Outcome { created, ..base }
        }
        pe_seams::Outcome::Refused { op, reason } => {
            let f = map_refusal(snap, e, op, &reason);
            Outcome { status: Status::Refused, refusal: Some(f), count: 0, ..base }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::to_world;
    use crate::resolve::resolve_tx;
    use crate::world::WorldView;
    use pe_core::{ChainId, EntryRef, Frame, Hash, LineId, MemberKey, Offset, Pos};
    use pe_seams::world_api::{Json, Transaction as ApiTx, Version};
    use pe_sim::Transform;
    use pe_world::components::{Name, Protection};
    use pe_world::edit::Actor;
    use pe_world::{BlobStore, World, perm};
    use std::sync::Arc;

    const ALICE: MemberKey = MemberKey([1; 32]);
    const BOB: MemberKey = MemberKey([2; 32]);

    fn id(n: u128) -> pe_core::ObjectId {
        pe_core::ObjectId::from_u128(n)
    }
    fn ver(b: u8, op: u32) -> pe_seams::Version {
        pe_seams::Version { entry: EntryRef(Hash([b; 32])), op }
    }

    /// A Table (root, protected by Alice) with a Lamp under it, a Floor root, and a "box" template; Alice and Bob may
    /// edit.
    fn world() -> (Arc<WorldSnapshot>, WorldView) {
        let mut w = World::new(1, BlobStore::default());
        w.set_member(ALICE, perm::EDIT);
        w.set_member(BOB, perm::EDIT);
        for (n, name, parent) in [
            (10u128, "Table", None),
            (20, "Lamp", Some(10)),
            (30, "Floor", None),
            (40, "Templates", None),
            (50, "box", Some(40)),
        ] {
            let text = w.blobs().put(name.as_bytes());
            w.create(id(n), parent.map(id), ALICE, ver(n as u8, 0)).unwrap();
            let t = match parent {
                None => Transform::at(Pos::ORIGIN.checked_add(Offset([n as f64, 0.0, 0.0])).unwrap()),
                Some(_) => Transform::local([0.0, 1.0, 0.0]),
            };
            w.insert(id(n), t).unwrap();
            w.insert(id(n), Name { text }).unwrap();
        }
        w.insert(id(10), Protection { owner: ALICE }).unwrap();
        w.set_frame(Frame(5));
        let s = w.snapshot(Frame(5));
        (s.clone(), WorldView::new(s, None))
    }

    fn draft(author: MemberKey) -> Draft {
        Draft { author, line: LineId(Hash([7; 32])), chain: ChainId(Hash([8; 32])), seq: 3 }
    }

    fn run(author: MemberKey, ops: Json, seen: &dyn Fn(ObjectId) -> Option<Version>) -> Outcome {
        let (s, v) = world();
        let tx: ApiTx = serde_json::from_value(json!({ "label": "t", "ops": ops })).unwrap();
        let r = resolve_tx(&v, &tx, seen).map_err(|e| e.message).unwrap();
        let e = to_world(&r, Actor::Person, &s).map_err(|e| format!("{e:?}")).unwrap();
        dry_run(&s, &r, &e, &draft(author))
    }

    #[test]
    fn an_accepted_dry_run_reports_provisional_created_ids() {
        let o = run(ALICE, json!([{"op": "create", "as": "$cup", "parent": "/Floor", "name": "Cup"}]), &|_| None);
        assert!(o.dry_run);
        assert_eq!(o.status, Status::Accepted, "{:?}", o.refusal);
        let d = draft(ALICE);
        let want = pe_core::ObjectId::created(d.line, d.author, d.chain, d.seq, 0);
        assert_eq!(o.created["$cup"].id, ObjectId::from(want));
        assert!(o.created["$cup"].version.0.ends_with(".0"));
    }

    // chunks-reviewer's Medium: a new object's version names the last op that touched it.
    #[test]
    fn a_created_version_names_the_last_op_that_touched_it() {
        let ops = json!([
            {"op": "create", "as": "$b", "parent": "/Floor", "name": "Red box", "template": "box"},
            {"op": "set", "id": "$b", "field": "Transform.scale", "value": [2, 2, 2]},
            {"op": "create", "as": "$c", "parent": "/Floor", "name": "Cup"},
            {"op": "create", "parent": "$c", "name": "Handle"},
        ]);
        let o = run(ALICE, ops, &|_| None);
        assert_eq!(o.status, Status::Accepted, "{:?}", o.refusal);
        // pe-world ops: 0 Copy box, 1 SetRow Name on it, 2 SetField scale on it, 3 Create Cup, 4 Create Handle under Cup.
        assert!(o.created["$b"].version.0.ends_with(".2"), "{}", o.created["$b"].version.0);
        assert!(o.created["$c"].version.0.ends_with(".4"), "{}", o.created["$c"].version.0);
    }

    #[test]
    fn refusals_map_to_the_api_and_name_the_api_op() {
        // A stale version: the connection read an older one.
        let old = |_: ObjectId| Some(version_text(&ver(99, 0)));
        let o = run(ALICE, json!([{"op": "delete", "id": "/Floor"}]), &old);
        assert_eq!(o.status, Status::Refused);
        let f = o.refusal.unwrap();
        // A precondition, not an op, failed: the refusal names the object and both versions.
        assert_eq!(f.code, RefusalCode::StaleVersion, "{}", f.message);
        assert_eq!(f.read, Some(version_text(&ver(99, 0))));
        assert_eq!(f.now, Some(version_text(&ver(30, 0))));
        assert_eq!(f.object.map(|b| b.name).as_deref(), Some("Floor"));
        // A protection: Bob may add a child under the protected Table (7.8), but not copy it.
        let o = run(BOB, json!([{"op": "create", "parent": "/Table", "name": "Cup"}]), &|_| None);
        assert_eq!(o.status, Status::Accepted, "{:?}", o.refusal);
        let o = run(
            BOB,
            json!([{"op": "set", "id": "/Floor", "field": "Transform.scale", "value": [1, 1, 1]}, {"op": "duplicate", "id": "/Table", "parent": "/Floor"}]),
            &|_| None,
        );
        assert_eq!(o.status, Status::Refused);
        let f = o.refusal.unwrap();
        assert_eq!((f.code, f.op), (RefusalCode::Protected, Some(1)), "{}", f.message);
        assert_eq!(f.details, Some(json!({ "owner": MemberId::from(ALICE).to_string() })));
    }
}

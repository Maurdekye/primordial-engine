//! A change's dry run on pe-world, and pe-world's outcomes as the API's (api.md 5.3.2, 5.3.3, 9.2 step 3).
//!
//! `dry_run` runs pe-world's one edit path (`WorldSnapshot::dry_run_state`) on the base snapshot, before anything
//! is signed, and reads each created object's id and version from the scratch world it returns: pe-world's own
//! `touch` result, no second copy of its rules. Ids derive from the draft's coordinates, which are a prediction
//! (another entry on the chain may go first), and versions name pe-world's stand-in draft entry, so both are
//! provisional, as the outcome's `dry_run` flag says; a live change's come from its receipt and the next snapshot. A refusal names the API op it came from (a `where` or a `set` of several fields is
//! many pe-world ops).
//!
//! A stale refusal carries the winning change's id in `details.winner_change`; `Api` fills the refusal's `winner`
//! (frame, member, producer, label, value) from the change log once this machine has stepped it.
//!
//! Not yet: `Note::Orphaned` in the outcome (it needs `Outcome.orphaned` in pe-seams), and the dry run's field diff.

use pe_core::Frame as CoreFrame;
use pe_seams::world_api::{
    IdVersion, MemberId, ObjectBrief, ObjectId, Outcome, Refusal, RefusalCode, ScriptVersion, Status, TxId,
};
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
        // pe-world's cap on an entry's inline bytes (world-model.md 3.4): pe-api's own 1 KiB check comes first, so
        // only an undo grown past it through a race reaches this (ruling).
        Reason::TooLarge { limit } if *limit as usize == pe_world::inline::MAX_INLINE_BYTES => {
            let mut f = undo_in_parts();
            f.details = Some(json!({ "limit": limit }));
            f
        }
        Reason::TooLarge { limit } => {
            let mut f = refusal(RefusalCode::TooLarge, None, format!("The change creates more than {limit} objects."));
            f.details = Some(json!({ "limit": limit }));
            f
        }
        // pe-world's shape limit (world-model.md 7.1): the slowest machine's measured bound, for the whole world.
        Reason::TooManyShapes { limit, count } => {
            let mut f = refusal(
                RefusalCode::TooLarge,
                None,
                format!(
                    "The world can hold at most {limit} shapes (things that collide), and this change would leave                      {count}. Nothing was changed."
                ),
            );
            f.details = Some(json!({ "limit": limit, "count": count }));
            let over = count.saturating_sub(*limit);
            let shapes = if over == 1 { "shape" } else { "shapes" };
            f.next = Some(format!("Delete at least {over} {shapes} first, or add that many fewer."));
            f
        }
        Reason::Held { by } => {
            let mut f = refusal(RefusalCode::Held, api_op, "Someone is holding it.");
            f.details = Some(json!({ "by": MemberId::from(*by).to_string() }));
            f
        }
        Reason::OutOfReach | Reason::NotYourHand => refusal(RefusalCode::NotPermitted, api_op, "Not yours to move."),
        // world-model.md 8.1: the author's own cooldown on the object; retryable from `until`.
        Reason::Cooldown { until } => {
            let mut f = refusal(RefusalCode::NotReady, api_op, "You let go of it a moment ago; wait a little.");
            f.details = Some(json!({ "until_frame": until.0 }));
            f.next = Some(format!("Try again from frame {}.", until.0));
            f
        }
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
        // A script's ops name no object (world-model.md 7.9).
        Op::Register { .. } | Op::CommitScript { .. } | Op::AttestBuild { .. } | Op::BuildFailed { .. } => return None,
    };
    if let Target::Existing(id) = t { Some(*id) } else { None }
}

/// The change's dry run on `snap`: pe-world's edit path on a scratch copy, as from `draft`, changing nothing.
/// The key `Outcome.scripts` gives a committed script: its `$name` if this change creates it, else its id.
pub fn script_key(r: &Resolved, script: &pe_world::edit::AssetTarget) -> String {
    match script {
        pe_world::edit::AssetTarget::New(k) => {
            r.script_refs.iter().find(|(_, v)| *v == k).map(|(n, _)| n.clone()).unwrap_or_default()
        }
        pe_world::edit::AssetTarget::Existing(a) => crate::resolve::asset_text(a),
    }
}

fn inline_bytes(tx: &pe_world::edit::Transaction) -> usize {
    tx.inline.iter().map(Vec::len).sum()
}

fn undo_in_parts() -> Refusal {
    let mut f = refusal(
        RefusalCode::TooLarge,
        None,
        format!(
            "Undoing this would carry more than {} bytes of names; undo this in parts.",
            pe_world::inline::MAX_INLINE_BYTES
        ),
    );
    f.next =
        Some("Revert the later, smaller changes that touched these objects first, or edit them back by hand.".into());
    f
}

/// The inline-byte limits (api.md 5.3; rulings), checked before pe-world's dry run: a
/// change carries at most [`INLINE_MAX`](crate::edit::INLINE_MAX) bytes of names; a revert, up to pe-world's cap.
fn inline_refusal(e: &WorldEdit) -> Option<Refusal> {
    let n = inline_bytes(&e.tx);
    if e.undo {
        return (n > pe_world::inline::MAX_INLINE_BYTES).then(undo_in_parts);
    }
    (n > crate::edit::INLINE_MAX).then(|| {
        let mut f = refusal(
            RefusalCode::TooLarge,
            None,
            format!("The change carries {n} bytes of names, more than {}; split this change.", crate::edit::INLINE_MAX),
        );
        f.details = Some(json!({ "limit": crate::edit::INLINE_MAX, "bytes": n }));
        f
    })
}

/// Whether undoing an applied change would carry more names than one change may (ruling, point 2):
/// its recorded inverse, predicted by the dry run.
fn undo_refusal(e: &WorldEdit, inverse: Option<&[u8]>) -> Option<Refusal> {
    if e.undo {
        return None;
    }
    let n = inverse.and_then(pe_world::edit::Transaction::decode).map_or(0, |t| inline_bytes(&t));
    (n > crate::edit::INLINE_MAX).then(|| {
        let mut f = refusal(
            RefusalCode::TooLarge,
            None,
            format!(
                "Undoing this change would carry {n} bytes of names, more than {}; split this change.",
                crate::edit::INLINE_MAX
            ),
        );
        f.details = Some(json!({ "limit": crate::edit::INLINE_MAX, "undo_bytes": n }));
        f
    })
}

pub fn dry_run(snap: &WorldSnapshot, r: &Resolved, e: &WorldEdit, draft: &Draft) -> Outcome {
    let base = Outcome {
        dry_run: true,
        status: Status::Accepted,
        repaired_at: None,
        is_final: false,
        change: None,
        frame: None,
        final_frame: snap.final_frame().0,
        created: Default::default(),
        changed: vec![],
        count: r.count,
        refusal: None,
        diff: None,
        resent_as: None,
        held: None,
        activation: None,
        scripts: Default::default(),
    };
    // A commit's `activate_at` is the frame it applies at (api.md 7); live, step 2 sets it.
    let tx = if e.tx.ops.iter().any(|o| matches!(o, pe_world::edit::Op::CommitScript { .. })) {
        let mut tx = e.tx.clone();
        tx.set_activation(CoreFrame(snap.frame().0));
        std::borrow::Cow::Owned(tx)
    } else {
        std::borrow::Cow::Borrowed(&e.tx)
    };
    if let Some(f) = inline_refusal(e) {
        return Outcome { status: Status::Refused, refusal: Some(f), count: 0, ..base };
    }
    let (outcome, after) = snap.dry_run_state(CoreFrame(snap.frame().0), draft, &tx);
    match outcome {
        pe_seams::Outcome::Applied { notes, inverse, .. } => {
            if let Some(f) = undo_refusal(e, inverse.as_deref()) {
                return Outcome { status: Status::Refused, refusal: Some(f), count: 0, ..base };
            }
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
            // The script it commits, its id derived from the draft's coordinates as a new object's (api.md 7).
            let mut scripts = std::collections::BTreeMap::new();
            for op in &e.tx.ops {
                if let pe_world::edit::Op::CommitScript { script, .. } = op {
                    let a = match *script {
                        pe_world::edit::AssetTarget::Existing(a) => a,
                        pe_world::edit::AssetTarget::New(k) => {
                            pe_world::script::asset_created(draft.line, draft.author, draft.chain, draft.seq, k)
                        }
                    };
                    if let Some(p) = after.pending().get(&a) {
                        scripts.insert(
                            script_key(r, script),
                            ScriptVersion { id: crate::resolve::asset_text(&a), version: version_text(&p.version) },
                        );
                    }
                }
            }
            Outcome { created, scripts, ..base }
        }
        pe_seams::Outcome::Refused { op, reason } => {
            let f = map_refusal(snap, e, op, &reason);
            Outcome { status: Status::Refused, refusal: Some(f), count: 0, ..base }
        }
        // pe-world's dry run never returns these two: they are the replica's, in its change records (S5).
        pe_seams::Outcome::Unknown { repaired_at } => {
            Outcome { status: Status::Unknown, repaired_at: Some(repaired_at.frame.0), count: 0, ..base }
        }
        pe_seams::Outcome::Dropped { .. } => Outcome { status: Status::Dropped, count: 0, ..base },
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
        world_with(|_| {})
    }

    /// [`world`], with `more` done to it before its snapshot.
    fn world_with(more: impl FnOnce(&mut World)) -> (Arc<WorldSnapshot>, WorldView) {
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
        more(&mut w);
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
        let e = to_world(&r, Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap();
        dry_run(&s, &r, &e, &draft(author))
    }

    #[test]
    fn a_cooldown_is_not_ready_until_its_frame() {
        let (snap, _) = world();
        let tx = pe_world::edit::Transaction {
            inline: vec![],
            actor: pe_world::edit::Actor::Person,
            label: None,
            preconditions: vec![],
            ops: vec![],
            blobs: vec![],
        };
        let e = WorldEdit { tx, blobs: vec![], op_of: vec![], undo: false };
        let f = map_refusal(&snap, &e, None, &Reason::Cooldown { until: pe_core::Frame(42) });
        assert_eq!(f.code, RefusalCode::NotReady);
        assert_eq!(f.details.unwrap()["until_frame"], 42);
        assert!(f.next.unwrap().contains("42"));
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

    /// Names ride inline (world-model.md 3.4), at most 255 bytes each and 1 KiB a change (rulings).
    #[test]
    fn names_ride_inline_within_their_limits() {
        let name = |k: u8| String::from_utf8(vec![b'a' + k; crate::edit::NAME_MAX]).unwrap();
        let creates = |n: u8| {
            json!((0..n).map(|k| json!({"op": "create", "parent": "/Floor", "name": name(k)})).collect::<Vec<_>>())
        };
        // Four longest names: 1,020 bytes, carried, and pe-world takes them.
        let (s, v) = world();
        let tx: ApiTx = serde_json::from_value(json!({ "label": "t", "ops": creates(4) })).unwrap();
        let r = resolve_tx(&v, &tx, &|_| None).map_err(|e| e.message).unwrap();
        let e = to_world(&r, Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap();
        assert_eq!(e.tx.inline.len(), 4);
        assert!(pe_world::inline::canonical(&e.tx.inline));
        assert_eq!(dry_run(&s, &r, &e, &draft(ALICE)).status, Status::Accepted);
        // Five: over 1 KiB, refused before pe-world.
        let o = run(ALICE, creates(5), &|_| None);
        let f = o.refusal.expect("refused");
        assert_eq!((o.status, f.code), (Status::Refused, RefusalCode::TooLarge));
        assert!(f.message.contains("split this change"), "{}", f.message);
        // A name one byte too long.
        let long = String::from_utf8(vec![b'x'; crate::edit::NAME_MAX + 1]).unwrap();
        let tx: ApiTx = serde_json::from_value(
            json!({ "label": "t", "ops": [{"op": "create", "parent": "/Floor", "name": "ok"}, {"op": "create", "parent": "/Floor", "name": long}] }),
        )
        .unwrap();
        let r = resolve_tx(&v, &tx, &|_| None).map_err(|e| e.message).unwrap();
        match to_world(&r, Actor::Person, &s, None) {
            Err(crate::edit::Unmapped::Refused(f)) => assert_eq!((f.code, f.op), (RefusalCode::Invalid, Some(1))),
            other => panic!("{other:?}"),
        }
    }

    /// An undo's size (ruling): a change whose undo would carry over 1 KiB is refused at its dry run;
    /// a revert may carry up to pe-world's 2 KiB, and past that is refused "undo this in parts".
    #[test]
    fn undos_over_the_limits_are_refused() {
        let blobs = |n: u8| -> Vec<Vec<u8>> {
            let mut v: Vec<Vec<u8>> = (0..n).map(|k| vec![k; 255]).collect();
            v.sort_by_key(|b| pe_world::BlobRef::of(b).hash);
            v
        };
        let tx = |inline| pe_world::edit::Transaction {
            inline,
            actor: Actor::Person,
            label: None,
            preconditions: vec![],
            ops: vec![],
            blobs: vec![],
        };
        let forward = WorldEdit { tx: tx(vec![]), blobs: vec![], op_of: vec![], undo: false };
        let big = tx(blobs(5)).encode();
        let f = undo_refusal(&forward, Some(&big)).expect("refused");
        assert!(f.message.contains("Undoing this change") && f.message.contains("split this change"), "{}", f.message);
        assert!(undo_refusal(&forward, Some(&tx(blobs(4)).encode())).is_none());
        // A revert: 1,275 bytes is fine; 2,295 is refused in parts, as is pe-world's own TooLarge at its cap.
        let revert = |n| WorldEdit { tx: tx(blobs(n)), blobs: vec![], op_of: vec![], undo: true };
        assert!(inline_refusal(&revert(5)).is_none());
        assert!(undo_refusal(&revert(5), Some(&big)).is_none());
        assert!(inline_refusal(&revert(9)).expect("refused").message.contains("undo this in parts"));
        let (snap, _) = world();
        let limit = pe_world::inline::MAX_INLINE_BYTES as u32;
        let f = map_refusal(&snap, &revert(9), None, &Reason::TooLarge { limit });
        assert!(f.message.contains("undo this in parts"), "{}", f.message);
    }

    /// pe-world's shape limit reaches the agent as `too_large`, with both counts and what to do (world-model.md 7.1;
    /// the M1 recording's laptop, @coordinator-opus): `world()` with `n` more shaped objects under the
    /// Floor and a shape on the box template, so a `duplicate` of the box adds one.
    #[test]
    fn a_change_past_the_world_s_shape_limit_is_too_large_with_the_counts() {
        use pe_world::components::{Shape, shape_kind};
        use pe_world::edit::MAX_SHAPES;
        let max = MAX_SHAPES as u128;
        let with = |n: u128| {
            world_with(|w| {
                let ball =
                    Shape { kind: shape_kind::SPHERE, density: 20.0, size: [0.06, 0.0, 0.0], ..Shape::default() };
                w.insert(id(50), ball).unwrap();
                for k in 0..n {
                    w.create(id(1000 + k), Some(id(30)), ALICE, ver(1, 0)).unwrap();
                    w.insert(id(1000 + k), Transform::local([0.0, 1.0, 0.0])).unwrap();
                    w.insert(id(1000 + k), ball).unwrap();
                }
            })
        };
        let copy = json!([{"op": "duplicate", "id": "/Templates/box", "parent": "/Floor"}]);
        let dry = |n: u128| {
            let (s, v) = with(n);
            let tx: ApiTx = serde_json::from_value(json!({ "label": "t", "ops": copy })).unwrap();
            let r = resolve_tx(&v, &tx, &|_| None).map_err(|e| e.message).unwrap();
            let e = to_world(&r, Actor::Person, &s, None).map_err(|e| format!("{e:?}")).unwrap();
            dry_run(&s, &r, &e, &draft(ALICE))
        };
        // The box and `max - 1` others: its copy makes `max + 1`.
        let o = dry(max - 1);
        assert_eq!(o.status, Status::Refused);
        let f = o.refusal.unwrap();
        assert_eq!(f.code, RefusalCode::TooLarge, "{}", f.message);
        assert_eq!(f.details, Some(json!({ "limit": max as u64, "count": max as u64 + 1 })));
        assert!(f.message.contains(&format!("at most {max} shapes")) && f.message.contains("Nothing was changed"));
        assert_eq!(f.next.as_deref(), Some("Delete at least 1 shape first, or add that many fewer."));
        // One fewer, and the copy reaches the limit exactly.
        assert_eq!(dry(max - 2).status, Status::Accepted);
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

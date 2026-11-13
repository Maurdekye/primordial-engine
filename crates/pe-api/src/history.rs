//! `history` (api.md 5.5): the session's changes on this machine, newest first, from the change log's steps.
//!
//! - One entry per change stepped here (its newest step), with its status as the log knows it, and an engine-written
//!   summary ("Created Cup under Table; deleted Lamp", at most `SUMMARY_MAX` bytes).
//! - Filters: objects (`where`, matched on the newest snapshot, like `watch`), member, producer kind, a frame range,
//!   the caller's own. Paged by a cursor naming the last entry returned, in the order (frame, entry) descending.
//! - In M1 history is what the change log holds (its bound, `changes::CAPACITY`); from M3 the world record keeps it.

use pe_seams::world_api::{ApiError, Cursor, Frame, ObjectId};
use pe_seams::{Outcome as WorldOutcome, StepRecord};
use pe_world::WorldSnapshot;
use pe_world::edit::{Op, Target, Transaction};

/// The longest summary, in bytes.
pub const SUMMARY_MAX: usize = 200;
/// Entries per page: the default and the most.
pub const LIMIT_DEFAULT: u32 = 50;
pub const LIMIT_MAX: u32 = 256;

/// The cursor after `step`.
pub fn cursor_of(step: &StepRecord) -> Cursor {
    Cursor(format!(
        "h:{}.{}",
        step.frame.0,
        pe_seams::world_api::TxId::from(step.entry).0.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

/// The position a cursor names: (frame, entry bytes).
pub fn parse_cursor(c: &Cursor) -> Result<(Frame, [u8; 32]), ApiError> {
    let bad = || ApiError::arg("cursor", "Not a history cursor; call history without one.");
    let (f, e) = c.0.strip_prefix("h:").and_then(|r| r.split_once('.')).ok_or_else(bad)?;
    let frame: Frame = f.parse().map_err(|_| bad())?;
    if e.len() != 64 {
        return Err(bad());
    }
    let mut b = [0u8; 32];
    for (i, out) in b.iter_mut().enumerate() {
        *out = u8::from_str_radix(&e[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
    }
    Ok((frame, b))
}

/// The sort key, newest first: (frame, entry) descending.
pub fn key(step: &StepRecord) -> (Frame, [u8; 32]) {
    (step.frame.0, pe_seams::world_api::TxId::from(step.entry).0)
}

fn name(snap: &WorldSnapshot, id: pe_core::ObjectId) -> String {
    snap.name(id).unwrap_or_else(|| ObjectId::from(id).to_string())
}

/// What one change did, in a line: its first ops, then how many more.
pub fn summary(tx: Option<&Transaction>, outcome: &WorldOutcome, snap: &WorldSnapshot) -> String {
    let made: &[pe_core::ObjectId] = match outcome {
        WorldOutcome::Applied { created, .. } => created,
        WorldOutcome::Refused { .. } | WorldOutcome::Unknown { .. } | WorldOutcome::Dropped { .. } => &[],
    };
    let Some(tx) = tx else {
        return match outcome {
            WorldOutcome::Applied { .. } => "A change (its contents aren't held here).".into(),
            WorldOutcome::Refused { reason, .. } => format!("Refused ({reason:?})."),
            WorldOutcome::Unknown { repaired_at } => format!(
                "A change this machine can't know the result of: a repair at frame {} replaced the state.",
                repaired_at.frame.0
            ),
            WorldOutcome::Dropped { reason } => format!("Dropped ({reason:?})."),
        };
    };
    let obj = |t: &Target| match t {
        Target::Existing(id) => name(snap, *id),
        Target::New(k) => made.get(*k as usize).map_or_else(|| "a new object".into(), |id| name(snap, *id)),
        Target::Root => "the world".into(),
    };
    let comp = |c: pe_sim::ComponentId| {
        snap.schema().find(|d| d.id() == c).map_or_else(|| format!("#{:x}", c.0), |d| d.name().to_string())
    };
    let field = |c: pe_sim::ComponentId, f: u16| {
        snap.schema()
            .find(|d| d.id() == c)
            .and_then(|d| {
                d.fields()
                    .get(usize::from(f))
                    .map(|x| format!("{}.{}", d.name(), crate::schema::shown(d.name(), &x.name)))
            })
            .unwrap_or_else(|| format!("#{:x}.{f}", c.0))
    };
    let parts: Vec<String> = tx
        .ops
        .iter()
        .map(|op| match op {
            Op::Create { new, parent, .. } => format!("created {} under {}", obj(&Target::New(*new)), obj(parent)),
            Op::Copy { source, new, parent, .. } => {
                format!("copied {} as {} under {}", obj(source), obj(&Target::New(*new)), obj(parent))
            }
            Op::Delete { id } => format!("deleted {}", obj(id)),
            Op::Reparent { id, parent, .. } => format!("moved {} under {}", obj(id), obj(parent)),
            Op::SetField { id, component, field: f, .. } => format!("set {} on {}", field(*component, *f), obj(id)),
            Op::SetRow { id, row } => format!("set {} on {}", comp(row.component), obj(id)),
            Op::RemoveRow { id, component, .. } => format!("removed {} from {}", comp(*component), obj(id)),
            Op::Undelete { id } => format!("restored {}", name(snap, *id)),
            // Slice D (world-model.md 7.9): logic ops name no object; the summary's cap trims a long list.
            Op::Register { components } => {
                // Every Register carries the script's tag; it says nothing to a reader (chunks-reviewer's nit).
                let names: Vec<&str> =
                    components.iter().map(|d| d.name()).filter(|n| *n != pe_world::script::TAG_NAME).collect();
                format!("registered {}", names.join(", "))
            }
            Op::CommitScript { script, base, .. } => {
                let hex: String = script[..4].iter().map(|b| format!("{b:02x}")).collect();
                format!(
                    "committed {} of script {hex}…",
                    if base.is_some() { "a new version" } else { "the first version" }
                )
            }
        })
        .collect();
    let mut s = String::new();
    if let WorldOutcome::Refused { .. } = outcome {
        s.push_str("Refused: ");
    }
    // The first part always shows (cut if it must be); later parts while they and a "(+N more)" tail fit, leaving a
    // byte for the full stop (@chunks-reviewer's Low 1 on 8bb45d00).
    let room = SUMMARY_MAX - 1;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            s.push_str(p);
            continue;
        }
        let next = format!("; {p}");
        let tail = format!(" (+{} more)", parts.len() - i);
        if s.len() + next.len() + tail.len() > room {
            if s.len() + tail.len() <= room {
                s.push_str(&tail);
            }
            break;
        }
        s.push_str(&next);
    }
    if parts.is_empty() {
        s.push_str("no operations");
    }
    let mut out: String = s.chars().take(1).flat_map(char::to_uppercase).chain(s.chars().skip(1)).collect();
    if out.len() > room {
        let mut cut = room;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out.push('.');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pe_core::{EntryRef, Hash, MemberKey};

    fn step(frame: u64, e: u8) -> StepRecord {
        StepRecord {
            entry: EntryRef(Hash([e; 32])),
            author: MemberKey([1; 32]),
            class: pe_seams::Class::Edit,
            frame: pe_core::Frame(frame),
            outcome: WorldOutcome::Applied { created: vec![], overwrote: vec![], notes: vec![], inverse: None },
            rekeyed: Default::default(),
        }
    }

    #[test]
    fn a_summary_never_passes_its_limit_and_always_starts_with_the_first_op() {
        let w = pe_world::World::new(1, pe_world::BlobStore::default());
        let snap = w.snapshot(pe_core::Frame(0));
        let del = |n: u128| Op::Delete { id: Target::Existing(pe_core::ObjectId::from_u128(n)) };
        let tx = |ops: Vec<Op>| Transaction {
            actor: pe_world::edit::Actor::Person,
            label: None,
            preconditions: vec![],
            ops,
            blobs: vec![],
        };
        let applied = WorldOutcome::Applied { created: vec![], overwrote: vec![], notes: vec![], inverse: None };
        let many = summary(Some(&tx((1..40).map(del).collect())), &applied, &snap);
        assert!(many.len() <= SUMMARY_MAX && many.starts_with("Deleted o:") && many.ends_with("more)."), "{many}");
        assert_eq!(summary(Some(&tx(vec![])), &applied, &snap), "No operations.");
        let one = summary(Some(&tx(vec![del(1)])), &applied, &snap);
        assert!(one.starts_with("Deleted o:") && one.ends_with('.') && !one.contains("more"));
    }

    // Slice D (world-model.md 7.9): a script's ops are summarized without object ids.
    #[test]
    fn a_script_commit_is_summarized_by_its_components_and_script() {
        let w = pe_world::World::new(1, pe_world::BlobStore::default());
        let snap = w.snapshot(pe_core::Frame(0));
        let f = pe_sim::FieldDesc::new("hp", pe_sim::FieldKind::U32, 0, 1);
        let d = pe_sim::ComponentDesc::new(pe_sim::ComponentId(0x7000_0000_0000_0001), "Health", vec![f]).unwrap();
        let commit = Op::CommitScript {
            script: [0xab; 32],
            base: None,
            source: pe_world::components::BlobRef { hash: [1; 32], len: 3 },
            toolchain: Hash([2; 32]),
            sdk: Hash([3; 32]),
            author_module: Hash([4; 32]),
            activate_at: 9,
        };
        let tx = Transaction {
            actor: pe_world::edit::Actor::Person,
            label: None,
            preconditions: vec![],
            ops: vec![Op::Register { components: vec![d, pe_world::script::tag_desc(&Hash([5; 32]))] }, commit],
            blobs: vec![],
        };
        let applied = WorldOutcome::Applied { created: vec![], overwrote: vec![], notes: vec![], inverse: None };
        let s = summary(Some(&tx), &applied, &snap);
        assert_eq!(s, "Registered Health; committed the first version of script abababab….");
        assert!(crate::watch::touched_ids(Some(&tx), &applied).is_empty());
    }

    #[test]
    fn a_cursor_round_trips_and_garbage_is_refused() {
        let s = step(7, 0xab);
        assert_eq!(parse_cursor(&cursor_of(&s)).unwrap(), key(&s));
        for bad in ["", "h:", "h:x.00", "h:1.zz", "c:1.00"] {
            assert!(parse_cursor(&Cursor(bad.into())).is_err(), "{bad}");
        }
    }
}

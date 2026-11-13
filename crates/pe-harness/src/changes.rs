//! P16, the change record's contract (S5 v1.5, `StepRecord`), checked from a member's frame views and its final log:
//! a member's `FrameView::changes` agree with what became final.
//!
//! Per member, folding every view's `changes` in publication order (the newest record of an entry wins):
//! - **No late record:** no record of an entry reaches a view when an earlier view's `final_frame` is past the
//!   entry's frame (a reader has already told its verdict). `final_frame` never goes back. This is S5's reading as
//!   built (engine-architect, after design-reviewer's Low 2): a reader applies a view's `changes`
//!   before its `final_frame`, so a record may arrive in the same view as the `final_frame` that passes its entry
//!   (pe-replica settles inside `finalize`, before it publishes), never in a later one.
//! - **Complete:** every entry of the final log stamped below the last view's `final_frame` has a record, and its
//!   newest is not `Dropped`.
//! - **Dropped only outside the final order:** an entry whose newest record is stamped below that `final_frame` and
//!   that the final log doesn't hold is `Dropped`.
//!
//! Across members ([`p16_agree`]), pairwise: every entry of the final log below two members' common `final_frame` has
//! the same newest outcome on both, unless either records it `Unknown` (it knows no verdict). Outcomes are a function of the
//! state and the entry (world-model.md 7.7), so a difference is a divergence the roots would also show, or a record
//! that lies.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pe_seams::{EntryRef, FrameView, Outcome, SignedEntry};

use crate::clock::Nanos;
use crate::props::{Prop, Violation};
use crate::trace::Node;

/// A member's newest record of each entry, with the frame it was stepped at, and its last view's `final_frame`.
#[derive(Clone, Debug, Default)]
pub struct Verdicts {
    pub newest: BTreeMap<EntryRef, (u64, Outcome)>,
    pub final_frame: Option<u64>,
}

fn v(at: Nanos, node: Node, detail: String) -> Violation {
    Violation { prop: Prop::P16, at, node: Some(node), detail }
}

fn name(e: &EntryRef) -> String {
    format!("entry {e:?}")
}

/// Folds `node`'s views, in publication order, and checks them against its final log. Returns its verdicts for
/// [`p16_agree`].
pub fn p16_member(
    node: Node,
    views: &[(Nanos, Arc<FrameView>)],
    final_log: &[SignedEntry],
) -> (Verdicts, Vec<Violation>) {
    let mut out = Vec::new();
    let mut ver = Verdicts::default();
    let mut last_at = 0;
    for (at, view) in views {
        last_at = *at;
        for r in &view.changes {
            if let Some(f) = ver.final_frame.filter(|&f| r.frame.0 < f) {
                let detail =
                    format!("{} stamped {} recorded after final_frame {f} passed it", name(&r.entry), r.frame.0);
                out.push(v(*at, node, detail));
            }
            ver.newest.insert(r.entry, (r.frame.0, r.outcome.clone()));
        }
        let now = view.final_frame.map(|f| f.0);
        match (ver.final_frame, now) {
            (Some(was), Some(now)) if now < was => {
                out.push(v(*at, node, format!("final_frame went back from {was} to {now}")));
            }
            (Some(was), None) => out.push(v(*at, node, format!("final_frame went from {was} to none"))),
            _ => {}
        }
        if now.is_some() {
            ver.final_frame = now;
        }
    }
    let Some(ff) = ver.final_frame else { return (ver, out) };
    let all: BTreeSet<EntryRef> = final_log.iter().map(SignedEntry::id).collect();
    for e in final_log.iter().filter(|e| e.entry.stamp.0 < ff) {
        let stamp = e.entry.stamp.0;
        match ver.newest.get(&e.id()) {
            None => {
                let detail = format!("final {} stamped {stamp} has no record below final_frame {ff}", name(&e.id()));
                out.push(v(last_at, node, detail));
            }
            Some((_, Outcome::Dropped { reason })) => {
                let detail = format!("final {} stamped {stamp} is recorded Dropped ({reason:?})", name(&e.id()));
                out.push(v(last_at, node, detail));
            }
            Some(_) => {}
        }
    }
    for (e, (f, o)) in &ver.newest {
        if *f < ff && !all.contains(e) && !matches!(o, Outcome::Dropped { .. }) {
            let detail = format!("{} stamped {f} is outside the final order but recorded {o:?}", name(e));
            out.push(v(last_at, node, detail));
        }
    }
    (ver, out)
}

/// Every final entry below two members' common `final_frame` has the same newest outcome on both, unless either
/// records it `Unknown`. Every pair is compared over its own common `final_frame` (design-reviewer's Low 1), and a
/// difference is reported on the higher-numbered member of the pair.
pub fn p16_agree(at: Nanos, members: &BTreeMap<Node, Verdicts>, final_log: &[SignedEntry]) -> Vec<Violation> {
    let mut out = Vec::new();
    let unknown = |o: &Outcome| matches!(o, Outcome::Unknown { .. });
    let all: Vec<(&Node, &Verdicts)> = members.iter().collect();
    for (i, &(&m, x)) in all.iter().enumerate() {
        for &(&n, y) in &all[i + 1..] {
            let (Some(a), Some(b)) = (x.final_frame, y.final_frame) else { continue };
            let ff = a.min(b);
            for e in final_log.iter().filter(|e| e.entry.stamp.0 < ff) {
                let (Some((_, a)), Some((_, b))) = (x.newest.get(&e.id()), y.newest.get(&e.id())) else {
                    continue;
                };
                if a != b && !unknown(a) && !unknown(b) {
                    let stamp = e.entry.stamp.0;
                    let detail = format!("final {} stamped {stamp}: {b:?} here, {a:?} on member {m}", name(&e.id()));
                    out.push(v(at, n, detail));
                }
            }
        }
    }
    out
}

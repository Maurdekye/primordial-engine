//! P16, the change record's contract (S5 v1.5), on a ball-pit run: a clean run keeps it, and not vacuously; each
//! planted fault in a member's `FrameView::changes` is reported by its own rule (DESIGN.md 6.2).

use std::collections::BTreeMap;
use std::sync::Arc;

use pe_harness::ballpit::{self, BallPit, BallPitRun};
use pe_harness::changes::{self, Verdicts};
use pe_harness::clock::{MS, SEC};
use pe_harness::props::{Prop, Violation};
use pe_seams::{EntryRef, FrameView, Hash, Outcome, Reason, SignedEntry, VoidReason};

fn run() -> BallPitRun {
    ballpit::run(&BallPit::new(7, 50 * MS, 3, 48, 4 * SEC, 400 * MS)).expect("the run builds")
}

type Views = Vec<(u64, Arc<FrameView>)>;

/// Member `n`'s views and final log.
fn member(r: &BallPitRun, n: u32) -> (Views, Vec<SignedEntry>) {
    (r.views[&n].clone(), r.finals[&n].entries.clone())
}

/// The first final entry stamped below the member's last `final_frame`, and the index of the view holding its newest
/// record and that record's index there.
fn a_final_entry(views: &Views, log: &[SignedEntry]) -> (EntryRef, usize, usize) {
    let ff = views.iter().rev().find_map(|(_, v)| v.final_frame).expect("a final frame").0;
    let e = log.iter().find(|e| e.entry.stamp.0 < ff).expect("a final entry below the final frame").id();
    let (vi, ri) = views
        .iter()
        .enumerate()
        .rev()
        .find_map(|(vi, (_, v))| v.changes.iter().rposition(|r| r.entry == e).map(|ri| (vi, ri)))
        .expect("a record of it");
    (e, vi, ri)
}

/// Runs `edit` on a copy of view `vi`.
fn edit_view(views: &mut Views, vi: usize, edit: impl FnOnce(&mut FrameView)) {
    let mut v = (*views[vi].1).clone();
    edit(&mut v);
    views[vi].1 = Arc::new(v);
}

fn p16(n: u32, views: &Views, log: &[SignedEntry]) -> (Verdicts, Vec<Violation>) {
    changes::p16_member(n, views, log)
}

fn assert_fires(vs: &[Violation], needle: &str) {
    assert!(vs.iter().any(|v| v.prop == Prop::P16 && v.detail.contains(needle)), "no P16 '{needle}' in {vs:#?}");
}

#[test]
fn a_clean_run_keeps_p16_and_checks_something() {
    let r = run();
    let vs = r.p16();
    assert!(vs.is_empty(), "{vs:#?}");
    for n in r.views.keys() {
        let (views, log) = member(&r, *n);
        let (ver, _) = p16(*n, &views, &log);
        let ff = ver.final_frame.expect("the member saw a final frame");
        let covered = log.iter().filter(|e| e.entry.stamp.0 < ff).count();
        // The pushes are entries: 4 s of a 3-member pit has several below the final frame on every member.
        assert!(covered >= 3, "member {n}: only {covered} final entries below final_frame {ff}");
        assert!(ver.newest.len() >= covered, "member {n}: {} records", ver.newest.len());
    }
}

#[test]
fn a_final_entry_with_no_record_fires() {
    let r = run();
    let (mut views, log) = member(&r, 1);
    let (e, _, _) = a_final_entry(&views, &log);
    for vi in 0..views.len() {
        edit_view(&mut views, vi, |v| v.changes.retain(|x| x.entry != e));
    }
    assert_fires(&p16(1, &views, &log).1, "has no record below final_frame");
}

#[test]
fn a_final_entry_recorded_dropped_fires() {
    let r = run();
    let (mut views, log) = member(&r, 1);
    let (_, vi, ri) = a_final_entry(&views, &log);
    edit_view(&mut views, vi, |v| v.changes[ri].outcome = Outcome::Dropped { reason: VoidReason::Late });
    assert_fires(&p16(1, &views, &log).1, "is recorded Dropped");
}

#[test]
fn an_entry_outside_the_final_order_not_dropped_fires() {
    let r = run();
    let (mut views, log) = member(&r, 1);
    let (_, vi, ri) = a_final_entry(&views, &log);
    edit_view(&mut views, vi, |v| {
        let mut x = v.changes[ri].clone();
        x.entry = EntryRef(Hash([0xab; 32]));
        v.changes.push(x);
    });
    assert_fires(&p16(1, &views, &log).1, "is outside the final order");
}

#[test]
fn a_record_after_its_final_frame_passed_fires() {
    let r = run();
    let (mut views, log) = member(&r, 1);
    let (_, vi, ri) = a_final_entry(&views, &log);
    let rec = views[vi].1.changes[ri].clone();
    let last = views.len() - 1;
    edit_view(&mut views, last, |v| v.changes.push(rec));
    assert_fires(&p16(1, &views, &log).1, "recorded after final_frame");
}

#[test]
fn members_disagreeing_on_a_final_verdict_fires() {
    let r = run();
    let mut verdicts = BTreeMap::new();
    for n in r.views.keys() {
        let (views, log) = member(&r, *n);
        verdicts.insert(*n, p16(*n, &views, &log).0);
    }
    let (views, log) = member(&r, 2);
    let (e, _, _) = a_final_entry(&views, &log);
    let wrong = Outcome::Refused { op: None, reason: Reason::Malformed };
    verdicts.get_mut(&2).expect("member 2").newest.get_mut(&e).expect("its record").1 = wrong;
    let longest = r.finals.values().map(|l| l.entries.clone()).max_by_key(Vec::len).expect("a final log");
    let vs = changes::p16_agree(0, &verdicts, &longest);
    assert!(vs.iter().any(|v| v.node == Some(2) && v.detail.contains("on member 1")), "{vs:#?}");
}

#[test]
fn two_members_disagreeing_where_the_first_knows_no_verdict_fires() {
    // design-reviewer's Low 1: comparing only with the first member would miss this.
    let r = run();
    let mut verdicts = BTreeMap::new();
    for n in r.views.keys() {
        let (views, log) = member(&r, *n);
        verdicts.insert(*n, p16(*n, &views, &log).0);
    }
    let (views, log) = member(&r, 2);
    let (e, _, _) = a_final_entry(&views, &log);
    let cut = r.finals[&1].cuts.values().next().expect("a cut").at;
    verdicts.get_mut(&1).expect("member 1").newest.get_mut(&e).expect("its record").1 =
        Outcome::Unknown { repaired_at: cut };
    verdicts.get_mut(&3).expect("member 3").newest.get_mut(&e).expect("its record").1 =
        Outcome::Refused { op: None, reason: Reason::Malformed };
    let longest = r.finals.values().map(|l| l.entries.clone()).max_by_key(Vec::len).expect("a final log");
    let vs = changes::p16_agree(0, &verdicts, &longest);
    assert!(vs.iter().any(|v| v.node == Some(3) && v.detail.contains("on member 2")), "{vs:#?}");
    assert!(!vs.iter().any(|v| v.detail.contains("on member 1")), "Unknown is exempt: {vs:#?}");
}

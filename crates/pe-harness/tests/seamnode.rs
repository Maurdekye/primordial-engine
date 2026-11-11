//! The runner end to end over `SeamNode`: the lockstep mock behind S3's `Agreement`, `KvSeamReplica` behind S2's
//! `Replica`, and the drive loop (DESIGN.md 12, M0 step 2). The same controls as tests/runner.rs, now through the
//! real seam traits: a clean run passes every property; a silent member makes the unanimous mock pause (P8 only);
//! garbage is refused; and a late entry is voided by a cut that every member names alike (P4).

use std::collections::BTreeMap;

use pe_harness::clock::{MS, Nanos, SEC};
use pe_harness::kv::{KvReplica, Op, Template};
use pe_harness::kvrep::payload;
use pe_harness::lockstep::LockstepAgreement;
use pe_harness::net::{LinkCfg, NodeId};
use pe_harness::props::{self, Prop, Violation};
use pe_harness::seamnode::SeamNode;
use pe_harness::sim::{NodeFault, Run, Scenario, reference_log, replay, run};
use pe_harness::trace::Ev;
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Frame, SimRate, VoidReason};

const HZ: u32 = 60;
const CADENCE_NS: i64 = 100_000_000;

fn frame_at(f: u64) -> Nanos {
    f * SEC / u64::from(HZ)
}

fn nodes(n: u32, t: &Template, objects: &[u128]) -> BTreeMap<NodeId, SeamNode<LockstepAgreement>> {
    let keys = TestKeys::new(9, n);
    let rate = SimRate::new(HZ).unwrap_or_default();
    (1..=n).map(|id| (id, SeamNode::lockstep(&keys, id, t, objects, rate, CADENCE_NS))).collect()
}

fn check(r: &Run, n: u32, t: &Template, objects: &[u128]) -> Vec<Violation> {
    let ids: Vec<NodeId> = (1..=n).collect();
    let rep = replay(KvReplica::new(&ids, t, objects), &reference_log(r));
    let mut vs = props::defaults(&r.trace, &rep);
    vs.extend(props::p8_no_pause(&r.trace));
    vs
}

/// A void as the trace records it: the node, its deciding (line, height) and the reason.
type Voided = (NodeId, Option<(u64, u64)>, u16);

fn finals_of(r: &Run, n: NodeId) -> usize {
    r.trace.of(n).filter(|x| matches!(x.ev, Ev::Final { .. })).count()
}

#[test]
fn a_clean_run_over_the_seams_passes_every_property_and_is_deterministic() {
    let t = Template::AllAdmin;
    let mk = || {
        let mut sc = Scenario::new(11, 3 * SEC)
            .at(frame_at(10), 1, Op::Set { obj: 10, field: 0, value: 4 })
            .at(frame_at(10), 2, Op::Add { obj: 10, field: 0, delta: 1 })
            .at(frame_at(30), 3, Op::Create { obj: 11 })
            .at(frame_at(31), 3, Op::Set { obj: 11, field: 1, value: 8 });
        sc.link = LinkCfg { delay_ns: 25 * MS, jitter_ns: 10 * MS, ..Default::default() };
        sc
    };
    let a = run(&mk(), nodes(3, &t, &[10]));
    let vs = check(&a, 3, &t, &[10]);
    assert!(vs.is_empty(), "{vs:#?}");
    // A cut every 100 ms for 3 s, less the last few in flight.
    assert!(finals_of(&a, 1) >= 25, "the session made progress: {}", finals_of(&a, 1));
    let log = reference_log(&a);
    assert_eq!(log.iter().map(|(_, e)| e.len()).sum::<usize>(), 4, "every input is in the final log: {log:?}");
    let b = run(&mk(), nodes(3, &t, &[10]));
    assert_eq!(a.trace.records, b.trace.records, "same scenario, same trace");
}

#[test]
fn a_silent_member_makes_the_unanimous_mock_pause_and_only_p8_fires() {
    let t = Template::AllAdmin;
    let mut sc = Scenario::new(3, 6 * SEC).fault(NodeFault::Silent { node: 3, at: SEC, until: u64::MAX });
    sc.faulty.insert(3);
    let r = run(&sc, nodes(3, &t, &[]));
    let vs = check(&r, 3, &t, &[]);
    assert!(vs.iter().all(|v| v.prop == Prop::P8), "{vs:#?}");
    assert!(vs.iter().any(|v| v.node == Some(1)) && vs.iter().any(|v| v.node == Some(2)), "{vs:#?}");
}

#[test]
fn garbage_over_the_seams_is_refused_and_nothing_aborts() {
    let t = Template::AllAdmin;
    let mut sc = Scenario::new(2, 2 * SEC)
        .fault(NodeFault::Garbage { from: 4, to: 1, at: 500 * MS, bytes: vec![0xFF; 5], label: "junk".into() })
        .fault(NodeFault::Garbage { from: 4, to: 2, at: 600 * MS, bytes: vec![1, 0, 0], label: "bad entry".into() })
        .fault(NodeFault::Garbage { from: 4, to: 3, at: 700 * MS, bytes: vec![3; 200], label: "bad vote".into() });
    sc.faulty.insert(4);
    let r = run(&sc, nodes(4, &t, &[]));
    let vs = check(&r, 4, &t, &[]);
    assert!(vs.is_empty(), "{vs:#?}");
    assert_eq!(r.trace.invalid_inputs.len(), 3);
    assert_eq!(r.trace.records.iter().filter(|x| matches!(x.ev, Ev::Fault { author: 4, .. })).count(), 3);
}

#[test]
fn a_back_dated_entry_is_voided_by_the_same_cut_on_every_member() {
    // Member 4 signs a valid entry stamped at frame 2 and sends it at 1 s, long after the cut at frame 6 is final
    // without it. Uncovered, it is held and never applied; relayed to member 4's own node, it enters 4's next mark,
    // and that cut voids it as late on every member alike (P4; only a covered entry is void, mccb3d2b98d1c). No root
    // moves (P1, P2).
    let t = Template::AllAdmin;
    let keys = TestKeys::new(9, 4);
    let sorted = keys.sorted_keys();
    let i4 = (0..4).find(|&j| keys.key(j) == sorted[3]).unwrap_or(0);
    let entry =
        EntryBuilder::new(&keys, i4).next(Class::Edit, Frame(2), payload(&[Op::Set { obj: 10, field: 0, value: 9 }]));
    let mut bytes = vec![1u8];
    bytes.extend(entry.encode());
    let mut sc = Scenario::new(4, 2 * SEC);
    for to in 1..=3 {
        sc = sc.fault(NodeFault::Garbage { from: 4, to, at: SEC, bytes: bytes.clone(), label: "back-dated".into() });
    }
    sc.faulty.insert(4);
    let r = run(&sc, nodes(4, &t, &[10]));
    let vs: Vec<Violation> = check(&r, 4, &t, &[10]).into_iter().filter(|v| v.prop != Prop::P6).collect();
    assert!(vs.is_empty(), "{vs:#?}");
    let by: Vec<Voided> = r
        .trace
        .records
        .iter()
        .filter_map(|x| match x.ev {
            Ev::Voided { by, reason, .. } => Some((x.node, by, reason)),
            _ => None,
        })
        .collect();
    assert_eq!(by.len(), 4, "{by:?}");
    let first = by[0].1;
    assert!(first.is_some_and(|(_, h)| h > 1), "decided by a cut after the one it missed: {by:?}");
    assert!(by.iter().all(|&(_, b, reason)| b == first && reason == VoidReason::Late as u16), "{by:?}");
}

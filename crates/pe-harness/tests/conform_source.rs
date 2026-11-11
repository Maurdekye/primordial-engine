//! The entry source's conformance suite (coordinator m7940e24cabf1): one set of scenarios, run against every
//! agreement the harness has. Today that is the lockstep mock, with and without its relay; pe-agree joins once it
//! exists, through its test-util codec.
//!
//! The contract (S3 v1.1, `EntrySource` and `Agreement`), as P15 checks it on every honest member:
//! - `wants_roots` and `final_cut` never hand over a cut unless the member holds every entry it names, void ones
//!   included;
//! - a final cut's height never goes back;
//! - below the final frame, `entries_at` holds only entries the final cut covers and has not voided (an uncovered
//!   entry is held but never applied).
//!
//! - retention (S3 v1.2): `kept_from` never goes back and is truthful, and the replica's `keep_final_from(cv, floor)`
//!   is all a source that trims below `Cv` needs.
//!
//! Plus P1-P6 on every scenario. The controls plant the d1 defect (`vote_without_holding`) and a lying `kept_from`,
//! and P15 must fire. A time cap past the floor ends in `Finalized::Damaged`, recorded as `Ev::Damaged`.

use std::collections::BTreeMap;

use pe_harness::clock::{MS, Nanos, SEC};
use pe_harness::kv::{KvReplica, Op, Template};
use pe_harness::kvrep::payload;
use pe_harness::lockstep::LockstepAgreement;
use pe_harness::net::{LinkCfg, Loss, NodeId};
use pe_harness::props::{self, Prop, Violation};
use pe_harness::seamnode::SeamNode;
use pe_harness::sim::{NodeFault, Run, Scenario, reference_log, replay, run};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Agreement, Class, Frame, SignedEntry, SimRate};

const HZ: u32 = 60;
const CADENCE_NS: i64 = 100_000_000;
const OBJ: u128 = 10;

fn frame_at(f: u64) -> Nanos {
    f * SEC / u64::from(HZ)
}

/// What the suite needs from an agreement under test: nodes for a session, and its entry framing for the adversary.
struct Target<A: Agreement> {
    name: &'static str,
    nodes: fn(&TestKeys, u32) -> BTreeMap<NodeId, SeamNode<A>>,
    frame_entry: fn(&SignedEntry) -> Vec<u8>,
}

fn lockstep_with(
    keys: &TestKeys,
    n: u32,
    set: fn(&mut LockstepAgreement),
) -> BTreeMap<NodeId, SeamNode<LockstepAgreement>> {
    let rate = SimRate::new(HZ).unwrap_or_default();
    (1..=n)
        .map(|id| {
            let mut node = SeamNode::lockstep(keys, id, &Template::AllAdmin, &[OBJ], rate, CADENCE_NS);
            set(&mut node.agree);
            (id, node)
        })
        .collect()
}

fn lockstep_nodes(keys: &TestKeys, n: u32, relay: bool, defect: bool) -> BTreeMap<NodeId, SeamNode<LockstepAgreement>> {
    if defect {
        lockstep_with(keys, n, |a| (a.relay, a.vote_without_holding) = (false, true))
    } else if relay {
        lockstep_with(keys, n, |_| {})
    } else {
        lockstep_with(keys, n, |a| a.relay = false)
    }
}

fn targets() -> Vec<Target<LockstepAgreement>> {
    vec![
        Target {
            name: "lockstep, relay and fetch",
            nodes: |k, n| lockstep_nodes(k, n, true, false),
            frame_entry: LockstepAgreement::entry_message,
        },
        Target {
            name: "lockstep, fetch only",
            nodes: |k, n| lockstep_nodes(k, n, false, false),
            frame_entry: LockstepAgreement::entry_message,
        },
        Target {
            name: "lockstep, trims below Cv",
            nodes: |k, n| lockstep_with(k, n, |a| a.trim_below_cv = true),
            frame_entry: LockstepAgreement::entry_message,
        },
    ]
}

fn judge(r: &Run, n: u32) -> Vec<Violation> {
    let ids: Vec<NodeId> = (1..=n).collect();
    let rep = replay(KvReplica::new(&ids, &Template::AllAdmin, &[OBJ]), &reference_log(r));
    props::defaults(&r.trace, &rep)
}

/// Honest traffic over a jittery, lossy link.
fn clean<A: Agreement>(t: &Target<A>) -> Vec<Violation> {
    let keys = TestKeys::new(41, 3);
    let mut sc = Scenario::new(41, 3 * SEC)
        .at(frame_at(10), 1, Op::Set { obj: OBJ, field: 0, value: 4 })
        .at(frame_at(11), 2, Op::Add { obj: OBJ, field: 0, delta: 1 })
        .at(frame_at(40), 3, Op::Create { obj: 11 });
    sc.link =
        LinkCfg { delay_ns: 20 * MS, jitter_ns: 10 * MS, loss: Loss::Bernoulli { ppm: 10_000 }, ..Default::default() };
    let r = run(&sc, (t.nodes)(&keys, 3));
    judge(&r, 3)
}

/// A faulty author's chain with a hole, its later entries sent to one member only, the hole filled late.
fn hole<A: Agreement>(t: &Target<A>, nodes: BTreeMap<NodeId, SeamNode<A>>, keys: &TestKeys) -> Run {
    let sorted = keys.sorted_keys();
    let m = (0..3).find(|&j| keys.key(j) == sorted[2]).unwrap_or(0);
    let mut chain = EntryBuilder::new(keys, m);
    let e1 = chain.next(Class::Edit, Frame(30), payload(&[Op::Set { obj: OBJ, field: 0, value: 1 }]));
    let e2 = chain.next(Class::Edit, Frame(31), payload(&[Op::Set { obj: OBJ, field: 1, value: 2 }]));
    let e3 = chain.next(Class::Edit, Frame(32), payload(&[Op::Set { obj: OBJ, field: 2, value: 3 }]));
    let send = |to: NodeId, f: u64, e: &SignedEntry| NodeFault::Send {
        from: 3,
        to,
        at: frame_at(f),
        bytes: (t.frame_entry)(e),
    };
    let mut sc = Scenario::new(42, 3 * SEC);
    sc.link = LinkCfg { delay_ns: 10 * MS, ..Default::default() };
    for to in [1, 2, 3] {
        sc = sc.fault(send(to, 30, &e1));
    }
    for to in [1, 3] {
        sc = sc.fault(send(to, 32, &e3));
    }
    for to in [2, 3] {
        sc = sc.fault(send(to, 50, &e2));
    }
    sc.faulty.insert(3);
    run(&sc, nodes)
}

/// A valid entry back-dated below a final cut: held, never applied, then voided by the cut that covers it.
fn back_dated<A: Agreement>(t: &Target<A>) -> Vec<Violation> {
    let keys = TestKeys::new(43, 4);
    let sorted = keys.sorted_keys();
    let m = (0..4).find(|&j| keys.key(j) == sorted[3]).unwrap_or(0);
    let e =
        EntryBuilder::new(&keys, m).next(Class::Edit, Frame(2), payload(&[Op::Set { obj: OBJ, field: 0, value: 9 }]));
    let mut sc = Scenario::new(43, 2 * SEC);
    for to in [1, 2, 3] {
        sc = sc.fault(NodeFault::Send { from: 4, to, at: SEC, bytes: (t.frame_entry)(&e) });
    }
    sc.faulty.insert(4);
    let r = run(&sc, (t.nodes)(&keys, 4));
    judge(&r, 4)
}

#[test]
fn every_agreement_keeps_the_entry_source_contract() {
    for t in targets() {
        for (scenario, vs) in [
            ("clean", clean(&t)),
            ("hole", {
                let keys = TestKeys::new(42, 3);
                let r = hole(&t, (t.nodes)(&keys, 3), &keys);
                judge(&r, 3)
            }),
            ("back-dated", back_dated(&t)),
        ] {
            assert!(vs.is_empty(), "{} / {scenario}: {vs:#?}", t.name);
        }
    }
}

#[test]
fn p15_bites_on_a_vote_without_the_entries() {
    // The control: the mock's d1 defect hands over the cut that names e3 before e2 has arrived.
    let keys = TestKeys::new(42, 3);
    let t = Target {
        name: "defect",
        nodes: |k, n| lockstep_nodes(k, n, false, true),
        frame_entry: LockstepAgreement::entry_message,
    };
    let r = hole(&t, (t.nodes)(&keys, 3), &keys);
    let vs = props::p15_source_contract(&r.trace);
    assert!(vs.iter().any(|v| v.prop == Prop::P15 && v.detail.starts_with("wants_roots")), "{vs:#?}");
}

#[test]
fn p15_bites_on_a_kept_from_that_lies() {
    // The retention control: the source trims below Cv but says it keeps everything.
    let keys = TestKeys::new(41, 3);
    let t = Target {
        name: "lying kept_from",
        nodes: |k, n| lockstep_with(k, n, |a| (a.trim_below_cv, a.kept_from_lies) = (true, true)),
        frame_entry: LockstepAgreement::entry_message,
    };
    let sc = Scenario::new(41, 2 * SEC).at(frame_at(10), 1, Op::Set { obj: OBJ, field: 0, value: 4 });
    let r = run(&sc, (t.nodes)(&keys, 3));
    let vs = props::p15_source_contract(&r.trace);
    assert!(vs.iter().any(|v| v.detail.starts_with("kept_from")), "{vs:#?}");
}

#[test]
fn a_time_cap_past_the_floor_is_damaged_never_a_wait_and_keeps_the_contract() {
    // A cap of two frames trims what the first cut's root needs before its marks arrive: every member's replica
    // answers Damaged for it, recorded as such, and the source still keeps its contract (no P15).
    let keys = TestKeys::new(41, 3);
    let nodes = lockstep_with(&keys, 3, |a| a.trim_cap = Some(2));
    let sc = Scenario::new(41, 2 * SEC).at(frame_at(10), 1, Op::Set { obj: OBJ, field: 0, value: 4 });
    let r = run(&sc, nodes);
    for n in 1..=3 {
        assert!(
            r.trace.records.iter().any(|x| x.node == n && matches!(x.ev, pe_harness::trace::Ev::Damaged { .. })),
            "member {n} records no Damaged"
        );
    }
    let vs = props::p15_source_contract(&r.trace);
    assert!(vs.is_empty(), "{vs:#?}");
}

#[test]
fn p15_bites_on_a_frame_dropped_above_kept_from() {
    // A source that keeps kept_from and the final frame's predecessor but not frame 20 (no cut ends at 21, and the
    // replica stalls there, so finals stop at 18): the rotating probe up to the present finds it (md0cb46c5f152).
    let keys = TestKeys::new(41, 3);
    let nodes = lockstep_with(&keys, 3, |a| a.drop_frame = Some(20));
    let sc = Scenario::new(41, 2 * SEC).at(frame_at(10), 1, Op::Set { obj: OBJ, field: 0, value: 4 });
    let r = run(&sc, nodes);
    let vs = props::p15_source_contract(&r.trace);
    assert!(vs.iter().any(|v| v.detail.contains("frame 20 is at or above kept_from")), "{vs:#?}");
}

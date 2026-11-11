//! probe-d1, the Critical unvalidated-entry confirm (DESIGN.md 9.1), through `SeamNode` over the real seams.
//!
//! V (node 1), H (2) and M (3); M is faulty. M's chain is built by the harness with `EntryBuilder` and delivered with
//! the agreement's own entry framing (`entry_message`; pe-agree's test-util codec once it exists). M's own node gets
//! what M sends, so its marks can cover its chain. k (stamped 100) goes out at frame 100; k+1 (stamped 100) is
//! withheld; k+2 (back-dated to 97) goes to V alone among the honest at frame 103, so it arrives past a hole with a
//! stamp below k's. When k+1 arrives depends on the variant.
//!
//! The rules are the coordinator's rulings of 2026-11-11: S3 disseminates (me16a12755f58: relay, then fetch-by-name
//! at the cut), and only a covered entry can be void (mccb3d2b98d1c: an uncovered entry is held, never applied; the
//! first cut that covers it decides). What must hold, in every variant:
//! - P1-P6 and P8;
//! - k is final-effective on every honest member;
//! - k+2 is never final-effective (except in the control twin);
//! - every honest member holds the same verdict on k+2, from the same cut (P4). Where a cut covers it (main, early),
//!   that verdict is `BackStamped` on every honest member; while nothing covers it (never), no member voids it.
//!
//! Each variant runs twice: with relay and fetch, and with the relay dropped, so the fetch path alone must bring k+2
//! to H (coordinator me16a12755f58). A control with both off must fail: H can never vote for the cut that covers
//! k+2, and the session stalls (P8).
//!
//! Not here yet: the verdict from `conform` (WS-model's transition function; `BackStamped` stands in for it), the
//! n = 4 Byzantine row, the joiner and restarted-chain rows (model v1), and the three mutation controls (pe-agree's).

use std::collections::BTreeMap;

use pe_harness::clock::{Nanos, SEC};
use pe_harness::kv::{KvEntry, KvReplica, Op, Template};
use pe_harness::kvrep::payload;
use pe_harness::lockstep::LockstepAgreement;
use pe_harness::net::{LinkCfg, NodeId};
use pe_harness::props::{self, Prop, Violation};
use pe_harness::seamnode::{SeamNode, trace_seq};
use pe_harness::sim::{NodeFault, Run, Scenario, reference_log, replay, run};
use pe_harness::trace::Ev;
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Frame, SignedEntry, SimRate, VoidReason};

const HZ: u32 = 60;
const CADENCE_NS: i64 = 100_000_000;
const OBJ: u128 = 10;
/// k+2's value: never final-effective, outside the control twin.
const K2_VALUE: i64 = 997;
const HONEST: [NodeId; 2] = [1, 2];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    /// k+1 arrives after a final cut past frame 97.
    Main,
    /// k+1 never arrives.
    Never,
    /// k+1 arrives before any cut passes frame 97.
    Early,
    /// The control twin: k+2 stamped 101, above k, sent at frame 101, with k+1 early, so the first cut past its stamp
    /// covers it. Valid, final and effective everywhere. (With k+1 late, a later cut would cover k+2 below the
    /// previous cut's frame, and it would be `Late` under the covered-only rule.)
    Twin,
}

const VARIANTS: [Variant; 4] = [Variant::Main, Variant::Never, Variant::Early, Variant::Twin];

/// How the agreement disseminates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dissem {
    RelayAndFetch,
    FetchOnly,
    /// The control: neither.
    Neither,
}

fn frame_at(f: u64) -> Nanos {
    f * SEC / u64::from(HZ)
}

struct D1 {
    run: Run,
    k2: SignedEntry,
}

fn d1(variant: Variant, dissem: Dissem) -> D1 {
    let keys = TestKeys::new(21, 3);
    let sorted = keys.sorted_keys();
    let m = (0..3).find(|&j| keys.key(j) == sorted[2]).unwrap_or(0);
    let mut chain = EntryBuilder::new(&keys, m);
    let set = |field: u16, value: i64| payload(&[Op::Set { obj: OBJ, field, value }]);
    let k = chain.next(Class::Edit, Frame(100), set(0, 1));
    let k1 = chain.next(Class::Edit, Frame(100), payload(&[Op::Add { obj: OBJ, field: 0, delta: 10 }]));
    let (k2_stamp, k2_at) = if variant == Variant::Twin { (101, 101) } else { (97, 103) };
    let k2 = chain.next(Class::Edit, Frame(k2_stamp), set(1, K2_VALUE));

    let msg = LockstepAgreement::entry_message;
    let send = |to: NodeId, at: Nanos, e: &SignedEntry| NodeFault::Send { from: 3, to, at, bytes: msg(e) };
    let mut sc = Scenario::new(31, 5 * SEC).at(frame_at(60), 1, Op::Set { obj: OBJ, field: 2, value: 5 });
    sc.link = LinkCfg { delay_ns: 10_000_000, ..Default::default() };
    for to in [1, 2, 3] {
        sc = sc.fault(send(to, frame_at(100), &k));
    }
    for to in [1, 3] {
        sc = sc.fault(send(to, frame_at(k2_at), &k2));
    }
    let k1_at = match variant {
        Variant::Main => Some(frame_at(140)),
        Variant::Early | Variant::Twin => Some(frame_at(100) + 1),
        Variant::Never => None,
    };
    if let Some(at) = k1_at {
        for to in [1, 2, 3] {
            sc = sc.fault(send(to, at, &k1));
        }
    }
    sc.faulty.insert(3);
    let rate = SimRate::new(HZ).unwrap_or_default();
    let mut nodes: BTreeMap<NodeId, SeamNode<LockstepAgreement>> =
        (1..=3).map(|id| (id, SeamNode::lockstep(&keys, id, &Template::AllAdmin, &[OBJ], rate, CADENCE_NS))).collect();
    for n in nodes.values_mut() {
        n.agree.relay = dissem == Dissem::RelayAndFetch;
        n.agree.fetch = dissem != Dissem::Neither;
    }
    D1 { run: run(&sc, nodes), k2 }
}

fn effective(log: &[(u64, Vec<KvEntry>)], field: u16, value: i64) -> bool {
    log.iter().flat_map(|(_, e)| e).any(|e| e.author == 3 && e.op == Op::Set { obj: OBJ, field, value })
}

fn expect(vs: &mut Vec<Violation>, node: NodeId, what: String) {
    vs.push(props::v(Prop::Expect, 0, Some(node), what));
}

/// Every property and expectation of the probe, for one variant.
fn check(variant: Variant, dissem: Dissem) -> Vec<Violation> {
    let D1 { run: r, k2 } = d1(variant, dissem);
    let rep = replay(KvReplica::new(&[1, 2, 3], &Template::AllAdmin, &[OBJ]), &reference_log(&r));
    let mut vs = props::defaults(&r.trace, &rep);
    vs.extend(props::p8_no_pause(&r.trace));
    let seq = trace_seq(&k2.id());
    let mut verdicts = BTreeMap::new();
    for x in &r.trace.records {
        if let Ev::Voided { entry, by, reason } = x.ev
            && entry.seq == seq
            && HONEST.contains(&x.node)
        {
            verdicts.insert(x.node, (by, reason));
        }
    }
    for n in HONEST {
        let log = &r.final_logs[&n];
        if !effective(log, 0, 1) {
            expect(&mut vs, n, "k is not final-effective".into());
        }
        let k2_effective = effective(log, 1, K2_VALUE);
        if k2_effective != (variant == Variant::Twin) {
            expect(&mut vs, n, format!("k+2 final-effective: {k2_effective}"));
        }
        let v = verdicts.get(&n);
        match variant {
            Variant::Main | Variant::Early => {
                if v.is_none_or(|&(_, reason)| reason != VoidReason::BackStamped as u16) {
                    expect(&mut vs, n, format!("k+2 not voided as back-stamped: {v:?}"));
                }
            }
            Variant::Never | Variant::Twin => {
                if v.is_some() {
                    expect(&mut vs, n, format!("k+2 voided: {v:?}"));
                }
            }
        }
    }
    let distinct: std::collections::BTreeSet<_> = verdicts.values().collect();
    if distinct.len() > 1 {
        vs.push(props::v(Prop::P4, 0, None, format!("members disagree on k+2: {verdicts:?}")));
    }
    vs
}

#[test]
fn d1_holds_in_every_variant() {
    for v in VARIANTS {
        let vs = check(v, Dissem::RelayAndFetch);
        assert!(vs.is_empty(), "{v:?}: {vs:#?}");
    }
}

#[test]
fn d1_holds_with_the_relay_dropped_through_fetch_alone() {
    for v in VARIANTS {
        let vs = check(v, Dissem::FetchOnly);
        assert!(vs.is_empty(), "{v:?}: {vs:#?}");
    }
}

#[test]
fn without_dissemination_the_probe_sees_the_stall() {
    // The control: H never holds k+2, so it never votes for the cut that covers it, and the unanimous mock stalls.
    let vs = check(Variant::Main, Dissem::Neither);
    assert!(vs.iter().any(|v| v.prop == Prop::P8), "{vs:#?}");
}

#[test]
fn d1_is_deterministic() {
    let a = d1(Variant::Main, Dissem::FetchOnly).run.trace.records;
    assert_eq!(a, d1(Variant::Main, Dissem::FetchOnly).run.trace.records);
}

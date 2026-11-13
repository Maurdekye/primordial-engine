//! src/s2replay.rs, P2's straight replay through S2, checked against the key-value oracle P2 already trusts: on runs
//! over `KvSeamReplica`, a fresh replica stepping the final log through S2 gives the same root at every final frame
//! as `sim::replay` over `KvReplica`, with a voided (back-dated) entry absent from both.

use std::collections::{BTreeMap, BTreeSet};

use pe_harness::clock::{MS, Nanos, SEC};
use pe_harness::kv::{KvReplica, Op, Template};
use pe_harness::kvrep::{KvSeamReplica, payload};
use pe_harness::lockstep::LockstepAgreement;
use pe_harness::net::{LinkCfg, NodeId};
use pe_harness::s2replay;
use pe_harness::seamnode::{FinalLog, FinalSink, SeamNode};
use pe_harness::sim::{NodeFault, Run, Scenario, reference_log, replay, run};
use pe_harness::trace::Ev;
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, Frame, SimRate};

const HZ: u32 = 60;
const CADENCE_NS: i64 = 100_000_000;

fn frame_at(f: u64) -> Nanos {
    f * SEC / u64::from(HZ)
}

/// Runs `sc` over `n` members, each recording its final log; checks the two replays agree, and returns how many
/// roots they compared and the reference log's entry count.
fn both_replays(sc: &Scenario, n: u32, t: &Template, objects: &[u128]) -> (usize, usize) {
    let keys = TestKeys::new(9, n);
    let rate = SimRate::new(HZ).unwrap_or_default();
    let mut sinks: BTreeMap<NodeId, FinalSink> = BTreeMap::new();
    let nodes: BTreeMap<NodeId, SeamNode<LockstepAgreement>> = (1..=n)
        .map(|id| {
            let s = FinalSink::default();
            sinks.insert(id, s.clone());
            (id, SeamNode::lockstep(&keys, id, t, objects, rate, CADENCE_NS).record_final(s))
        })
        .collect();
    let r: Run = run(sc, nodes);
    let honest = &r.trace.honest;
    let logs: Vec<FinalLog> = sinks
        .iter()
        .filter(|(id, _)| honest.contains(id))
        .map(|(_, s)| s.lock().map(|l| l.clone()).unwrap_or_default())
        .collect();
    let frames: BTreeSet<u64> = r
        .trace
        .records
        .iter()
        .filter(|x| honest.contains(&x.node))
        .filter_map(|x| match x.ev {
            Ev::Final { frame, .. } => Some(frame),
            _ => None,
        })
        .collect();
    let reference = s2replay::reference(&logs);
    let s2 = s2replay::replay(KvSeamReplica::new(keys.sorted_keys(), t, objects), &reference, &frames);
    let ids: Vec<NodeId> = (1..=n).collect();
    let kv = replay(KvReplica::new(&ids, t, objects), &reference_log(&r));
    let s2_frames: BTreeSet<u64> = s2.keys().map(|k| k.1).collect();
    assert_eq!(s2_frames, frames, "the S2 replay has a root at every final frame");
    for (k, root) in &s2 {
        assert_eq!(kv.get(k), Some(root), "frame {}: S2 replay and the key-value oracle differ", k.1);
    }
    (s2.len(), reference.entries.len())
}

#[test]
fn the_s2_replay_matches_the_key_value_oracle_on_a_clean_run() {
    let t = Template::AllAdmin;
    let mut sc = Scenario::new(11, 3 * SEC)
        .at(frame_at(10), 1, Op::Set { obj: 10, field: 0, value: 4 })
        .at(frame_at(10), 2, Op::Add { obj: 10, field: 0, delta: 1 })
        .at(frame_at(30), 3, Op::Create { obj: 11 })
        .at(frame_at(31), 3, Op::Set { obj: 11, field: 1, value: 8 });
    sc.link = LinkCfg { delay_ns: 25 * MS, jitter_ns: 10 * MS, ..Default::default() };
    let (roots, entries) = both_replays(&sc, 3, &t, &[10]);
    assert!(roots >= 25, "compared {roots} roots");
    assert_eq!(entries, 4, "every input is in the final log");
}

#[test]
fn the_s2_replay_matches_the_key_value_oracle_with_a_voided_entry() {
    // tests/seamnode.rs's back-dated entry: member 4 signs one stamped at frame 2 and sends it at 1 s, and a later
    // cut voids it as late. It is absent from the final log, so neither replay applies it.
    let t = Template::AllAdmin;
    let keys = TestKeys::new(9, 4);
    let sorted = keys.sorted_keys();
    let i4 = (0..4).find(|&j| keys.key(j) == sorted[3]).unwrap_or(0);
    let entry =
        EntryBuilder::new(&keys, i4).next(Class::Edit, Frame(2), payload(&[Op::Set { obj: 10, field: 0, value: 9 }]));
    let mut bytes = vec![1u8];
    bytes.extend(entry.encode());
    let mut sc = Scenario::new(4, 2 * SEC).at(frame_at(40), 1, Op::Set { obj: 10, field: 1, value: 3 });
    for to in 1..=3 {
        sc = sc.fault(NodeFault::Garbage { from: 4, to, at: SEC, bytes: bytes.clone(), label: "back-dated".into() });
    }
    sc.faulty.insert(4);
    let (roots, entries) = both_replays(&sc, 4, &t, &[10]);
    assert!(roots >= 15, "compared {roots} roots");
    assert_eq!(entries, 1, "only the honest input is final");
}

//! The runner end to end over the mock node (DESIGN.md 12, M0 step 1), and the probes in the pending register.
//!
//! `LockstepNode` is the old unanimous design on purpose. A clean run must pass every property. A silent member must
//! make it pause, so P8 fires on a real run and not only on a hand-built trace. Until pe-agree replaces the mock, the
//! probes fail with P8 and nothing else, as the pending register requires.

use std::collections::BTreeMap;

use pe_harness::clock::{MS, Nanos, SEC};
use pe_harness::kv::{KvReplica, Op, Template, perm};
use pe_harness::mock::LockstepNode;
use pe_harness::net::{LinkCfg, Loss, NodeId};
use pe_harness::pending::{Pending, Verdict, judge};
use pe_harness::props::{self, Prop, Violation};
use pe_harness::sim::{NodeFault, Run, Scenario, reference_log, replay, run};
use pe_harness::trace::Ev;

const HZ: u32 = 60;

fn frame_at(f: u64) -> Nanos {
    f * SEC / u64::from(HZ)
}

fn nodes(members: &[NodeId], t: &Template, objects: &[u128]) -> BTreeMap<NodeId, LockstepNode> {
    members.iter().map(|&m| (m, LockstepNode::new(m, members, t, objects, HZ))).collect()
}

/// Every default property, plus P8, with P2's replay built from the reference final log.
fn check(r: &Run, members: &[NodeId], t: &Template, objects: &[u128]) -> Vec<Violation> {
    let rep = replay(KvReplica::new(members, t, objects), &reference_log(r));
    let mut vs = props::defaults(&r.trace, &rep);
    vs.extend(props::p8_no_pause(&r.trace));
    vs
}

fn finals_of(r: &Run, n: NodeId) -> usize {
    r.trace.of(n).filter(|x| matches!(x.ev, Ev::Final { .. })).count()
}

#[test]
fn a_clean_run_passes_every_property_and_is_deterministic() {
    let m = [1, 2, 3];
    let t = Template::AllAdmin;
    let lossy = LinkCfg { delay_ns: 25 * MS, jitter_ns: 10 * MS, ..Default::default() };
    let mk = || {
        let mut sc = Scenario::new(11, 3 * SEC)
            .at(frame_at(10), 1, Op::Set { obj: 10, field: 0, value: 4 })
            .at(frame_at(10), 2, Op::Add { obj: 10, field: 0, delta: 1 })
            .at(frame_at(30), 3, Op::Create { obj: 11 });
        sc.link = lossy.clone();
        sc
    };
    let a = run(&mk(), nodes(&m, &t, &[10]));
    let vs = check(&a, &m, &t, &[10]);
    assert!(vs.is_empty(), "{vs:#?}");
    assert!(finals_of(&a, 1) > 100, "the session made progress: {}", finals_of(&a, 1));
    let b = run(&mk(), nodes(&m, &t, &[10]));
    assert_eq!(a.trace.records, b.trace.records, "same scenario, same trace");
}

#[test]
fn a_silent_member_freezes_the_lockstep_mock_and_p8_catches_it() {
    // The h1 control at run level: unanimous lockstep stops everyone when one member goes quiet.
    let m = [1, 2, 3];
    let t = Template::AllAdmin;
    let mut sc = Scenario::new(3, 6 * SEC).fault(NodeFault::Silent { node: 3, at: SEC, until: u64::MAX });
    sc.faulty.insert(3);
    let r = run(&sc, nodes(&m, &t, &[]));
    let vs = check(&r, &m, &t, &[]);
    assert!(!vs.is_empty());
    assert!(vs.iter().all(|v| v.prop == Prop::P8), "{vs:#?}");
    assert!(vs.iter().any(|v| v.node == Some(1)) && vs.iter().any(|v| v.node == Some(2)));
}

#[test]
fn loss_slows_the_mock_but_breaks_no_safety_property() {
    // Reliable lanes retransmit lost entries (S4), so loss delays the lockstep mock; if a delay passes its 2 s window
    // it pauses. Safety holds either way, and only P8 may complain.
    let m = [1, 2, 3];
    let t = Template::AllAdmin;
    let mut sc = Scenario::new(5, 4 * SEC);
    sc.link = LinkCfg { loss: Loss::Bernoulli { ppm: 2_000 }, ..Default::default() };
    let r = run(&sc, nodes(&m, &t, &[]));
    let vs = check(&r, &m, &t, &[]);
    assert!(vs.iter().all(|v| v.prop == Prop::P8), "{vs:#?}");
}

#[test]
fn garbage_from_an_adversary_is_refused_and_nothing_aborts() {
    let m = [1, 2, 3, 4];
    let t = Template::AllAdmin;
    let mut sc = Scenario::new(2, 2 * SEC)
        .fault(NodeFault::Garbage { from: 4, to: 1, at: 500 * MS, bytes: vec![0xFF; 5], label: "short".into() })
        .fault(NodeFault::Garbage {
            from: 4,
            to: 2,
            at: 600 * MS,
            bytes: pe_harness::mock::encode(3, 40, &[]),
            label: "another's name".into(),
        });
    sc.faulty.insert(4);
    let r = run(&sc, nodes(&m, &t, &[]));
    let vs = check(&r, &m, &t, &[]);
    assert!(vs.is_empty(), "{vs:#?}");
    assert_eq!(r.trace.invalid_inputs.len(), 2);
}

#[test]
fn p6_bites_at_run_level_when_an_invalid_input_goes_unrefused() {
    // A valid message in its own name is not invalid. Labelling it invalid must make P6 fire: the control that the
    // runner's invalid-input record and P6 are wired together.
    let m = [1, 2, 3, 4];
    let t = Template::AllAdmin;
    let mut sc = Scenario::new(2, SEC).fault(NodeFault::Garbage {
        from: 4,
        to: 1,
        at: 500 * MS,
        bytes: pe_harness::mock::encode(4, 1_000, &[]),
        label: "not actually invalid".into(),
    });
    sc.faulty.insert(4);
    let r = run(&sc, nodes(&m, &t, &[]));
    assert!(props::p6_no_abort(&r.trace).iter().any(|v| v.prop == Prop::P6));
}

// The probes, against the mock, as pending tests (DESIGN.md 9).

/// probe-capture-close, public-template variant (DESIGN.md 9.2). Member 4 joined without admin rights. Its role
/// capture is refused, the roles are unchanged, and it goes silent at 3 s. Waits for pe-agree: the mock freezes.
fn probe_capture_close_public() -> Vec<Violation> {
    let m = [1, 2, 3, 4];
    let t = Template::Public { owners: vec![1, 2, 3], joiner: 1, joiner_perms: perm::EDIT };
    let mut sc = Scenario::new(7, 12 * SEC)
        .at(frame_at(60), 1, Op::Set { obj: 10, field: 0, value: 6 })
        .at(frame_at(120), 4, Op::DefineRole { role: 2, perms: 0 })
        .at(frame_at(121), 4, Op::SetRole { member: 1, role: 2 })
        .at(frame_at(122), 4, Op::SetRole { member: 2, role: 2 })
        .at(frame_at(123), 4, Op::SetRole { member: 3, role: 2 })
        .fault(NodeFault::Silent { node: 4, at: 3 * SEC, until: u64::MAX });
    sc.faulty.insert(4);
    let r = run(&sc, nodes(&m, &t, &[10]));
    let mut vs = check(&r, &m, &t, &[10]);
    // Expectations: each of member 4's four governance entries is refused on every honest node.
    for n in [1, 2, 3] {
        let refused = r.trace.of(n).filter(|x| matches!(x.ev, Ev::Refused { entry, .. } if entry.author == 4)).count();
        if refused != 4 {
            vs.push(props::v(Prop::Expect, 0, Some(n), format!("expected 4 refusals of member 4, saw {refused}")));
        }
    }
    vs
}

/// probe-twosilent5-kick (DESIGN.md 9.3), its first bullets: five members, 4 and 5 silent at 3 s; members 1-3 must
/// never pause. The kick itself needs pe-agree's governance entries; the mock has none. Waits for pe-agree.
fn probe_twosilent5() -> Vec<Violation> {
    let m = [1, 2, 3, 4, 5];
    let t = Template::AllAdmin;
    let mut sc = Scenario::new(9, 15 * SEC)
        .at(frame_at(60), 1, Op::Add { obj: 10, field: 0, delta: 6 })
        .fault(NodeFault::Silent { node: 4, at: 3 * SEC, until: u64::MAX })
        .fault(NodeFault::Silent { node: 5, at: 3 * SEC, until: u64::MAX });
    sc.faulty.extend([4, 5]);
    let r = run(&sc, nodes(&m, &t, &[10]));
    check(&r, &m, &t, &[10])
}

const REGISTER: &[Pending] = &[
    Pending {
        name: "probe-capture-close (public)",
        milestone: "M1: pe-agree",
        expect: Prop::P8,
        run: probe_capture_close_public,
    },
    Pending {
        name: "probe-twosilent5-kick (no freeze)",
        milestone: "M1: pe-agree",
        expect: Prop::P8,
        run: probe_twosilent5,
    },
];

#[test]
fn pending_probes_fail_with_their_named_property_only() {
    for p in REGISTER {
        match judge(p) {
            Verdict::FailsAsExpected => {}
            Verdict::NowPasses => panic!("{} ({}) now passes: move it out of the register", p.name, p.milestone),
            Verdict::WrongFailure(v) => panic!("{} fails for another reason: {v:#?}", p.name),
        }
    }
}

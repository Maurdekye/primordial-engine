//! The consensus core on a simulated network: seeded delays, crashed members, wrong roots, slow links.

use std::collections::BTreeMap;

use pe_seams::test_util::TestKeys;
use pe_seams::{Cut, CutRef, Frame, Hash, Hasher, HostTime, LineId, MemberKey, SelfTest, Signer};

use crate::core::{Block, BlockId, Config, Core, CoreEvent, CoreFault, Msg, Report, Timing, To, report_bytes};

const MS: i64 = 1_000_000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % ((hi - lo).max(1) as u64)) as i64
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Honest,
    /// Never acts and receives nothing.
    Crashed,
    /// Votes with a root nobody else has.
    WrongRoot,
    /// Byzantine (@design-reviewer's High, m03081706a05a): signs its vote for the current proposal with a view 1000
    /// above the real one, once per view.
    InflateView,
}

struct Sim {
    keys: TestKeys,
    nodes: Vec<Core>,
    roles: Vec<Role>,
    idx: BTreeMap<MemberKey, usize>,
    /// (deliver at, seq, from, to, msg)
    net: BTreeMap<(i64, u64), (usize, usize, Msg)>,
    seq: u64,
    now: i64,
    rng: Rng,
    /// One-way delay range per member's link, ms.
    delay: Vec<(i64, i64)>,
    finals: Vec<Vec<Block>>,
    events: Vec<Vec<CoreEvent>>,
    faults: Vec<CoreFault>,
    inflated: std::collections::BTreeSet<u64>,
    /// (member, from ms, until ms): every message to or from it sent in that window is lost.
    partition: Option<(usize, i64, i64)>,
}

fn root_of(cut: &Cut, wrong: bool) -> Hash {
    let mut h = Hasher::new("pe.test.root");
    h.write_u64(cut.at.height).write_u64(cut.at.frame.0).write_u8(wrong as u8);
    h.finish()
}

impl Sim {
    fn new(n: u32, seed: u64, roles: Vec<Role>, timing: Timing) -> Sim {
        let keys = TestKeys::new(seed, n);
        let members = keys.sorted_keys();
        let line = LineId::origin();
        let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
        let cfg = Config { line, members: members.clone(), genesis, timing, epoch: 0 };
        let nodes = (0..n as usize).map(|i| Core::new(cfg.clone(), keys.key(i), HostTime(0))).collect();
        let idx = (0..n as usize).map(|i| (keys.key(i), i)).collect();
        Sim {
            keys,
            nodes,
            roles,
            idx,
            net: BTreeMap::new(),
            seq: 0,
            now: 0,
            rng: Rng(seed),
            delay: vec![(5, 30); n as usize],
            finals: vec![Vec::new(); n as usize],
            events: vec![Vec::new(); n as usize],
            faults: Vec::new(),
            inflated: Default::default(),
            partition: None,
        }
    }

    fn send(&mut self, from: usize, to: usize, msg: Msg) {
        if self.roles[to] == Role::Crashed || to == from {
            return;
        }
        if let Some((k, a, b)) = self.partition
            && (to == k || from == k)
            && self.now >= a * MS
            && self.now < b * MS
        {
            return;
        }
        let (a, b) = self.delay[from];
        let (c, d) = self.delay[to];
        let at = self.now + (self.rng.range(a, b) + self.rng.range(c, d)) / 2 * MS;
        self.seq += 1;
        self.net.insert((at, self.seq), (from, to, msg));
    }

    fn flush(&mut self, i: usize) {
        let out = self.nodes[i].take_out();
        for (to, msg) in out {
            match to {
                To::All => {
                    for j in 0..self.nodes.len() {
                        self.send(i, j, msg.clone());
                    }
                }
                To::One(k) => {
                    let j = self.idx[&k];
                    self.send(i, j, msg);
                }
            }
        }
        for e in self.nodes[i].take_events() {
            if let CoreEvent::Final { block, .. } = &e {
                self.finals[i].push(block.clone());
            }
            self.events[i].push(e);
        }
    }

    fn act(&mut self, i: usize) {
        if self.roles[i] == Role::Crashed {
            return;
        }
        let now = HostTime(self.now);
        let signer = self.keys.signer(i);
        self.nodes[i].tick(now, &signer);
        if let Some(lead) = self.nodes[i].lead().filter(|l| now >= l.not_before) {
            {
                let p = &lead.parent.cut.at;
                let cut = Cut {
                    at: CutRef { line: p.line, height: p.height + 1, frame: Frame(p.frame.0 + 6) },
                    heads: Vec::new(),
                };
                self.nodes[i].propose(cut, now, &signer, &self.keys);
            }
        }
        if self.roles[i] == Role::InflateView {
            if let Some(p) = self.nodes[i].votable() {
                let id = p.block.id();
                let root = root_of(&p.block.cut, false);
                let view = self.nodes[i].view() + 1000;
                if self.inflated.insert(self.nodes[i].view()) {
                    let sig = signer.sign(&crate::core::vote_bytes(LineId::origin(), view, id, root));
                    let v = crate::core::Vote {
                        view,
                        block: id,
                        root,
                        voter: self.keys.key(i),
                        self_test: SelfTest::NotRun,
                        heads: Vec::new(),
                        lateness: Vec::new(),
                        sig,
                    };
                    for j in 0..self.nodes.len() {
                        self.send(i, j, Msg::Vote(v.clone()));
                    }
                }
            }
            self.flush(i);
            return;
        }
        if let Some(p) = self.nodes[i].votable() {
            let id = p.block.id();
            let root = root_of(&p.block.cut, self.roles[i] == Role::WrongRoot);
            self.nodes[i].vote(id, root, SelfTest::NotRun, Vec::new(), now, &signer);
        }
        self.flush(i);
    }

    fn run(&mut self, until_ms: i64) {
        let end = until_ms * MS;
        while self.now < end {
            let step = self.now + MS;
            while let Some((&k, _)) = self.net.iter().next() {
                if k.0 > step {
                    break;
                }
                let (from, to, msg) = self.net.remove(&k).unwrap();
                let fk = self.keys.key(from);
                if let Err(f) = self.nodes[to].receive(fk, msg, HostTime(k.0), &self.keys) {
                    self.faults.push(f);
                }
                self.flush(to);
            }
            self.now = step;
            for i in 0..self.nodes.len() {
                self.act(i);
            }
        }
    }

    /// Every pair of live members' final chains agree on their common prefix.
    fn assert_agreement(&self) {
        for i in 0..self.nodes.len() {
            for j in 0..self.nodes.len() {
                let (a, b) = (&self.finals[i], &self.finals[j]);
                for k in 0..a.len().min(b.len()) {
                    assert_eq!(a[k].id(), b[k].id(), "members {i} and {j} disagree at final #{k}");
                }
            }
            for (k, b) in self.finals[i].iter().enumerate() {
                assert_eq!(b.height(), k as u64 + 1, "member {i}: final heights must run 1, 2, ...");
            }
        }
    }

    fn min_final(&self) -> usize {
        (0..self.nodes.len()).filter(|&i| self.roles[i] != Role::Crashed).map(|i| self.finals[i].len()).min().unwrap()
    }
}

fn fast() -> Timing {
    Timing { cadence_ns: 50 * MS, min_timeout_ns: 200 * MS, max_timeout_ns: 5_000 * MS }
}

#[test]
fn four_honest_members_finalize_the_same_chain() {
    for seed in 1..=5 {
        let mut s = Sim::new(4, seed, vec![Role::Honest; 4], fast());
        s.run(5_000);
        s.assert_agreement();
        assert!(s.min_final() >= 30, "seed {seed}: only {} final", s.min_final());
        assert!(s.faults.is_empty(), "{:?}", s.faults);
    }
}

#[test]
fn a_crashed_member_slows_but_never_stops_the_line() {
    for seed in 1..=5 {
        let mut s = Sim::new(4, seed, vec![Role::Honest, Role::Honest, Role::Crashed, Role::Honest], fast());
        s.run(10_000);
        s.assert_agreement();
        assert!(s.min_final() >= 20, "seed {seed}: only {} final", s.min_final());
    }
}

#[test]
fn reputation_skips_the_crashed_leader_at_n3() {
    // At n = 3 with one crashed member, rotation alone loses every third view and with it most two-chains; with
    // reputation the crashed leader is skipped after its first timed-out view in each committed stretch.
    let mut s = Sim::new(3, 7, vec![Role::Honest, Role::Crashed, Role::Honest], fast());
    s.run(10_000);
    s.assert_agreement();
    assert!(s.min_final() >= 20, "only {} final", s.min_final());
}

#[test]
fn two_members_finalize() {
    for seed in 1..=5 {
        let mut s = Sim::new(2, seed, vec![Role::Honest; 2], fast());
        s.run(5_000);
        s.assert_agreement();
        assert!(s.min_final() >= 20, "seed {seed}: only {} final", s.min_final());
    }
}

#[test]
fn the_timeout_adapts_to_a_slow_member_at_n2() {
    // One member's link takes 300-400 ms each way, far beyond the 200 ms floor: early timeouts spoil views until the
    // timeout grows past the round trip, then the line finalizes.
    let mut s = Sim::new(2, 3, vec![Role::Honest; 2], fast());
    s.delay[1] = (300, 400);
    s.run(30_000);
    s.assert_agreement();
    assert!(s.min_final() >= 5, "only {} final", s.min_final());
}

#[test]
fn a_wrong_root_counts_for_nothing_but_the_quorum_finalizes() {
    let mut s = Sim::new(4, 11, vec![Role::Honest, Role::WrongRoot, Role::Honest, Role::Honest], fast());
    s.run(5_000);
    s.assert_agreement();
    assert!(s.min_final() >= 20);
}

#[test]
fn at_n2_a_wrong_root_splits_and_nothing_is_final() {
    let mut s = Sim::new(2, 5, vec![Role::Honest, Role::WrongRoot], fast());
    s.run(3_000);
    assert_eq!(s.min_final(), 0);
    assert!(s.events[0].iter().any(|e| matches!(e, CoreEvent::RootSplit { .. })));
    // Each split view is abandoned at once and does not grow the timeout: views keep turning over at about the
    // cadence plus a round trip (~0.1 s), not at a growing timeout (which would allow only a handful in 3 s).
    assert!(s.nodes[0].view() >= 15, "only {} views in 3 s", s.nodes[0].view());
}

#[test]
fn a_forged_vote_is_refused() {
    let mut s = Sim::new(4, 2, vec![Role::Honest; 4], fast());
    s.run(500);
    let view = s.nodes[0].view();
    let id = crate::core::BlockId(Hash::of("x", b"y"));
    let forged = crate::core::Vote {
        view,
        block: id,
        root: Hash::ZERO,
        voter: s.keys.key(1),
        self_test: SelfTest::NotRun,
        heads: Vec::new(),
        lateness: Vec::new(),
        sig: s.keys.signer(2).sign(b"not the vote bytes"),
    };
    let r = s.nodes[0].receive(s.keys.key(1), Msg::Vote(forged), HostTime(s.now), &s.keys);
    assert_eq!(r, Err(CoreFault::BadSignature));
}

#[test]
fn a_second_different_vote_in_one_view_is_equivocation() {
    let mut s = Sim::new(4, 2, vec![Role::Honest; 4], fast());
    s.run(500);
    let view = s.nodes[0].view() + 5;
    let k1 = s.keys.signer(1);
    let mk = |b: &[u8]| {
        let block = crate::core::BlockId(Hash::of("x", b));
        crate::core::Vote {
            view,
            block,
            root: Hash::ZERO,
            voter: s.keys.key(1),
            self_test: SelfTest::NotRun,
            heads: Vec::new(),
            lateness: Vec::new(),
            sig: k1.sign(&crate::core::vote_bytes(LineId::origin(), view, block, Hash::ZERO)),
        }
    };
    let (a, b) = (mk(b"a"), mk(b"b"));
    let now = HostTime(s.now);
    assert_eq!(s.nodes[0].receive(s.keys.key(1), Msg::Vote(a.clone()), now, &s.keys), Ok(()));
    assert_eq!(s.nodes[0].receive(s.keys.key(1), Msg::Vote(a), now, &s.keys), Ok(()));
    assert_eq!(s.nodes[0].receive(s.keys.key(1), Msg::Vote(b), now, &s.keys), Err(CoreFault::Equivocation));
}

#[test]
fn rates_for_the_record() {
    // Not a pass/fail bound: prints final cuts per second for the README of this crate (cargo test -- --nocapture).
    for (name, n, roles, slow) in [
        ("n4", 4, vec![Role::Honest; 4], None),
        ("n4-crash", 4, vec![Role::Honest, Role::Honest, Role::Crashed, Role::Honest], None),
        ("n3-crash", 3, vec![Role::Honest, Role::Crashed, Role::Honest], None),
        ("n2", 2, vec![Role::Honest; 2], None),
        ("n2-slow", 2, vec![Role::Honest; 2], Some(1)),
    ] {
        let mut s = Sim::new(n, 1, roles, fast());
        if let Some(i) = slow {
            s.delay[i] = (300, 400);
        }
        s.run(20_000);
        println!("{name}: {:.1} final cuts/s over 20 s (cadence 50 ms, links 5-30 ms)", s.min_final() as f64 / 20.0);
    }
}

#[cfg(feature = "mutants")]
#[test]
fn no_timeouts_lets_a_crashed_leader_stall_the_line() {
    use crate::core::Mutation;
    for mutated in [false, true] {
        let mut s = Sim::new(4, 3, vec![Role::Honest; 4], fast());
        let l1 = s.nodes[0].leader(1).unwrap();
        let crashed = (0..4).find(|&i| s.keys.key(i) == l1).unwrap();
        s.roles[crashed] = Role::Crashed;
        if mutated {
            for n in &mut s.nodes {
                n.set_mutation(Some(Mutation::NoTimeouts));
            }
        }
        s.run(3_000);
        if mutated {
            assert_eq!(s.min_final(), 0, "without timeouts the crashed leader of view 1 stalls the line");
        } else {
            assert!(s.min_final() >= 10);
        }
    }
}

#[cfg(feature = "mutants")]
#[test]
fn mutations_are_inert_unless_named_and_quorum_one_too_small_shrinks_q() {
    use crate::core::Mutation;
    let mut s = Sim::new(4, 1, vec![Role::Honest; 4], fast());
    assert_eq!(s.nodes[0].quorum(), 3);
    assert!(!s.nodes[0].mutated(Mutation::NoLockRule));
    s.nodes[0].set_mutation(Some(Mutation::QuorumOneTooSmall));
    assert_eq!(s.nodes[0].quorum(), 2);
    s.nodes[0].set_mutation(None);
    assert_eq!(s.nodes[0].quorum(), 3);
}

#[test]
fn members_derive_the_same_leader_from_a_proposal_whatever_they_committed() {
    // Option (b): the leader of a view comes from the chain the proposal's parent certifies, not from the member's own
    // committed chain. A slow member and a crashed one leave members at different committed heights during the run;
    // every 10 ms, for each recent proposal all honest members hold, they must name the same leader for the next
    // views. Red (design-reviewer m21de433b06f1): the old own-chain rule fails this.
    for seed in 1..=3 {
        let mut s = Sim::new(4, seed, vec![Role::Honest, Role::Honest, Role::Crashed, Role::Honest], fast());
        s.delay[3] = (120, 200);
        let honest = [0usize, 1, 3];
        let (mut compared, mut apart) = (0, 0);
        for t in (10..=4_000).step_by(10) {
            s.run(t);
            let heights: Vec<u64> = honest.iter().map(|&i| s.nodes[i].committed().1).collect();
            if heights.iter().any(|h| *h != heights[0]) {
                apart += 1;
            }
            let recent: Vec<BlockId> = s.events[0]
                .iter()
                .rev()
                .filter_map(|e| if let CoreEvent::Proposed { block, .. } = e { Some(*block) } else { None })
                .take(4)
                .collect();
            for id in recent {
                let Some(view) = s.nodes[0].block(&id).map(|b| b.view) else { continue };
                for v in view + 1..=view + 3 {
                    let named: Vec<_> = honest.iter().filter_map(|&i| s.nodes[i].leader_on(v, &id)).collect();
                    if named.len() == honest.len() {
                        assert!(
                            named.iter().all(|k| *k == named[0]),
                            "seed {seed}, t = {t} ms: leaders differ for view {v} (committed heights {heights:?})"
                        );
                        compared += 1;
                    }
                }
            }
        }
        s.assert_agreement();
        assert!(apart >= 20, "seed {seed}: committed heights differed at only {apart} samples");
        assert!(compared >= 300, "seed {seed}: only {compared} comparisons");
        assert!(s.min_final() >= 10, "seed {seed}: only {} final", s.min_final());
    }
}

/// The verdicts the latest RootSplit at member `i` shows, per voter.
fn shown_verdicts(s: &Sim, i: usize) -> BTreeMap<MemberKey, SelfTest> {
    let roots = s.events[i]
        .iter()
        .rev()
        .find_map(|e| if let CoreEvent::RootSplit { roots, .. } = e { Some(roots.clone()) } else { None })
        .expect("a split");
    roots.into_iter().flat_map(|(_, l)| l).collect()
}

#[test]
fn verdicts_are_signed_final_per_split_and_shown_alike_everywhere() {
    // S3 v1.3: n = 4, two wrong roots, so every view splits 2-2 on the same cut. Member 0 reports Passed; member 2
    // equivocates, Passed to member 0 and Failed to member 1. Relay brings both to everyone: each honest member
    // reports the equivocation and shows member 2 as Failed, whatever order the verdicts came in.
    let mut s = Sim::new(4, 9, vec![Role::Honest, Role::Honest, Role::WrongRoot, Role::WrongRoot], fast());
    s.run(400);
    let block = s.events[0]
        .iter()
        .find_map(|e| if let CoreEvent::RootSplit { block, .. } = e { Some(*block) } else { None })
        .expect("a 2-2 split");
    let cut = s.nodes[0].cut_ref(&block).unwrap();
    let (k0, k2) = (s.keys.key(0), s.keys.key(2));

    // A verdict for another split, or none, is refused.
    let other = CutRef { height: cut.height + 1, ..cut };
    let bad = Report {
        cut,
        verdict: SelfTest::Passed { split: other },
        voter: k2,
        sig: s.keys.signer(2).sign(&report_bytes(cut, SelfTest::Passed { split: other })),
    };
    assert_eq!(s.nodes[0].receive(k2, Msg::Report(bad), HostTime(s.now), &s.keys), Err(CoreFault::BadReport));

    let signer0 = s.keys.signer(0);
    assert!(s.nodes[0].report(SelfTest::Passed { split: cut }, &signer0));
    assert!(!s.nodes[0].report(SelfTest::Failed { split: cut }, &signer0), "the first verdict is final");
    s.flush(0);
    let signer2 = s.keys.signer(2);
    for (to, verdict) in [(0, SelfTest::Passed { split: cut }), (1, SelfTest::Failed { split: cut })] {
        let r = Report { cut, verdict, voter: k2, sig: signer2.sign(&report_bytes(cut, verdict)) };
        s.send(2, to, Msg::Report(r));
    }
    s.run(800);
    for i in [0, 1] {
        assert!(
            s.events[i].iter().any(|e| matches!(e, CoreEvent::VerdictEquivocation { member, .. } if *member == k2)),
            "member {i} saw no verdict equivocation"
        );
    }
    let (v0, v1) = (shown_verdicts(&s, 0), shown_verdicts(&s, 1));
    for k in v0.keys().filter(|k| v1.contains_key(k)) {
        assert_eq!(v0[k], v1[k], "every member shows the same verdicts");
    }
    assert!(v1.contains_key(&k0) && v1.contains_key(&k2));
    assert_eq!(v0.get(&k2), Some(&SelfTest::Failed { split: cut }));
    assert_eq!(v0.get(&k0), Some(&SelfTest::Passed { split: cut }));
}

#[test]
fn a_member_voting_with_an_inflated_view_neither_stalls_nor_moves_anyone() {
    // @design-reviewer's High (m03081706a05a): before the fix, honest members finalized 0-2 cuts in 10 s and jumped to
    // view ~1000. A vote counts only in its block's own view (one this far ahead is dropped unread), so the line runs as
    // with a crashed member.
    for seed in 1..=5 {
        let mut s = Sim::new(4, seed, vec![Role::Honest, Role::Honest, Role::Honest, Role::InflateView], fast());
        s.run(10_000);
        s.assert_agreement();
        let finals: Vec<usize> = (0..3).map(|i| s.finals[i].len()).collect();
        let views: Vec<u64> = (0..3).map(|i| s.nodes[i].view()).collect();
        assert!(finals.iter().all(|f| *f >= 50), "seed {seed}: finals {finals:?}, views {views:?}");
        assert!(views.iter().all(|v| *v < 1000), "seed {seed}: views {views:?}");
    }
}

#[test]
fn a_stale_self_test_report_is_dropped_without_a_fault() {
    let mut s = Sim::new(4, 2, vec![Role::Honest; 4], fast());
    s.run(2_000);
    assert!(s.nodes[0].committed().1 >= 2);
    let k1 = s.keys.key(1);
    let cut = CutRef { line: LineId::origin(), height: 1, frame: Frame(6) };
    let verdict = SelfTest::Passed { split: cut };
    let r = Report { cut, verdict, voter: k1, sig: s.keys.signer(1).sign(&report_bytes(cut, verdict)) };
    assert_eq!(s.nodes[0].receive(k1, Msg::Report(r), HostTime(s.now), &s.keys), Ok(()));
}

#[test]
fn a_vote_naming_another_view_than_its_block_is_a_fault() {
    let mut s = Sim::new(4, 4, vec![Role::Honest; 4], fast());
    s.run(500);
    let block = s.finals[0].last().expect("a final block").clone();
    let id = block.id();
    // A view member 3 can't have voted in yet, within the window.
    let view = s.nodes[0].view() + 2;
    let k3 = s.keys.key(3);
    let root = root_of(&block.cut, false);
    let sig = s.keys.signer(3).sign(&crate::core::vote_bytes(LineId::origin(), view, id, root));
    let v = crate::core::Vote {
        view,
        block: id,
        root,
        voter: k3,
        self_test: SelfTest::NotRun,
        heads: Vec::new(),
        lateness: Vec::new(),
        sig,
    };
    let now = HostTime(s.now);
    assert_eq!(s.nodes[0].receive(k3, Msg::Vote(v), now, &s.keys), Err(CoreFault::BadVote));
}

#[test]
fn a_member_cut_off_for_two_seconds_catches_up_and_the_line_resumes() {
    // M1 requirement (coordinator m871a9a3a61d9, @design-reviewer m58bfc0fc985a): n = 4 with member 3 crashed, so the
    // other three need each other for every certificate. Member 1 loses every message for 2 s; afterwards the line
    // must resume, though no message sent during the partition is ever delivered.
    for seed in 1..=5 {
        let mut s = Sim::new(4, seed, vec![Role::Honest, Role::Honest, Role::Honest, Role::Crashed], fast());
        s.partition = Some((1, 2_000, 4_000));
        s.run(4_000);
        let before: Vec<usize> = (0..3).map(|i| s.finals[i].len()).collect();
        s.run(10_000);
        s.assert_agreement();
        let after: Vec<usize> = (0..3).map(|i| s.finals[i].len()).collect();
        let views: Vec<u64> = (0..3).map(|i| s.nodes[i].view()).collect();
        assert!(
            (0..3).all(|i| after[i] >= before[i] + 20),
            "seed {seed}: finals {before:?} -> {after:?}, views {views:?}"
        );
    }
}

#[test]
fn a_member_cut_off_while_the_others_go_on_finalizes_again() {
    // All honest at n = 4: members 0, 2 and 3 keep finalizing while member 1 is cut off for 2 s and falls many views
    // behind; once back, member 1 finalizes again.
    for seed in 1..=3 {
        let mut s = Sim::new(4, seed, vec![Role::Honest; 4], fast());
        s.partition = Some((1, 1_000, 3_000));
        s.run(3_000);
        let before = s.finals[1].len();
        assert!(s.finals[0].len() >= before + 20, "seed {seed}: the others stopped too");
        s.run(6_000);
        s.assert_agreement();
        assert!(s.finals[1].len() >= before + 20, "seed {seed}: member 1 stuck at {before} finals");
    }
}

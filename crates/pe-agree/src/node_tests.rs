//! Nodes on a simulated network: members submit entries, cuts become final, and every member reports the same final
//! log and the same verdicts at the same cuts.

use std::collections::BTreeMap;

use bytes::Bytes;
use pe_seams::agree::{AgreeEvent, Agreement, Cut, CutRef, VoidReason, VoteResult};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{Class, EntryRef, EntrySource, Frame, FrameClock, Hash, Hasher, HostTime, Lane, LineId, SelfTest};

use crate::core::Timing;
use crate::{Node, NodeConfig};

const MS: i64 = 1_000_000;
const FRAME_NS: i64 = 16_666_667;

struct Clock;

impl FrameClock for Clock {
    fn rate_hz(&self) -> u32 {
        60
    }
    fn frame_at(&self, now: HostTime) -> Option<Frame> {
        (now.0 >= 0).then_some(Frame((now.0 / FRAME_NS) as u64))
    }
    fn host_time_of(&self, frame: Frame) -> HostTime {
        HostTime(frame.0 as i64 * FRAME_NS)
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
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

/// The root an honest replica computes at a cut: any deterministic function of the cut will do here.
fn root(cut: &Cut) -> Hash {
    let mut h = Hasher::new("pe.test.root");
    h.write_u64(cut.at.height).write_u64(cut.at.frame.0);
    for x in &cut.heads {
        h.write_hash(&x.entry.0);
    }
    h.finish()
}

struct Sim {
    keys: TestKeys,
    nodes: Vec<Node>,
    builders: Vec<EntryBuilder>,
    net: BTreeMap<(i64, u64), (usize, usize, Lane, Bytes)>,
    seq: u64,
    now: i64,
    rng: Rng,
    delay: Vec<(i64, i64)>,
    /// Every member's stamp lags its present by this many frames (a slow or lying author).
    stamp_lag: Vec<u64>,
    /// Submit an entry every this many ms (0: never).
    every_ms: Vec<i64>,
    events: Vec<Vec<AgreeEvent>>,
    submitted: Vec<Vec<EntryRef>>,
    /// Each vote's result, and that of a revote at once after it (S3 v1.3).
    vote_results: Vec<(VoteResult, VoteResult)>,
}

impl Sim {
    fn new(n: u32, seed: u64) -> Sim {
        Sim::with(n, seed, |_| {})
    }

    fn with(n: u32, seed: u64, f: impl Fn(&mut NodeConfig)) -> Sim {
        let keys = TestKeys::new(seed, n);
        let line = LineId::origin();
        let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
        let mut cfg = NodeConfig::new(line, keys.sorted_keys(), genesis);
        cfg.timing = Timing { cadence_ns: 100 * MS, min_timeout_ns: 300 * MS, max_timeout_ns: 5_000 * MS };
        f(&mut cfg);
        let nodes = (0..n as usize)
            .map(|i| Node::new(cfg.clone(), Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0)))
            .collect();
        let builders = (0..n as usize).map(|i| EntryBuilder::new(&keys, i)).collect();
        Sim {
            nodes,
            builders,
            net: BTreeMap::new(),
            vote_results: Vec::new(),
            seq: 0,
            now: 0,
            rng: Rng(seed),
            delay: vec![(5, 30); n as usize],
            stamp_lag: vec![0; n as usize],
            every_ms: vec![50; n as usize],
            events: vec![Vec::new(); n as usize],
            submitted: vec![Vec::new(); n as usize],
            keys,
        }
    }

    fn deliver_out(&mut self, i: usize, out: Vec<pe_seams::agree::Outgoing>) {
        for o in out {
            let j = (0..self.nodes.len()).find(|&j| self.keys.key(j) == o.to).unwrap();
            let (a, b) = self.delay[i];
            let (c, d) = self.delay[j];
            let at = self.now + (self.rng.range(a, b) + self.rng.range(c, d)) / 2 * MS;
            self.seq += 1;
            self.net.insert((at, self.seq), (i, j, o.lane, o.bytes));
        }
    }

    fn act(&mut self, i: usize) {
        let now = HostTime(self.now);
        if self.every_ms[i] > 0 && self.now > 0 && (self.now / MS) % self.every_ms[i] == 0 {
            let present = Clock.frame_at(now).unwrap();
            let stamp = Frame(present.0.saturating_sub(self.stamp_lag[i]));
            let e = self.builders[i].next_unsigned(Class::Input, stamp, Bytes::from(vec![i as u8; 8]));
            if let Ok(s) = self.nodes[i].submit(e) {
                self.submitted[i].push(s.id());
            } else {
                // Refused locally (too late): the builder's chain moved on without it, so start a fresh chain.
                self.builders[i] = EntryBuilder::new(&self.keys, i);
                self.every_ms[i] = 0;
            }
        }
        let out = self.nodes[i].tick(now, &Clock);
        self.deliver_out(i, out);
        for cut in self.nodes[i].wants_roots() {
            let first = self.nodes[i].vote(&cut.at, root(&cut), SelfTest::NotRun);
            let again = self.nodes[i].vote(&cut.at, root(&cut), SelfTest::NotRun);
            self.vote_results.push((first, again));
        }
        let out = self.nodes[i].tick(now, &Clock);
        self.deliver_out(i, out);
        let ev = self.nodes[i].events();
        self.events[i].extend(ev);
    }

    fn run(&mut self, until_ms: i64) {
        let end = until_ms * MS;
        while self.now < end {
            let step = self.now + MS;
            while let Some((&k, _)) = self.net.iter().next() {
                if k.0 > step {
                    break;
                }
                let (from, to, lane, bytes) = self.net.remove(&k).unwrap();
                let fk = self.keys.key(from);
                let r = self.nodes[to].receive(fk, lane, &bytes, HostTime(k.0));
                assert!(r.is_ok(), "honest bytes refused: {r:?}");
            }
            self.now = step;
            for i in 0..self.nodes.len() {
                self.act(i);
            }
        }
    }

    fn final_frame(&self, i: usize) -> Frame {
        self.nodes[i].final_cut().map_or(Frame(0), |c| c.cut.at.frame)
    }

    /// The final log every member reports agrees, frame by frame, below the lowest final frame.
    fn assert_same_final_log(&self) -> usize {
        let low = (0..self.nodes.len()).map(|i| self.final_frame(i)).min().unwrap();
        let mut count = 0;
        for f in 0..low.0 {
            let a = self.nodes[0].entries_at(Frame(f)).map(|v| v.iter().map(|e| e.id()).collect::<Vec<_>>());
            for i in 1..self.nodes.len() {
                let b = self.nodes[i].entries_at(Frame(f)).map(|v| v.iter().map(|e| e.id()).collect::<Vec<_>>());
                assert_eq!(a, b, "members 0 and {i} differ at frame {f}");
            }
            count += a.map_or(0, |v| v.len());
        }
        count
    }

    fn voids(&self, i: usize) -> Vec<(EntryRef, VoidReason, CutRef)> {
        self.events[i]
            .iter()
            .filter_map(|e| match e {
                AgreeEvent::Voided { entry, reason, cut, .. } => Some((*entry, *reason, *cut)),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn four_members_agree_on_the_final_log() {
    for seed in 1..=3 {
        let mut s = Sim::new(4, seed);
        s.run(4_000);
        let n = s.assert_same_final_log();
        // Each member submits 20 entries a second; nearly all of the first ~3.5 s are final.
        assert!(n >= 4 * 60, "seed {seed}: only {n} final entries");
        for i in 0..4 {
            assert!(s.voids(i).is_empty(), "seed {seed}: honest entries voided: {:?}", s.voids(i));
            assert!(s.nodes[i].final_cut().is_some());
        }
    }
}

#[test]
fn two_members_agree_on_the_final_log() {
    let mut s = Sim::new(2, 9);
    s.run(4_000);
    assert!(s.assert_same_final_log() >= 2 * 60);
}

#[test]
fn a_late_author_is_voided_at_the_same_cut_everywhere() {
    // Member 3 stamps its entries 60 frames (1 s) in the past: once a final cut's frame has passed a stamp, an entry
    // first included by a later cut is late. Every member names the same deciding cut for each void.
    let mut s = Sim::new(4, 5);
    s.stamp_lag[3] = 60;
    s.run(4_000);
    s.assert_same_final_log();
    let v0 = s.voids(0);
    assert!(!v0.is_empty(), "the lagging author's entries should be void as late");
    assert!(v0.iter().all(|v| v.1 == VoidReason::Late));
    for i in 1..4 {
        let vi = s.voids(i);
        let common = v0.len().min(vi.len());
        assert_eq!(v0[..common], vi[..common], "member {i} decided a void differently");
    }
}

#[test]
fn a_crashed_member_does_not_stop_the_others() {
    let mut s = Sim::new(4, 2);
    // Member 1 never acts: its outbox is never drained and nothing reaches it.
    let crashed = 1;
    s.every_ms[crashed] = 0;
    let end = 5_000 * MS;
    while s.now < end {
        let step = s.now + MS;
        while let Some((&k, _)) = s.net.iter().next() {
            if k.0 > step {
                break;
            }
            let (from, to, lane, bytes) = s.net.remove(&k).unwrap();
            if to == crashed {
                continue;
            }
            let _ = s.nodes[to].receive(s.keys.key(from), lane, &bytes, HostTime(k.0));
        }
        s.now = step;
        for i in (0..4).filter(|&i| i != crashed) {
            s.act(i);
        }
    }
    let live: Vec<usize> = (0..4).filter(|&i| i != crashed).collect();
    let low = live.iter().map(|&i| s.final_frame(i)).min().unwrap();
    assert!(low.0 >= 60 * 3, "final frame only {low:?} after 5 s");
}

#[test]
fn the_final_log_is_kept_from_cv_and_the_time_cap() {
    let mut s = Sim::new(2, 4);
    s.run(3_000);
    let ff = s.final_frame(0);
    assert!(ff.0 > 100);
    assert_eq!(s.nodes[0].kept_from(), Frame(0));
    s.nodes[0].keep_final_from(Frame(50), Frame(80));
    assert_eq!(s.nodes[0].kept_from(), Frame(50));
    assert_eq!(s.nodes[0].entries_at(Frame(49)), None);
    assert!(s.nodes[0].entries_at(Frame(50)).is_some());
    // Forward only.
    s.nodes[0].keep_final_from(Frame(10), Frame(10));
    assert_eq!(s.nodes[0].kept_from(), Frame(50));
}

#[test]
fn a_hold_stops_votes_from_its_frame_until_released() {
    let mut s = Sim::new(2, 6);
    s.run(1_000);
    let h0 = s.nodes[0].stats().final_height;
    let hold = Frame(s.final_frame(0).0 + 1);
    s.nodes[0].hold_votes_from(hold);
    s.run(2_000);
    // At n = 2 both votes are needed: beyond the cuts already certified when the hold came, nothing becomes final.
    let h1 = s.nodes[0].stats().final_height;
    assert!(h1 <= h0 + 2, "final height {h0} -> {h1} under a hold");
    s.nodes[0].release_votes_from(hold);
    s.run(3_500);
    assert!(s.nodes[0].stats().final_height > h1 + 5);
}

#[test]
fn garbage_bytes_are_a_fault_never_a_panic() {
    let mut s = Sim::new(2, 1);
    let k = s.keys.key(1);
    for b in [&b""[..], &b"\x01"[..], &b"\x06\xff\xff\xff\xff"[..], &[0x02; 200][..]] {
        assert!(s.nodes[0].receive(k, Lane::Protocol, b, HostTime(0)).is_err());
    }
    let stranger = pe_seams::MemberKey([9; 32]);
    assert!(s.nodes[0].receive(stranger, Lane::Protocol, b"\x04", HostTime(0)).is_err());
}

#[cfg(feature = "mutants")]
#[test]
fn no_signature_check_holds_a_forged_entry() {
    use crate::codec::{Wire, encode};
    use crate::core::Mutation;
    for mutated in [false, true] {
        let keys = TestKeys::new(8, 3);
        let line = LineId::origin();
        let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
        let mut cfg = NodeConfig::new(line, keys.sorted_keys(), genesis);
        if mutated {
            cfg.mutation = Some(Mutation::NoSignatureCheck);
        }
        let mut node = Node::new(cfg, Box::new(keys.signer(0)), Box::new(keys.clone()), HostTime(0));
        // Member 2 signs an entry claiming member 1 as its author.
        let mut b1 = EntryBuilder::new(&keys, 1);
        let b2 = EntryBuilder::new(&keys, 2);
        let forged = b2.sign(b1.next_unsigned(Class::Input, Frame(1), Bytes::from_static(b"f")));
        let bytes = encode(&Wire::Entries(vec![forged]));
        let r = node.receive(keys.key(2), Lane::Action, &bytes, HostTime(0));
        assert_eq!(r.is_ok(), mutated);
    }
}

#[test]
fn a_leader_naming_its_present_voids_honest_entries_in_flight() {
    // The regression case behind cut_lag_s (coordinator m0901fd194db2): with no lag, entries still in flight when a
    // cut passes their stamp are late. With the default (adaptive) lag, the same run voids nothing.
    let no_lag = |c: &mut NodeConfig| {
        c.cut_lag_s = 0.0;
        c.cut_lag_floor_s = 0.0;
        c.cut_lag_cap_s = 0.0;
        c.cut_lag_margin_s = 0.0;
    };
    let mut s = Sim::with(4, 1, no_lag);
    s.run(4_000);
    s.assert_same_final_log();
    assert!(!s.voids(0).is_empty(), "with no lag, in-flight honest entries should be late");
    let mut s = Sim::new(4, 1);
    s.run(4_000);
    assert!(s.voids(0).is_empty(), "{:?}", s.voids(0));
}

#[test]
fn the_cut_lag_adapts_to_slow_links() {
    // Links of 150-250 ms one way: the 0.25 s starting lag is too short for the slowest; once measured, the lag
    // covers the 95th percentile plus the margin and no honest entry is void.
    let mut s = Sim::new(4, 12);
    s.delay.fill((150, 250));
    s.run(8_000);
    s.assert_same_final_log();
    let lag = s.nodes[0].cut_lag_frames();
    assert!((15..=25).contains(&lag), "lag {lag} frames");
    let after_warmup: Vec<_> = s.voids(0).into_iter().filter(|v| v.2.frame.0 > 4 * 60).collect();
    assert!(after_warmup.is_empty(), "{after_warmup:?}");
    assert!(s.final_frame(0).0 > 6 * 60);
}

#[test]
fn a_flooding_author_is_held_to_its_budget() {
    use crate::codec::{Wire, encode};
    let keys = TestKeys::new(8, 3);
    let line = LineId::origin();
    let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
    let cfg = NodeConfig::new(line, keys.sorted_keys(), genesis);
    let (burst, cap) = (cfg.rate_burst as usize, cfg.pending_cap);
    let mut node = Node::new(cfg, Box::new(keys.signer(0)), Box::new(keys.clone()), HostTime(0));
    let mut b = EntryBuilder::new(&keys, 1);
    // 1000 entries in one instant: the burst passes, the rest are refused as over the rate.
    let mut ok = 0;
    for i in 0..1000u64 {
        let e = b.next(Class::Input, Frame(i / 4), Bytes::from_static(b"x"));
        if node.receive(keys.key(1), Lane::Action, &encode(&Wire::Entries(vec![e])), HostTime(0)).is_ok() {
            ok += 1;
        }
    }
    assert_eq!(ok, burst);
    let refused = node
        .events()
        .iter()
        .filter(|e| matches!(e, AgreeEvent::Refused { kind: pe_seams::FaultKind::RateExceeded, .. }))
        .count();
    assert_eq!(refused, 1000 - burst);
    // Over time the rate refills, but never past the cap on entries held beyond the final cut (none is final here).
    let mut held = ok;
    for i in 0..20_000i64 {
        let e = b.next(Class::Input, Frame(250 + i as u64), Bytes::from_static(b"x"));
        let at = HostTime(i * 10 * MS);
        if node.receive(keys.key(1), Lane::Action, &encode(&Wire::Entries(vec![e])), at).is_ok() {
            held += 1;
        }
    }
    assert_eq!(held, cap);
}

#[test]
fn votes_carry_heads_and_the_leader_fetches_what_it_lacks() {
    // Member 3 sends its entries only to member 2 and relays are off, so leaders other than 2 learn of them from
    // member 2's votes (heads beyond the cut) and fetch them: they become final anyway (bounded progress).
    let mut s = Sim::with(4, 21, |c| c.relay = false);
    s.every_ms = vec![0, 0, 0, 50];
    let end = 4_000 * MS;
    while s.now < end {
        let step = s.now + MS;
        while let Some((&k, _)) = s.net.iter().next() {
            if k.0 > step {
                break;
            }
            let (from, to, lane, bytes) = s.net.remove(&k).unwrap();
            // Member 3's entries (its own sends and its fetch answers) reach member 2 only.
            if from == 3 && to != 2 && lane == Lane::Action {
                continue;
            }
            let _ = s.nodes[to].receive(s.keys.key(from), lane, &bytes, HostTime(k.0));
        }
        s.now = step;
        for i in 0..4 {
            s.act(i);
        }
    }
    let n = s.assert_same_final_log();
    assert!(n >= 40, "only {n} of member 3's entries final");
}

#[test]
fn a_revote_in_the_same_view_is_already_voted_and_an_unknown_cut_is_not_votable() {
    let mut s = Sim::new(4, 3);
    s.run(2_000);
    let voted = s.vote_results.iter().filter(|r| r.0 == VoteResult::Voted).count();
    assert!(voted >= 20, "only {voted} votes");
    for (first, again) in &s.vote_results {
        match first {
            VoteResult::Voted => assert_eq!(*again, VoteResult::AlreadyVotedThisView),
            other => assert_eq!(again, other),
        }
    }
    let unknown = CutRef { line: LineId::origin(), height: 9_999, frame: Frame(1) };
    assert_eq!(s.nodes[0].vote(&unknown, Hash::ZERO, SelfTest::NotRun), VoteResult::NotVotable);
}

/// One node of three, fed by hand; with its rate burst.
fn lone_node(seed: u64) -> (TestKeys, Node, usize) {
    let keys = TestKeys::new(seed, 3);
    let line = LineId::origin();
    let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
    let cfg = NodeConfig::new(line, keys.sorted_keys(), genesis);
    let burst = cfg.rate_burst as usize;
    let node = Node::new(cfg, Box::new(keys.signer(0)), Box::new(keys.clone()), HostTime(0));
    (keys, node, burst)
}

#[test]
fn a_forged_entry_never_spends_the_claimed_authors_budget() {
    // @design-reviewer's Medium 1: member 2 sends 1000 entries that claim member 1 with bad signatures. They are
    // refused as member 2's fault, and member 1's own burst still passes in full.
    use crate::codec::{Wire, encode};
    let (keys, mut node, burst) = lone_node(8);
    let mut forger = EntryBuilder::new(&keys, 1);
    for i in 0..1000u64 {
        let mut e = forger.next(Class::Input, Frame(i / 4), Bytes::from_static(b"f"));
        e.sig = pe_seams::Signature([7; 64]);
        assert!(node.receive(keys.key(2), Lane::Action, &encode(&Wire::Entries(vec![e])), HostTime(0)).is_err());
    }
    let faults: Vec<_> =
        node.events().into_iter().filter_map(|e| if let AgreeEvent::Fault(f) = e { Some(f) } else { None }).collect();
    assert_eq!(faults.len(), 1000);
    assert!(faults.iter().all(|f| f.from == keys.key(2) && f.author.is_none()));
    let mut real = EntryBuilder::new(&keys, 1);
    let ok = (0..burst as u64)
        .filter(|i| {
            let e = real.next(Class::Input, Frame(*i / 4), Bytes::from_static(b"x"));
            node.receive(keys.key(1), Lane::Action, &encode(&Wire::Entries(vec![e])), HostTime(0)).is_ok()
        })
        .count();
    assert_eq!(ok, burst);
}

#[test]
fn vote_heads_are_hints_bounded_budgeted_and_expiring() {
    // @design-reviewer's Medium 2: a verified vote's heads are fetched as hints. Entries they name still pass the
    // author's budget, at most 256 are asked at once, and an unanswered hint is dropped.
    use crate::codec::{Wire, decode, encode};
    use crate::core::{BlockId, Msg, Vote, vote_bytes};
    use pe_seams::Signer;
    let (keys, mut node, burst) = lone_node(9);
    assert!(burst < 300);
    let line = LineId::origin();
    let vote = |heads: Vec<pe_seams::agree::ChainHead>| {
        let block = BlockId(Hash::of("unknown", b"block"));
        let sig = keys.signer(1).sign(&vote_bytes(line, 1, block, Hash::ZERO));
        Wire::Core(Msg::Vote(Vote {
            view: 1,
            block,
            root: Hash::ZERO,
            voter: keys.key(1),
            self_test: SelfTest::NotRun,
            heads,
            sig,
        }))
    };
    let mut b = EntryBuilder::new(&keys, 1);
    let entries: Vec<_> = (0..300u64).map(|i| b.next(Class::Input, Frame(i / 4), Bytes::from_static(b"h"))).collect();
    let heads = entries
        .iter()
        .map(|e| pe_seams::agree::ChainHead {
            chain: e.entry.chain,
            author: e.entry.author,
            seq: e.entry.seq,
            entry: e.id(),
        })
        .collect();
    assert!(node.receive(keys.key(1), Lane::Protocol, &encode(&vote(heads)), HostTime(0)).is_ok());
    let asked: usize = node
        .tick(HostTime(0), &Clock)
        .iter()
        .filter_map(|o| if let Ok(Wire::FetchEntries(ids)) = decode(&o.bytes) { Some(ids.len()) } else { None })
        .max()
        .unwrap_or(0);
    assert!(asked > 0 && asked <= 256, "asked {asked}");
    // Hinted entries arriving at once: only the burst passes.
    let ok = entries
        .into_iter()
        .filter(|e| {
            node.receive(keys.key(1), Lane::Action, &encode(&Wire::Entries(vec![e.clone()])), HostTime(0)).is_ok()
        })
        .count();
    assert_eq!(ok, burst, "hinted entries pass the author's budget like any other");
    // Made-up heads nobody answers: dropped after the hint's lifetime.
    let fake = (0..1000u64)
        .map(|i| pe_seams::agree::ChainHead {
            chain: pe_seams::ChainId(Hash::of("c", &i.to_le_bytes())),
            author: keys.key(2),
            seq: 1,
            entry: EntryRef(Hash::of("e", &i.to_le_bytes())),
        })
        .collect();
    assert!(node.receive(keys.key(1), Lane::Protocol, &encode(&vote(fake)), HostTime(0)).is_ok());
    let _first_asks = node.tick(HostTime(0), &Clock);
    let later = node.tick(HostTime(10_000 * MS), &Clock);
    let still: usize = later
        .iter()
        .filter_map(|o| if let Ok(Wire::FetchEntries(ids)) = decode(&o.bytes) { Some(ids.len()) } else { None })
        .sum();
    assert_eq!(still, 0, "an unanswered hint is asked for ever");
}

#[test]
fn final_entries_past_the_final_cut_is_none_not_a_partial_log() {
    let mut s = Sim::new(4, 5);
    s.run(2_000);
    let fc = s.nodes[0].final_cut().expect("a final cut").cut.at;
    let start = CutRef { line: LineId::origin(), height: 0, frame: s.nodes[0].kept_from() };
    assert!(s.nodes[0].final_entries(&start, &fc).is_some());
    let past = CutRef { frame: Frame(fc.frame.0 + 1_000), ..fc };
    assert_eq!(s.nodes[0].final_entries(&start, &past), None);
}

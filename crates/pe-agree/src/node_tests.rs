//! Nodes on a simulated network: members submit entries, cuts become final, and every member reports the same final
//! log and the same verdicts at the same cuts.

use std::collections::BTreeMap;

use bytes::Bytes;
use pe_seams::agree::{AgreeEvent, Agreement, Cut, CutRef, VoidReason, VoteResult};
use pe_seams::test_util::{EntryBuilder, TestKeys};
use pe_seams::{
    Class, EntryRef, EntrySource, Frame, FrameClock, Hash, Hasher, HostTime, Lane, LineId, MemberKey, SelfTest,
};

use crate::core::Timing;
use crate::{Node, NodeConfig};

const MS: i64 = 1_000_000;
const SEC: i64 = 1_000_000_000;

/// The world's frame clock at a rate in Hz.
#[derive(Clone, Copy)]
struct Clock(u32);

impl FrameClock for Clock {
    fn rate_hz(&self) -> u32 {
        self.0
    }
    fn frame_at(&self, now: HostTime) -> Option<Frame> {
        (now.0 >= 0).then_some(Frame((now.0 as i128 * self.0 as i128 / SEC as i128) as u64))
    }
    fn host_time_of(&self, frame: Frame) -> HostTime {
        HostTime(((frame.0 as i128 * SEC as i128 + self.0 as i128 - 1) / self.0 as i128) as i64)
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

/// A cut handed out for roots, with what `entries_for_cut` answered for each frame below it.
type Rooted = (Cut, Vec<(Frame, Vec<EntryRef>)>);

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
    /// A member that withholds its own proposal in every second view it leads (a Byzantine leader), and per view
    /// it led, whether that view's proposal was withheld.
    withhold: Option<usize>,
    led: BTreeMap<u64, bool>,
    /// Per member, per-frame entries are stamped this many frames ahead of its present (stamp-ahead, pe-app's side).
    stamp_lead: Vec<u64>,
    /// Submit an entry every this many ms (0: never).
    every_ms: Vec<i64>,
    events: Vec<Vec<AgreeEvent>>,
    submitted: Vec<Vec<EntryRef>>,
    /// The frame each per-frame entry was submitted at (parallel to `submitted` for per-frame members).
    submitted_at: Vec<Vec<Frame>>,
    /// Each vote's result, and that of a revote at once after it (S3 v1.3).
    vote_results: Vec<(VoteResult, VoteResult)>,
    /// (member, from ms, until ms): every message to or from it sent in that window is lost.
    partition: Option<(usize, i64, i64)>,
    /// A member cut off for good (as if crashed): every message to or from it is lost.
    cut_off: Option<usize>,
    /// (from, to, from ms, until ms): one direction of one link is down; what `from` sends `to` then is lost.
    link_down: Option<(usize, usize, i64, i64)>,
    /// (from, to, extra ms, jitter ms): one direction of one link is slower by extra plus up to jitter ms.
    slow_link: Option<(usize, usize, i64, i64)>,
    /// The world's frame clock.
    clock: Clock,
    /// (hands, edits): entries a member submits each frame, on one chain (overrides `every_ms` when set).
    per_frame: Vec<Option<(u32, u32)>>,
    /// The frame each member last submitted its per-frame entries for.
    last_frame: Vec<Option<Frame>>,
    /// (member, from ms, until ms): everything it sends in that window is held and sent at its end (a stalled uplink
    /// flushing).
    stall: Option<(usize, i64, i64)>,
    /// (from ms, until ms): everything every member sends in that window is held and delivered at its end, as a
    /// reliable transport flushes its queue when a dark path returns (pe-net), unlike `blackout_until`, which drops.
    stall_all: Option<(i64, i64)>,
    /// (period ms, length ms, from ms, until ms, member): repeated stalls, each held and flushed at its end like
    /// `stall_all`; only that member's uplink when a member is given, else every member's.
    stall_every: Option<(i64, i64, i64, i64, Option<usize>)>,
    /// A member whose entries peers may refuse as over the budget (the run counts the errors instead of failing).
    flooder: Option<usize>,
    /// Errors `receive` returned (only with a flooder).
    refused: usize,
    /// A member that keeps making hand entries whatever its backlog (its transport flushes everything after a stall).
    unpaced: Option<usize>,
    /// Per member: each cut handed out for roots, with what `entries_for_cut` answered for every frame from the
    /// member's final frame up to the cut's (the pipelined-void property, coordinator m90c32fc9ceb2).
    rooted: Vec<Vec<Rooted>>,
    /// The deepest pipeline a member rooted a cut on (blocks above its final cut).
    max_depth: usize,
    /// A member whose every outgoing message is held until [`Sim::release`].
    holding: Option<usize>,
    held: Vec<(usize, usize, Lane, Bytes)>,
    /// Every message sent before this time (ns) is lost.
    blackout_until: i64,
    /// Per member: frames its hand entries were paced (backlogged), and frames the old rule (entries beyond the final
    /// cut over the burst) would have paced.
    paced_frames: Vec<usize>,
    old_rule_frames: Vec<usize>,
    /// Per member: of the paced frames, those near the receivers' pending cap (`Node::near_pending_cap`).
    near_cap_frames: Vec<usize>,
    /// Count range refetches each member sends for this chain (decodes protocol messages; off by default).
    count_ranges: Option<pe_seams::ChainId>,
    /// (members, from frame, until ms): those members vote a wrong root for every cut at or past the frame until the
    /// time (a persistent divergence, repaired then), and the right one after.
    wrong_root: Option<(Vec<usize>, u64, i64)>,
    /// Vote as pe-replica's driver and pe-diverge do: each cut once with the root it has; once a member sees a split of
    /// it, once more with its self-test verdict (a report: Failed for a member in `wrong_root`, Passed otherwise); and
    /// once more when its root for a cut it voted changes (the repair's revote). Never again per view.
    vote_once: bool,
    /// Per member: the root it last gave each cut, and the split cuts it sent a verdict for.
    given: Vec<BTreeMap<CutRef, Hash>>,
    verdict_sent: Vec<Vec<CutRef>>,
    /// Self-test reports each member sent (every copy).
    reports_sent: Vec<usize>,
    /// Per member: each cut it was asked to root, by its CutRef.
    cuts_seen: Vec<BTreeMap<CutRef, Cut>>,
    /// (member, from ms): the first block fetch that member sends from then on is lost, every copy of it.
    lose_fetch: Option<(usize, i64)>,
    /// When that fetch was sent (ns), and the id it asked for.
    lost_fetch: Option<(i64, crate::core::BlockId)>,
    range_asks: Vec<usize>,
    /// Per member: also submit one edit every this many frames, on the per-frame chain (0: never).
    edit_every: Vec<u64>,
    /// Per member: the edits submitted through `edit_every`, with their stamps.
    edits: Vec<Vec<(EntryRef, Frame)>>,
    /// Per member: not started yet (a joiner before its join): it neither acts nor receives, and what is sent to it is
    /// lost.
    absent: Vec<bool>,
    /// The configuration every member started from (a joiner's template).
    base: NodeConfig,
}

impl Sim {
    fn new(n: u32, seed: u64) -> Sim {
        Sim::with(n, seed, |_| {})
    }

    fn with(n: u32, seed: u64, f: impl Fn(&mut NodeConfig)) -> Sim {
        Sim::with_each(n, seed, |_, c| f(c))
    }

    /// Each member's config set apart (a member with different settings).
    fn with_each(n: u32, seed: u64, f: impl Fn(usize, &mut NodeConfig)) -> Sim {
        Sim::build(n, n, seed, f)
    }

    /// `n` keys, of which only the first `present` are genesis members; the others are joiners, absent until
    /// [`Sim::join`] starts them.
    fn with_joiners(n: u32, present: u32, seed: u64) -> Sim {
        Sim::build(n, present, seed, |_, _| {})
    }

    fn build(n: u32, present: u32, seed: u64, f: impl Fn(usize, &mut NodeConfig)) -> Sim {
        let keys = TestKeys::new(seed, n);
        let line = LineId::origin();
        let genesis = Cut { at: CutRef { line, height: 0, frame: Frame(0) }, heads: Vec::new() };
        let mut members: Vec<MemberKey> = (0..present as usize).map(|i| keys.key(i)).collect();
        members.sort();
        let mut cfg = NodeConfig::new(line, members, genesis);
        cfg.timing = Timing { cadence_ns: 100 * MS, min_timeout_ns: 300 * MS, max_timeout_ns: 5_000 * MS };
        let nodes = (0..n as usize)
            .map(|i| {
                let mut cfg = cfg.clone();
                f(i, &mut cfg);
                Node::new(cfg, Box::new(keys.signer(i)), Box::new(keys.clone()), HostTime(0))
            })
            .collect();
        let builders = (0..n as usize).map(|i| EntryBuilder::new(&keys, i)).collect();
        Sim {
            nodes,
            builders,
            net: BTreeMap::new(),
            vote_results: Vec::new(),
            partition: None,
            cut_off: None,
            link_down: None,
            slow_link: None,
            clock: Clock(60),
            per_frame: vec![None; n as usize],
            last_frame: vec![None; n as usize],
            stall: None,
            stall_all: None,
            stall_every: None,
            flooder: None,
            refused: 0,
            unpaced: None,
            rooted: vec![Vec::new(); n as usize],
            max_depth: 0,
            holding: None,
            held: Vec::new(),
            blackout_until: 0,
            paced_frames: vec![0; n as usize],
            old_rule_frames: vec![0; n as usize],
            near_cap_frames: vec![0; n as usize],
            count_ranges: None,
            lose_fetch: None,
            wrong_root: None,
            vote_once: false,
            given: vec![BTreeMap::new(); n as usize],
            verdict_sent: vec![Vec::new(); n as usize],
            reports_sent: vec![0; n as usize],
            cuts_seen: vec![BTreeMap::new(); n as usize],
            lost_fetch: None,
            range_asks: vec![0; n as usize],
            edit_every: vec![0; n as usize],
            edits: vec![Vec::new(); n as usize],
            absent: (0..n).map(|i| i >= present).collect(),
            base: cfg,
            seq: 0,
            now: 0,
            rng: Rng(seed),
            delay: vec![(5, 30); n as usize],
            stamp_lag: vec![0; n as usize],
            withhold: None,
            led: BTreeMap::new(),
            stamp_lead: vec![0; n as usize],
            every_ms: vec![50; n as usize],
            events: vec![Vec::new(); n as usize],
            submitted: vec![Vec::new(); n as usize],
            submitted_at: vec![Vec::new(); n as usize],
            keys,
        }
    }

    fn deliver_out(&mut self, i: usize, out: Vec<pe_seams::agree::Outgoing>) {
        for o in out {
            if let Some(c) = self.count_ranges
                && o.lane == Lane::Protocol
                && matches!(crate::codec::decode(&o.bytes), Ok(crate::codec::Wire::FetchRange { chain, .. }) if chain == c)
            {
                self.range_asks[i] += 1;
            }
            let j = (0..self.nodes.len()).find(|&j| self.keys.key(j) == o.to).unwrap();
            if o.lane == Lane::Protocol
                && matches!(crate::codec::decode(&o.bytes), Ok(crate::codec::Wire::Core(crate::core::Msg::Report(_))))
            {
                self.reports_sent[i] += 1;
            }
            if let Some((k, from)) = self.lose_fetch
                && i == k
                && self.now >= from * MS
                && o.lane == Lane::Protocol
                && let Ok(crate::codec::Wire::Core(crate::core::Msg::Fetch(id))) = crate::codec::decode(&o.bytes)
            {
                let (at, lost) = *self.lost_fetch.get_or_insert((self.now, id));
                if at == self.now && lost == id {
                    continue;
                }
            }
            if self.withhold == Some(i)
                && o.lane == Lane::Protocol
                && let Ok(crate::codec::Wire::Core(crate::core::Msg::Propose(p))) = crate::codec::decode(&o.bytes)
            {
                let n = self.led.len();
                if *self.led.entry(p.block.view).or_insert(n % 2 == 1) {
                    continue;
                }
            }
            if let Some((k, a, b)) = self.partition
                && (i == k || j == k)
                && self.now >= a * MS
                && self.now < b * MS
            {
                continue;
            }
            if let Some((a, b, from, until)) = self.link_down
                && i == a
                && j == b
                && self.now >= from * MS
                && self.now < until * MS
            {
                continue;
            }
            if self.cut_off.is_some_and(|k| i == k || j == k) || self.now < self.blackout_until || self.absent[j] {
                continue;
            }
            if self.holding == Some(i) {
                self.held.push((i, j, o.lane, o.bytes));
                continue;
            }
            let mut sent = self.now;
            if let Some((k, a, b)) = self.stall
                && i == k
                && self.now >= a * MS
                && self.now < b * MS
            {
                sent = b * MS;
            }
            if let Some((a, b)) = self.stall_all
                && self.now >= a * MS
                && self.now < b * MS
            {
                sent = b * MS;
            }
            if let Some((period, len, from, until, who)) = self.stall_every
                && who.is_none_or(|k| k == i)
                && self.now >= from * MS
                && self.now < until * MS
                && (self.now - from * MS) % (period * MS) < len * MS
            {
                sent = self.now - (self.now - from * MS) % (period * MS) + len * MS;
            }
            let (a, b) = self.delay[i];
            let (c, d) = self.delay[j];
            let mut at = sent + (self.rng.range(a, b) + self.rng.range(c, d)) / 2 * MS;
            if let Some((x, y, extra, jitter)) = self.slow_link
                && i == x
                && j == y
            {
                at += (extra + self.rng.range(0, jitter)) * MS;
            }
            self.seq += 1;
            self.net.insert((at, self.seq), (i, j, o.lane, o.bytes));
        }
    }

    /// Send everything the held member queued, each with a fresh delay.
    fn release(&mut self) {
        self.holding = None;
        for (i, j, lane, bytes) in std::mem::take(&mut self.held) {
            let (a, b) = self.delay[i];
            let (c, d) = self.delay[j];
            let at = self.now + (self.rng.range(a, b) + self.rng.range(c, d)) / 2 * MS;
            self.seq += 1;
            self.net.insert((at, self.seq), (i, j, lane, bytes));
        }
    }

    fn act(&mut self, i: usize) {
        let now = HostTime(self.now);
        let clock = self.clock;
        if let Some((hands, edits)) = self.per_frame[i] {
            let present = clock.frame_at(now).unwrap();
            if self.now > 0 && self.last_frame[i] != Some(present) {
                self.last_frame[i] = Some(present);
                // As pe-app does: no new hand entries while the unconfirmed backlog is over the burst (they would be
                // void as late); edits always go (coordinator m120f73db271d, point 3).
                // A flooder ignores it.
                // Through the seam, as pe-app reads it: a missing override would show here as never pacing.
                let over = Agreement::backlogged(&self.nodes[i]);
                if over {
                    self.paced_frames[i] += 1;
                    if self.nodes[i].near_pending_cap() {
                        self.near_cap_frames[i] += 1;
                    }
                }
                if self.nodes[i].unfinal_own() > self.nodes[i].burst() {
                    self.old_rule_frames[i] += 1;
                }
                let hands = if over && self.flooder != Some(i) && self.unpaced != Some(i) { 0 } else { hands };
                let stamp = Frame(present.0 + self.stamp_lead[i]);
                for k in 0..hands + edits {
                    let class = if k < hands { Class::Input } else { Class::Edit };
                    let e = self.builders[i].next_unsigned(class, stamp, Bytes::from(vec![i as u8; 24]));
                    let s = self.nodes[i].submit(e).expect("own per-frame entry refused");
                    self.submitted[i].push(s.id());
                    self.submitted_at[i].push(stamp);
                }
                if self.edit_every[i] > 0 && present.0.is_multiple_of(self.edit_every[i]) {
                    let e = self.builders[i].next_unsigned(Class::Edit, stamp, Bytes::from(vec![i as u8; 24]));
                    let s = self.nodes[i].submit(e).expect("own edit refused");
                    self.submitted[i].push(s.id());
                    self.submitted_at[i].push(stamp);
                    self.edits[i].push((s.id(), stamp));
                }
            }
        } else if self.every_ms[i] > 0 && self.now > 0 && (self.now / MS) % self.every_ms[i] == 0 {
            let present = clock.frame_at(now).unwrap();
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
        let out = self.nodes[i].tick(now, &clock);
        self.deliver_out(i, out);
        if self.vote_once {
            self.vote_as_driver(i);
            let out = self.nodes[i].tick(now, &clock);
            self.deliver_out(i, out);
            let ev = self.nodes[i].events();
            self.events[i].extend(ev);
            return;
        }
        for cut in self.nodes[i].wants_roots() {
            let from = self.nodes[i].final_cut().map_or(Frame(0), |c| c.cut.at.frame);
            let answer = (from.0..cut.at.frame.0)
                .map(|f| {
                    let v = self.nodes[i].entries_for_cut(&cut, Frame(f)).expect("a rooted cut's entries are held");
                    (Frame(f), v.iter().map(|e| e.id()).collect())
                })
                .collect();
            let depth = self.nodes[i].rooting_depth(&cut).expect("a rooted cut descends from the final cut");
            self.max_depth = self.max_depth.max(depth);
            if !self.rooted[i].iter().any(|(c, _)| *c == cut) {
                self.rooted[i].push((cut.clone(), answer));
            }
            let mut r = root(&cut);
            if let Some((who, from, until)) = &self.wrong_root
                && who.contains(&i)
                && cut.at.frame.0 >= *from
                && self.now < until * MS
            {
                r.0[0] ^= 1;
            }
            let first = self.nodes[i].vote(&cut.at, r, SelfTest::NotRun);
            let again = self.nodes[i].vote(&cut.at, r, SelfTest::NotRun);
            self.vote_results.push((first, again));
        }
        let out = self.nodes[i].tick(now, &clock);
        self.deliver_out(i, out);
        let ev = self.nodes[i].events();
        self.events[i].extend(ev);
    }

    /// The root member `i` computes for `cut` now (`wrong_root`).
    fn root_now(&self, i: usize, cut: &Cut) -> Hash {
        let mut r = root(cut);
        if let Some((who, from, until)) = &self.wrong_root
            && who.contains(&i)
            && cut.at.frame.0 >= *from
            && self.now < until * MS
        {
            r.0[0] ^= 1;
        }
        r
    }

    /// `vote_once`'s voting: see the field.
    fn vote_as_driver(&mut self, i: usize) {
        let wrong = self.wrong_root.as_ref().is_some_and(|(who, _, _)| who.contains(&i));
        let verdict = |cut: CutRef, sent: &Vec<CutRef>| {
            if !sent.contains(&cut) {
                SelfTest::NotRun
            } else if wrong {
                SelfTest::Failed { split: cut }
            } else {
                SelfTest::Passed { split: cut }
            }
        };
        for cut in self.nodes[i].wants_roots() {
            self.cuts_seen[i].entry(cut.at).or_insert_with(|| cut.clone());
            if self.given[i].contains_key(&cut.at) {
                continue;
            }
            let r = self.root_now(i, &cut);
            let v = verdict(cut.at, &self.verdict_sent[i]);
            let _ = self.nodes[i].vote(&cut.at, r, v);
            self.given[i].insert(cut.at, r);
        }
        // A split seen: the self-test's verdict, once (a report).
        let splits: Vec<CutRef> = self.events[i]
            .iter()
            .filter_map(|e| if let AgreeEvent::RootSplit { cut, .. } = e { Some(*cut) } else { None })
            .collect();
        for c in splits {
            if self.verdict_sent[i].contains(&c) {
                continue;
            }
            let Some(&r) = self.given[i].get(&c) else { continue };
            self.verdict_sent[i].push(c);
            let v = verdict(c, &self.verdict_sent[i]);
            let _ = self.nodes[i].vote(&c, r, v);
        }
        // The repair's revote: a cut whose root this member now computes differently, once.
        let changed: Vec<(CutRef, Hash)> = self.given[i]
            .iter()
            .filter_map(|(c, r)| {
                let cut = self.rooted_cut(i, c)?;
                let now = self.root_now(i, &cut);
                (now != *r).then_some((*c, now))
            })
            .collect();
        for (c, r) in changed {
            let v = verdict(c, &self.verdict_sent[i]);
            let _ = self.nodes[i].vote(&c, r, v);
            self.given[i].insert(c, r);
        }
    }

    /// The full cut member `i` was asked to root for `at` (kept by `vote_once` through `rooted`).
    fn rooted_cut(&self, i: usize, at: &CutRef) -> Option<Cut> {
        self.cuts_seen[i].get(at).cloned()
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
                if self.absent[to] {
                    continue;
                }
                let fk = self.keys.key(from);
                let r = self.nodes[to].receive(fk, lane, &bytes, HostTime(k.0));
                if self.flooder.is_some() && r.is_err() {
                    self.refused += 1;
                } else {
                    assert!(r.is_ok(), "honest bytes refused: {r:?}");
                }
            }
            self.now = step;
            for i in 0..self.nodes.len() {
                if !self.absent[i] {
                    self.act(i);
                }
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

    /// Every cut a member rooted that became final: what it answered for the root is exactly the final log there.
    /// Returns how many rooted cuts were checked and how many entries they voided below their own frame.
    fn assert_roots_match_the_final_log(&self) -> (usize, usize) {
        let (mut checked, mut voided) = (0, 0);
        for i in 0..self.nodes.len() {
            let finals: Vec<CutRef> = self.events[i]
                .iter()
                .filter_map(|e| if let AgreeEvent::CutFinal { cut, .. } = e { Some(*cut) } else { None })
                .collect();
            for (cut, answer) in &self.rooted[i] {
                if !finals.contains(&cut.at) {
                    continue;
                }
                checked += 1;
                for (f, ids) in answer {
                    let Some(fin) = self.nodes[i].entries_at(*f) else { continue };
                    let fin: Vec<EntryRef> = fin.iter().map(|e| e.id()).collect();
                    assert_eq!(*ids, fin, "member {i}: root of {:?} at frame {f:?} differs from the final log", cut.at);
                }
            }
            voided += self.voids(i).len();
            assert_eq!(self.nodes[i].core().shared_cuts(), 0, "member {i} looked up a cut held under two parents");
        }
        (checked, voided)
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
        // A grid of one frame: rounding down to a coarser grid is itself a lag of up to the interval.
        c.cut_interval_s = 0.0;
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
fn void_reason_answers_each_final_void() {
    // S2's `void_reason` (S2 v1.6; P15 in the harness reads it): every entry a final cut voided answers with the
    // reason the Voided event gave, on every member, and an effective entry answers None.
    let no_lag = |c: &mut NodeConfig| {
        c.cut_lag_s = 0.0;
        c.cut_lag_floor_s = 0.0;
        c.cut_lag_cap_s = 0.0;
        c.cut_lag_margin_s = 0.0;
        c.cut_interval_s = 0.0;
    };
    let mut s = Sim::with(4, 1, no_lag);
    s.run(4_000);
    for i in 0..4 {
        let voids = s.voids(i);
        assert!(!voids.is_empty(), "member {i}: with no lag, in-flight honest entries should be late");
        for (entry, reason, _) in &voids {
            assert_eq!(EntrySource::void_reason(&s.nodes[i], entry), Some(*reason), "member {i}");
        }
        let f = s.final_frame(i);
        let effective: Vec<EntryRef> = (1..f.0)
            .flat_map(|k| EntrySource::entries_at(&s.nodes[i], Frame(k)).unwrap_or(&[]).iter().map(|e| e.id()))
            .collect();
        assert!(!effective.is_empty());
        for e in &effective {
            assert_eq!(EntrySource::void_reason(&s.nodes[i], e), None, "member {i}: an effective entry");
        }
    }
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
    let budget = cfg.rate_budget(60);
    let (burst, cap) = (budget.burst as usize, budget.pending);
    let mut node = Node::new(cfg, Box::new(keys.signer(0)), Box::new(keys.clone()), HostTime(0));
    let mut b = EntryBuilder::new(&keys, 1);
    // 2000 entries in one instant: the burst is held, as many as the pending cap wait for the budget, and only the
    // rest are refused, as over the budget and its queue (coordinator m120f73db271d: delay, never lose, up to a cap).
    let mut ok = 0;
    for i in 0..2000u64 {
        let e = b.next(Class::Input, Frame(i / 4), Bytes::from_static(b"x"));
        if node.receive(keys.key(1), Lane::Action, &encode(&Wire::Entries(vec![e])), HostTime(0)).is_ok() {
            ok += 1;
        }
    }
    assert_eq!(node.held_entries(), burst);
    assert_eq!(node.deferred_entries(), cap);
    assert_eq!(ok, burst + cap);
    let refused = node
        .events()
        .iter()
        .filter(|e| matches!(e, AgreeEvent::Refused { kind: pe_seams::FaultKind::RateExceeded, .. }))
        .count();
    assert_eq!(refused, 2000 - burst - cap);
    // Over time the rate admits the waiting entries, but never past the cap on entries held beyond the final cut
    // (none is final here).
    for i in 1..=200i64 {
        let e = b.next(Class::Input, Frame(600 + i as u64), Bytes::from_static(b"x"));
        let _ = node.receive(keys.key(1), Lane::Action, &encode(&Wire::Entries(vec![e])), HostTime(i * 100 * MS));
    }
    assert_eq!(node.held_entries(), cap);
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
    let burst = cfg.rate_budget(60).burst as usize;
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
            lateness: Vec::new(),
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
        .tick(HostTime(0), &Clock(60))
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
    assert_eq!(ok, 300, "over the budget, hinted entries wait rather than fail");
    assert_eq!(node.held_entries(), burst, "hinted entries pass the author's budget like any other");
    assert_eq!(node.deferred_entries(), 300 - burst);
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
    let _first_asks = node.tick(HostTime(0), &Clock(60));
    let later = node.tick(HostTime(10_000 * MS), &Clock(60));
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

#[test]
fn a_voter_flooding_hints_cannot_crowd_out_an_honest_voters_hint() {
    // @design-reviewer's Low on f1c1755b: member 2 sends signed votes with 1024 made-up heads each; member 1's vote then
    // names one real entry, which is still fetched (each voter has its own share of the hint slots).
    use crate::codec::{Wire, decode, encode};
    use crate::core::{BlockId, Msg, Vote, vote_bytes};
    use pe_seams::Signer;
    let (keys, mut node, _) = lone_node(10);
    let line = LineId::origin();
    let vote = |voter: usize, view: u64, tag: &[u8], heads: Vec<pe_seams::agree::ChainHead>| {
        let block = BlockId(Hash::of("unknown", tag));
        let sig = keys.signer(voter).sign(&vote_bytes(line, view, block, Hash::ZERO));
        Wire::Core(Msg::Vote(Vote {
            view,
            block,
            root: Hash::ZERO,
            voter: keys.key(voter),
            self_test: SelfTest::NotRun,
            heads,
            lateness: Vec::new(),
            sig,
        }))
    };
    for k in 0..8u64 {
        let fake = (0..1024u64)
            .map(|i| pe_seams::agree::ChainHead {
                chain: pe_seams::ChainId(Hash::of("c", &(k * 10_000 + i).to_le_bytes())),
                author: keys.key(1),
                seq: 1,
                entry: EntryRef(Hash::of("e", &(k * 10_000 + i).to_le_bytes())),
            })
            .collect();
        let w = vote(2, 1 + k, &k.to_le_bytes(), fake);
        assert!(node.receive(keys.key(2), Lane::Protocol, &encode(&w), HostTime(0)).is_ok());
    }
    let _ = node.tick(HostTime(0), &Clock(60));
    let mut b = EntryBuilder::new(&keys, 2);
    let real = b.next(Class::Input, Frame(1), Bytes::from_static(b"r"));
    let head = pe_seams::agree::ChainHead {
        chain: real.entry.chain,
        author: real.entry.author,
        seq: real.entry.seq,
        entry: real.id(),
    };
    assert!(
        node.receive(keys.key(1), Lane::Protocol, &encode(&vote(1, 1, b"honest", vec![head])), HostTime(0)).is_ok()
    );
    let asked: Vec<EntryRef> = node
        .tick(HostTime(1), &Clock(60))
        .iter()
        .filter_map(|o| if let Ok(Wire::FetchEntries(ids)) = decode(&o.bytes) { Some(ids) } else { None })
        .flatten()
        .collect();
    assert!(asked.contains(&real.id()), "the honest voter's hint was crowded out");
}

#[test]
fn a_member_cut_off_for_two_seconds_catches_up_on_the_final_log() {
    // M1 requirement (coordinator m871a9a3a61d9): n = 4, member 3 silent (it submits and receives nothing), member 1
    // loses every message for 2 s. The line resumes, and every member then has the same final log.
    let mut s = Sim::new(4, 13);
    s.partition = Some((1, 2_000, 4_000));
    s.cut_off = Some(3);
    s.every_ms[3] = 0;
    s.run(4_000);
    let before = s.final_frame(0);
    s.run(10_000);
    let after: Vec<Frame> = (0..3).map(|i| s.final_frame(i)).collect();
    assert!(after.iter().all(|f| f.0 > before.0 + 60), "final frames {before:?} -> {after:?}");
    let low = *after.iter().min().unwrap();
    let mut n = 0;
    for f in 0..low.0 {
        let ids = |i: usize| s.nodes[i].entries_at(Frame(f)).map(|v| v.iter().map(|e| e.id()).collect::<Vec<_>>());
        assert_eq!(ids(0), ids(1), "members 0 and 1 differ at frame {f}");
        assert_eq!(ids(0), ids(2), "members 0 and 2 differ at frame {f}");
        n += ids(0).map_or(0, |v| v.len());
    }
    assert!(n >= 100, "only {n} final entries");
}

#[test]
fn a_member_cut_off_while_the_others_go_on_catches_up_on_the_final_log() {
    let mut s = Sim::new(4, 14);
    s.partition = Some((1, 1_000, 3_000));
    s.run(3_000);
    let behind = s.final_frame(1);
    s.run(7_000);
    assert!(s.final_frame(1).0 > behind.0 + 60, "member 1 stuck at {behind:?}");
    let n = s.assert_same_final_log();
    assert!(n >= 100, "only {n} final entries");
}

#[test]
fn a_repair_longer_than_16_views_certifies_the_pinned_cut() {
    // Coordinator m72cbd9a9f47f (@diverge-eng's trial m218ed0cdea8b): a driver votes each cut once, and a revote after
    // a repair is answered AlreadyVotedThisView, so with the pin alone nobody votes in the pinned views and the split
    // never certifies. pe-agree casts each member's newest root again on every pinned re-proposal, without a report.
    // n = 2, member 1's root wrong from frame 120 until 5 s (about 30 split views), voting as the driver does.
    let mut s = Sim::new(2, 43);
    s.vote_once = true;
    s.wrong_root = Some((vec![1], 120, 5_000));
    s.run(9_000);
    let mut split_cuts: Vec<CutRef> = Vec::new();
    for e in s.events.iter().flatten() {
        if let AgreeEvent::RootSplit { cut, .. } = e
            && !split_cuts.contains(cut)
        {
            split_cuts.push(*cut);
        }
    }
    assert_eq!(split_cuts.len(), 1, "one split cut: {split_cuts:?}");
    let split = split_cuts[0];
    let recasts: Vec<u64> = s.nodes.iter().map(|x| x.recasts()).collect();
    assert!(recasts.iter().all(|&r| r > 16), "the repair spans more than 16 pinned views: recasts {recasts:?}");
    for i in 0..2 {
        let finals: Vec<CutRef> = s.events[i]
            .iter()
            .filter_map(|e| if let AgreeEvent::CutFinal { cut, .. } = e { Some(*cut) } else { None })
            .collect();
        assert!(finals.contains(&split), "member {i} never finalized the split cut {split:?}");
        assert!(s.final_frame(i).0 > split.frame.0 + 60, "member {i} stopped at {:?}", s.final_frame(i));
    }
    // One verdict each, relayed once by the other: never one per pinned view.
    assert!(s.reports_sent.iter().all(|&n| n <= 2), "reports sent {:?}", s.reports_sent);
    assert!(s.reports_sent.iter().sum::<usize>() >= 2, "no verdict reported: {:?}", s.reports_sent);
    s.assert_same_final_log();
}

#[test]
fn a_root_split_pins_its_cut_until_the_height_is_certified() {
    // L4', coordinator mff2b493b8111: after a root split the leader proposes the split cut again, frame and heads, so a
    // repair's revote for that cut lands, and the split is one episode (one cut), however many views the repair takes.
    // n = 2 with member 1 diverged; n = 4 with members 2 and 3 diverged (two against two: no root reaches q = 3).
    for (n, wrong, seed) in [(2u32, vec![1usize], 41u64), (4, vec![2, 3], 42)] {
        let mut s = Sim::new(n, seed);
        // As pe-replica's driver and pe-diverge vote: each cut once, the verdict, and the repair's revote.
        s.vote_once = true;
        s.wrong_root = Some((wrong, 120, 3_500));
        s.run(6_000);
        let mut split_cuts: Vec<CutRef> = Vec::new();
        for e in s.events.iter().flatten() {
            if let AgreeEvent::RootSplit { cut, .. } = e
                && !split_cuts.contains(cut)
            {
                split_cuts.push(*cut);
            }
        }
        assert_eq!(split_cuts.len(), 1, "n = {n}: one split cut, not a new one per view: {split_cuts:?}");
        let split = split_cuts[0];
        for i in 0..n as usize {
            let finals: Vec<CutRef> = s.events[i]
                .iter()
                .filter_map(|e| if let AgreeEvent::CutFinal { cut, .. } = e { Some(*cut) } else { None })
                .collect();
            assert!(
                finals.contains(&split),
                "n = {n}: member {i} never finalized the split cut {split:?}; finals at its height: {:?}",
                finals.iter().filter(|c| c.height == split.height).collect::<Vec<_>>()
            );
            assert!(s.final_frame(i).0 > split.frame.0 + 60, "n = {n}: member {i} stopped at {:?}", s.final_frame(i));
        }
        s.assert_same_final_log();
    }
}

#[test]
fn a_lost_block_fetch_is_asked_again() {
    // @design-reviewer's Low 2 on 5d532552: n = 3, member 2 cut off while 0 and 1 go on (q = 2), so it comes back
    // with a gap; the first fetch it sends for the gap is lost. Every later proposal's parent is an orphan it holds,
    // so nothing else asks for the gap: only the re-send fills it.
    let mut s = Sim::new(3, 15);
    s.partition = Some((2, 1_000, 3_000));
    s.lose_fetch = Some((2, 3_000));
    s.run(3_000);
    let behind = s.final_frame(2);
    s.run(9_000);
    assert!(s.lost_fetch.is_some(), "member 2 sent no fetch after the outage");
    assert!(s.nodes[2].core().refetches() >= 1, "the gap filled without a re-send");
    assert!(s.final_frame(2).0 > behind.0 + 60, "member 2 stuck at {behind:?} (others at {:?})", s.final_frame(0));
    let n = s.assert_same_final_log();
    assert!(n >= 100, "only {n} final entries");
}

#[test]
fn honest_runs_ask_for_no_block_twice() {
    // The re-send costs nothing where no fetch is lost: n = 2, 3 and 4 with no faults, and n = 4 with a jittery link
    // reordering proposals (blocks reach members before their parents: the gaps fill within a round trip).
    let mut orphans = 0;
    for (n, seed, jitter) in
        [(2u32, 31, false), (3, 32, false), (4, 33, false), (4, 34, true), (4, 35, true), (3, 36, true)]
    {
        let mut s = Sim::new(n, seed);
        if jitter {
            s.delay[1] = (2, 180);
        }
        s.run(6_000);
        let re: Vec<u64> = s.nodes.iter().map(|x| x.core().refetches()).collect();
        assert!(re.iter().all(|&r| r == 0), "n = {n}, seed {seed}: fetches sent again {re:?}");
        orphans += s.nodes.iter().map(|x| x.core().orphans_adopted()).sum::<u64>();
        s.assert_same_final_log();
    }
    // The orphan case's runs (rooted_cuts_match_through_pipelines_abandoned_cuts_and_orphans): a jittery link, a
    // lagging author, and one member cut off for 0.5 s. Its losses are of messages sent while it is cut off.
    let mut sent_again = 0;
    for seed in 1..=16u64 {
        let mut s = Sim::new(4, 400 + seed);
        s.delay[1] = (2, 180);
        s.stamp_lag[3] = 4;
        let start = 900 + 37 * seed as i64;
        s.partition = Some(((seed % 4) as usize, start, start + 500));
        s.run(3_000);
        s.assert_same_final_log();
        orphans += s.nodes.iter().map(|x| x.core().orphans_adopted()).sum::<u64>();
        sent_again += s.nodes.iter().map(|x| x.core().refetches()).sum::<u64>();
    }
    eprintln!("orphans adopted {orphans}, fetches sent again {sent_again}");
    assert_eq!(sent_again, 0, "fetches sent again in the orphan runs");
    assert!(orphans >= 1, "no block reached a member before its parent");
}

// The rate budget follows the world's frame rate (@design-reviewer's Low 1 on pe-app step 2; design agreed with
// @engine-architect m216ee6ef5fc2).

fn genesis_cut() -> Cut {
    Cut { at: CutRef { line: LineId::origin(), height: 0, frame: Frame(0) }, heads: Vec::new() }
}

#[test]
fn the_budget_follows_the_worlds_rate() {
    use crate::RateBudget;
    let cfg = NodeConfig::new(LineId::origin(), Vec::new(), genesis_cut());
    let b = |per_s, burst, pending| RateBudget { per_s, burst, pending };
    assert_eq!(cfg.rate_budget(30), b(150.0, 75, 600), "30 Hz: the floors");
    assert_eq!(cfg.rate_budget(60), b(255.0, 127, 1_020));
    assert_eq!(cfg.rate_budget(90), b(367.5, 183, 1_470));
    assert_eq!(cfg.rate_budget(120), b(480.0, 240, 1_920));
}

/// One change to a node's settings.
type Change = Box<dyn Fn(&mut NodeConfig)>;

#[test]
fn the_consensus_hash_covers_exactly_the_must_match_settings() {
    // For the M1 interim's genesis record (coordinator md1a4c7ca9c0e, mb33c54e8dece).
    let keys = TestKeys::new(3, 2);
    let base = NodeConfig::new(LineId::origin(), keys.sorted_keys(), genesis_cut());
    let h = base.consensus_hash();
    let changes: Vec<(&str, Change)> = vec![
        ("frame_tolerance_s", Box::new(|c| c.frame_tolerance_s = 0.2)),
        ("entries_per_frame", Box::new(|c| c.entries_per_frame = 4)),
        ("extra_per_s", Box::new(|c| c.extra_per_s = 31)),
        ("budget_slack", Box::new(|c| c.budget_slack = 1.5)),
        ("burst_s", Box::new(|c| c.burst_s = 0.6)),
        ("pending_s", Box::new(|c| c.pending_s = 5.0)),
        ("min_rate_per_s", Box::new(|c| c.min_rate_per_s = 151)),
        ("min_burst", Box::new(|c| c.min_burst = 61)),
        ("min_pending", Box::new(|c| c.min_pending = 601)),
        ("defer_bytes", Box::new(|c| c.defer_bytes += 1)),
        ("cut_interval_s", Box::new(|c| c.cut_interval_s = 0.2)),
        ("cadence", Box::new(|c| c.timing.cadence_ns += 1)),
        ("min timeout", Box::new(|c| c.timing.min_timeout_ns += 1)),
        ("max timeout", Box::new(|c| c.timing.max_timeout_ns += 1)),
    ];
    for (what, f) in changes {
        let mut c = base.clone();
        f(&mut c);
        assert_ne!(c.consensus_hash(), h, "{what} must change the hash");
    }
    let locals: Vec<(&str, Change)> = vec![
        ("cut_lag_s", Box::new(|c| c.cut_lag_s = 0.5)),
        ("fetch_retry_ns", Box::new(|c| c.fetch_retry_ns += 1)),
        ("keep_s", Box::new(|c| c.keep_s = 1.0)),
        ("keep_bytes", Box::new(|c| c.keep_bytes = 1)),
        ("relay", Box::new(|c| c.relay = false)),
        // Carried by the genesis record itself, not the hash (@engine-architect m8e5846cff93e).
        ("line", Box::new(|c| c.line = LineId(Hash::of("l", b"other")))),
        ("members", Box::new(|c| c.members.truncate(1))),
        ("genesis frame", Box::new(|c| c.genesis.at.frame = Frame(5))),
        ("genesis_gov", Box::new(|c| c.genesis_gov = pe_seams::GovState::default())),
    ];
    for (what, f) in locals {
        let mut c = base.clone();
        f(&mut c);
        assert_eq!(c.consensus_hash(), h, "{what} must not change the hash");
    }
}

/// Four members on one world clock, each submitting `hands` inputs and `edits` edits every frame on one chain, for
/// 10 s.
fn per_frame_run(rate: u32, hands: u32, edits: u32, seed: u64) -> Sim {
    let mut s = Sim::new(4, seed);
    s.clock = Clock(rate);
    s.per_frame = vec![Some((hands, edits)); 4];
    s.run(10_000);
    s
}

/// No member refused, faulted or voided an honest entry, and the final log agrees and covers at least 8 of the 10 s.
fn assert_all_honest_entries_pass(s: &Sim, per_frame: u32, what: &str) {
    for i in 0..4 {
        for e in &s.events[i] {
            assert!(
                !matches!(e, AgreeEvent::Refused { .. } | AgreeEvent::Fault(_)),
                "{what}: member {i} refused an honest entry: {e:?}"
            );
        }
        let voids = s.voids(i);
        assert!(voids.is_empty(), "{what}: {} honest entries voided, first {:?}", voids.len(), voids.first());
    }
    let n = s.assert_same_final_log();
    let want = 4 * per_frame as usize * s.clock.0 as usize * 8;
    assert!(n >= want, "{what}: only {n} final entries (want at least {want})");
}

#[test]
fn two_hands_per_frame_pass_at_30_60_and_120_hz() {
    for rate in [30, 60, 120] {
        let s = per_frame_run(rate, 2, 0, rate as u64);
        assert_all_honest_entries_pass(&s, 2, &format!("{rate} Hz"));
    }
}

#[test]
fn two_hands_and_an_edit_per_frame_pass_at_30_60_and_120_hz() {
    // pe-app paces its edit queue to at most one entry per frame on the same chain as the hands (@engine-architect
    // m216ee6ef5fc2): 90/s against the 150 floor at 30 Hz, 180 against 255 at 60 Hz, 360 against 480 at 120 Hz.
    for rate in [30, 60, 120] {
        let s = per_frame_run(rate, 2, 1, 1_000 + rate as u64);
        assert_all_honest_entries_pass(&s, 3, &format!("{rate} Hz, with an edit"));
    }
}

#[test]
fn a_flooder_at_twice_the_budget_is_held_to_it() {
    // One author sends 960 entries a second at 120 Hz, twice its budget, evenly: the burst and then the rate pass.
    use crate::codec::{Wire, encode};
    let (keys, mut node, _) = lone_node(8);
    let budget = NodeConfig::new(LineId::origin(), keys.sorted_keys(), genesis_cut()).rate_budget(120);
    let clock = Clock(120);
    let _ = node.tick(HostTime(0), &clock);
    let sent_per_s = 2 * budget.per_s as i64;
    let mut b = EntryBuilder::new(&keys, 1);
    let mut first_s = 0;
    for i in 0..3 * sent_per_s {
        let at = HostTime(i * SEC / sent_per_s);
        let e = b.next(Class::Input, clock.frame_at(at).unwrap(), Bytes::from_static(b"x"));
        let _ = node.receive(keys.key(1), Lane::Action, &encode(&Wire::Entries(vec![e])), at);
        if i + 1 == sent_per_s {
            first_s = node.held_entries() as i64;
        }
    }
    // Held: the burst and then the rate; the rest waits (and would be refused once its queue is full).
    let total = node.held_entries() as i64;
    assert_eq!(node.deferred_entries() as i64, 3 * sent_per_s - total);
    let (burst, rate) = (budget.burst as i64, budget.per_s as i64);
    assert!((first_s - (burst + rate)).abs() <= 2, "first second: {first_s} passed, want {}", burst + rate);
    assert!((total - (burst + 3 * rate)).abs() <= 2, "three seconds: {total} passed, want {}", burst + 3 * rate);
    assert!((total as usize) < budget.pending, "the pending cap is not what held it");
}

#[test]
fn a_flooding_member_cannot_starve_two_hands_per_frame() {
    // Member 3 submits twice its budget every frame at 120 Hz (8 entries a frame); the others send two hands each.
    // Peers refuse the flood beyond the budget, never an honest entry, and the honest entries all become final.
    let rate = 120;
    let budget = NodeConfig::new(LineId::origin(), Vec::new(), genesis_cut()).rate_budget(rate);
    let flood = (2.0 * budget.per_s / rate as f64).ceil() as u32;
    let mut s = Sim::new(4, 77);
    s.clock = Clock(rate);
    s.per_frame = vec![Some((2, 0)), Some((2, 0)), Some((2, 0)), Some((flood, 0))];
    s.flooder = Some(3);
    s.run(10_000);
    let k3 = s.keys.key(3);
    for i in 0..3 {
        let mut refused = 0;
        for e in &s.events[i] {
            match e {
                AgreeEvent::Refused { author, .. } => {
                    assert_eq!(*author, k3, "member {i} refused an honest entry: {e:?}");
                    refused += 1;
                }
                AgreeEvent::Fault(f) => assert!(f.author == Some(k3) || f.from == k3, "honest fault: {f:?}"),
                _ => {}
            }
        }
        assert!(refused > 0, "member {i} never refused the flood");
    }
    s.assert_same_final_log();
    // Every honest entry submitted in the first 8 s is final at member 0, none void.
    let low = (0..4).map(|i| s.final_frame(i)).min().unwrap();
    assert!(low.0 >= 8 * rate as u64, "final frame only {low:?}");
    let mut final_ids = std::collections::HashSet::new();
    for f in 0..low.0 {
        final_ids.extend(s.nodes[0].entries_at(Frame(f)).unwrap().iter().map(|e| e.id()));
    }
    for i in 0..3 {
        let first_8s = |(_, f): &(&EntryRef, &Frame)| f.0 < 8 * rate as u64;
        let honest: Vec<EntryRef> =
            s.submitted[i].iter().zip(&s.submitted_at[i]).filter(first_8s).map(|(id, _)| *id).collect();
        let missing = honest.iter().filter(|id| !final_ids.contains(id)).count();
        assert_eq!(missing, 0, "member {i}: {missing} honest entries not final");
        assert!(s.voids(0).iter().all(|v| !honest.contains(&v.0)));
    }
    eprintln!(
        "flood at 120 Hz: {} receive errors, member 3 final entries: {} of {}",
        s.refused,
        s.submitted[3].iter().filter(|id| final_ids.contains(id)).count(),
        s.submitted[3].len()
    );
}

/// Member 0's uplink stalls for `stall_ms` from 3 s at 120 Hz with two hands per frame, then sends everything at once.
/// Returns the sim, the refusals per member, and how long after the stall member 0's entries are all final again:
/// the first whole second from which every entry it submitted (up to 2 s before the run's end) is final.
fn stall_run(stall_ms: i64, run_ms: i64) -> (Sim, [usize; 4], Option<i64>) {
    stall_run_seeded(stall_ms, run_ms, 31)
}

fn stall_run_seeded(stall_ms: i64, run_ms: i64, seed: u64) -> (Sim, [usize; 4], Option<i64>) {
    stall_run_with(stall_ms, run_ms, seed, true)
}

fn stall_run_with(stall_ms: i64, run_ms: i64, seed: u64, paced: bool) -> (Sim, [usize; 4], Option<i64>) {
    let rate = 120;
    let mut s = Sim::new(4, seed);
    if !paced {
        // Overflow refusals are expected from an unpaced author's long stall: counted, not failed on.
        s.unpaced = Some(0);
        s.flooder = Some(0);
    }
    s.clock = Clock(rate);
    s.per_frame = vec![Some((2, 0)); 4];
    let end = 3_000 + stall_ms;
    s.stall = Some((0, 3_000, end));
    s.run(run_ms);
    let k0 = s.keys.key(0);
    let mut refused = [0usize; 4];
    for (i, r) in refused.iter_mut().enumerate() {
        for e in &s.events[i] {
            match e {
                AgreeEvent::Refused { author, .. } => {
                    assert_eq!(*author, k0, "member {i} refused another member's entry: {e:?}");
                    *r += 1;
                }
                AgreeEvent::Fault(f) => assert!(f.author == Some(k0) || f.from == k0, "fault not about 0: {f:?}"),
                _ => {}
            }
        }
    }
    s.assert_same_final_log();
    let low = (0..4).map(|i| s.final_frame(i)).min().unwrap();
    let mut final_ids = std::collections::HashSet::new();
    for f in 0..low.0 {
        final_ids.extend(s.nodes[1].entries_at(Frame(f)).unwrap().iter().map(|e| e.id()));
    }
    // Submitted ids are in order, two per frame from frame 1.
    // Member 0's entries submitted in [from_ms, to_ms).
    let between = |from_ms: i64, to_ms: i64| {
        let (a, b) = (rate as i64 * from_ms / 1_000, rate as i64 * to_ms / 1_000);
        s.submitted[0]
            .iter()
            .zip(&s.submitted_at[0])
            .filter(move |(_, f)| (f.0 as i64) >= a && (f.0 as i64) < b)
            .map(|(id, _)| *id)
    };
    let recovered = (0..(run_ms - 2_000 - end) / 1_000).find(|n| {
        let mut window = between(end + n * 1_000, run_ms - 2_000).peekable();
        window.peek().is_some() && window.all(|id| final_ids.contains(&id))
    });
    let per_s: Vec<usize> = (0..run_ms / 1_000)
        .map(|sec| between(sec * 1_000, (sec + 1) * 1_000).filter(|id| final_ids.contains(id)).count())
        .collect();
    // Never lost (@design-reviewer's Low 3): every peer's chain for member 0 links through every entry member 0 made
    // up to 2 s before the end, void or not. That is chain completeness, which "lost" depends on.
    let cutoff = rate as u64 * (run_ms as u64 - 2_000) / 1_000;
    let made = s.submitted_at[0].iter().filter(|f| f.0 < cutoff).count() as u64;
    let (key0, _) = s.nodes[0].own_head().expect("member 0 made entries");
    for i in 1..4 {
        let linked = s.nodes[i].linked_seq(&key0);
        assert!(linked >= made, "member {i} links member 0's chain through {linked}, short of {made}: entries lost");
    }
    let first_refusal = s.events[1].iter().position(|e| matches!(e, AgreeEvent::Refused { .. }));
    eprintln!("member 0 final per second {per_s:?}; final frame {low:?}; first refusal at event {first_refusal:?}");
    (s, refused, recovered)
}

#[test]
fn a_stalled_uplink_flushing_a_second_of_hands_at_120_hz() {
    // A 1 s stall flushes 240 hand entries, inside the full burst of 240: nothing is refused. The entries stamped
    // during the stall are late by then and void at the same cut everywhere; that is the stall, not the budget.
    let (s, refused, recovered) = stall_run(1_000, 10_000);
    assert_eq!(refused, [0; 4]);
    assert!(recovered.is_some_and(|n| n <= 1), "member 0 not final again within 1 s of the stall: {recovered:?}");
    eprintln!("stall 1 s at 120 Hz: refused {refused:?}, voided at member 1: {}", s.voids(1).len());
}

#[test]
fn a_stalled_uplink_flushing_three_seconds_of_hands_at_120_hz() {
    // @design-reviewer's Low 1 (me5616ae97a1b): VR Wi-Fi stalls for 2-3 s. A 3 s stall flushes 720 entries into a
    // burst of 240; every peer refuses part of it and reports a RateExceeded fault against an honest member. The
    // member's entries after the stall must still become final, soon.
    let (s, refused, recovered) = stall_run(3_000, 14_000);
    let faults: Vec<usize> =
        (0..4).map(|i| s.events[i].iter().filter(|e| matches!(e, AgreeEvent::Fault(_))).count()).collect();
    eprintln!(
        "stall 3 s at 120 Hz: refused {refused:?}, faults {faults:?}, voided at member 1: {}, final again {recovered:?} s after",
        s.voids(1).len()
    );
    assert!(recovered.is_some_and(|n| n <= 2), "member 0 not final again within 2 s of the stall: {recovered:?}");
}

// The pipelined-void fix (@harness-eng m365a498e6849; coordinator m90c32fc9ceb2): the entries a cut is rooted with are
// exactly what the final log yields at that cut once it is final.

#[test]
fn a_rooted_cut_is_simulated_with_exactly_its_final_entries() {
    // Late entries (an author stamping 1 s in the past), a member cut off for 2 s, and a slow link: each makes cuts
    // void entries they include while the cut before them is still in flight.
    let mut runs: Vec<(&str, Sim)> = Vec::new();
    for seed in 1..=3 {
        let mut s = Sim::new(4, 100 + seed);
        s.stamp_lag[3] = 30 + 10 * seed;
        runs.push(("late author", s));
        let mut s = Sim::new(4, 200 + seed);
        s.partition = Some((2, 1_000, 3_000));
        s.stamp_lag[2] = 12;
        runs.push(("partition", s));
        let mut s = Sim::new(3, 300 + seed);
        s.delay[1] = (60, 140);
        s.stamp_lag[1] = 9;
        runs.push(("slow link", s));
    }
    for (what, mut s) in runs {
        s.run(5_000);
        s.assert_same_final_log();
        let (checked, voided) = s.assert_roots_match_the_final_log();
        assert!(checked > 20, "{what}: only {checked} rooted cuts became final");
        eprintln!("{what}: {checked} rooted cuts checked, {voided} voids");
    }
}

/// The cuts some member rooted that never became final, each with the final cut at its height, where the final one
/// makes effective an entry stamped below the abandoned cut's frame that the abandoned cut did not include: an entry
/// the abandoned cut's frame must not void.
fn abandoned_cuts(s: &Sim) -> (usize, usize) {
    let finals: BTreeMap<u64, CutRef> = s.events[0]
        .iter()
        .filter_map(|e| if let AgreeEvent::CutFinal { cut, .. } = e { Some((cut.height, *cut)) } else { None })
        .collect();
    let (mut abandoned, mut below) = (0, 0);
    let mut seen = Vec::new();
    for i in 0..s.nodes.len() {
        for (cut, _) in &s.rooted[i] {
            let Some(fin) = finals.get(&cut.at.height) else { continue };
            if *fin == cut.at || seen.contains(&cut.at) {
                continue;
            }
            seen.push(cut.at);
            abandoned += 1;
            for f in fin.frame.0.saturating_sub(60)..cut.at.frame.0.min(fin.frame.0) {
                let Some(v) = s.nodes[0].entries_at(Frame(f)) else { continue };
                if v.iter().any(|e| !cut.includes(&e.entry.chain, e.entry.seq)) {
                    below += 1;
                    break;
                }
            }
        }
    }
    (abandoned, below)
}

#[test]
fn rooted_cuts_match_through_pipelines_abandoned_cuts_and_orphans() {
    // The coordinator's cases (m90c32fc9ceb2), each shown to happen and checked by the root = final log property:
    // a pipeline two or more deep; a cut abandoned at a view change whose replacement includes an entry stamped below
    // the abandoned cut's frame; and a block reaching a member before its parent.
    let (mut depth, mut abandoned, mut below, mut orphans, mut checked) = (0, 0, 0, 0, 0);
    for seed in 1..=16u64 {
        let mut s = Sim::new(4, 400 + seed);
        // A jittery link reorders proposals; a member cut off while it collects votes abandons the cut voted on; a
        // lagging author keeps entries in flight below the abandoned cut's frame.
        s.delay[1] = (2, 180);
        s.stamp_lag[3] = 4;
        let start = 900 + 37 * seed as i64;
        s.partition = Some(((seed % 4) as usize, start, start + 500));
        s.run(3_000);
        s.assert_same_final_log();
        checked += s.assert_roots_match_the_final_log().0;
        depth = depth.max(s.max_depth);
        let (a, b) = abandoned_cuts(&s);
        abandoned += a;
        below += b;
        orphans += (0..4).map(|i| s.nodes[i].core().orphans_adopted()).sum::<u64>();
    }
    eprintln!(
        "checked {checked}, deepest pipeline {depth}, abandoned {abandoned} ({below} with an entry below), orphans {orphans}"
    );
    assert!(depth >= 2, "no pipeline two deep (deepest {depth})");
    // The abandoned cut whose replacement includes an entry below its frame is forced by the scripted test
    // (an_abandoned_cuts_replacement_applies_an_entry_below_its_frame); here it is only reported.
    let _ = (abandoned, below);
    assert!(orphans >= 1, "no block reached a member before its parent");
}

#[test]
fn an_abandoned_cuts_replacement_applies_an_entry_below_its_frame() {
    // The coordinator's scripted case (m1fd241111a15). Member 3's uplink is held, so entries it stamps go unseen.
    // Once some member roots a cut A whose frame is past the hold, a blackout loses A's votes, so A is never certified
    // and is abandoned at the view change. Member 3's held entries are then released, and the replacement B, at A's
    // height on A's parent, includes some stamped below A's frame. They are effective under B (B's parent is
    // older), so every member's root for B must apply them, and it must match the final log.
    for seed in 1..=4u64 {
        let mut s = Sim::new(4, 500 + seed);
        s.run(1_000);
        s.holding = Some(3);
        let hold_frame = Clock(60).frame_at(HostTime(s.now)).unwrap();
        let hold_from = s.submitted[3].len();
        let mut abandoned: Option<Cut> = None;
        while s.now < 3_000 * MS && abandoned.is_none() {
            let next = s.now / MS + 1;
            s.run(next);
            abandoned = (0..3)
                .find_map(|i| s.rooted[i].last().map(|r| r.0.clone()).filter(|c| c.at.frame.0 > hold_frame.0 + 8));
        }
        let a = abandoned.expect("a cut past the hold was rooted");
        s.net.clear();
        s.blackout_until = s.now + 60 * MS;
        let next = s.now / MS + 60;
        s.run(next);
        s.release();
        s.run(6_000);
        s.assert_same_final_log();
        let (checked, _) = s.assert_roots_match_the_final_log();
        let fin = s.events[0]
            .iter()
            .find_map(|e| match e {
                AgreeEvent::CutFinal { cut, .. } if cut.height == a.at.height => Some(*cut),
                _ => None,
            })
            .expect("a cut final at the abandoned height");
        assert_ne!(fin, a.at, "seed {seed}: the cut was not abandoned");
        // A held entry of member 3, stamped below A's frame, A didn't include, effective in the final log.
        let held = &s.submitted[3][hold_from..];
        let below: Vec<Frame> = (hold_frame.0..a.at.frame.0)
            .map(Frame)
            .filter(|f| {
                s.nodes[0].entries_at(*f).is_some_and(|v| {
                    v.iter().any(|e| held.contains(&e.id()) && !a.includes(&e.entry.chain, e.entry.seq))
                })
            })
            .collect();
        assert!(!below.is_empty(), "seed {seed}: no held entry below A's frame {:?} became final", a.at.frame);
        // Held entries stamped below the parent of the cut that first includes them are rightly late; none is
        // voided otherwise.
        assert!(s.voids(0).iter().filter(|v| held.contains(&v.0)).all(|v| v.1 == VoidReason::Late));
        // Every member rooted the replacement, so its root over those entries was checked against the final log.
        for i in 0..3 {
            assert!(s.rooted[i].iter().any(|r| r.0.at == fin), "seed {seed}: member {i} never rooted the replacement");
        }
        eprintln!(
            "seed {seed}: abandoned {:?}, replacement {:?}, held entries final below A's frame at {} frames, {checked} roots checked",
            a.at,
            fin,
            below.len()
        );
    }
}

#[test]
fn an_honest_member_is_final_again_soon_after_a_stall_of_1_3_or_10_s() {
    // The coordinator's acceptance (m120f73db271d): the rate limiter may delay a valid entry but never lose one. An
    // honest member whose uplink stalls 1 s, 3 s or 10 s at 120 Hz (two hands a frame) has its entries final again
    // within a stated time of the stall's end, over several seeds, and a stall the queue absorbs reports no fault.
    for stall in [1_000, 3_000, 10_000] {
        for seed in [31, 32, 33] {
            let (s, refused, recovered) = stall_run_seeded(stall, 3_000 + stall + 5_000, seed);
            let faults: usize =
                (0..4).map(|i| s.events[i].iter().filter(|e| matches!(e, AgreeEvent::Fault(_))).count()).sum();
            eprintln!(
                "stall {stall} ms seed {seed}: refused {refused:?}, faults {faults}, final again {recovered:?} s after"
            );
            assert!(recovered.is_some_and(|n| n <= 2), "stall {stall} ms seed {seed}: not final again within 2 s");
            if stall <= 3_000 {
                assert_eq!(faults, 0, "stall {stall} ms seed {seed}: faults for a stall the queue absorbs");
            }
        }
    }
}

#[test]
fn an_unpaced_author_flushing_a_stall_is_delayed_never_lost() {
    // What pe-agree adds by itself, for an author that doesn't stop making hand entries (or whose transport flushes
    // everything after a stall): a 3 s stall's 720 entries wait in the queue rather than being refused, with no
    // fault; a 10 s stall's 2,400 overflow the queue, the latest leave or are refused, and the range refetch brings
    // them back, so the author's entries are final again, later.
    let (s, refused, recovered) = stall_run_with(3_000, 3_000 + 3_000 + 5_000, 31, false);
    let faults: usize = (0..4).map(|i| s.events[i].iter().filter(|e| matches!(e, AgreeEvent::Fault(_))).count()).sum();
    eprintln!("unpaced 3 s: refused {refused:?}, faults {faults}, final again {recovered:?} s after");
    assert_eq!(faults, 0, "a stall the queue absorbs reports no fault");
    assert!(recovered.is_some_and(|n| n <= 2), "unpaced 3 s: not final again within 2 s");
    let (_, refused, recovered) = stall_run_with(10_000, 3_000 + 10_000 + 14_000, 31, false);
    eprintln!("unpaced 10 s: refused {refused:?}, final again {recovered:?} s after");
    assert!(recovered.is_some_and(|n| n <= 11), "unpaced 10 s: not final again within 11 s");
}

// @design-reviewer's review of the stall fix (m15c6eb656e23).

/// The give-up count, plus asks a healthy run makes anyway (a gap that outlasts the wait).
const RANGE_GIVE_UP_FOR_TEST: usize = 5 + 5;

#[test]
fn pacing_never_holds_back_a_healthy_member_at_the_lag_cap() {
    // Medium 1: three entries a frame at 120 Hz (the most the budget allows) with the cut lag pinned at its cap, no
    // stall. Entries beyond the final cut run to about 400, over the burst of 244, so the old rule (unfinal_own >
    // burst) paces a healthy member; the unacknowledged backlog must not.
    for seed in [41, 42] {
        let mut s = Sim::with(4, seed, |c| {
            c.cut_lag_floor_s = c.cut_lag_cap_s;
        });
        s.clock = Clock(120);
        s.per_frame = vec![Some((2, 1)); 4];
        s.run(8_000);
        s.assert_same_final_log();
        eprintln!("seed {seed}: paced frames {:?}, old rule would pace {:?}", s.paced_frames, s.old_rule_frames);
        assert!(s.old_rule_frames.iter().all(|n| *n > 0), "the old rule never tripped: the test doesn't bite");
        assert_eq!(s.paced_frames, vec![0; 4], "a healthy member was paced");
    }
}

#[test]
fn a_member_repeating_range_refetches_gets_bounded_entries_back() {
    // Medium 2 and its later Low 1: a member asking for 256 entries every millisecond, cycling over three authors'
    // chains, gets back at most one author's burst plus its rate in all: about one author's stream, whatever it asks.
    let mut s = Sim::new(4, 51);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)); 4];
    s.run(4_000);
    let asks: Vec<Vec<u8>> = (0..3)
        .map(|i| {
            let (key, _) = s.nodes[i].own_head().unwrap();
            crate::codec::encode(&crate::codec::Wire::FetchRange {
                author: key.author,
                chain: key.chain,
                from_seq: 1,
                count: crate::codec::MAX_FETCH as u32,
            })
        })
        .collect();
    let k3 = s.keys.key(3);
    let clock = s.clock;
    let mut served = 0usize;
    let secs = 2;
    for ms in 0..secs * 1_000 {
        let at = HostTime(s.now + ms * MS);
        s.nodes[1].receive(k3, Lane::Protocol, &asks[ms as usize % 3], at).unwrap();
        for o in s.nodes[1].tick(at, &clock) {
            if o.to == k3
                && let Ok(crate::codec::Wire::Entries(es)) = crate::codec::decode(&o.bytes)
            {
                served += es.len();
            }
        }
    }
    let b = s.nodes[1].config().rate_budget(120);
    let bound = b.burst as usize + (b.per_s * secs as f64).ceil() as usize;
    eprintln!("served {served} entries in {secs} s against a bound of {bound} (asked for {})", 256 * secs * 1_000);
    assert!(served > 0, "nothing served");
    assert!(served <= bound, "served {served} entries, over {bound}");
}

#[test]
fn a_vote_claiming_a_seq_nobody_has_is_forgotten() {
    // Low 1: a Byzantine voter's heads claim seq 2^60 on an honest chain, and seqs on fabricated chains. The claim is
    // asked for a bounded number of times, then forgotten, and the same voter can't raise it again on that chain;
    // fabricated chains are bounded per author. Member 0 stops making entries first: while its chain grows, a gap
    // never lasts long enough to be asked for, so the claim costs nothing.
    let mut s = Sim::new(4, 61);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)); 4];
    s.run(2_000);
    s.per_frame[0] = None;
    s.every_ms[0] = 0;
    let (key0, _) = s.nodes[0].own_head().unwrap();
    let k3 = s.keys.key(3);
    s.count_ranges = Some(key0.chain);
    s.nodes[1].claim_for_test(k3, key0, 1 << 60);
    assert_eq!(s.nodes[1].vote_claim(&key0), Some(1 << 60));
    for c in 0..100u32 {
        let mut h = Hasher::new("fake");
        h.write_u64(c as u64);
        let fake = pe_log::ChainKey { author: key0.author, chain: pe_seams::ChainId(h.finish()) };
        s.nodes[1].claim_for_test(k3, fake, 1 << 40);
    }
    let (live, ignored) = s.nodes[1].vote_claims();
    assert!(live + ignored <= 1 + 4, "claims not bounded: {live} live, {ignored} given up");
    s.run(2_000 + 20_000);
    // Re-raising after the give-up is ignored.
    s.nodes[1].claim_for_test(k3, key0, 1 << 60);
    let asks_then = s.range_asks[1];
    s.run(2_000 + 25_000);
    let (live, ignored) = s.nodes[1].vote_claims();
    let later = s.range_asks[1] - asks_then;
    eprintln!(
        "range asks by member 1 on member 0's chain: {asks_then} in 20 s, then {later} in 5 s; claims {live} live, {ignored} given up"
    );
    assert!(s.nodes[1].vote_claim(&key0).is_none_or(|c| c < 1 << 60), "the claim nobody answers is still live");
    assert!(asks_then <= 3 * RANGE_GIVE_UP_FOR_TEST, "asked {asks_then} times for a claim nobody answers");
    assert!(later <= 3, "a given-up claim is asked again ({later} asks)");
    s.assert_same_final_log();
}

#[test]
fn backlogged_answers_through_the_agreement_trait() {
    // pe-app reads `Agreement::backlogged()` through the seam. The provided default is `false`, so a missing or
    // misspelled override in `impl Agreement for Node` would compile silently and only show as "never paces" on two
    // machines (@design-reviewer m6464c2c3d8d6). Member 0's uplink stalls from 1 s; 1.5 s in, its backlog is past
    // the burst.
    let mut s = Sim::new(4, 31);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)); 4];
    s.stall = Some((0, 1_000, 3_000));
    s.run(2_500);
    assert!(s.nodes[0].backlogged(), "member 0 not backlogged 1.5 s into its stall: the test doesn't bite");
    let a: &dyn Agreement = &s.nodes[0];
    assert!(a.backlogged(), "Agreement::backlogged doesn't reach Node::backlogged");
}

// The cut grid (@engine-architect m1a122523d255): every cut's frame is a multiple of the world's cut interval in
// frames, so the replica's live root serves every vote.

/// The frames of the cuts member `i` saw become final.
fn final_frames(s: &Sim, i: usize) -> Vec<u64> {
    s.events[i]
        .iter()
        .filter_map(|e| if let AgreeEvent::CutFinal { cut, .. } = e { Some(cut.frame.0) } else { None })
        .collect()
}

#[test]
fn every_final_cut_is_on_the_grid_at_60_90_and_120_hz() {
    for (rate, k) in [(60, 6), (90, 9), (120, 12)] {
        let mut s = Sim::new(4, 71);
        s.clock = Clock(rate);
        s.per_frame = vec![Some((2, 0)); 4];
        s.run(5_000);
        s.assert_same_final_log();
        assert_eq!(s.nodes[0].config().cut_every(rate), k);
        let frames = final_frames(&s, 1);
        eprintln!("{rate} Hz: {} final cuts, last at frame {:?}", frames.len(), frames.last());
        assert!(frames.len() >= 30, "{rate} Hz: only {} final cuts in 5 s", frames.len());
        assert!(frames.iter().all(|f| f % k == 0), "{rate} Hz: a final cut off the grid of {k}: {frames:?}");
    }
}

#[test]
fn a_cut_off_the_grid_gets_no_honest_vote() {
    // Member 0 runs a 0.15 s interval (9 frames at 60 Hz) against the others' 0.1 s (6 frames). Its cuts at 9, 27, 45,
    // ... are off the honest grid and refused; those at multiples of 18 are on both and may pass. The line stays live.
    let mut s = Sim::with_each(4, 72, |i, c| {
        if i == 0 {
            c.cut_interval_s = 0.15;
        }
    });
    s.run(6_000);
    s.assert_same_final_log();
    let proposed_off: Vec<u64> = s.events[1]
        .iter()
        .filter_map(|e| if let AgreeEvent::Proposed { cut, .. } = e { Some(cut.frame.0) } else { None })
        .filter(|f| f % 6 != 0)
        .collect();
    for i in 1..4 {
        let frames = final_frames(&s, i);
        assert!(frames.len() >= 20, "member {i}: only {} final cuts", frames.len());
        assert!(frames.iter().all(|f| f % 6 == 0), "member {i}: a final cut off the honest grid: {frames:?}");
        // What the name claims (@engine-architect's nit): no honest member votes for an off-grid cut, final or not.
        let voted_off: Vec<u64> = s.events[i]
            .iter()
            .filter_map(|e| if let AgreeEvent::Voted { cut, .. } = e { Some(cut.frame.0) } else { None })
            .filter(|f| f % 6 != 0)
            .collect();
        assert!(voted_off.is_empty(), "member {i} voted for off-grid cuts at {voted_off:?}");
    }
    eprintln!("off-grid proposals seen by member 1: {}; final cuts {}", proposed_off.len(), final_frames(&s, 1).len());
    assert!(!proposed_off.is_empty(), "member 0 never proposed off the grid: the test doesn't bite");
}

// Entries made during an outage (@item:primordial-engine/pe-agree-entries-made-during-an-outage-must-not-b; coordinator
// md904d9b1a9c7): "delayed, never lost" holds for edits whatever the cause.

/// Two members at 120 Hz, two hands a frame and an edit every 10 frames each, lose every message for `dark_ms` from
/// 3 s. Returns the sim and the frames the blackout started and ended at.
fn outage_run(dark_ms: i64, run_ms: i64, seed: u64) -> (Sim, Frame, Frame) {
    outage_run_with(2, dark_ms, run_ms, seed, |_| {})
}

fn outage_run_with(n: u32, dark_ms: i64, run_ms: i64, seed: u64, f: impl Fn(&mut NodeConfig)) -> (Sim, Frame, Frame) {
    let mut s = Sim::with(n, seed, f);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)); n as usize];
    s.edit_every = vec![10; n as usize];
    s.run(3_000);
    let from = s.clock.frame_at(HostTime(s.now)).unwrap();
    s.blackout_until = s.now + dark_ms * MS;
    s.run(3_000 + dark_ms);
    let until = s.clock.frame_at(HostTime(s.now)).unwrap();
    s.run(run_ms);
    (s, from, until)
}

/// Late voids at member `i` of entries stamped in [from, until), as (hands, edits).
fn outage_voids(s: &Sim, i: usize, from: Frame, until: Frame) -> (usize, usize) {
    let edits: Vec<EntryRef> = s.edits.iter().flatten().map(|e| e.0).collect();
    let stamped: Vec<EntryRef> = (0..s.nodes.len())
        .flat_map(|a| s.submitted[a].iter().zip(&s.submitted_at[a]))
        .filter(|(_, f)| **f >= from && **f < until)
        .map(|(e, _)| *e)
        .collect();
    let late: Vec<EntryRef> =
        s.voids(i).into_iter().filter(|v| v.1 == VoidReason::Late && stamped.contains(&v.0)).map(|v| v.0).collect();
    let e = late.iter().filter(|v| edits.contains(v)).count();
    (late.len() - e, e)
}

#[test]
fn no_entry_made_during_a_ten_second_outage_is_voided_as_late() {
    // The repro, @shader-lang's blackout test in pe-agree's sim (m118d7ea7bf46: 0-151 Late voids per run at n = 2).
    for seed in [61, 62, 63, 64] {
        let (s, from, until) = outage_run(10_000, 30_000, seed);
        s.assert_same_final_log();
        let firsts: Vec<Option<CutRef>> = (0..2)
            .map(|i| {
                s.events[i].iter().find_map(|e| match e {
                    AgreeEvent::CutFinal { cut, .. } if cut.frame > until => Some(*cut),
                    _ => None,
                })
            })
            .collect();
        let v: Vec<(usize, usize)> = (0..2).map(|i| outage_voids(&s, i, from, until)).collect();
        let edits_final = (0..2)
            .map(|a| {
                s.edits[a]
                    .iter()
                    .filter(|(e, f)| s.nodes[0].entries_at(*f).is_some_and(|v| v.iter().any(|x| x.id() == *e)))
                    .count()
            })
            .collect::<Vec<_>>();
        eprintln!(
            "seed {seed}: dark {from:?}..{until:?}; first final after {firsts:?}; Late voids (hands, edits) {v:?}; edits final {edits_final:?} of {:?}; final frames {:?}",
            s.edits.iter().map(|e| e.len()).collect::<Vec<_>>(),
            (0..2).map(|i| s.final_frame(i)).collect::<Vec<_>>()
        );
        assert_eq!(v, vec![(0, 0); 2], "seed {seed}: entries made during the outage were voided as late");
    }
}

#[cfg(feature = "mutants")]
#[test]
fn each_side_of_the_outage_rule_alone() {
    // Which side does the work at n = 2 (a measurement; the red steps are the code-level ones).
    use crate::core::Mutation;
    for m in [Mutation::NoAbstention, Mutation::NoBacklogCap] {
        for seed in [61, 62, 63, 64] {
            let (s, from, until) = outage_run_with(2, 10_000, 30_000, seed, |c| c.mutation = Some(m));
            s.assert_same_final_log();
            let v: Vec<(usize, usize)> = (0..2).map(|i| outage_voids(&s, i, from, until)).collect();
            eprintln!("{m:?} seed {seed}: Late voids (hands, edits) {v:?}");
        }
    }
}

/// Four members at 120 Hz, two hands a frame and an edit every 10 frames each; what member 3 sends member 0 is lost
/// for 10 s from 3 s, so member 0 learns of member 3's entries only from the others' heads. Returns the sim and
/// member 3's Late voids (hands, edits) at member 0 among entries stamped while the link was down.
fn one_link_run(seed: u64, f: impl Fn(&mut NodeConfig)) -> (Sim, (usize, usize)) {
    let mut s = Sim::with(4, seed, f);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)); 4];
    s.edit_every = vec![10; 4];
    s.link_down = Some((3, 0, 3_000, 13_000));
    s.run(3_000);
    let from = s.clock.frame_at(HostTime(s.now)).unwrap();
    s.run(13_000);
    let until = s.clock.frame_at(HostTime(s.now)).unwrap();
    s.run(20_000);
    s.assert_same_final_log();
    let edits: Vec<EntryRef> = s.edits[3].iter().map(|e| e.0).collect();
    let stamped: Vec<EntryRef> = s.submitted[3]
        .iter()
        .zip(&s.submitted_at[3])
        .filter(|(_, f)| **f >= from && **f < until)
        .map(|(e, _)| *e)
        .collect();
    let late: Vec<EntryRef> =
        s.voids(0).into_iter().filter(|v| v.1 == VoidReason::Late && stamped.contains(&v.0)).map(|v| v.0).collect();
    let e = late.iter().filter(|v| edits.contains(v)).count();
    (s, (late.len() - e, e))
}

#[test]
fn a_leader_missing_one_members_entries_caps_its_cut_below_them() {
    for seed in [71, 72, 73] {
        let (s, v) = one_link_run(seed, |_| {});
        eprintln!(
            "seed {seed}: member 3's Late voids (hands, edits) {v:?}; final frames {:?}",
            (0..4).map(|i| s.final_frame(i)).collect::<Vec<_>>()
        );
        assert_eq!(v, (0, 0), "seed {seed}: member 3's entries voided as late");
    }
}

#[cfg(feature = "mutants")]
#[test]
fn without_the_backlog_cap_a_leader_missing_one_members_entries_voids_them() {
    use crate::core::Mutation;
    for seed in [71, 72, 73] {
        let (_, v) = one_link_run(seed, |c| c.mutation = Some(Mutation::NoBacklogCap));
        eprintln!("NoBacklogCap seed {seed}: member 3's Late voids (hands, edits) {v:?}");
    }
}

#[test]
fn no_entry_made_during_a_ten_second_outage_of_four_members_is_voided_as_late() {
    for seed in [81, 82, 83] {
        let (s, from, until) = outage_run_with(4, 10_000, 30_000, seed, |_| {});
        s.assert_same_final_log();
        let v: Vec<(usize, usize)> = (0..4).map(|i| outage_voids(&s, i, from, until)).collect();
        eprintln!(
            "n = 4 seed {seed}: Late voids (hands, edits) {v:?}; final frames {:?}",
            (0..4).map(|i| s.final_frame(i)).collect::<Vec<_>>()
        );
        assert_eq!(v, vec![(0, 0); 4], "seed {seed}: entries made during the outage were voided as late");
    }
}

/// Late voids of entries stamped during a 10 s outage, summed over the members, at n = 4 with `m`, per seed.
#[cfg(feature = "mutants")]
fn outage_control(m: crate::core::Mutation, seeds: impl IntoIterator<Item = u64>) -> Vec<(u64, usize)> {
    seeds
        .into_iter()
        .map(|seed| {
            let (s, from, until) = outage_run_with(4, 10_000, 30_000, seed, |c| c.mutation = Some(m));
            s.assert_same_final_log();
            (seed, (0..4).map(|i| outage_voids(&s, i, from, until)).map(|(h, e)| h + e).sum())
        })
        .collect()
}

#[cfg(feature = "mutants")]
#[test]
fn at_four_members_the_cap_alone_holds_an_outages_backlog() {
    // At n = 4 three votes make a quorum, so an abstaining owner can't stop a cut that omits its backlog: the leader's
    // cap must, and with the cap and no abstention nothing stamped in the outage is voided.
    let v = outage_control(crate::core::Mutation::NoAbstention, 81..=84);
    eprintln!("n = 4 NoAbstention: Late voids per seed {v:?}");
    assert!(v.iter().all(|(_, n)| *n == 0), "the cap alone should hold at n = 4: {v:?}");
}

#[cfg(feature = "mutants")]
#[test]
fn at_four_members_the_outage_rule_without_its_cap_voids_the_backlog() {
    // The control: without the leader's cap, an omitted backlog can be voided. Which seeds bite depends on races
    // after the return; the sim is deterministic per seed (BTreeMaps throughout, @design-reviewer mdb7e1d5116ad; it
    // used to bite on HashMap order). Measured [V, main dd765d0a + the BTreeMaps]: 4 voids on seed 86, 8 on 94, 8 on
    // 99, none on the other 17 of 81-100. If a timing change stops these three biting, pick again with
    // `outage_control_seed_sweep` rather than guessing.
    let v = outage_control(crate::core::Mutation::NoBacklogCap, [86, 94, 99]);
    eprintln!("n = 4 NoBacklogCap: Late voids per seed {v:?}");
    assert!(v.iter().map(|(_, n)| n).sum::<usize>() > 0, "the control didn't bite: without the cap nothing was voided");
}

#[cfg(feature = "mutants")]
#[test]
#[ignore = "seed picker for the control above: cargo test -p pe-agree --release --all-features --lib -- --ignored outage_control_seed_sweep --nocapture"]
fn outage_control_seed_sweep() {
    for m in [crate::core::Mutation::NoAbstention, crate::core::Mutation::NoBacklogCap] {
        eprintln!("n = 4 {m:?}: Late voids per seed {:?}", outage_control(m, 81..=100));
    }
}

#[test]
fn a_claimed_backlog_never_sent_holds_cuts_back_once_and_only_for_the_wait() {
    // @fable-reviewer's trap (coordinator md904d9b1a9c7): a member that claims entries and never sends them. Member 3
    // crashes at 3 s; every other member is told (as by a vote's heads) that its chain reaches far beyond what they
    // hold. Their cuts are capped at its last held stamp for at most the wait (the pending cap at the author's rate
    // plus 1 s: 1,920 / 480 + 1 = 5 s at 120 Hz), then go on, and the claim doesn't hold them back again.
    let mut s = Sim::new(4, 91);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)); 4];
    s.run(3_000);
    s.cut_off = Some(3);
    let k3 = s.keys.key(3);
    let e3 = (0..s.final_frame(0).0)
        .rev()
        .find_map(|f| s.nodes[0].entries_at(Frame(f)).and_then(|v| v.iter().find(|e| e.entry.author == k3).cloned()))
        .expect("member 3 has a final entry");
    let key = pe_log::ChainKey::of(&e3);
    for i in 0..3 {
        s.nodes[i].claim_for_test(k3, key, e3.entry.seq + 100_000);
    }
    let mut finals = Vec::new();
    for t in (3_100..=20_000).step_by(100) {
        s.run(t);
        finals.push((t, s.final_frame(0)));
    }
    s.assert_same_final_log();
    // The longest time member 0's final frame stood still after the claim.
    let mut stood = 0;
    let mut since = 3_000;
    for w in finals.windows(2) {
        if w[1].1 > w[0].1 {
            since = w[1].0;
        } else {
            stood = stood.max(w[1].0 - since);
        }
    }
    let late = s.final_frame(0).0 as i64 * 1_000 / 120;
    eprintln!("claimed backlog never sent: final frame stood still at most {stood} ms; final at {late} ms of 20,000");
    assert!(stood <= 6_500, "cuts held back {stood} ms, past the 5 s wait");
    assert!(late >= 17_000, "finality didn't resume: final at {late} ms");
}

#[test]
fn the_abstention_bound_keeps_each_own_entry_to_two_views_when_leaders_alternate() {
    // @design-reviewer's L1: omissions alternate e1, e2, e1, e2, e1 over views 1-5; neither entry costs more than
    // two views, and once e1's are used up nothing at or below it is abstained for again.
    let mut b = crate::node::AbstainBook::default();
    let mut used = std::collections::BTreeMap::<u64, usize>::new();
    for (view, seq) in [(1, 5), (2, 7), (3, 5), (4, 7), (5, 5), (6, 7), (7, 3)] {
        // Progress after every view here, so only the per-entry bound applies.
        b.progressed(0);
        if b.abstains(seq, view) {
            assert!(b.note(seq, view));
            *used.entry(seq).or_default() += 1;
        }
    }
    assert_eq!(used, [(5, 2), (7, 2)].into_iter().collect(), "abstentions per own entry");
    assert!(b.abstains(9, 8), "a later entry still gets its views");
    // Without progress, at most two abstentions in all, whatever the entries.
    let mut b = crate::node::AbstainBook::default();
    let n = (1..=6).filter(|v| b.abstains(10 + v, *v) && b.note(10 + v, *v)).count();
    assert_eq!(n, 2, "abstentions between two final cuts");
}

/// n = 2 at 120 Hz with two hands a frame each; what member 1 sends member 0 takes `extra` ms more, plus up to
/// `jitter`. Returns the sim and the longest time member 0's final frame stood still after the first 3 s, in ms.
fn slow_link_run(extra: i64, jitter: i64, seed: u64) -> (Sim, i64) {
    slow_link_run_with(extra, jitter, seed, |_| {})
}

fn slow_link_run_with(extra: i64, jitter: i64, seed: u64, f: impl Fn(&mut NodeConfig)) -> (Sim, i64) {
    let mut s = Sim::with(2, seed, f);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)); 2];
    // B also makes an edit every 10 frames (@design-reviewer's L2): does an edit survive the slow link?
    s.edit_every = vec![0, 10];
    s.slow_link = Some((1, 0, extra, jitter));
    let (mut last, mut since, mut worst) = (Frame(0), 3_000, 0);
    for t in (100..=33_000).step_by(100) {
        s.run(t);
        let f = s.final_frame(0);
        if f > last {
            last = f;
            since = t;
        } else if t > 3_000 {
            worst = worst.max(t - since);
        }
    }
    s.assert_same_final_log();
    (s, worst)
}

/// n = 2 with one slow direction (B -> A, or A -> B when `rev`) and no lead, for 30 s: the mean of (present -
/// final frame) after 5 s, the longest stand-still of the final frame after 5 s (ms), and the first final (ms).
fn one_way_run(rate: u32, seed: u64, delay: i64, rev: bool) -> (f64, i64, i64) {
    let mut s = Sim::new(2, seed);
    s.clock = Clock(rate);
    s.per_frame = vec![Some((2, 0)); 2];
    s.slow_link = Some(if rev { (0, 1, delay, 0) } else { (1, 0, delay, 0) });
    let (mut last, mut since, mut worst, mut lag_sum, mut n, mut first) = (Frame(0), 5_000, 0, 0.0, 0, None);
    for t in (100..=30_000).step_by(100) {
        s.run(t);
        let f = s.final_frame(0);
        if f.0 > 0 && first.is_none() {
            first = Some(t);
        }
        if t > 5_000 {
            lag_sum += t as f64 / 1000.0 - f.0 as f64 / rate as f64;
            n += 1;
            if f > last {
                since = t;
            } else {
                worst = worst.max(t - since);
            }
        }
        last = f;
    }
    s.assert_same_final_log();
    (lag_sum / n as f64, worst, first.unwrap_or(i64::MAX))
}

#[test]
fn an_asymmetric_link_never_alternates_certified_and_timed_out_views_for_good() {
    // The finality-stall item (@coordinator-opus md72c13f27272, mdf35a62dc095, m16df0b1e1d39): n = 2, B -> A +1.2 s,
    // A -> B fast, no lead. B gets A's proposals and votes at once, so it forms each certificate first and enters the
    // next view at once; A, who needs B's vote over the slow link, proposes 1.2 s later. With the backoff reset by
    // every certificate and the timeout based only on succeeded views, B's ~1.1 s timeout expired first in every
    // other view: views alternated certified and timed out, and at 60 Hz 4 of these 12 seeds stood still 23.9 s.
    // With the backoff halved at commits and the proposal wait in the timeout, every stand-still stays within 8.5 s
    // and the mean lag within 6.5 s (ruling m8722b9f1f17d; measured 8.4 s and 6.14 s at worst). (The first sample of the proposal wait is unclamped, by ruling m16df0b1e1d39: a
    // slow first view is real information, so one seed's first final moves from 6.3 s to 8.8 s.)
    let mut out = Vec::new();
    for (rate, seeds) in [(60u32, 301..=312u64), (120, 301..=306)] {
        for seed in seeds {
            let (lag, worst, first) = one_way_run(rate, seed, 1_200, false);
            eprintln!(
                "{rate} Hz seed {seed}: mean lag {lag:.2} s; stood still at most {worst} ms; first final {first} ms"
            );
            out.push((rate, seed, lag, worst));
        }
    }
    for (rate, seed, lag, worst) in out {
        assert!(worst <= 8_500, "{rate} Hz seed {seed}: the final frame stood still {worst} ms");
        assert!(lag <= 6.5, "{rate} Hz seed {seed}: mean lag {lag:.2} s");
    }
}

fn one_way_sweep(rev: bool) {
    // One-way delays from 0.3 to 3 s, at 60 and 120 Hz (rulings m16df0b1e1d39, m8722b9f1f17d): no stand-still past
    // 5 x delay + 8 s (measured at worst: 0.7 s at 0.6 s, 18.0 s at 2 s, 22.1 s at 3 s).
    // Past the lag cap without a lead, the slow member abstains twice per final and each abstention costs a slow round
    // trip (agreement.md 3, the known limit past stamp-ahead's clamp), so the bound grows with the delay.
    let mut out = Vec::new();
    for delay in [300i64, 600, 2_000, 3_000] {
        for rate in [60u32, 120] {
            for seed in [301u64, 302] {
                let (lag, worst, _) = one_way_run(rate, seed, delay, rev);
                eprintln!(
                    "rev {rev} {delay} ms {rate} Hz seed {seed}: mean lag {lag:.2} s; stood still at most {worst} ms"
                );
                out.push((delay, rate, seed, worst));
            }
        }
    }
    for (delay, rate, seed, worst) in out {
        assert!(worst <= 5 * delay + 8_000, "rev {rev} {delay} ms {rate} Hz seed {seed}: stood still {worst} ms");
    }
}

#[test]
fn one_way_delays_up_to_3_s_from_b_never_stall_finality() {
    one_way_sweep(false);
}

#[test]
fn one_way_delays_up_to_3_s_from_a_never_stall_finality() {
    one_way_sweep(true);
}

#[test]
fn the_timeout_comes_back_down_when_a_slow_link_recovers() {
    // The finality-stall item (@coordinator-opus mdf35a62dc095): B -> A +1.2 s for 10 s, then 50 ms. The backoff
    // (halved at each commit) and the proposal wait (an average over views) grew on the slow link; both must come
    // back down, so a recovered link isn't left with multi-second view timeouts.
    for (rate, seed) in [(60u32, 311u64), (60, 312), (120, 311)] {
        let mut s = Sim::new(2, seed);
        s.clock = Clock(rate);
        s.per_frame = vec![Some((2, 0)); 2];
        s.slow_link = Some((1, 0, 1_200, 0));
        s.run(10_000);
        let high: Vec<i64> = (0..2).map(|i| s.nodes[i].core().timeout() / 1_000_000).collect();
        s.slow_link = Some((1, 0, 50, 0));
        let mut down = None;
        for t in (10_100..=25_000).step_by(100) {
            s.run(t);
            let worst = (0..2).map(|i| s.nodes[i].core().timeout()).max().unwrap();
            if down.is_none() && worst < 1_000_000_000 {
                down = Some(t - 10_000);
            }
        }
        let end: Vec<i64> = (0..2).map(|i| s.nodes[i].core().timeout() / 1_000_000).collect();
        eprintln!(
            "{rate} Hz seed {seed}: timeouts at the drop {high:?} ms; under 1 s from {down:?} ms after it; at 25 s {end:?} ms"
        );
        assert!(down.is_some_and(|ms| ms <= 10_000), "{rate} Hz seed {seed}: the timeout stayed up: {down:?}");
        assert!(end.iter().all(|ms| *ms < 1_000), "{rate} Hz seed {seed}: timeouts at 25 s {end:?} ms");
    }
}

#[test]
fn a_leader_withholding_every_second_view_is_demoted_after_its_first() {
    // The finality-stall item (@coordinator-opus mdf35a62dc095): n = 4 at 120 Hz, member 3 withholds its proposal in
    // every second view it leads. The reputation rule (the latest timed-out view's leader is skipped) demotes it after
    // its first withheld view, so it leads at most two views and withholds one. The proposal wait (sampled only from
    // proposals that arrive, clamped at the timeout) must not let it raise the others' timeouts: they end under 1 s.
    for seed in [401u64, 402, 403] {
        let mut s = Sim::new(4, seed);
        s.clock = Clock(120);
        s.per_frame = vec![Some((2, 0)); 4];
        s.withhold = Some(3);
        s.run(30_000);
        let withheld = s.led.values().filter(|w| **w).count();
        let timeouts: Vec<i64> = (0..3).map(|i| s.nodes[i].core().timeout() / 1_000_000).collect();
        let final_s = s.final_frame(0).0 as f64 / 120.0;
        eprintln!(
            "seed {seed}: member 3 led {} views, withheld {withheld}; final at {final_s:.1} s of 30; honest timeouts {timeouts:?} ms",
            s.led.len()
        );
        assert!(s.led.len() <= 2 && withheld <= 1, "seed {seed}: member 3 led {:?}", s.led);
        assert!(final_s >= 28.0, "seed {seed}: final at {final_s:.1} s");
        assert!(timeouts.iter().all(|ms| *ms < 1_000), "seed {seed}: honest timeouts {timeouts:?} ms");
        s.assert_same_final_log();
    }
}

#[test]
fn an_outages_backlog_is_never_sampled_as_lateness() {
    // @design-reviewer m7a78b8eaa1e1 (@item:primordial-engine/pe-agree-an-outage-s-backlog-must-not-be-sampled): after
    // a total blackout each member re-pushes its backlog, which can arrive before any vote or timeout claims it. It
    // was sampled with its whole outage age: the first lateness report read 22 s, and pe-app's stamp lead jumped to its
    // 3 s clamp. After the return, the reports and the cut lag must stay near the link's real delay (5-30 ms here).
    for (rate, seed) in [(120u32, 501u64), (120, 502), (60, 503)] {
        let mut s = Sim::new(2, seed);
        s.clock = Clock(rate);
        s.per_frame = vec![Some((2, 0)); 2];
        s.run(6_000);
        // The path goes dark for 10 s, and the transport delivers what was sent meanwhile when it returns.
        s.stall_all = Some((6_000, 16_000));
        s.run(16_000);
        let (mut report, mut lag) = (i64::MIN, 0u64);
        for t in (16_100..=30_000).step_by(100) {
            s.run(t);
            for i in 0..2 {
                for r in Agreement::own_lateness(&s.nodes[i]) {
                    report = report.max(r.frames);
                }
                lag = lag.max(s.nodes[i].cut_lag_frames());
            }
        }
        let counts = s.nodes[0].lateness_counts().get(&s.keys.key(1)).copied().unwrap_or_default();
        let ms = |f: i64| f * 1000 / rate as i64;
        eprintln!(
            "{rate} Hz seed {seed}: after the return, highest report {} ms, highest cut lag {} ms; B's arrivals at A {counts:?}",
            ms(report),
            ms(lag as i64)
        );
        assert!(ms(report) <= 100, "{rate} Hz seed {seed}: a report read {} ms after the blackout", ms(report));
        assert!(ms(lag as i64) <= 150, "{rate} Hz seed {seed}: the cut lag reached {} ms", ms(lag as i64));
        s.assert_same_final_log();
    }
}

/// Repeated short stalls (coordinator mddb626a425c8: the outage mark's threshold, 1 s against 0.3 s). Measurement
/// only: n = 2, 60 Hz, 30 s, hands on both and an edit from B every 10 frames; stalls of `len` ms every `period` ms
/// from 3 s to 27 s, held and flushed. Returns (late voids, edits voided, longest stand-still of member 0's final
/// frame after 3 s in ms, mean of present minus final at member 0 from 3 s in ms, highest report in ms, highest cut
/// lag in ms).
///
/// Measured [V, release, main 4dcc0574 + this setting] (seeds 601-604; mark 1 s -> 0.3 s):
/// - 0.5 s every 3 s, both uplinks: late voids 0 -> 0; stand-still 600 -> 600 ms; mean final lag 583 -> 366 ms;
///   report 466-483 -> 16 ms; cut lag 466-483 -> 66 ms.
/// - 0.8 s every 2 s, both: 0 -> 0; 900 -> 900 ms; 1,100-1,116 -> 516-533 ms; 766-783 -> 16 ms; 783 -> 66 ms.
/// - 0.5 s every 3 s, B only: 0 -> 0; 500-600 -> 500-600 ms; 416-433 -> 350-366 ms; 466-483 -> 16 ms.
/// - 0.8 s every 2 s, B only: 0 -> 0; 900 -> 800-900 ms; 716-733 -> 500 ms; 766-783 -> 16 ms.
/// - 0.3 s every 1.5 s, both (at the threshold, so no mark either way): 0 -> 0; reports 266-283 ms both.
fn repeated_stall_run(silence_s: Option<f64>, period: i64, len: i64, who: Option<usize>, seed: u64) -> [i64; 6] {
    let mut s = Sim::with(2, seed, |c| {
        if let Some(x) = silence_s {
            c.outage_silence_s = x;
        }
    });
    s.clock = Clock(60);
    s.per_frame = vec![Some((2, 0)); 2];
    s.edit_every = vec![0, 10];
    s.stall_every = Some((period, len, 3_000, 27_000, who));
    let ms = |f: i64| f * 1000 / 60;
    let (mut last, mut since, mut worst) = (Frame(0), 3_000, 0);
    let (mut lag_sum, mut lag_n, mut report, mut cut_lag) = (0i64, 0i64, i64::MIN, 0u64);
    for t in (100..=30_000).step_by(100) {
        s.run(t);
        let f = s.final_frame(0);
        if f > last {
            last = f;
            since = t;
        } else if t > 3_000 {
            worst = worst.max(t - since);
        }
        if t > 3_000 {
            let present = s.clock.frame_at(HostTime(t * MS)).unwrap();
            lag_sum += present.0 as i64 - f.0 as i64;
            lag_n += 1;
            for i in 0..2 {
                for r in Agreement::own_lateness(&s.nodes[i]) {
                    report = report.max(r.frames);
                }
                cut_lag = cut_lag.max(s.nodes[i].cut_lag_frames());
            }
        }
    }
    s.assert_same_final_log();
    let late = s.voids(0).iter().filter(|v| matches!(v.1, VoidReason::Late)).count() as i64;
    let edits: Vec<EntryRef> = s.edits[1].iter().map(|e| e.0).collect();
    let edit_voids = s.voids(0).iter().filter(|v| edits.contains(&v.0)).count() as i64;
    [late, edit_voids, worst, ms(lag_sum / lag_n.max(1)), ms(report), ms(cut_lag as i64)]
}

#[test]
fn a_half_second_dropout_is_not_sampled_as_lateness() {
    // The outage mark at 0.3 s (coordinator mddb626a425c8): a 0.5 s dropout of both uplinks, every 3 s, is an outage.
    // Its flushed backlog must not reach the reports or the cut lag (at 1 s, they read 466-483 ms), and at n = 2 the
    // stalls' entries wait rather than void.
    for seed in [601, 602] {
        let [late, edits, _, _, report, lag] = repeated_stall_run(None, 3_000, 500, None, seed);
        assert_eq!((late, edits), (0, 0), "seed {seed}: late voids");
        assert!(report <= 100, "seed {seed}: a report read {report} ms after a 0.5 s dropout");
        assert!(lag <= 150, "seed {seed}: the cut lag reached {lag} ms after a 0.5 s dropout");
    }
}

#[test]
#[ignore = "measurement (the outage mark's threshold): cargo test -p pe-agree --release --lib -- --ignored repeated_stalls --nocapture"]
fn repeated_stalls_at_two_outage_thresholds() {
    for (period, len, who) in
        [(3_000, 500, None), (2_000, 800, None), (3_000, 500, Some(1)), (2_000, 800, Some(1)), (1_500, 300, None)]
    {
        for silence in [1.0, 0.3] {
            let mut rows = Vec::new();
            for seed in [601, 602, 603, 604] {
                rows.push(repeated_stall_run(Some(silence), period, len, who, seed));
            }
            let col = |k: usize| rows.iter().map(|r| r[k].to_string()).collect::<Vec<_>>().join("/");
            eprintln!(
                "stall {len} ms every {period} ms ({}), mark {silence} s: late voids {}, edits voided {}, stand-still max {} ms, mean final lag {} ms, report max {} ms, cut lag max {} ms",
                who.map_or("both".to_string(), |k| format!("member {k} only")),
                col(0),
                col(1),
                col(2),
                col(3),
                col(4),
                col(5)
            );
        }
    }
}

#[test]
fn a_slow_link_above_the_lag_cap_still_finalizes() {
    // @fable-reviewer's concern (coordinator m60983b248518): at n = 2, a sustained one-way delay from B to A above
    // the lag means every cut A leads omits a fresh B entry, so B abstains in every view A leads, each time for a
    // new entry. The final frame must keep advancing, fixed delay and jittering.
    // The first two are above the lag cap (1 s); the third, below it, must void no edit.
    for (extra, jitter) in [(1_200, 0), (900, 600), (500, 400)] {
        for seed in [101, 102] {
            let (s, worst) = slow_link_run(extra, jitter, seed);
            let voids = s.voids(0).len();
            let edits: Vec<EntryRef> = s.edits[1].iter().map(|e| e.0).collect();
            let edit_voids = s.voids(0).iter().filter(|v| edits.contains(&v.0)).count();
            let final_s = s.final_frame(0).0 as f64 / 120.0;
            eprintln!(
                "B -> A +{extra} ms (+0-{jitter} ms) seed {seed}: final stood still at most {worst} ms; final at {final_s:.1} s of 33; voids {voids}, of them edits {edit_voids} of {}",
                edits.len()
            );
            assert!(final_s >= 25.0, "finality stalled: final at {final_s:.1} s");
            if extra + jitter < 1_000 {
                assert_eq!(edit_voids, 0, "an edit voided on a link below the lag cap");
            }
            assert!(worst <= 8_000, "the final frame stood still {worst} ms");
        }
    }
}

#[test]
fn votes_tell_a_slow_member_how_late_its_entries_arrive() {
    // Stamp-ahead (agreement.md 3; @item:primordial-engine/pe-app-stamps-a-slow-member-s-entries-ahead-by-i): A's votes
    // and timeouts carry how late B's entries reach A, so B's driver can stamp its own entries ahead. B -> A is slow
    // (+1.2 s fixed, +0.9-1.5 s jittered); A -> B is fast. B reads A's report with `own_lateness`, near the one-way
    // delay and over a recent seq of its chain; A reads B's, near zero.
    for (extra, jitter) in [(1_200, 0), (900, 600)] {
        for seed in [103, 104] {
            let (s, _) = slow_link_run(extra, jitter, seed);
            let (a, b) = (s.keys.key(0), s.keys.key(1));
            let on_b = Agreement::own_lateness(&s.nodes[1]);
            assert_eq!(on_b.len(), 1, "seed {seed}: B has no single report: {on_b:?}");
            assert_eq!(on_b[0].reporter, a);
            let ms = on_b[0].frames * 1000 / 120;
            let made = s.submitted[1].len() as u64;
            let counts = s.nodes[0].lateness_counts().get(&b).copied().unwrap_or_default();
            let on_a = Agreement::own_lateness(&s.nodes[0]);
            eprintln!(
                "B -> A +{extra} ms (+0-{jitter} ms) seed {seed}: A reports B's p95 {ms} ms over seq {} of {made}; B reports A's {:?} frames; B's arrivals at A {counts:?}",
                on_b[0].seq,
                on_a.iter().map(|r| r.frames).collect::<Vec<_>>()
            );
            assert!(
                ms >= extra && ms <= extra + jitter + 300,
                "seed {seed}: A reports {ms} ms for a {extra}+{jitter} ms link"
            );
            // The window is recent: within about two seconds of B's entries (two hands a frame and the edits).
            assert!(made - on_b[0].seq <= 600, "seed {seed}: the report covers seq {} of {made}", on_b[0].seq);
            assert_eq!(on_a.len(), 1, "seed {seed}: A has no single report: {on_a:?}");
            assert_eq!(on_a[0].reporter, b);
            assert!(
                on_a[0].frames * 1000 / 120 <= 300,
                "seed {seed}: B reports A's entries {} frames late",
                on_a[0].frames
            );
            assert!(
                counts.sampled > counts.backlog + counts.fetched,
                "seed {seed}: most of B's arrivals unsampled: {counts:?}"
            );
        }
    }
}

#[test]
fn a_rising_lead_never_reads_as_a_longer_delay() {
    // @design-reviewer's M1 on 0238ca46: B raises its stamp lead once, by 0.5 s, on a slow B -> A link. A report
    // corrected by the lead its seq's entry was stamped with (pe-app's `reported + lead(seq)`) must never exceed the
    // link's top delay: with the window's newest seq it read as the delay plus the step until the old samples aged out,
    // and the lead would ratchet up.
    const STEP: u64 = 60;
    for (extra, jitter) in [(1_200, 0), (900, 600)] {
        for seed in [105, 106] {
            let mut s = Sim::new(2, seed);
            s.clock = Clock(120);
            s.per_frame = vec![Some((2, 0)); 2];
            s.edit_every = vec![0, 10];
            s.slow_link = Some((1, 0, extra, jitter));
            s.run(10_000);
            // Every entry of B's chain after this seq is stamped STEP frames ahead.
            let step_seq = s.submitted[1].len() as u64;
            s.stamp_lead[1] = STEP;
            let (mut worst, mut newer) = (i64::MIN, 0usize);
            for t in (10_100..=20_000).step_by(100) {
                s.run(t);
                let r = Agreement::own_lateness(&s.nodes[1]);
                assert_eq!(r.len(), 1, "seed {seed}: B has no single report at {t} ms: {r:?}");
                let lead = if r[0].seq > step_seq { STEP as i64 } else { 0 };
                newer += (lead > 0) as usize;
                worst = worst.max(r[0].frames + lead);
            }
            let worst_ms = worst * 1000 / 120;
            eprintln!(
                "B -> A +{extra} ms (+0-{jitter} ms) seed {seed}: corrected delay at most {worst_ms} ms after a {STEP}-frame step; {newer} of 100 reports past the step"
            );
            // A guard that the window rolled past the step (votes, and so reports, are sparser on this link since the
            // backoff fix).
            assert!(newer >= 30, "seed {seed}: the reports never moved past the step ({newer})");
            assert!(
                worst_ms <= extra + jitter + 50,
                "seed {seed}: corrected delay {worst_ms} ms on a {extra}+{jitter} ms link"
            );
        }
    }
}

#[test]
fn a_falling_delay_shows_through_a_lead_that_is_too_large() {
    // @engine-architect's Medium on 0238ca46 and @design-reviewer's fall case: B stamps 1.2 s ahead on a 1.2 s link,
    // then the link's delay drops to 0.3 s. B's entries now arrive before their stamps; the report must say so
    // (negative), so the corrected delay follows the link down within a window, and pe-app can shed the lead. A
    // report clamped at 0 would read as 1.2 s for good.
    const LEAD: u64 = 144;
    for seed in [107, 108] {
        let mut s = Sim::new(2, seed);
        s.clock = Clock(120);
        s.per_frame = vec![Some((2, 0)); 2];
        s.edit_every = vec![0, 10];
        s.slow_link = Some((1, 0, 1_200, 0));
        s.run(6_000);
        let lead_seq = s.submitted[1].len() as u64;
        s.stamp_lead[1] = LEAD;
        let corrected = |s: &Sim| {
            let r = Agreement::own_lateness(&s.nodes[1]);
            assert_eq!(r.len(), 1, "seed {seed}: B has no single report: {r:?}");
            let lead = if r[0].seq > lead_seq { LEAD as i64 } else { 0 };
            (r[0].frames + lead) * 1000 / 120
        };
        s.run(10_000);
        let settled = corrected(&s);
        s.slow_link = Some((1, 0, 300, 0));
        // Entries sent just before the drop still take 1.2 s, and their lateness is real: the report can follow only
        // once they have arrived, within a window (128 of B's entries, about 0.5 s of them) after that. Entries sent
        // after the drop overtake them but wait for them in the chain, so they read late too: about 1.2 + 0.53 s, and
        // the 100 ms step (1.8 s measured on seed 107; within 1.7 s while pacing wrongly held B's hands on the slow
        // link, @item:primordial-engine/pe-agree-on-a-slow-not-stalled-link-backlogged-r).
        let (mut followed, mut after) = (None, Vec::new());
        for t in (10_100..=14_000).step_by(100) {
            s.run(t);
            let ms = corrected(&s);
            if followed.is_none() && ms <= 350 {
                followed = Some(t - 10_000);
            }
            if followed.is_some() {
                after.push(ms);
            }
        }
        eprintln!(
            "seed {seed}: corrected delay {settled} ms at a 1.2 s link with a 1.2 s lead; at 0.3 s from {followed:?} ms after the drop, then {after:?} ms"
        );
        assert!((1_150..=1_250).contains(&settled), "seed {seed}: settled at {settled} ms");
        assert!(
            followed.is_some_and(|ms| ms <= 1_900),
            "seed {seed}: the report didn't follow the delay down: {followed:?}"
        );
        assert!(after.iter().all(|ms| *ms <= 350), "seed {seed}: the report went back up: {after:?}");
    }
}

// Membership change by epochs, Leave only (agreement.md 8; @item:primordial-engine/m1-leave-a-member-leaves-gracefully-and-the-rest).

/// Member `i` signs its `Leave` now and makes no more entries.
fn leave(s: &mut Sim, i: usize) -> EntryRef {
    s.per_frame[i] = None;
    s.every_ms[i] = 0;
    let present = s.clock.frame_at(HostTime(s.now)).unwrap();
    let e = s.builders[i].next_unsigned(Class::Protocol, present, Bytes::from_static(crate::LEAVE));
    let e = s.nodes[i].submit(e).expect("own Leave refused");
    s.submitted[i].push(e.id());
    s.submitted_at[i].push(present);
    e.id()
}

#[test]
fn a_member_leaves_and_the_other_finalizes_alone() {
    // M1 demo step 8 (model v1's leave-2to1): B leaves at 3 s, stays for E's suffix, and quits at 5 s; A carries on
    // alone with quorum(1) = 1, and its final frame keeps advancing.
    for seed in [111, 112] {
        let mut s = Sim::new(2, seed);
        s.clock = Clock(120);
        s.per_frame = vec![Some((2, 0)); 2];
        s.run(3_000);
        let l = leave(&mut s, 1);
        // Nothing more from B: submit refuses after its own Leave.
        let next = s.builders[1].next_unsigned(Class::Input, Frame(400), Bytes::from_static(b"x"));
        assert_eq!(s.nodes[1].submit(next).err(), Some(pe_seams::agree::SubmitError::AfterLeave));
        s.run(5_000);
        let a = s.keys.key(0);
        for i in 0..2 {
            assert_eq!(s.nodes[i].members(), vec![a], "seed {seed}: member {i}'s epoch didn't end at E");
            assert!(s.nodes[i].core().epoch() == 1, "seed {seed}: member {i} not in epoch 1");
        }
        let at_quit = s.final_frame(0);
        s.cut_off = Some(1);
        s.run(15_000);
        s.assert_same_final_log();
        let leave_final =
            (0..at_quit.0).any(|f| s.nodes[0].entries_at(Frame(f)).is_some_and(|v| v.iter().any(|e| e.id() == l)));
        let fin = s.final_frame(0).0 as f64 / 120.0;
        eprintln!(
            "seed {seed}: final at the quit {:.2} s, at the end {fin:.2} s of 15; leave final {leave_final}",
            at_quit.0 as f64 / 120.0
        );
        assert!(leave_final, "seed {seed}: the Leave isn't in the final log");
        assert!(fin >= 14.0, "seed {seed}: A alone didn't finalize: final at {fin:.2} s");
        let faults: usize = s.events[0].iter().filter(|e| matches!(e, AgreeEvent::Fault(_))).count();
        assert_eq!(faults, 0, "seed {seed}: faults at A");
    }
}

#[test]
fn one_of_three_leaves_and_the_two_go_on() {
    // Model v1's leave-3to2: member 2 leaves; members 0 and 1 (q = 2) keep finalizing, and the leaver's later
    // messages and entries raise no fault.
    for seed in [121, 122] {
        let mut s = Sim::new(3, seed);
        s.clock = Clock(120);
        s.per_frame = vec![Some((2, 0)); 3];
        s.run(3_000);
        leave(&mut s, 2);
        s.run(5_000);
        let kept: Vec<_> = {
            let mut v = vec![s.keys.key(0), s.keys.key(1)];
            v.sort();
            v
        };
        for i in 0..3 {
            assert_eq!(s.nodes[i].members(), kept, "seed {seed}: member {i}");
        }
        s.cut_off = Some(2);
        s.run(12_000);
        s.assert_same_final_log();
        let fin = s.final_frame(0).0 as f64 / 120.0;
        eprintln!("seed {seed}: 3 -> 2, final at {fin:.2} s of 12");
        assert!(fin >= 11.0, "seed {seed}: the two didn't go on");
        for i in 0..2 {
            assert!(!s.events[i].iter().any(|e| matches!(e, AgreeEvent::Fault(_))), "seed {seed}: a fault at {i}");
        }
    }
}

#[test]
fn an_entry_after_the_leave_is_void_and_nothing_follows_it() {
    // A faulty leaver signs an entry after its Leave (pe-agree refuses to; this one is made by hand) and sends it; it
    // is void as AfterLeave at E, the same everywhere (agreement.md 8).
    let mut s = Sim::new(2, 131);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)); 2];
    s.run(3_000);
    leave(&mut s, 1);
    let present = s.clock.frame_at(HostTime(s.now)).unwrap();
    let late = s.builders[1].next(Class::Input, present, Bytes::from_static(b"after"));
    let bytes = crate::codec::encode(&crate::codec::Wire::Entries(vec![late.clone()]));
    let from = s.keys.key(1);
    s.nodes[0].receive(from, Lane::Action, &bytes, HostTime(s.now)).expect("a signed entry, held");
    s.run(6_000);
    s.assert_same_final_log();
    for i in 0..2 {
        let v: Vec<_> = s.voids(i).into_iter().filter(|v| v.0 == late.id()).collect();
        eprintln!("member {i}: {v:?}");
        assert!(v.len() == 1 && v[0].1 == VoidReason::AfterLeave, "member {i}: the entry after the Leave: {v:?}");
    }
    let cuts: Vec<CutRef> = (0..2).map(|i| s.voids(i).into_iter().find(|v| v.0 == late.id()).unwrap().2).collect();
    assert_eq!(cuts[0], cuts[1], "decided at different cuts");
}

#[test]
fn submit_refuses_an_own_entry_stamped_below_the_chains_last_stamp() {
    // S3 v1.7 BelowOwnStamp: an honest member never signs an entry that would be void as back-stamped.
    let mut s = Sim::new(2, 141);
    s.every_ms = vec![0, 0];
    let a = s.builders[0].next_unsigned(Class::Edit, Frame(10), Bytes::from_static(b"a"));
    s.nodes[0].submit(a).expect("first");
    let b = s.builders[0].next_unsigned(Class::Edit, Frame(9), Bytes::from_static(b"b"));
    assert_eq!(s.nodes[0].submit(b).err(), Some(pe_seams::agree::SubmitError::BelowOwnStamp));
}

/// n = 2: B leaves at 3 s and quits at the first tick where `quit` holds for it (at most 10 s later). Returns whether
/// A reached epoch 1, who had E final first (0 or 1), and how long after its Leave B quit, in ms.
fn quit_run(seed: u64, slow: Option<(usize, usize, i64, i64)>, quit: impl Fn(&Node) -> bool) -> (bool, usize, i64) {
    let mut s = Sim::new(2, seed);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)); 2];
    s.slow_link = slow;
    s.run(3_000);
    leave(&mut s, 1);
    let left = s.now / MS;
    let mut first = None;
    while s.now < 13_000 * MS && !quit(&s.nodes[1]) {
        let next = s.now / MS + 1;
        s.run(next);
        if first.is_none() {
            first = (0..2).find(|&i| s.nodes[i].core().epoch() == 1);
        }
    }
    let quit_ms = s.now / MS - left;
    // The process exits: what it hadn't delivered yet is lost too.
    s.net.retain(|_, m| m.0 != 1);
    s.cut_off = Some(1);
    s.run(s.now / MS + 10_000);
    (s.nodes[0].core().epoch() == 1, first.unwrap_or(9), quit_ms)
}

#[test]
fn a_leaver_that_waits_for_leave_settled_never_strands_the_other() {
    // @design-reviewer's M2 on e3597563 (coordinator m5e5018854743): at n = 2 whoever forms the certificate on E's
    // suffix child has E final first. A leaver that quits when E is final at itself can strand the other in epoch 0
    // with q = 2; one that waits for `leave_settled` (every remaining member told it E is final there) never does.
    // Each link direction in turn is slower, so each member is the first to have E final in some runs.
    let mut stranded_naive = 0;
    let mut firsts = [0; 2];
    let mut runs = 0;
    for slow in [None, Some((0, 1, 40, 0)), Some((1, 0, 40, 0))] {
        for seed in [151, 152, 153] {
            let (ok, first, ms) = quit_run(seed, slow, Agreement::leave_settled);
            eprintln!(
                "{slow:?} seed {seed}: quit on leave_settled after {ms} ms; E final first at member {first}; A in epoch 1: {ok}"
            );
            assert!(ok, "{slow:?} seed {seed}: A was stranded although B waited for leave_settled");
            assert!(ms < 10_000, "{slow:?} seed {seed}: leave_settled never came");
            if first < 2 {
                firsts[first] += 1;
            }
            let (naive, _, _) = quit_run(seed, slow, |n| n.core().epoch() == 1);
            stranded_naive += usize::from(!naive);
            runs += 1;
        }
    }
    eprintln!(
        "E final first at A in {} runs, at B in {}; quitting at E's local finality stranded A in {stranded_naive} of {runs}",
        firsts[0], firsts[1]
    );
    // The control: quitting at E's local finality, with the exit losing what was in flight, does strand A sometimes
    // (measured: 3 of 9), so the test can tell the two apart.
    assert!(stranded_naive > 0, "the naive quit never stranded A: the test doesn't exercise M2");
    assert!(firsts[0] > 0 && firsts[1] > 0, "each member should have E final first in some run: {firsts:?}");
}

#[test]
fn the_last_member_leaving_is_void_and_the_line_keeps_it() {
    // @design-reviewer's L1 on e3597563: both members of a pair leave in one cut. The first Leave in the final log's
    // order takes effect; the second would leave no members, so it is void as LastMember, and that member stays.
    let mut s = Sim::new(2, 161);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)); 2];
    s.run(3_000);
    let l0 = leave(&mut s, 0);
    let l1 = leave(&mut s, 1);
    s.run(6_000);
    let voided: Vec<_> = (0..2).flat_map(|i| s.voids(i)).filter(|v| v.0 == l0 || v.0 == l1).collect();
    eprintln!(
        "voids of the two Leaves: {voided:?}; members {:?}",
        (0..2).map(|i| s.nodes[i].members().len()).collect::<Vec<_>>()
    );
    assert_eq!(voided.len(), 2, "one Leave void, at both members");
    assert!(voided.iter().all(|v| v.1 == VoidReason::LastMember && v.0 == voided[0].0));
    let stays = if voided[0].0 == l0 { 0 } else { 1 };
    for i in 0..2 {
        assert_eq!(s.nodes[i].members(), vec![s.keys.key(stays)], "member {i}");
    }
}

/// n = 2, B -> A +`delay` ms, A -> B fast, two hands a frame at 60 Hz for 30 s; B stamps `lead_ms` ahead.
fn slow_return_run(seed: u64, delay: i64, lead_ms: u64, stall: Option<(usize, i64, i64)>) -> Sim {
    let mut s = Sim::new(2, seed);
    s.per_frame = vec![Some((2, 0)); 2];
    s.slow_link = Some((1, 0, delay, 0));
    s.stamp_lead[1] = lead_ms * 60 / 1_000;
    s.stall = stall;
    s
}

#[test]
fn a_slow_link_that_keeps_flowing_never_paces_its_fast_member() {
    // @item:primordial-engine/pe-agree-on-a-slow-not-stalled-link-backlogged-r (@engine-architect's stamp-ahead
    // measurement): B's votes name A's entries a slow round trip late, so with a fixed grace A (the fast uplink) counted
    // them as a backlog. Before the fix, in this sim [V]: with no lead, A was paced on 748-890 of its 1,800 frames at
    // D = 1.2 s and 383-562 at 0.9 s; with B stamping ahead by D + 0.1 s (pe-app's case), A on 640-970 and B, whose
    // entries then arrive in time, on 660-750 at 1.2 s. Each member now allows each peer's ack delay, less how late that
    // peer reports its entries (agreement.md 3, pacing): only the return path.
    for seed in [301u64, 302] {
        // No lead: A is never paced; B, whose entries arrive late against their stamps, still is at 0.9 s.
        for delay in [300i64, 900] {
            let mut s = slow_return_run(seed, delay, 0, None);
            s.run(30_000);
            eprintln!("B -> A +{delay} ms, no lead, seed {seed}: paced frames (A, B) {:?} of 1,800", s.paced_frames);
            assert_eq!(s.paced_frames[0], 0, "+{delay} ms seed {seed}: the fast member was paced");
        }
        // With the lead, B's entries arrive in time: neither member is paced.
        for delay in [900i64, 1_200] {
            let mut s = slow_return_run(seed, delay, delay as u64 + 100, None);
            s.run(30_000);
            eprintln!(
                "B -> A +{delay} ms, lead {} ms, seed {seed}: paced frames (A, B) {:?}",
                delay + 100,
                s.paced_frames
            );
            assert_eq!(s.paced_frames, vec![0, 0], "+{delay} ms with a lead, seed {seed}: a member was paced");
        }
    }
}

#[test]
fn a_stalled_uplink_still_paces_on_a_slow_link() {
    // The control for the ack delay: B -> A +1.2 s with B's lead, then A's uplink stalls from 10 to 14 s. The delay
    // sampled before the stall stays, so A is paced within it, from about the grace (0.4 s) plus the ack delay (about
    // 1.3 s) plus the burst (127 entries, about 1 s) into it: a stall can't be told sooner over a slow return path.
    for seed in [301u64, 302] {
        let mut s = slow_return_run(seed, 1_200, 1_300, Some((0, 10_000, 14_000)));
        s.run(10_000);
        let before = s.paced_frames[0];
        s.run(14_000);
        let during = s.paced_frames[0] - before;
        eprintln!("seed {seed}: A paced on {before} frames before the stall, {during} in its 4 s (240 frames)");
        assert_eq!(before, 0, "seed {seed}: paced before the stall");
        assert!(during >= 60, "seed {seed}: A paced on only {during} frames of its stall");
    }
}

#[test]
fn a_no_lead_stand_still_never_feeds_itself_through_the_pending_cap() {
    // @coordinator-opus m93a90907057e. Past the lead's clamp (B -> A +1.2 s, no lead) finality stands still for seconds
    // (agreement.md 3). With A paced only by its unacknowledged backlog, A's entries beyond the final cut reached B's
    // per-author pending cap (1,020 at 60 Hz) about 8.5 s in; B deferred them, couldn't hold A's cuts and couldn't vote,
    // so the stand-still fed itself: 21.5 s on 4 of these 12 seeds [V], against at most 8.4 s while the old rule
    // happened to pace A. A now also paces near the cap: the stand-still stays within the paced bound, and A is paced
    // there only (never by its backlog on this flowing link).
    let mut near = 0;
    for seed in 301..=312u64 {
        let mut s = slow_return_run(seed, 1_200, 0, None);
        let (mut last, mut since, mut worst) = (Frame(0), 5_000, 0);
        for t in (100..=30_000).step_by(100) {
            s.run(t);
            let f = s.final_frame(0);
            if t > 5_000 {
                if f > last {
                    since = t;
                } else {
                    worst = worst.max(t - since);
                }
            }
            last = f;
        }
        eprintln!(
            "seed {seed}: final stood still at most {worst} ms; A paced on {} frames, {} of them near the cap",
            s.paced_frames[0], s.near_cap_frames[0]
        );
        assert!(worst <= 8_500, "seed {seed}: the final frame stood still {worst} ms");
        assert_eq!(s.paced_frames[0], s.near_cap_frames[0], "seed {seed}: A paced by its backlog on a flowing link");
        near += s.near_cap_frames[0];
    }
    assert!(near > 0, "A never came near the pending cap: the test doesn't bite");
}

// Membership change by epochs, Admit (agreement.md 8; pe-seams gov; coordinator m6cdd383ce41c, mbd24467040cd).

/// Member `i` signs an `Admit` of member `j`'s key with `role`.
fn admit(s: &mut Sim, i: usize, j: usize, role: u32) -> EntryRef {
    let present = s.clock.frame_at(HostTime(s.now)).unwrap();
    let payload = crate::node::Admit::new(s.keys.key(j), role).encode();
    let e = s.builders[i].next_unsigned(Class::Protocol, present, payload);
    let e = s.nodes[i].submit(e).expect("own Admit refused");
    s.submitted[i].push(e.id());
    s.submitted_at[i].push(present);
    e.id()
}

impl Sim {
    /// Run until member `i` is in `epoch`, at most `within_ms` more.
    fn run_to_epoch(&mut self, i: usize, epoch: u64, within_ms: i64) {
        let end = self.now / MS + within_ms;
        while self.nodes[i].epoch() < epoch && self.now / MS < end {
            let next = self.now / MS + 10;
            self.run(next);
        }
        assert_eq!(self.nodes[i].epoch(), epoch, "member {i} didn't reach epoch {epoch} within {within_ms} ms");
    }

    /// Start joiner `j` from inviter `i`'s start record, its invite pinning the epoch before `i`'s (`pinned_members`
    /// its members). It makes `per_frame` entries from then on.
    fn join(&mut self, j: usize, i: usize, pinned_members: &[usize], per_frame: Option<(u32, u32)>) {
        let start = self.nodes[i].epoch_start().expect("the inviter's start").to_vec();
        let pinned = self.sorted(pinned_members);
        let node = Node::join(
            self.base.clone(),
            &start,
            self.nodes[i].epoch() - 1,
            &pinned,
            Box::new(self.keys.signer(j)),
            Box::new(self.keys.clone()),
            HostTime(self.now),
        )
        .expect("a valid start refused");
        self.nodes[j] = node;
        self.builders[j] = EntryBuilder::new(&self.keys, j);
        self.absent[j] = false;
        self.per_frame[j] = per_frame;
    }

    /// The final logs of `who` agree frame by frame from `from` up to their lowest final frame; how many entries.
    fn assert_same_final_log_among(&self, who: &[usize], from: Frame) -> usize {
        let low = who.iter().map(|&i| self.final_frame(i)).min().unwrap();
        let ids =
            |i: usize, f: u64| self.nodes[i].entries_at(Frame(f)).map(|v| v.iter().map(|e| e.id()).collect::<Vec<_>>());
        let mut count = 0;
        for f in from.0..low.0 {
            let a = ids(who[0], f);
            for &i in &who[1..] {
                assert_eq!(a, ids(i, f), "members {} and {i} differ at frame {f}", who[0]);
            }
            count += a.map_or(0, |v| v.len());
        }
        count
    }

    fn faults(&self, i: usize) -> usize {
        self.events[i].iter().filter(|e| matches!(e, AgreeEvent::Fault(_))).count()
    }

    fn sorted(&self, who: &[usize]) -> Vec<MemberKey> {
        let mut v: Vec<MemberKey> = who.iter().map(|&i| self.keys.key(i)).collect();
        v.sort();
        v
    }

    /// Whether `id` is in member `i`'s final log below frame `below`.
    fn is_final(&self, i: usize, id: EntryRef, below: Frame) -> bool {
        (0..below.0).any(|f| self.nodes[i].entries_at(Frame(f)).is_some_and(|v| v.iter().any(|e| e.id() == id)))
    }
}

#[test]
fn a_lone_member_admits_a_joiner_and_the_two_finalize() {
    // M1's join from a cut (model v1's join-1to2): A plays alone and admits B at 2 s; E is final at A at once (q = 1),
    // and B starts from A's start record in epoch 1, with E as its genesis. Both then finalize together (q = 2), and
    // B's certified root at E is the root of A's E.
    for seed in [401u64, 402] {
        let mut s = Sim::with_joiners(2, 1, seed);
        s.clock = Clock(120);
        s.per_frame = vec![Some((2, 0)), None];
        s.run(2_000);
        let ad = admit(&mut s, 0, 1, 3);
        s.run_to_epoch(0, 1, 3_000);
        assert_eq!(s.nodes[0].members(), s.sorted(&[0, 1]), "seed {seed}: A's epoch 1");
        // pe-app's inviter reads both through S3 v1.11; the trait's defaults (0, None) would leave every join unstarted.
        let a: &dyn Agreement = &s.nodes[0];
        assert_eq!(a.epoch(), 1, "seed {seed}: Agreement::epoch doesn't reach Node::epoch");
        assert_eq!(
            a.epoch_start(),
            s.nodes[0].epoch_start().map(<[u8]>::to_vec),
            "seed {seed}: Agreement::epoch_start doesn't reach Node::epoch_start"
        );
        assert!(a.epoch_start().is_some(), "seed {seed}: no start record at A");
        let e_cut = s.nodes[0].core().genesis().cut.clone();
        let e_frame = e_cut.at.frame;
        let next = s.now / MS + 200;
        s.run(next);
        s.join(1, 0, &[0], Some((2, 0)));
        assert_eq!(s.nodes[1].epoch(), 1);
        assert_eq!(s.nodes[1].start_root(), Some(root(&e_cut)), "seed {seed}: B's certified root at E");
        s.run(12_000);
        for i in 0..2 {
            assert_eq!(s.nodes[i].members(), s.sorted(&[0, 1]), "seed {seed}: member {i}");
            assert_eq!(s.faults(i), 0, "seed {seed}: faults at {i}: {:?}", s.events[i]);
        }
        let n = s.assert_same_final_log_among(&[0, 1], e_frame);
        let fin = [s.final_frame(0).0 as f64 / 120.0, s.final_frame(1).0 as f64 / 120.0];
        let b_final = s.submitted[1].iter().filter(|id| s.is_final(0, **id, s.final_frame(0))).count();
        eprintln!(
            "seed {seed}: E at {:.2} s; final at {fin:?} of 12 s; {n} entries agreed from E; {b_final} of B's final",
            e_frame.0 as f64 / 120.0
        );
        assert!(fin.iter().all(|f| *f >= 11.0), "seed {seed}: the pair didn't finalize after the join: {fin:?}");
        assert!(b_final > 1_000, "seed {seed}: only {b_final} of B's entries final at A");
        assert!(s.is_final(0, ad, e_frame), "seed {seed}: the Admit isn't in A's final log below E");
    }
}

#[test]
fn an_admin_admits_an_admin_leaves_and_the_new_admin_admits_a_third() {
    // Coordinator mbd24467040cd: governance is per epoch, so a line whose opener left can still admit. A admits B as
    // an admin, A leaves (B alone, epoch 2), and B admits C (epoch 3: B and C).
    let mut s = Sim::with_joiners(3, 1, 411);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)), None, None];
    s.run(1_000);
    admit(&mut s, 0, 1, crate::node::ROLE_ADMIN | 3);
    s.run_to_epoch(0, 1, 3_000);
    s.join(1, 0, &[0], Some((2, 0)));
    s.run(4_000);
    assert!(s.nodes[1].gov().is_admin(&s.keys.key(1)), "B wasn't admitted as an admin");
    leave(&mut s, 0);
    s.run_to_epoch(1, 2, 5_000);
    let mut waited = 0;
    while !Agreement::leave_settled(&s.nodes[0]) && waited < 5_000 {
        let next = s.now / MS + 10;
        s.run(next);
        waited += 10;
    }
    s.cut_off = Some(0);
    assert_eq!(s.nodes[1].members(), s.sorted(&[1]), "A didn't leave");
    assert_eq!(s.nodes[1].gov().admins().collect::<Vec<_>>(), vec![s.keys.key(1)]);
    let next = s.now / MS + 1_000;
    s.run(next);
    admit(&mut s, 1, 2, 3);
    s.run_to_epoch(1, 3, 3_000);
    s.join(2, 1, &[1], Some((2, 0)));
    let e_frame = s.nodes[1].core().genesis().cut.at.frame;
    let end = s.now / MS + 8_000;
    s.run(end);
    for i in 1..3 {
        assert_eq!(s.nodes[i].members(), s.sorted(&[1, 2]), "member {i}");
        assert_eq!(s.faults(i), 0, "faults at {i}: {:?}", s.events[i]);
    }
    let n = s.assert_same_final_log_among(&[1, 2], e_frame);
    let fin = s.final_frame(2).0 as f64 / 120.0;
    eprintln!("B and C agree on {n} entries from E3; C final at {fin:.2} s of {:.2}", s.now as f64 / 1e9);
    assert!(fin >= s.now as f64 / 1e9 - 1.0, "B and C didn't finalize");
}

#[test]
fn an_admit_by_a_non_admin_is_not_authorized_and_a_second_admit_changes_nothing() {
    // The governance fold's two voids: B, admitted without ADMIN, admits C: void as NotAuthorized at both members; A
    // admits B again: void as NoChange. Neither starts an epoch.
    let mut s = Sim::with_joiners(3, 1, 421);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)), None, None];
    s.run(1_000);
    admit(&mut s, 0, 1, 3);
    s.run_to_epoch(0, 1, 3_000);
    s.join(1, 0, &[0], Some((2, 0)));
    s.run(3_000);
    let by_b = admit(&mut s, 1, 2, 3);
    let again = admit(&mut s, 0, 1, 3);
    s.run(6_000);
    for i in 0..2 {
        let v: Vec<_> = s.voids(i).into_iter().filter(|v| v.0 == by_b || v.0 == again).map(|v| (v.0, v.1)).collect();
        eprintln!("member {i}: {v:?}");
        assert!(v.contains(&(by_b, VoidReason::NotAuthorized)), "member {i}: B's Admit: {v:?}");
        assert!(v.contains(&(again, VoidReason::NoChange)), "member {i}: the second Admit of B: {v:?}");
        assert_eq!(s.nodes[i].epoch(), 1, "member {i} changed epochs");
        assert_eq!(s.faults(i), 0, "faults at {i}");
    }
}

#[test]
fn a_start_is_refused_for_another_epoch_a_forged_vote_or_a_key_not_admitted() {
    use crate::epoch::StartError;
    let mut s = Sim::with_joiners(3, 1, 431);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)), None, None];
    s.run(1_000);
    admit(&mut s, 0, 1, 3);
    s.run_to_epoch(0, 1, 3_000);
    let bytes = s.nodes[0].epoch_start().unwrap().to_vec();
    let a = [s.keys.key(0)];
    let join = |bytes: &[u8], pinned: u64, who: usize| {
        Node::join(
            s.base.clone(),
            bytes,
            pinned,
            &a,
            Box::new(s.keys.signer(who)),
            Box::new(s.keys.clone()),
            HostTime(0),
        )
        .map(|_| ())
    };
    assert_eq!(join(&bytes, 0, 1), Ok(()));
    assert_eq!(join(&bytes, 1, 1), Err(StartError::OtherEpoch { start: 0, pinned: 1 }));
    assert_eq!(join(&bytes, 0, 2), Err(StartError::NotAdmitted));
    assert_eq!(join(&bytes[..bytes.len() - 1], 0, 1), Err(StartError::Malformed));
    let mut start = crate::codec::decode_start(&bytes).unwrap();
    start.chain[0].1.votes[0].1.0[0] ^= 1;
    assert_eq!(join(&crate::codec::encode_start(&start), 0, 1), Err(StartError::Certificate("a bad vote signature")));
    let mut start = crate::codec::decode_start(&bytes).unwrap();
    start.chain.truncate(1);
    assert!(matches!(join(&crate::codec::encode_start(&start), 0, 1), Err(StartError::Certificate(_))));
}

#[test]
fn a_leaver_admitting_its_successor_in_the_same_cut_waits_until_it_has_started() {
    // @design-reviewer's Low 1 on b84295a2: A alone admits B and leaves in one cut, so epoch 1 is B alone. A must not
    // quit (leave_settled) until B has started from E and told it so; B, alone (q = 1), then finalizes on its own.
    let mut s = Sim::with_joiners(2, 1, 441);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)), None];
    s.run(1_000);
    admit(&mut s, 0, 1, crate::node::ROLE_ADMIN | 3);
    leave(&mut s, 0);
    s.run_to_epoch(0, 1, 3_000);
    assert_eq!(s.nodes[0].members(), s.sorted(&[1]), "A's epoch 1 should be B alone");
    let next = s.now / MS + 2_000;
    s.run(next);
    assert!(!Agreement::leave_settled(&s.nodes[0]), "A settled before B started");
    s.join(1, 0, &[0], Some((2, 0)));
    let mut waited = 0;
    while !Agreement::leave_settled(&s.nodes[0]) && waited < 3_000 {
        let next = s.now / MS + 10;
        s.run(next);
        waited += 10;
    }
    eprintln!("A settled {waited} ms after B started");
    assert!(Agreement::leave_settled(&s.nodes[0]), "A never settled after B started");
    s.cut_off = Some(0);
    let e_frame = s.nodes[1].core().genesis().cut.at.frame;
    let end = s.now / MS + 5_000;
    s.run(end);
    let fin = s.final_frame(1).0 as f64 / 120.0;
    let b_final = s.submitted[1].iter().filter(|id| s.is_final(1, **id, s.final_frame(1))).count();
    eprintln!(
        "B alone: final at {fin:.2} s of {:.2}; {b_final} of its entries final; E at {e_frame:?}",
        s.now as f64 / 1e9
    );
    assert!(fin >= s.now as f64 / 1e9 - 1.0, "B alone didn't finalize");
    assert!(b_final > 500, "B's entries didn't become final");
    assert_eq!(s.faults(1), 0, "faults at B: {:?}", s.events[1]);
}

// The epoch-certificate fetch (agreement.md 8; @item:primordial-engine/pe-agree-a-member-that-missed-its-epoch-s-reconf).

/// Run until member `i`'s final frame passes `frame`, at most `within_ms` more; false if it didn't.
fn run_until_final(s: &mut Sim, i: usize, frame: Frame, within_ms: i64) -> bool {
    let end = s.now / MS + within_ms;
    while s.final_frame(i) <= frame && s.now / MS < end {
        let next = s.now / MS + 10;
        s.run(next);
    }
    s.final_frame(i) > frame
}

#[test]
fn a_member_that_missed_its_epochs_end_past_the_old_cores_fetches_the_certificate() {
    // @design-reviewer's M3 on e3597563: 3 -> 2. Member 2 leaves at 3 s; member 1 is cut off from then until 40 s,
    // past the 30 s the others keep the old core. Members 0 and 2 finalize E (q = 2 of 3); 0 switches to epoch 1
    // {0, 1} (q = 2), where nothing is final without 1, and 2 quits. Back at 40 s, 1 sees 0's epoch-1 messages, asks
    // 0 for the certificate that ended epoch 0, commits E, fetches what it lacks, switches, and the two finalize again.
    for seed in [501, 502] {
        let mut s = Sim::new(3, seed);
        s.clock = Clock(120);
        s.per_frame = vec![Some((2, 0)); 3];
        s.run(3_000);
        s.partition = Some((1, 3_000, 40_000));
        leave(&mut s, 2);
        s.run_to_epoch(0, 1, 5_000);
        let switched = s.now / MS;
        s.run(switched + 1_000);
        s.absent[2] = true;
        s.run(40_000);
        assert_eq!(s.nodes[1].epoch(), 0, "seed {seed}: member 1 switched while cut off");
        let stood = s.final_frame(0);
        s.run_to_epoch(1, 1, 10_000);
        let reached = s.now / MS - 40_000;
        assert_eq!(s.nodes[1].members(), s.sorted(&[0, 1]), "seed {seed}: member 1's epoch 1");
        let again = run_until_final(&mut s, 0, stood, 10_000);
        let resumed = s.now / MS - 40_000;
        s.run(55_000);
        let n = s.assert_same_final_log_among(&[0, 1], Frame(0));
        let fin = s.final_frame(0).0 as f64 / 120.0;
        eprintln!(
            "seed {seed}: 0 switched at {switched} ms; final stood at {:.2} s; 1 in epoch 1 {reached} ms after the \
             partition, 0 final again after {resumed} ms; final at {fin:.2} s of 55; {n} entries agreed",
            stood.0 as f64 / 120.0
        );
        assert!(again, "seed {seed}: nothing became final again");
        assert!(fin >= 54.0, "seed {seed}: the two didn't go on: final at {fin:.2} s");
        for i in 0..2 {
            assert_eq!(s.faults(i), 0, "seed {seed}: faults at {i}");
        }
    }
}

#[test]
fn a_member_that_missed_two_epochs_fetches_one_certificate_for_each() {
    // 4 -> 3 -> 2 with member 1 cut off throughout: 3 leaves (epoch 1 is {0, 1, 2}, q = 2), then 2 leaves (epoch 2 is
    // {0, 1}). Back after both old cores are gone, 1 fetches epoch 0's certificate from 0, switches to epoch 1, then
    // fetches epoch 1's, and switches to epoch 2.
    for seed in [511, 512] {
        let mut s = Sim::new(4, seed);
        s.clock = Clock(120);
        s.per_frame = vec![Some((2, 0)); 4];
        s.run(3_000);
        s.partition = Some((1, 3_000, 45_000));
        leave(&mut s, 3);
        s.run_to_epoch(0, 1, 5_000);
        let t = s.now / MS;
        s.run(t + 1_000);
        s.absent[3] = true;
        leave(&mut s, 2);
        s.run_to_epoch(0, 2, 5_000);
        let t = s.now / MS;
        s.run(t + 1_000);
        s.absent[2] = true;
        s.run(45_000);
        assert_eq!(s.nodes[1].epoch(), 0, "seed {seed}: member 1 switched while cut off");
        let stood = s.final_frame(0);
        s.run_to_epoch(1, 2, 20_000);
        let reached = s.now / MS - 45_000;
        assert_eq!(s.nodes[1].members(), s.sorted(&[0, 1]), "seed {seed}: member 1's epoch 2");
        let again = run_until_final(&mut s, 0, stood, 10_000);
        s.run(60_000);
        let n = s.assert_same_final_log_among(&[0, 1], Frame(0));
        let fin = s.final_frame(0).0 as f64 / 120.0;
        eprintln!("seed {seed}: 1 in epoch 2 {reached} ms after the partition; final at {fin:.2} s of 60; {n} entries");
        assert!(again, "seed {seed}: nothing became final again");
        assert!(fin >= 59.0, "seed {seed}: the two didn't go on: final at {fin:.2} s");
        for i in 0..2 {
            assert_eq!(s.faults(i), 0, "seed {seed}: faults at {i}");
        }
    }
}

#[test]
fn a_forged_epoch_certificate_is_refused_and_the_genuine_one_taken() {
    // As above (3 -> 2, member 1 cut off past the old cores), but 1 is handed certificates directly: a forged vote, a
    // forged proposal, one with no two-chain, and one for another epoch, then the genuine one.
    use crate::codec::{Wire, encode};
    let mut s = Sim::new(3, 521);
    s.clock = Clock(120);
    s.per_frame = vec![Some((2, 0)); 3];
    s.run(3_000);
    s.partition = Some((1, 3_000, 40_000));
    leave(&mut s, 2);
    s.run_to_epoch(0, 1, 5_000);
    let t = s.now / MS;
    s.run(t + 1_000);
    s.absent[2] = true;
    s.run(36_000);
    let (proposals, top) = s.nodes[0].retired_cert(0).expect("epoch 0's certificate kept");
    let a = s.keys.key(0);
    let now = HostTime(s.now);
    let give = |s: &mut Sim, w: Wire| s.nodes[1].receive(a, Lane::Protocol, &encode(&w), now).map_err(|f| f.detail);
    let cert =
        |proposals: Vec<crate::core::Proposal>, top: crate::core::Qc| Wire::EpochCert { epoch: 0, proposals, top };

    let mut forged = top.clone();
    forged.votes[0].1.0[0] ^= 1;
    let r = give(&mut s, cert(proposals.clone(), forged));
    assert_eq!(r, Err("an epoch certificate refused: a bad vote signature".into()));

    let mut ps = proposals.clone();
    ps[0].sig.0[0] ^= 1;
    let r = give(&mut s, cert(ps, top.clone()));
    assert!(r.as_ref().is_err_and(|d| d.contains("BadSignature")), "a forged proposal: {r:?}");

    let r = give(&mut s, cert(proposals[..1].to_vec(), proposals[1].parent_qc.clone()));
    assert!(r.as_ref().is_err_and(|d| d.contains("two-chain")), "only E: {r:?}");

    let r = give(&mut s, Wire::EpochCert { epoch: 7, proposals: proposals.clone(), top: top.clone() });
    assert_eq!(r, Ok(()), "another epoch's is dropped without a fault");
    assert_eq!(s.nodes[1].epoch(), 0);

    assert_eq!(give(&mut s, cert(proposals, top)), Ok(()));
    s.run_to_epoch(1, 1, 10_000);
    assert_eq!(s.nodes[1].members(), s.sorted(&[0, 1]));
}

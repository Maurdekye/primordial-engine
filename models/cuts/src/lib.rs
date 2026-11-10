//! Model v0 of agreed cuts (WS-model, re-founding plan 4.4). See README.md for the rules, the properties, their
//! controls and the bounds checked.
//!
//! The rule modelled: members take turns (a seeded rotation over views) to propose a cut, a block naming a frame and,
//! for each author, the last entry of its chain the cut covers. A proposal carries its parent's quorum certificate;
//! after a view that timed out it carries a timeout certificate as well, and must extend a block at least as high as
//! the highest certificate in it (the lock). Members vote once per view. A cut is final when its child, proposed in the
//! very next view, gets a quorum (the two-chain rule of the chained-HotStuff family, as in Jolteon / DiemBFT v4).
//! Entries a final cut covers take effect in chain order; one stamped strictly below a stamp earlier in its author's
//! chain (an equal stamp is valid, ordered by seq), or before the previous cut's frame, is void; an entry stamped before the cut's frame and not covered is void.

use std::collections::BTreeSet;
use std::fmt;

use stateright::{Model, Property};

pub const NMAX: usize = 4;
pub const HMAX: usize = 3;
pub const NONE: u8 = u8::MAX;
/// Frames per cut: the cut at height h covers stamps below `FRAMES * h`.
pub const FRAMES: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    None,
    /// Crashed from the start: sends nothing.
    Silent,
    /// Runs the cut protocol honestly but lies with its own entries: forks a sequence number, back-dates, forges an
    /// entry in another author's name, replays one signed for another world, and sends any of them to anyone, or
    /// withholds them.
    Author,
    /// Author faults, plus any vote, timeout or proposal: it equivocates freely (n >= 4 only, coordinator 2026-11-10).
    Byzantine,
}

/// Controls: each removes one rule, and the checker must find a counterexample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutation {
    /// The d1 defect: a member builds and votes for a cut without validating each author's chain in order. It takes an
    /// entry past a hole at face value.
    VoteWithoutChainValidation,
    /// A voter accepts the proposer's cut without checking it.
    AcceptProposalUnchecked,
    /// The quorum is one smaller than D80 (b)'s formula.
    QuorumOneTooSmall,
    /// A voter ignores the timeout certificate's highest certificate (no lock).
    NoLockRule,
    /// A voter does not check that an entry is signed by its author for this world (own hands, world id).
    NoSignatureCheck,
    /// Nobody times out a silent or faulty proposer.
    NoTimeouts,
    /// Votes carry no state root: a member whose replica diverged confirms the quorum's cut without learning it.
    VoteWithoutRoot,
}

#[derive(Clone, Debug)]
pub struct EntryDef {
    pub name: String,
    /// The author the entry claims.
    pub author: u8,
    pub seq: u8,
    /// The entry it names as previous in its author's chain (an index into `entries`), or NONE at seq 1.
    pub prev: u8,
    pub stamp: u8,
    /// Who actually signed it, and for which world (the signed bytes bind both).
    pub signer: u8,
    pub world: u8,
}

impl EntryDef {
    pub fn signed_ok(&self) -> bool {
        self.signer == self.author && self.world == 0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Adversary {
    pub fork: bool,
    pub backdate: bool,
    pub forge: bool,
    pub replay: bool,
    /// The faulty member may send its entries to anyone at any time. Off, they reach others only inside its
    /// proposals (the n = 4 quorum checks, where entries only tell its equivocating blocks apart).
    pub send: bool,
}

impl Adversary {
    pub const ALL: Adversary = Adversary { fork: true, backdate: true, forge: true, replay: true, send: true };
}

#[derive(Clone, Debug)]
pub struct Cuts {
    pub n: u8,
    /// The faulty member is always member 0; `offset` decides which views it leads.
    pub fault: Fault,
    /// Views 1..=views are played; nobody proposes or times out after the last.
    pub views: u8,
    /// Heights whose cuts carry entries (1 or 2). Blocks go one higher, since a cut is final only through a child.
    pub heights: u8,
    /// How many honest members author one entry per content height.
    pub honest_authors: u8,
    pub offset: u8,
    /// Liveness profile: timeouts fire only once every honest step is done (the network has settled), messages are
    /// never lost, and every end state must have a final cut at every honest member.
    pub liveness: bool,
    pub mutation: Option<Mutation>,
    pub send_entries: bool,
    /// List only actions that change the state (for Stateright's random walks, whose path reconstruction breaks
    /// on actions that do nothing).
    pub productive_only: bool,
    /// A fault-injected honest member whose replica computes a wrong root (WS-diverge 3.2): it runs the protocol
    /// honestly, but its votes carry the wrong root.
    pub corrupt: Option<u8>,
    pub entries: Vec<EntryDef>,
}

impl Cuts {
    pub fn new(n: u8, fault: Fault, views: u8, heights: u8, honest_authors: u8, adv: Adversary, offset: u8) -> Cuts {
        assert!((3..=NMAX as u8).contains(&n) && (1..HMAX as u8).contains(&heights) && honest_authors < n && (honest_authors > 0 || fault != Fault::None));
        let mut entries: Vec<EntryDef> = Vec::new();
        for a in 1..=honest_authors {
            let mut prev = NONE;
            for h in 1..=heights {
                let i = entries.len() as u8;
                entries.push(EntryDef { name: format!("m{a}.{h}"), author: a, seq: h, prev, stamp: FRAMES * (h - 1) + 1, signer: a, world: 0 });
                prev = i;
            }
        }
        if matches!(fault, Fault::Author | Fault::Byzantine) {
            let s1 = entries.len() as u8;
            entries.push(EntryDef { name: "f.1a".into(), author: 0, seq: 1, prev: NONE, stamp: 2, signer: 0, world: 0 });
            if adv.fork {
                entries.push(EntryDef { name: "f.1b".into(), author: 0, seq: 1, prev: NONE, stamp: 2, signer: 0, world: 0 });
            }
            if adv.backdate {
                // Stamped before f.1a, which comes before it in the chain: void by the horizon rule, but valid alone.
                entries.push(EntryDef { name: "f.2<".into(), author: 0, seq: 2, prev: s1, stamp: 1, signer: 0, world: 0 });
            }
            if adv.forge {
                entries.push(EntryDef { name: "forge(m1.1)".into(), author: 1, seq: 1, prev: NONE, stamp: 1, signer: 0, world: 0 });
            }
            if adv.replay {
                entries.push(EntryDef { name: "f.1@w1".into(), author: 0, seq: 1, prev: NONE, stamp: 1, signer: 0, world: 1 });
            }
        }
        assert!(entries.len() <= 16);
        Cuts { n, fault, views, heights, honest_authors, offset, liveness: false, mutation: None, send_entries: adv.send, productive_only: false, corrupt: None, entries }
    }

    pub fn live(mut self) -> Self {
        self.liveness = true;
        self
    }

    pub fn corrupt(mut self, m: u8) -> Self {
        self.corrupt = Some(m);
        self
    }

    /// The root member m computes for any block: the right one (true), unless its replica is fault-injected. In v0 the
    /// root is abstract: one bit, right or wrong.
    pub fn own_root(&self, m: u8) -> bool {
        self.corrupt != Some(m)
    }

    fn roots(&self) -> bool {
        self.mutation != Some(Mutation::VoteWithoutRoot)
    }

    pub fn mutate(mut self, m: Mutation) -> Self {
        self.mutation = Some(m);
        self
    }

    pub fn faulty(&self) -> u8 {
        0
    }

    /// D80 (b), coordinator ruling 2026-11-10: t = floor((n-1)/3), q = floor((n+t)/2) + 1.
    pub fn quorum(&self) -> usize {
        let n = self.n as usize;
        let t = (n - 1) / 3;
        let q = (n + t) / 2 + 1;
        if self.mutation == Some(Mutation::QuorumOneTooSmall) { q - 1 } else { q }
    }

    pub fn leader(&self, view: u8) -> u8 {
        (view - 1 + self.offset) % self.n
    }

    fn hcap(&self) -> u8 {
        self.heights + 1
    }

    fn byz(&self) -> bool {
        self.fault == Fault::Byzantine
    }

    /// Runs the cut protocol (honest members, and an author-faulty member).
    pub fn protocol(&self, m: u8) -> bool {
        m < self.n && !(m == 0 && matches!(self.fault, Fault::Silent | Fault::Byzantine))
    }

    /// Whom the properties are about.
    pub fn honest(&self, m: u8) -> bool {
        m < self.n && (m != 0 || self.fault == Fault::None)
    }

    fn has(&self, m: u16, e: u8) -> bool {
        m & (1 << e) != 0
    }

    fn sig_checked(&self, e: u8) -> bool {
        self.mutation == Some(Mutation::NoSignatureCheck) || self.entries[e as usize].signed_ok()
    }

    /// Votes for `b` with root `r`: the honest ones sent, and the Byzantine member's, which counts for anything (it
    /// equivocates). Without roots in votes (the control), every vote for `b` counts.
    fn votes(&self, s: &State, b: &Block, r: bool) -> usize {
        let honest = s.net.iter().filter(|m| matches!(m, Msg::Vote { block, root, .. } if block == b && (*root == r || !self.roots()))).count();
        honest + usize::from(self.byz())
    }

    pub fn certified(&self, s: &State, c: &Cert) -> bool {
        c.block.h == 0 || self.votes(s, &c.block, c.root) >= self.quorum()
    }

    fn certified_blocks(&self, s: &State) -> Vec<Cert> {
        let mut out = vec![Cert::GENESIS];
        for m in &s.net {
            if let Msg::Vote { block, root, .. } = m {
                let c = Cert { block: *block, root: *root || !self.roots() };
                if !out.contains(&c) && self.certified(s, &c) {
                    out.push(c);
                }
            }
        }
        out
    }

    fn timeouts(&self, s: &State, view: u8) -> Vec<Cert> {
        s.net.iter().filter_map(|m| match m { Msg::Timeout { view: v, high, .. } if *v == view => Some(*high), _ => None }).collect()
    }

    /// The highest certificates a valid timeout certificate for `view` can show: the maximum over each quorum-sized set
    /// of timeouts (the Byzantine member's own timeout may show genesis). Empty if no certificate can be formed.
    fn tc_maxima(&self, s: &State, view: u8) -> Vec<Cert> {
        let mut pool = self.timeouts(s, view);
        if self.byz() {
            pool.push(Cert::GENESIS);
        }
        let q = self.quorum();
        let mut out: Vec<Cert> = Vec::new();
        if pool.len() < q {
            return out;
        }
        for mask in 0u32..(1 << pool.len()) {
            if mask.count_ones() as usize != q {
                continue;
            }
            let best = (0..pool.len()).filter(|i| mask & (1 << i) != 0).map(|i| pool[i]).max_by_key(|c| c.block.view()).unwrap();
            if !out.contains(&best) {
                out.push(best);
            }
        }
        out
    }

    /// The chain segment a cut adds for author `a`: from `to` back to (excluding) `from`, in chain order, by the prev
    /// links the entries name. None if the walk doesn't reach `from`.
    fn segment(&self, from: u8, to: u8) -> Option<Vec<u8>> {
        let mut seg = Vec::new();
        let mut x = to;
        while x != from {
            if x == NONE || seg.len() > 8 {
                return None;
            }
            seg.push(x);
            x = self.entries[x as usize].prev;
        }
        seg.reverse();
        Some(seg)
    }

    /// Does a voter that knows the entries `known` accept `block`'s cut as extending its parent's?
    fn cut_ok(&self, block: &Block, known: u16) -> bool {
        let parent = block.parent();
        let (pc, nc) = (parent.cut(), block.cut());
        let h = block.h;
        if self.mutation == Some(Mutation::AcceptProposalUnchecked) {
            return true;
        }
        if h > self.heights {
            return nc == pc;
        }
        let hi = FRAMES * h;
        for a in 0..self.n as usize {
            if nc[a] == pc[a] {
                continue;
            }
            if self.mutation == Some(Mutation::VoteWithoutChainValidation) {
                // d1: the named entry is checked alone; the chain up to it is not.
                let e = &self.entries[nc[a] as usize];
                if !(self.has(known, nc[a]) && e.author as usize == a && self.sig_checked(nc[a]) && e.stamp < hi) {
                    return false;
                }
                continue;
            }
            let Some(seg) = self.segment(pc[a], nc[a]) else { return false };
            for &x in &seg {
                let e = &self.entries[x as usize];
                if !(self.has(known, x) && e.author as usize == a && self.sig_checked(x) && e.stamp < hi) {
                    return false;
                }
            }
        }
        true
    }

    /// An honest proposer's cut at the child of `parent`: for each author, the longest linked, signed chain it holds,
    /// stamped below the frame. Returns the cut and the entries the proposal carries.
    fn build_cut(&self, parent: &Block, held: u16) -> ([u8; NMAX], u16) {
        let mut cut = parent.cut();
        let h = parent.h + 1;
        let mut carried = 0u16;
        if h > self.heights {
            return (cut, 0);
        }
        let hi = FRAMES * h;
        for a in 0..self.n as usize {
            loop {
                let next = (0..self.entries.len() as u8).find(|&x| {
                    let e = &self.entries[x as usize];
                    self.has(held, x) && e.author as usize == a && e.prev == cut[a] && self.sig_checked(x) && e.stamp < hi
                });
                match next {
                    Some(x) => {
                        cut[a] = x;
                        carried |= 1 << x;
                    }
                    None => break,
                }
            }
            if self.mutation == Some(Mutation::VoteWithoutChainValidation) {
                // d1: past a hole, take the furthest entry held anyway.
                let seq0 = if cut[a] == NONE { 0 } else { self.entries[cut[a] as usize].seq };
                let far = (0..self.entries.len() as u8)
                    .filter(|&x| {
                        let e = &self.entries[x as usize];
                        self.has(held, x) && e.author as usize == a && e.seq > seq0 && self.sig_checked(x) && e.stamp < hi
                    })
                    .max_by_key(|&x| self.entries[x as usize].seq);
                if let Some(x) = far {
                    cut[a] = x;
                    carried |= 1 << x;
                }
            }
        }
        (cut, carried)
    }

    /// The verdicts a member with entries `known` and final block `fin` reaches: (effective, void). With `known` = every
    /// entry this is the ground truth. A covered entry whose earlier chain is not all known is pending (in neither set),
    /// except under the d1 mutation, which judges it alone.
    pub fn verdicts(&self, fin: &Block, known: u16) -> (u16, u16) {
        let (mut eff, mut void) = (0u16, 0u16);
        let top = fin.h.min(self.heights);
        let mut covered = 0u16;
        let d1 = self.mutation == Some(Mutation::VoteWithoutChainValidation);
        for k in 1..=top {
            let lo = FRAMES * (k - 1);
            let anc = fin.ancestor(k);
            let (pc, nc) = (anc.parent().cut(), anc.cut());
            for a in 0..self.n as usize {
                if nc[a] == pc[a] {
                    continue;
                }
                let seg = self.segment(pc[a], nc[a]).unwrap_or_else(|| vec![nc[a]]);
                for &x in &seg {
                    covered |= 1 << x;
                    if !self.has(known, x) {
                        continue;
                    }
                    let e = &self.entries[x as usize];
                    // The entries before x in its author's chain, by its prev links.
                    let mut earlier = Vec::new();
                    let mut y = e.prev;
                    while y != NONE {
                        earlier.push(y);
                        y = self.entries[y as usize].prev;
                    }
                    let all_known = earlier.iter().all(|&y| self.has(known, y));
                    let ok = if all_known {
                        e.stamp >= lo && earlier.iter().all(|&y| self.entries[y as usize].stamp <= e.stamp)
                    } else if d1 {
                        e.stamp >= lo
                    } else {
                        continue;
                    };
                    if ok { eff |= 1 << x } else { void |= 1 << x }
                }
            }
        }
        let frame = FRAMES * top;
        for x in 0..self.entries.len() as u8 {
            if self.has(known, x) && covered & (1 << x) == 0 && self.entries[x as usize].stamp < frame {
                void |= 1 << x;
            }
        }
        (eff, void)
    }

    fn all_entries(&self) -> u16 {
        ((1u32 << self.entries.len()) - 1) as u16
    }

    // ---- member steps ----

    /// A member learns that `x` is certified: its highest certificate, the view, and the two-chain commit.
    fn note_cert(&self, mm: &mut Member, me: u8, c: Cert) {
        let x = c.block;
        // RootSplit: a quorum's root for this block is not the one this member computed.
        if x.h > 0 && self.roots() && c.root != self.own_root(me) {
            mm.split = true;
        }
        if x.view() > mm.high.block.view() {
            mm.high = c;
        }
        if x.view() >= mm.view {
            mm.view = x.view() + 1;
        }
        if x.h >= 2 {
            let p = x.parent();
            if p.h >= 1 && x.view() == p.view() + 1 {
                if mm.committed.extends(&p) {
                } else if p.extends(&mm.committed) {
                    mm.committed = p;
                } else {
                    mm.undone = true; // a final cut that does not extend this member's final cut
                }
            }
        }
    }

    fn on_propose(&self, s: &mut State, m: u8, block: Block, proot: bool, tc: Option<Cert>, carried: u16) {
        let mut mm = s.m[m as usize];
        let v = block.view();
        let p = block.parent();
        mm.held |= carried;
        self.note_cert(&mut mm, m, Cert { block: p, root: proot });
        if let Some(t) = tc {
            self.note_cert(&mut mm, m, t);
            if v > mm.view {
                mm.view = v;
            }
        }
        let lock_ok = p.view() + 1 == v || tc.is_some_and(|t| p.view() >= t.block.view()) || self.mutation == Some(Mutation::NoLockRule);
        if v == mm.view && v > mm.voted && v > mm.timed_out && v <= self.views && lock_ok && self.cut_ok(&block, mm.held) {
            mm.voted = v;
            s.net.insert(Msg::Vote { voter: m, block, root: self.own_root(m) });
        }
        s.m[m as usize] = mm;
    }

    /// No honest member can make a step, timeouts included: whatever the faulty member does or withholds, the honest
    /// side is done. Liveness is judged here, so a faulty member that stays silent can't hide a stall.
    pub fn honest_stuck(&self, s: &State) -> bool {
        let mut acts = Vec::new();
        let timeouts = self.mutation != Some(Mutation::NoTimeouts) && (!self.liveness || self.quiescent(s));
        self.honest_actions(s, &mut acts, timeouts);
        acts.into_iter().all(|a| self.next_state(s, a).is_none())
    }

    fn quiescent(&self, s: &State) -> bool {
        let mut acts = Vec::new();
        self.honest_actions(s, &mut acts, false);
        acts.into_iter().all(|a| self.next_state(s, a).is_none())
    }

    fn honest_actions(&self, s: &State, acts: &mut Vec<Action>, with_timeouts: bool) {
        let certified = self.certified_blocks(s);
        for m in 0..self.n {
            if !self.protocol(m) {
                continue;
            }
            for msg in &s.net {
                match msg {
                    Msg::Entry { to, .. } if *to == m => acts.push(Action::Deliver(m, *msg)),
                    Msg::Propose { .. } => acts.push(Action::Deliver(m, *msg)),
                    _ => {}
                }
            }
            for c in &certified {
                if c.block.h > 0 {
                    acts.push(Action::Observe(m, *c));
                }
            }
            let mm = &s.m[m as usize];
            if mm.view <= self.views && !self.tc_maxima(s, mm.view).is_empty() {
                let mut opts = vec![mm.high];
                for t in self.timeouts(s, mm.view) {
                    if !opts.contains(&t) {
                        opts.push(t);
                    }
                }
                for o in opts {
                    acts.push(Action::AdvanceTc(m, o));
                }
            }
            acts.push(Action::Propose(m));
            if with_timeouts {
                acts.push(Action::Timeout(m));
            }
        }
    }

    fn adversary_actions(&self, s: &State, acts: &mut Vec<Action>) {
        if !matches!(self.fault, Fault::Author | Fault::Byzantine) {
            return;
        }
        for e in (0..self.entries.len() as u8).filter(|_| self.send_entries) {
            if self.entries[e as usize].signer != 0 {
                continue;
            }
            for to in 1..self.n {
                if !self.has(s.m[to as usize].held, e) {
                    acts.push(Action::AdvEntry(to, e));
                }
            }
        }
        if !self.byz() {
            return;
        }
        let certified = self.certified_blocks(s);
        for v in 1..=self.views {
            if self.leader(v) != 0 {
                continue;
            }
            let mut tcs: Vec<Option<Cert>> = vec![None];
            if v > 1 {
                tcs.extend(self.tc_maxima(s, v - 1).into_iter().map(Some));
            }
            for pc in certified.iter().filter(|c| c.block.view() < v && c.block.h < self.hcap()) {
                let parent = &pc.block;
                for cut in self.adversary_cuts(parent) {
                    let block = parent.child(v, cut);
                    let mut carried = 0u16;
                    for a in 0..self.n as usize {
                        let mut x = cut[a];
                        while x != NONE {
                            carried |= 1 << x;
                            x = self.entries[x as usize].prev;
                        }
                    }
                    for tc in &tcs {
                        for to in 1..self.n {
                            acts.push(Action::AdvPropose(to, Msg::Propose { block, proot: pc.root, tc: *tc, carried }));
                        }
                    }
                }
            }
        }
    }

    /// The Byzantine proposer's cuts at the child of `parent`: per author, keep the parent's or name any entry that
    /// claims that author (forks, back-dated, forged, replayed, out of window).
    fn adversary_cuts(&self, parent: &Block) -> Vec<[u8; NMAX]> {
        let pc = parent.cut();
        if parent.h + 1 > self.heights {
            return vec![pc];
        }
        let mut out = vec![pc];
        for a in 0..self.n as usize {
            let opts: Vec<u8> = (0..self.entries.len() as u8).filter(|&x| self.entries[x as usize].author as usize == a && x != pc[a]).collect();
            let mut next = Vec::new();
            for c in &out {
                for &x in &opts {
                    let mut c2 = *c;
                    c2[a] = x;
                    next.push(c2);
                }
            }
            out.extend(next);
        }
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Block {
    pub h: u8,
    pub views: [u8; HMAX],
    /// cuts[k][a]: the last entry of author a covered at height k + 1 (cumulative), or NONE.
    pub cuts: [[u8; NMAX]; HMAX],
}

impl Block {
    pub const GENESIS: Block = Block { h: 0, views: [0; HMAX], cuts: [[NONE; NMAX]; HMAX] };

    pub fn view(&self) -> u8 {
        if self.h == 0 { 0 } else { self.views[self.h as usize - 1] }
    }

    pub fn cut(&self) -> [u8; NMAX] {
        if self.h == 0 { [NONE; NMAX] } else { self.cuts[self.h as usize - 1] }
    }

    pub fn parent(&self) -> Block {
        let mut b = *self;
        if b.h > 0 {
            let k = b.h as usize - 1;
            b.views[k] = 0;
            b.cuts[k] = [NONE; NMAX];
            b.h -= 1;
        }
        b
    }

    pub fn child(&self, view: u8, cut: [u8; NMAX]) -> Block {
        let mut b = *self;
        let k = b.h as usize;
        b.views[k] = view;
        b.cuts[k] = cut;
        b.h += 1;
        b
    }

    pub fn ancestor(&self, h: u8) -> Block {
        let mut b = *self;
        while b.h > h {
            b = b.parent();
        }
        b
    }

    /// Is `a` this block or one of its ancestors?
    pub fn extends(&self, a: &Block) -> bool {
        a.h <= self.h && (0..a.h as usize).all(|k| a.views[k] == self.views[k] && a.cuts[k] == self.cuts[k])
    }
}

impl fmt::Debug for Block {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.h == 0 {
            return write!(f, "genesis");
        }
        let parts: Vec<String> = (0..self.h as usize)
            .map(|k| {
                let c: Vec<String> = self.cuts[k].iter().map(|&x| if x == NONE { "-".into() } else { x.to_string() }).collect();
                format!("v{}[{}]", self.views[k], c.join(","))
            })
            .collect();
        write!(f, "{}", parts.join(">"))
    }
}

/// A certified block, with the root its quorum voted for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Cert {
    pub block: Block,
    pub root: bool,
}

impl Cert {
    pub const GENESIS: Cert = Cert { block: Block::GENESIS, root: true };
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Msg {
    Entry { to: u8, e: u8 },
    /// From the view's leader; the parent's certificate is implied (the parent must be certified), `tc` is the highest
    /// certificate of the timeout certificate for the view before, if the proposal needs one.
    Propose { block: Block, proot: bool, tc: Option<Cert>, carried: u16 },
    /// Over (block, the voter's own root).
    Vote { voter: u8, block: Block, root: bool },
    Timeout { voter: u8, view: u8, high: Cert },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Member {
    pub view: u8,
    pub voted: u8,
    pub timed_out: u8,
    pub proposed: u8,
    /// The highest certified block it knows.
    pub high: Cert,
    pub held: u16,
    pub committed: Block,
    /// A final cut arrived that does not extend this member's final cut.
    pub undone: bool,
    /// It has seen a quorum certify a root it did not compute (RootSplit).
    pub split: bool,
}

impl Member {
    fn new(held: u16) -> Member {
        Member { view: 1, voted: 0, timed_out: 0, proposed: 0, high: Cert::GENESIS, held, committed: Block::GENESIS, undone: false, split: false }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct State {
    pub m: [Member; NMAX],
    /// Every message sent. Delivery doesn't remove one, so delay, reordering, duplication and loss are all covered.
    pub net: BTreeSet<Msg>,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Action {
    Deliver(u8, Msg),
    /// A member collects a quorum of votes for a block.
    Observe(u8, Cert),
    Timeout(u8),
    /// A member forms a timeout certificate for its view, adopting the given certificate from it.
    AdvanceTc(u8, Cert),
    Propose(u8),
    AdvEntry(u8, u8),
    AdvPropose(u8, Msg),
}

impl Model for Cuts {
    type State = State;
    type Action = Action;

    fn init_states(&self) -> Vec<State> {
        let mut m = [Member::new(0); NMAX];
        let mut net = BTreeSet::new();
        for (i, e) in self.entries.iter().enumerate() {
            m[e.signer as usize].held |= 1 << i;
            if e.signer != 0 || self.fault == Fault::None {
                for to in 0..self.n {
                    if to != e.signer && self.protocol(to) {
                        net.insert(Msg::Entry { to, e: i as u8 });
                    }
                }
            }
        }
        vec![State { m, net }]
    }

    fn actions(&self, s: &State, acts: &mut Vec<Action>) {
        let timeouts = self.mutation != Some(Mutation::NoTimeouts) && (!self.liveness || self.quiescent(s));
        self.honest_actions(s, acts, timeouts);
        self.adversary_actions(s, acts);
        if self.productive_only {
            acts.retain(|a| self.next_state(s, a.clone()).is_some());
        }
    }

    fn next_state(&self, s0: &State, a: Action) -> Option<State> {
        let mut s = s0.clone();
        match a {
            Action::Deliver(m, Msg::Entry { e, .. }) => {
                s.m[m as usize].held |= 1 << e;
            }
            Action::Deliver(m, Msg::Propose { block, proot, tc, carried }) | Action::AdvPropose(m, Msg::Propose { block, proot, tc, carried }) => {
                self.on_propose(&mut s, m, block, proot, tc, carried);
            }
            Action::Deliver(..) | Action::AdvPropose(..) => return None,
            Action::Observe(m, b) => {
                let mut mm = s.m[m as usize];
                self.note_cert(&mut mm, m, b);
                s.m[m as usize] = mm;
            }
            Action::Timeout(m) => {
                let mm = &mut s.m[m as usize];
                if mm.timed_out >= mm.view || mm.view > self.views {
                    return None;
                }
                mm.timed_out = mm.view;
                let (view, high) = (mm.view, mm.high);
                s.net.insert(Msg::Timeout { voter: m, view, high });
            }
            Action::AdvanceTc(m, o) => {
                let mut mm = s.m[m as usize];
                let v = mm.view;
                self.note_cert(&mut mm, m, o);
                mm.view = mm.view.max(v + 1);
                s.m[m as usize] = mm;
            }
            Action::Propose(m) => {
                let mut mm = s.m[m as usize];
                let v = mm.view;
                if self.leader(v) != m || v > self.views || mm.proposed >= v || mm.timed_out >= v {
                    return None;
                }
                let tc = if mm.high.block.view() + 1 == v {
                    None
                } else {
                    let maxima = self.tc_maxima(&s, v - 1);
                    let Some(t) = maxima.into_iter().min_by_key(|c| c.block.view()) else { return None };
                    if t.block.view() > mm.high.block.view() {
                        mm.high = t;
                    }
                    Some(t)
                };
                let (parent, proot) = (mm.high.block, mm.high.root);
                if parent.h >= self.hcap() {
                    return None;
                }
                let (cut, carried) = self.build_cut(&parent, mm.held);
                let block = parent.child(v, cut);
                mm.proposed = v;
                s.m[m as usize] = mm;
                s.net.insert(Msg::Propose { block, proot, tc, carried });
            }
            Action::AdvEntry(to, e) => {
                s.m[to as usize].held |= 1 << e;
            }
        }
        if s == *s0 { None } else { Some(s) }
    }

    fn properties(&self) -> Vec<Property<Self>> {
        let mut p = vec![
            Property::always("agreement", |c: &Cuts, s: &State| {
                let hs: Vec<&Member> = (0..c.n).filter(|&m| c.honest(m)).map(|m| &s.m[m as usize]).collect();
                hs.iter().all(|a| hs.iter().all(|b| a.committed.extends(&b.committed) || b.committed.extends(&a.committed)))
            }),
            Property::always("validity: chains", |c: &Cuts, s: &State| {
                (0..c.n).filter(|&m| c.honest(m)).all(|m| {
                    let fin = s.m[m as usize].committed;
                    (1..=fin.h).all(|k| {
                        let anc = fin.ancestor(k);
                        let (pc, nc) = (anc.parent().cut(), anc.cut());
                        if k > c.heights {
                            return nc == pc;
                        }
                        (0..c.n as usize).all(|a| {
                            nc[a] == pc[a]
                                || c.segment(pc[a], nc[a]).is_some_and(|seg| {
                                    seg.iter().all(|&x| {
                                        let e = &c.entries[x as usize];
                                        e.author as usize == a && e.signed_ok() && e.stamp < FRAMES * k
                                    })
                                })
                        })
                    })
                })
            }),
            // Two honest members that have both judged an entry against their final cuts judged it the same way.
            Property::always("validity: void set the same everywhere", |c: &Cuts, s: &State| {
                let v: Vec<(u16, u16)> = (0..c.n).filter(|&m| c.honest(m)).map(|m| c.verdicts(&s.m[m as usize].committed, s.m[m as usize].held)).collect();
                v.iter().all(|a| v.iter().all(|b| a.0 & b.1 == 0 && a.1 & b.0 == 0))
            }),
            // Every verdict an honest member has reached is the one the whole chain gives, so learning more entries never
            // changes it (D-1: a final frame never carries an entry that is later void); and final cuts only extend.
            Property::always("no final frame undone", |c: &Cuts, s: &State| {
                (0..c.n).filter(|&m| c.honest(m)).all(|m| {
                    let mm = &s.m[m as usize];
                    let (eff, void) = c.verdicts(&mm.committed, mm.held);
                    let (te, tv) = c.verdicts(&mm.committed, c.all_entries());
                    !mm.undone && eff & tv == 0 && void & te == 0
                })
            }),
            // A member never holds a final cut whose quorum root differs from its own without knowing it (RootSplit).
            Property::always("root: a diverged replica learns it at the cut", |c: &Cuts, s: &State| {
                (0..c.n).filter(|&m| c.honest(m)).all(|m| {
                    let mm = &s.m[m as usize];
                    mm.committed.h == 0 || c.own_root(m) || mm.split
                })
            }),
            Property::sometimes("a cut is final", |c: &Cuts, s: &State| (0..c.n).any(|m| c.honest(m) && s.m[m as usize].committed.h >= 1)),
        ];
        if self.liveness {
            p.push(Property::always("liveness: every end state has a final cut at every honest member", |c: &Cuts, s: &State| {
                (0..c.n).filter(|&m| c.honest(m)).all(|m| s.m[m as usize].committed.h >= 1) || !c.honest_stuck(s)
            }));
        }
        if self.corrupt.is_some() {
            p.push(Property::sometimes("a diverged replica sees RootSplit", |c: &Cuts, s: &State| {
                (0..c.n).any(|m| c.honest(m) && s.m[m as usize].split && s.m[m as usize].committed.h >= 1)
            }));
        }
        if matches!(self.fault, Fault::Author | Fault::Byzantine) {
            p.push(Property::sometimes("a faulty author's entry takes effect", |c: &Cuts, s: &State| {
                (0..c.n).any(|m| c.honest(m) && {
                    let mm = &s.m[m as usize];
                    let eff = c.verdicts(&mm.committed, mm.held).0;
                    (0..c.entries.len()).any(|x| c.entries[x].signer == 0 && eff & (1 << x) != 0)
                })
            }));
        }
        if self.honest_authors > 0 && !self.liveness {
            p.push(Property::sometimes("an honest entry is void (late)", |c: &Cuts, s: &State| {
                (0..c.n).any(|m| c.honest(m) && {
                    let mm = &s.m[m as usize];
                    let void = c.verdicts(&mm.committed, mm.held).1;
                    (0..c.entries.len()).any(|x| c.entries[x].signer != 0 && void & (1 << x) != 0)
                })
            }));
        }
        p
    }
}

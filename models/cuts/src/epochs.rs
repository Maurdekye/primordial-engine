//! Model v1: membership change by epochs (docs/spec/agreement.md 8; coordinator).
//!
//! The cut rules of v0 at a coarser grain, so the state stays small enough to search across an epoch change:
//! chained two-chain voting with timeouts and the lock on a timeout certificate's highest certificate, per epoch.
//! Entries are a handful: membership changes (Admit, Leave, Remove, Rechain) and a few ordinary ones, each with a
//! stamp. A cut names a frame (its parent's + 2) and the set of entries it covers (cumulative); an entry it newly
//! covers stamped below its parent's frame is late, and void.
//!
//! The rule modelled: a change takes effect only through a final cut. The first cut of a chain in epoch e that
//! covers an effective change is the reconfiguring cut E; its descendants in e are suffix cuts (E's frame and
//! entries, nothing new), which exist only to finalize E. When E is final at a member, epoch e+1 starts there from E:
//! the members change, the quorum is recomputed, views restart at 1. Messages name their epoch (the high nibble of
//! `ev`), and certificates count only the votes of the block's epoch's members.

use std::collections::BTreeSet;
use std::fmt;

use stateright::{Model, Property};

/// Keys.
pub const K: usize = 4;
/// Block heights the state can hold, across epochs.
pub const H: usize = 7;
/// Epochs a run can reach (at most two changes of membership).
pub const EM: usize = 3;
pub const NONE: u8 = u8::MAX;
/// The frames a cut names above its parent's.
pub const STEP: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Ord,
    Admit(u8),
    Leave,
    Remove(u8),
    /// Seq 1 of a restarted member's new chain under the same key.
    Rechain,
}

#[derive(Clone, Debug)]
pub struct EDef {
    pub name: &'static str,
    pub author: u8,
    /// 0: the key's first chain; 1: the chain its Rechain starts.
    pub chain: u8,
    /// The entry before it on its chain, or NONE.
    pub prev: u8,
    /// NONE: stamped when it is authored (a new chain's entries, at the frame of the cut that made the Rechain final).
    pub stamp: u8,
    pub kind: Kind,
    /// Authored at the start (held by its author, sent to everyone). Otherwise authored by an action.
    pub at_start: bool,
}

/// Controls: each breaks one rule of agreement.md 8, and the checker must find a counterexample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EMut {
    /// (a) A member moves to the next epoch when it sees a certified cut covering a change, not when it is final.
    AtInclusion,
    /// (b) Leaders propose, and voters accept, a descendant of E with something new (a frame, entries); the epoch
    /// still ends at E.
    NonEmptySuffix,
    /// (c) A certificate counts any key's vote, not only the block's epoch's members (a removed member's included).
    WrongEpochVote,
    /// (d) A rechained key's old chain stays open after E: its entries are still accepted.
    OldChainOpen,
    /// (e) No suffix rule, and the epoch's final log ends at the cut that finalized E rather than at E.
    EndAtFinalizer,
    /// (f1) A restart forgets the views it voted and timed out in.
    ForgetViews,
    /// (f2) A restart keeps its views but forgets its high_qc.
    ForgetHighQc,
}

#[derive(Clone, Debug)]
pub struct Epochs {
    pub name: &'static str,
    /// Epoch 0's members (bit per key).
    pub m0: u8,
    /// The Byzantine key, if any.
    pub byz: Option<u8>,
    /// The Byzantine key may sign Remove{honest} (governance authorized it).
    pub byz_authorized: bool,
    /// Which keys may sign Admit and Remove (abstract governance).
    pub auth: u8,
    /// The key that restarts once (an honest member), if any.
    pub restart: Option<u8>,
    pub entries: Vec<EDef>,
    /// Views played per epoch.
    pub views: u8,
    pub mutation: Option<EMut>,
    /// Shifts the rotation, so the Byzantine member leads the view a check needs.
    pub offset: u8,
    /// The Byzantine member may propose (off: it only votes and times out, which keeps the space small).
    pub byz_proposes: bool,
    /// A restarted key's old process lives on (a zombie) and may still author on the old chain: what the per-key
    /// floor and the closed old chain protect against.
    pub zombie: bool,
    /// List only actions that change the state (for random walks).
    pub productive_only: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Blk {
    pub h: u8,
    /// (epoch << 4) | view, per height.
    pub ev: [u8; H],
    pub fr: [u8; H],
    pub inc: [u16; H],
    pub by: [u8; H],
}

impl Blk {
    pub const GENESIS: Blk = Blk { h: 0, ev: [0; H], fr: [0; H], inc: [0; H], by: [NONE; H] };

    fn top(&self) -> Option<usize> {
        if self.h == 0 { None } else { Some(self.h as usize - 1) }
    }
    pub fn ev(&self) -> u8 {
        self.top().map_or(0, |k| self.ev[k])
    }
    pub fn epoch(&self) -> u8 {
        self.ev() >> 4
    }
    pub fn frame(&self) -> u8 {
        self.top().map_or(0, |k| self.fr[k])
    }
    pub fn inc(&self) -> u16 {
        self.top().map_or(0, |k| self.inc[k])
    }
    pub fn parent(&self) -> Blk {
        let mut b = *self;
        if let Some(k) = b.top() {
            b.ev[k] = 0;
            b.fr[k] = 0;
            b.inc[k] = 0;
            b.by[k] = NONE;
            b.h -= 1;
        }
        b
    }
    pub fn child(&self, ev: u8, fr: u8, inc: u16, by: u8) -> Blk {
        let mut b = *self;
        let k = b.h as usize;
        b.ev[k] = ev;
        b.fr[k] = fr;
        b.inc[k] = inc;
        b.by[k] = by;
        b.h += 1;
        b
    }
    pub fn ancestor(&self, h: u8) -> Blk {
        let mut b = *self;
        while b.h > h {
            b = b.parent();
        }
        b
    }
    /// Is `a` this block or one of its ancestors?
    pub fn extends(&self, a: &Blk) -> bool {
        a.h <= self.h && self.ancestor(a.h) == *a
    }
}

impl fmt::Debug for Blk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.h == 0 {
            return write!(f, "genesis");
        }
        let parts: Vec<String> = (0..self.h as usize)
            .map(|k| {
                format!("e{}v{}m{}@{}[{:b}]", self.ev[k] >> 4, self.ev[k] & 15, self.by[k], self.fr[k], self.inc[k])
            })
            .collect();
        write!(f, "{}", parts.join(">"))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Msg {
    Entry { to: u8, e: u8 },
    Propose { b: Blk },
    Vote { voter: u8, b: Blk },
    Timeout { voter: u8, ev: u8, high: Blk },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Mem {
    /// Its current epoch, or NONE before it is a member (a joiner).
    pub ep: u8,
    pub view: u8,
    /// The highest (epoch, view) it voted, timed out and proposed in: persisted safety data (agreement.md 8).
    pub voted: u8,
    pub tout: u8,
    pub proposed: u8,
    /// Its high_qc: the highest certified block it knows (persisted with the views).
    pub high: Blk,
    /// The end of its final log: the last final cut, cut back to E at an epoch change.
    pub fin: Blk,
    /// The two-chain rule finalized a cut here with something new after its epoch's E (under (b) or (e) only).
    pub past_e: bool,
    /// The two-chain rule finalized a cut here covering a rechained key's old-chain entry after its Rechain's E.
    pub two_chains: bool,
    /// ends[e]: E of epoch e, once final here.
    pub ends: [Blk; EM],
    pub held: u16,
    /// A final cut arrived that does not extend its final log.
    pub undone: bool,
    pub restarted: bool,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct EState {
    pub m: [Mem; K],
    pub net: BTreeSet<Msg>,
    /// Stamps, for entries stamped when authored.
    pub stamp: [u8; 16],
    /// Entries authored so far (bit per entry).
    pub authored: u16,
}

#[derive(Clone, PartialEq, Debug)]
pub enum EAction {
    Deliver(u8, Msg),
    Observe(u8, Blk),
    Timeout(u8),
    AdvanceTc(u8),
    Propose(u8),
    Restart(u8),
    /// Author the next entry on a new chain (after its Rechain is final).
    Author(u8, u8),
    AdvVote(Blk),
    AdvTimeout(u8, Blk),
    AdvPropose(Blk),
}

/// What a chain of blocks says: the members of each epoch it reaches, its E per epoch, and which entries took effect.
#[derive(Clone, Debug, Default)]
pub struct Chain {
    pub members: [u8; EM + 1],
    /// Height of E per epoch (NONE if the chain has none yet).
    pub e_at: [u8; EM],
    pub effective: u16,
    /// Entries a cut covered that are void.
    pub void: u16,
    /// Keys whose Rechain took effect, with the epoch it took effect in (the E that carried it).
    pub rechained: [u8; K],
    /// A suffix cut in this chain carries something new (frame or entries): only under (b) or (e).
    pub bad_suffix: bool,
    /// An entry of a rechained key's old chain was covered after its Rechain's E.
    pub old_after_rechain: bool,
}

pub fn quorum(n: u32) -> usize {
    let t = n.saturating_sub(1) / 3;
    ((n + t) / 2 + 1) as usize
}

impl Epochs {
    pub fn mutate(mut self, m: EMut) -> Self {
        self.mutation = Some(m);
        self
    }

    fn is(&self, m: EMut) -> bool {
        self.mutation == Some(m)
    }

    pub fn honest(&self, k: u8) -> bool {
        self.byz != Some(k)
    }

    fn may_sign_governance(&self, k: u8) -> bool {
        self.auth & (1 << k) != 0 || (self.byz == Some(k) && self.byz_authorized)
    }

    /// Walk a chain from genesis: members per epoch, E per epoch, and verdicts.
    pub fn analyze(&self, b: &Blk) -> Chain {
        let mut c = Chain { members: [0; EM + 1], e_at: [NONE; EM], rechained: [NONE; K], ..Default::default() };
        c.members[0] = self.m0;
        let mut cur = self.m0; // members of the epoch being walked, with its E's changes applied as they come
        let mut prev_frame = 0u8;
        let mut prev_inc = 0u16;
        for k in 0..b.h as usize {
            let ep = (b.ev[k] >> 4) as usize;
            if ep >= EM {
                break;
            }
            let new = b.inc[k] & !prev_inc;
            let in_suffix = c.e_at[ep] != NONE;
            if in_suffix && (new != 0 || b.fr[k] != prev_frame) {
                c.bad_suffix = true;
            }
            let mut members = c.members[ep];
            let mut changed = false;
            for e in 0..self.entries.len() {
                if new & (1 << e) == 0 {
                    continue;
                }
                let d = &self.entries[e];
                let stamp = d.stamp;
                let ok = self.valid_in(&c, ep, members, e as u8) && stamp != NONE && stamp >= prev_frame;
                // A rechained key's old-chain entry covered after its Rechain's E.
                if d.chain == 0
                    && c.rechained[d.author as usize] != NONE
                    && (c.rechained[d.author as usize] as usize) < ep
                {
                    c.old_after_rechain = true;
                }
                if !ok {
                    c.void |= 1 << e;
                    continue;
                }
                c.effective |= 1 << e;
                match d.kind {
                    Kind::Ord => {}
                    Kind::Admit(t) => {
                        members |= 1 << t;
                        changed = true;
                    }
                    Kind::Leave => {
                        members &= !(1 << d.author);
                        changed = true;
                    }
                    Kind::Remove(t) => {
                        members &= !(1 << t);
                        changed = true;
                    }
                    Kind::Rechain => {
                        c.rechained[d.author as usize] = ep as u8;
                        changed = true;
                    }
                }
            }
            if changed && !in_suffix {
                c.e_at[ep] = k as u8;
                c.members[ep + 1] = members;
            }
            cur = members;
            prev_frame = b.fr[k];
            prev_inc = b.inc[k];
        }
        let _ = cur;
        c
    }

    /// Whether entry e may take effect in epoch ep, judged against `members` (the epoch's, with earlier changes of
    /// the same cut applied).
    fn valid_in(&self, c: &Chain, ep: usize, members: u8, e: u8) -> bool {
        let d = &self.entries[e as usize];
        let a = d.author;
        if members & (1 << a) == 0 {
            return false;
        }
        if d.prev != NONE && c.effective & (1 << d.prev) == 0 {
            return false;
        }
        let rech = c.rechained[a as usize];
        if d.chain == 0 && rech != NONE && (rech as usize) < ep && !self.is(EMut::OldChainOpen) {
            return false; // the old chain closed at its Rechain's E
        }
        if d.chain == 1 && d.kind != Kind::Rechain && (rech == NONE || rech as usize >= ep) {
            return false; // a new chain's entries wait for its Rechain to be final
        }
        match d.kind {
            Kind::Ord | Kind::Leave | Kind::Rechain => true,
            Kind::Admit(t) => members & (1 << t) == 0 && self.may_sign_governance(a),
            Kind::Remove(t) => members & (1 << t) != 0 && t != a && self.may_sign_governance(a),
        }
    }

    fn stamp_of(&self, s: &EState, e: usize) -> u8 {
        if self.entries[e].stamp == NONE { s.stamp[e] } else { self.entries[e].stamp }
    }

    /// The chain analysis with the run's own stamps (entries stamped when authored).
    fn chain(&self, s: &EState, b: &Blk) -> Chain {
        if self.entries.iter().all(|d| d.stamp != NONE) {
            return self.analyze(b);
        }
        let mut me = self.clone();
        for (e, d) in me.entries.iter_mut().enumerate() {
            if d.stamp == NONE {
                d.stamp = s.stamp[e];
            }
        }
        me.analyze(b)
    }

    pub fn leader(&self, members: u8, ev: u8) -> u8 {
        let list: Vec<u8> = (0..K as u8).filter(|k| members & (1 << k) != 0).collect();
        if list.is_empty() {
            return NONE;
        }
        let (ep, v) = ((ev >> 4) as usize, (ev & 15) as usize);
        list[(v + ep + self.offset as usize) % list.len()]
    }

    /// The members of b's own epoch, from its chain.
    fn members_of(&self, s: &EState, b: &Blk) -> u8 {
        self.chain(s, b).members[b.epoch() as usize]
    }

    fn certified(&self, s: &EState, b: &Blk) -> bool {
        if b.h == 0 {
            return true;
        }
        let members = self.members_of(s, b);
        let n = members.count_ones();
        let votes = (0..K as u8)
            .filter(|&v| {
                (self.is(EMut::WrongEpochVote) || members & (1 << v) != 0)
                    && s.net.contains(&Msg::Vote { voter: v, b: *b })
            })
            .count();
        n > 0 && votes >= quorum(n)
    }

    /// The (epoch, view) of a block as a parent in epoch ep: the epoch's base (E of ep - 1, or genesis) counts as
    /// view 0 of ep.
    fn pev(&self, b: &Blk, ep: u8) -> u8 {
        if b.epoch() < ep { ep << 4 } else { b.ev() }
    }

    /// The highest certificates a timeout certificate for `ev` can carry: for each, a quorum of the epoch's members
    /// timed out with a high at or below it.
    fn tc_maxima(&self, s: &EState, members: u8, ev: u8) -> Vec<Blk> {
        let ep = ev >> 4;
        let touts: Vec<(u8, Blk)> = s
            .net
            .iter()
            .filter_map(|m| match m {
                Msg::Timeout { voter, ev: e2, high }
                    if *e2 == ev && (self.is(EMut::WrongEpochVote) || members & (1 << voter) != 0) =>
                {
                    Some((*voter, *high))
                }
                _ => None,
            })
            .collect();
        let q = quorum(members.count_ones());
        let mut out = Vec::new();
        for (_, h) in &touts {
            let x = self.pev(h, ep);
            let voters: BTreeSet<u8> = touts.iter().filter(|(_, h2)| self.pev(h2, ep) <= x).map(|(v, _)| *v).collect();
            if voters.len() >= q && !out.contains(h) {
                out.push(*h);
            }
        }
        out
    }

    /// The members of a member's current epoch, as its own final log says.
    fn my_members(&self, s: &EState, mm: &Mem) -> u8 {
        if mm.ep == NONE {
            return 0;
        }
        self.chain(s, &mm.fin).members[mm.ep as usize]
    }

    fn base(&self, mm: &Mem) -> Blk {
        if mm.ep == 0 || mm.ep == NONE { Blk::GENESIS } else { mm.ends[mm.ep as usize - 1] }
    }

    /// Does an E for b's epoch lie on b's chain at or below b (so b's children in that epoch are suffix cuts)?
    fn in_suffix(&self, s: &EState, b: &Blk) -> bool {
        let c = self.chain(s, b);
        let ep = b.epoch() as usize;
        ep < EM && c.e_at[ep] != NONE
    }

    /// The entries an honest proposer at parent p (epoch ep) covers: those it holds that may take effect, chain order
    /// kept, stamped at or above p's frame and below the cut's.
    fn build(&self, s: &EState, me: u8, p: &Blk, ep: u8, frame: u8) -> u16 {
        let mut inc = p.inc();
        let mut c = self.chain(s, p);
        let members = c.members[ep as usize];
        let mut cur = members;
        loop {
            let mut added = false;
            for e in 0..self.entries.len() {
                let bit = 1u16 << e;
                if inc & bit != 0 || s.m[me as usize].held & bit == 0 || s.authored & bit == 0 {
                    continue;
                }
                let st = self.stamp_of(s, e);
                if st < p.frame() || st >= frame {
                    continue;
                }
                if !self.valid_in(&c, ep as usize, cur, e as u8) {
                    continue;
                }
                inc |= bit;
                c.effective |= bit;
                match self.entries[e].kind {
                    Kind::Admit(t) => cur |= 1 << t,
                    Kind::Leave => cur &= !(1 << self.entries[e].author),
                    Kind::Remove(t) => cur &= !(1 << t),
                    _ => {}
                }
                added = true;
            }
            if !added {
                break;
            }
        }
        inc
    }

    /// An honest voter's check of a proposed block.
    fn votable(&self, s: &EState, me: u8, b: &Blk) -> bool {
        let mm = &s.m[me as usize];
        if mm.ep == NONE || b.h == 0 || b.epoch() != mm.ep || (b.ev() & 15) != mm.view || mm.view > self.views {
            return false;
        }
        if b.ev() <= mm.voted || b.ev() <= mm.tout {
            return false;
        }
        let members = self.my_members(s, mm);
        if self.members_of(s, b) != members || self.leader(members, b.ev()) != b.by[b.h as usize - 1] {
            return false;
        }
        let p = b.parent();
        let base = self.base(mm);
        if !p.extends(&base) || (p.epoch() != mm.ep && p != base) || !self.certified(s, &p) {
            return false;
        }
        // The lock: the parent is the previous view's block, or at least as high as a timeout certificate's highest.
        let pv = self.pev(&p, mm.ep);
        if pv + 1 != b.ev() {
            let tcs = self.tc_maxima(s, members, b.ev() - 1);
            let Some(lo) = tcs.iter().map(|h| self.pev(h, mm.ep)).min() else { return false };
            if pv < lo {
                return false;
            }
        }
        let suffix = self.in_suffix(s, &p) && p.epoch() == mm.ep && !self.is(EMut::EndAtFinalizer);
        if suffix && !self.is(EMut::NonEmptySuffix) {
            return b.frame() == p.frame() && b.inc() == p.inc();
        }
        if b.frame() != p.frame() + STEP && !(suffix && b.frame() == p.frame()) {
            return false;
        }
        // Every new entry is held, stamped below the frame, and may take effect (a late one is void, not refused).
        let new = b.inc() & !p.inc();
        let c = self.chain(s, &p);
        let mut cur = c.members[mm.ep as usize];
        let mut c2 = c.clone();
        for e in 0..self.entries.len() {
            let bit = 1u16 << e;
            if new & bit == 0 {
                continue;
            }
            if mm.held & bit == 0 || s.authored & bit == 0 || self.stamp_of(s, e) >= b.frame() {
                return false;
            }
            if !self.valid_in(&c2, mm.ep as usize, cur, e as u8) {
                return false;
            }
            c2.effective |= bit;
            match self.entries[e].kind {
                Kind::Admit(t) => cur |= 1 << t,
                Kind::Leave => cur &= !(1 << self.entries[e].author),
                Kind::Remove(t) => cur &= !(1 << t),
                _ => {}
            }
        }
        true
    }

    /// A member learns a certified block: its high, the two-chain rule, and an epoch change.
    fn observe(&self, s: &mut EState, me: u8, b: Blk) {
        let mut mm = s.m[me as usize];
        // The highest epoch b proves started: b's own, and the next if b finalizes its parent under the two-chain
        // rule and that parent's chain has the epoch's E (b with its parent is then the epoch certificate).
        let c = self.chain(s, &b);
        let eb = b.epoch();
        let p = b.parent();
        let next = p.h > 0
            && p.epoch() == eb
            && p.ev() + 1 == b.ev()
            && (eb as usize) < EM
            && self.chain(s, &p).e_at[eb as usize] != NONE;
        let target = if next && !self.is(EMut::EndAtFinalizer) { eb + 1 } else { eb };
        if target >= 1 && (target as usize) < EM + 1 && (mm.ep == NONE || target > mm.ep) {
            // A joiner, or a member that missed an epoch change: it adopts the epochs b proves, if they make it a
            // member and extend its final log (it fetches the epoch certificates).
            let cc = if target > eb { self.chain(s, &p) } else { c.clone() };
            let mut ends = mm.ends;
            let mut ok = cc.members[target as usize] & (1 << me) != 0;
            for e in 0..target as usize {
                if !ok || cc.e_at[e] == NONE {
                    ok = false;
                    break;
                }
                ends[e] = b.ancestor(cc.e_at[e] + 1);
            }
            if ok && ends[target as usize - 1].extends(&mm.fin) {
                mm.ends = ends;
                mm.ep = target;
                mm.fin = ends[target as usize - 1];
                mm.high = mm.fin;
                mm.view = 1;
            } else if mm.ep == NONE {
                return;
            }
        }
        if b.epoch() != mm.ep {
            s.m[me as usize] = mm;
            return;
        }
        if self.pev(&b, mm.ep) > self.pev(&mm.high, mm.ep) {
            mm.high = b;
        }
        // A certificate for view v moves the member to view v + 1.
        if (b.ev() & 15) >= mm.view {
            mm.view = (b.ev() & 15) + 1;
        }
        if self.is(EMut::AtInclusion) && self.in_suffix(s, &b) {
            let c = self.chain(s, &b);
            let e_blk = b.ancestor(c.e_at[mm.ep as usize] + 1);
            self.enter_next(&mut mm, e_blk);
            s.m[me as usize] = mm;
            return;
        }
        // Two-chain: b's parent is final if it is the previous view's block of the same epoch.
        let p = b.parent();
        if p.h > 0 && p.epoch() == b.epoch() && p.ev() + 1 == b.ev() {
            if !p.extends(&mm.fin) && !mm.fin.extends(&p) {
                mm.undone = true;
            }
            let pc = self.chain(s, &p);
            mm.past_e |= pc.bad_suffix;
            mm.two_chains |= pc.old_after_rechain;
            if p.extends(&mm.fin) {
                mm.fin = p;
                let c = self.chain(s, &p);
                let ep = mm.ep as usize;
                if ep < EM && c.e_at[ep] != NONE {
                    let e_blk = if self.is(EMut::EndAtFinalizer) { p } else { p.ancestor(c.e_at[ep] + 1) };
                    self.enter_next(&mut mm, e_blk);
                }
            }
        }
        s.m[me as usize] = mm;
    }

    fn enter_next(&self, mm: &mut Mem, e_blk: Blk) {
        let ep = mm.ep as usize;
        mm.ends[ep] = e_blk;
        mm.fin = e_blk;
        mm.ep += 1;
        mm.view = 1;
        mm.high = e_blk;
    }

    fn certs(&self, s: &EState) -> Vec<Blk> {
        let mut out: BTreeSet<Blk> = BTreeSet::new();
        for m in &s.net {
            if let Msg::Vote { b, .. } = m {
                if !out.contains(b) && self.certified(s, b) {
                    out.insert(*b);
                }
            }
        }
        out.into_iter().collect()
    }
}

impl Model for Epochs {
    type State = EState;
    type Action = EAction;

    fn init_states(&self) -> Vec<EState> {
        let joiner = |k: u8| self.m0 & (1 << k) == 0;
        let mut m = [Mem {
            ep: 0,
            view: 1,
            voted: 0,
            tout: 0,
            proposed: 0,
            high: Blk::GENESIS,
            fin: Blk::GENESIS,
            past_e: false,
            two_chains: false,
            ends: [Blk::GENESIS; EM],
            held: 0,
            undone: false,
            restarted: false,
        }; K];
        for k in 0..K as u8 {
            if joiner(k) {
                m[k as usize].ep = NONE;
            }
        }
        let mut s = EState { m, net: BTreeSet::new(), stamp: [0; 16], authored: 0 };
        for (i, d) in self.entries.iter().enumerate() {
            if d.at_start {
                self.author(&mut s, i as u8, d.stamp);
            }
        }
        vec![s]
    }

    fn actions(&self, s: &EState, acts: &mut Vec<EAction>) {
        for msg in &s.net {
            match msg {
                Msg::Entry { to, .. } => acts.push(EAction::Deliver(*to, *msg)),
                Msg::Propose { .. } => {
                    for k in 0..K as u8 {
                        if self.honest(k) {
                            acts.push(EAction::Deliver(k, *msg));
                        }
                    }
                }
                _ => {}
            }
        }
        let certs = self.certs(s);
        for k in 0..K as u8 {
            if !self.honest(k) {
                continue;
            }
            for c in &certs {
                acts.push(EAction::Observe(k, *c));
            }
            acts.push(EAction::Timeout(k));
            acts.push(EAction::AdvanceTc(k));
            acts.push(EAction::Propose(k));
            if self.restart == Some(k) {
                acts.push(EAction::Restart(k));
            }
            for (e, d) in self.entries.iter().enumerate() {
                if d.author == k && !d.at_start && d.kind != Kind::Rechain {
                    acts.push(EAction::Author(k, e as u8));
                }
            }
        }
        if let Some(z) = self.byz {
            for msg in &s.net {
                if let Msg::Propose { b } = msg {
                    acts.push(EAction::AdvVote(*b));
                }
            }
            // Timeouts naming any (epoch, view) with the lowest high it can (its epoch's base).
            let mut bases: BTreeSet<Blk> = BTreeSet::new();
            bases.insert(Blk::GENESIS);
            for c in &certs {
                let ch = self.chain(s, c);
                for e in 0..EM {
                    if ch.e_at[e] != NONE {
                        bases.insert(c.ancestor(ch.e_at[e] + 1));
                    }
                }
            }
            for b in &bases {
                for v in 1..=self.views {
                    let ev = ((b.epoch() + if b.h == 0 { 0 } else { 1 }) << 4) | v;
                    acts.push(EAction::AdvTimeout(ev, *b));
                }
            }
            // Proposals as any view's leader, from any certified block or base: honest-shaped, a suffix copy, or
            // with every entry it knows.
            if !self.byz_proposes {
                acts.dedup();
                self.keep_productive(s, acts);
                return;
            }
            let mut parents: BTreeSet<Blk> = certs.iter().copied().collect();
            parents.extend(bases.iter().copied());
            for p in &parents {
                if p.h as usize >= H {
                    continue;
                }
                let ch = self.chain(s, p);
                let base_next = p.h > 0 && ch.e_at.iter().any(|&e| e != NONE && e + 1 == p.h);
                let eps: Vec<u8> = if base_next { vec![p.epoch(), p.epoch() + 1] } else { vec![p.epoch()] };
                for ep in eps {
                    if ep as usize >= EM {
                        continue;
                    }
                    let members = ch.members[ep as usize];
                    for v in 1..=self.views {
                        let ev = (ep << 4) | v;
                        if self.leader(members, ev) != z || (p.epoch() == ep && p.ev() >= ev) {
                            continue;
                        }
                        let all = p.inc() | s.authored;
                        let honestish =
                            self.build(s, z, p, ep, p.frame() + STEP) | (s.m[z as usize].held & s.authored & !p.inc());
                        for (fr, inc) in [
                            (p.frame(), p.inc()),
                            (p.frame() + STEP, p.inc()),
                            (p.frame() + STEP, honestish),
                            (p.frame() + STEP, all),
                        ] {
                            acts.push(EAction::AdvPropose(p.child(ev, fr, inc, z)));
                        }
                    }
                }
            }
        }
        acts.dedup();
        self.keep_productive(s, acts);
    }

    fn next_state(&self, s0: &EState, a: EAction) -> Option<EState> {
        let mut s = s0.clone();
        match a {
            EAction::Deliver(k, Msg::Entry { e, .. }) => {
                s.m[k as usize].held |= 1 << e;
            }
            EAction::Deliver(k, Msg::Propose { b }) => {
                if !self.votable(&s, k, &b) {
                    return None;
                }
                s.m[k as usize].voted = b.ev();
                let p = b.parent();
                let ep = s.m[k as usize].ep;
                if self.pev(&p, ep) > self.pev(&s.m[k as usize].high, ep) {
                    s.m[k as usize].high = p;
                }
                s.net.insert(Msg::Vote { voter: k, b });
            }
            EAction::Deliver(..) => return None,
            EAction::Observe(k, b) => {
                self.observe(&mut s, k, b);
            }
            EAction::Timeout(k) => {
                let mm = &mut s.m[k as usize];
                if mm.ep == NONE || mm.view > self.views {
                    return None;
                }
                let ev = (mm.ep << 4) | mm.view;
                if mm.tout >= ev {
                    return None;
                }
                mm.tout = ev;
                let high = mm.high;
                s.net.insert(Msg::Timeout { voter: k, ev, high });
            }
            EAction::AdvanceTc(k) => {
                let mm = s.m[k as usize];
                if mm.ep == NONE {
                    return None;
                }
                let ev = (mm.ep << 4) | mm.view;
                let members = self.my_members(&s, &mm);
                let tcs = self.tc_maxima(&s, members, ev);
                let Some(hi) = tcs.into_iter().max_by_key(|h| self.pev(h, mm.ep)) else { return None };
                let mm = &mut s.m[k as usize];
                if self.pev(&hi, mm.ep) > self.pev(&mm.high, mm.ep) {
                    mm.high = hi;
                }
                mm.view += 1;
            }
            EAction::Propose(k) => {
                let mm = s.m[k as usize];
                if mm.ep == NONE || mm.view > self.views {
                    return None;
                }
                let ev = (mm.ep << 4) | mm.view;
                if mm.proposed >= ev || mm.tout >= ev {
                    return None;
                }
                let members = self.my_members(&s, &mm);
                if self.leader(members, ev) != k {
                    return None;
                }
                let p = mm.high;
                if p.h as usize >= H {
                    return None;
                }
                let pv = self.pev(&p, mm.ep);
                if pv + 1 != ev {
                    let tcs = self.tc_maxima(&s, members, ev - 1);
                    let Some(lo) = tcs.iter().map(|h| self.pev(h, mm.ep)).min() else { return None };
                    if pv < lo {
                        return None;
                    }
                }
                let suffix = p.epoch() == mm.ep
                    && self.in_suffix(&s, &p)
                    && !self.is(EMut::EndAtFinalizer)
                    && !self.is(EMut::NonEmptySuffix);
                let b = if suffix {
                    p.child(ev, p.frame(), p.inc(), k)
                } else {
                    let fr = p.frame() + STEP;
                    p.child(ev, fr, self.build(&s, k, &p, mm.ep, fr), k)
                };
                s.m[k as usize].proposed = ev;
                s.net.insert(Msg::Propose { b });
            }
            EAction::Restart(k) => {
                let mm = &mut s.m[k as usize];
                if mm.restarted || mm.ep == NONE {
                    return None;
                }
                mm.restarted = true;
                mm.held = 0;
                if self.is(EMut::ForgetViews) {
                    mm.voted = 0;
                    mm.tout = 0;
                    mm.proposed = 0;
                    mm.view = 1;
                }
                if self.is(EMut::ForgetHighQc) {
                    let base = self.base(mm);
                    mm.high = base;
                }
                // Its Rechain: seq 1 of a new chain, stamped now (at its final cut's frame).
                let r = self.entries.iter().position(|d| d.author == k && d.kind == Kind::Rechain)?;
                let st = s.m[k as usize].high.frame();
                self.author(&mut s, r as u8, st);
            }
            EAction::Author(k, e) => {
                let d = &self.entries[e as usize];
                if s.authored & (1 << e) != 0 || (d.prev != NONE && s.authored & (1 << d.prev) == 0) {
                    return None;
                }
                if s.m[k as usize].ep == NONE || (d.chain == 0 && s.m[k as usize].restarted && !self.zombie) {
                    return None; // a joiner authors once admitted; a restarted member's old chain is lost
                }
                if d.chain == 1 {
                    // A restarted member authors on its new chain only once its Rechain is final, stamped at or above
                    // E's frame.
                    if !self.restart_done(&s, k) {
                        return None;
                    }
                }
                let st = if d.stamp == NONE { s.m[k as usize].high.frame() } else { d.stamp };
                self.author(&mut s, e, st);
            }
            EAction::AdvVote(b) => {
                let z = self.byz?;
                s.net.insert(Msg::Vote { voter: z, b });
            }
            EAction::AdvTimeout(ev, high) => {
                let z = self.byz?;
                s.net.insert(Msg::Timeout { voter: z, ev, high });
            }
            EAction::AdvPropose(b) => {
                s.net.insert(Msg::Propose { b });
            }
        }
        if s == *s0 { None } else { Some(s) }
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            // Any two honest members' final logs: one extends the other, across epochs.
            Property::always("agreement", |c: &Epochs, s: &EState| {
                let hm: Vec<&Mem> = (0..K as u8)
                    .filter(|&k| c.honest(k) && s.m[k as usize].ep != NONE)
                    .map(|k| &s.m[k as usize])
                    .collect();
                hm.iter().all(|a| !a.undone && hm.iter().all(|b| a.fin.extends(&b.fin) || b.fin.extends(&a.fin)))
            }),
            // Two honest members that finalized epoch e's end agree on it, so on e+1's members.
            Property::always("same members per epoch", |c: &Epochs, s: &EState| {
                let hm: Vec<&Mem> = (0..K as u8)
                    .filter(|&k| c.honest(k) && s.m[k as usize].ep != NONE)
                    .map(|k| &s.m[k as usize])
                    .collect();
                hm.iter()
                    .all(|a| hm.iter().all(|b| (0..EM).all(|e| e as u8 >= a.ep.min(b.ep) || a.ends[e] == b.ends[e])))
            }),
            // Nothing new becomes final in an epoch after its E: a cut the two-chain rule finalized above E carries
            // E's frame and entries.
            Property::always("final log ends at E", |c: &Epochs, s: &EState| {
                (0..K as u8).filter(|&k| c.honest(k)).all(|k| !s.m[k as usize].past_e)
            }),
            // A rechained key's old chain is closed at E: none of its entries is covered after.
            Property::always("one live chain per key", |c: &Epochs, s: &EState| {
                (0..K as u8).filter(|&k| c.honest(k)).all(|k| !s.m[k as usize].two_chains)
            }),
            Property::sometimes("a cut final", |c: &Epochs, s: &EState| {
                (0..K as u8).any(|k| c.honest(k) && s.m[k as usize].fin.h > 0)
            }),
            Property::sometimes("epoch changed", |c: &Epochs, s: &EState| {
                (0..K as u8).any(|k| c.honest(k) && s.m[k as usize].ep != NONE && s.m[k as usize].ep >= 1)
            }),
            // An honest key never votes for two blocks in one (epoch, view).
            Property::always("no honest double vote", |c: &Epochs, s: &EState| {
                let votes: Vec<(u8, Blk)> = s
                    .net
                    .iter()
                    .filter_map(|m| if let Msg::Vote { voter, b } = m { Some((*voter, *b)) } else { None })
                    .collect();
                votes
                    .iter()
                    .all(|(v, b)| !c.honest(*v) || votes.iter().all(|(v2, b2)| v2 != v || b2.ev() != b.ev() || b2 == b))
            }),
            // The lock: an honest key's timeout for a view never carries a high_qc below the parent of a block it
            // voted for in an earlier view of that epoch.
            Property::always("timeouts keep the lock", |c: &Epochs, s: &EState| {
                s.net.iter().all(|m| {
                    let Msg::Timeout { voter, ev, high } = m else { return true };
                    if !c.honest(*voter) {
                        return true;
                    }
                    let ep = ev >> 4;
                    s.net.iter().all(|m2| match m2 {
                        Msg::Vote { voter: v2, b } if v2 == voter && b.epoch() == ep && b.ev() < *ev => {
                            c.pev(high, ep) >= c.pev(&b.parent(), ep)
                        }
                        _ => true,
                    })
                })
            }),
            // The change happened, and a cut after it became final (the model is not vacuous).
            Property::sometimes("changed and went on", |c: &Epochs, s: &EState| {
                (0..K as u8).any(|k| {
                    c.honest(k)
                        && s.m[k as usize].ep != NONE
                        && s.m[k as usize].ep >= 1
                        && s.m[k as usize].fin.epoch() >= 1
                })
            }),
        ]
    }
}

impl Epochs {
    fn keep_productive(&self, s: &EState, acts: &mut Vec<EAction>) {
        if self.productive_only {
            acts.retain(|a| self.next_state(s, a.clone()).is_some());
        }
    }

    fn author(&self, s: &mut EState, e: u8, stamp: u8) {
        let d = &self.entries[e as usize];
        s.authored |= 1 << e;
        s.stamp[e as usize] = stamp;
        s.m[d.author as usize].held |= 1 << e;
        for to in 0..K as u8 {
            if to != d.author && self.honest(to) {
                s.net.insert(Msg::Entry { to, e });
            }
        }
        if let Some(z) = self.byz {
            s.m[z as usize].held |= 1 << e;
        }
    }

    /// A restarted member's Rechain is final at it (it has moved past the epoch whose E carried it).
    fn restart_done(&self, s: &EState, k: u8) -> bool {
        let mm = &s.m[k as usize];
        mm.ep != NONE && mm.ep > 0 && self.chain(s, &mm.fin).rechained[k as usize] != NONE
    }
}

fn ent(name: &'static str, author: u8, chain: u8, prev: u8, stamp: u8, kind: Kind, at_start: bool) -> EDef {
    EDef { name, author, chain, prev, stamp, kind, at_start }
}

/// One row of the v1 suite: a configuration, the properties it must break (empty: none, and every example found),
/// and the depth bound.
pub struct Row {
    pub model: Epochs,
    pub expect_fail: &'static [&'static str],
    pub depth: usize,
    pub what: &'static str,
    /// Random walks instead of the bounded search: for controls whose counterexample is deeper than the bound.
    pub random: bool,
}

const ALL: u8 = 0b1111;

pub fn base(name: &'static str, m0: u8, entries: Vec<EDef>) -> Epochs {
    Epochs {
        name,
        m0,
        byz: None,
        byz_authorized: false,
        auth: m0,
        restart: None,
        entries,
        views: 3,
        mutation: None,
        offset: 0,
        byz_proposes: true,
        zombie: false,
        productive_only: false,
    }
}

/// Join 2 -> 3: members 1 and 2; 1 admits 3; 2 has an entry in flight; 3 authors once admitted.
pub fn join_2to3() -> Epochs {
    base(
        "join-2to3",
        0b0110,
        vec![
            ent("admit(3)", 1, 0, NONE, 0, Kind::Admit(3), true),
            ent("o2", 2, 0, NONE, 1, Kind::Ord, true),
            ent("j3", 3, 0, NONE, NONE, Kind::Ord, false),
        ],
    )
}

/// Join 1 -> 2 (M1's join from a cut): member 1 plays alone (q = 1) and admits 2, then makes an entry of its own; 2
/// authors once admitted. Epoch 1 has q = 2, so the joiner's vote is needed from its first cut on.
pub fn join_1to2() -> Epochs {
    base(
        "join-1to2",
        0b0010,
        vec![
            ent("admit(2)", 1, 0, NONE, 0, Kind::Admit(2), true),
            ent("o1", 1, 0, 0, 1, Kind::Ord, true),
            ent("j2", 2, 0, NONE, NONE, Kind::Ord, false),
        ],
    )
}

pub fn leave_3to2() -> Epochs {
    base(
        "leave-3to2",
        0b1110,
        vec![ent("leave(3)", 3, 0, NONE, 0, Kind::Leave, true), ent("o1", 1, 0, NONE, 1, Kind::Ord, true)],
    )
}

/// Leave 2 -> 1 (M1 demo step 8): members 1 and 2; 2 leaves, 1 has an entry in flight. Epoch 1 has one member and
/// q = 1, so 1 certifies alone. At n = 2 the suffix needs the leaver's vote, so it lingers until E is final.
pub fn leave_2to1() -> Epochs {
    base(
        "leave-2to1",
        0b0110,
        vec![ent("leave(2)", 2, 0, NONE, 0, Kind::Leave, true), ent("o1", 1, 0, NONE, 1, Kind::Ord, true)],
    )
}

pub fn join_3to4() -> Epochs {
    base(
        "join-3to4",
        0b1110,
        vec![
            ent("admit(0)", 1, 0, NONE, 0, Kind::Admit(0), true),
            ent("o2", 2, 0, NONE, 1, Kind::Ord, true),
            ent("j0", 0, 0, NONE, NONE, Kind::Ord, false),
        ],
    )
}

/// Remove 4 -> 3: member 0 is Byzantine and not authorized; 1 removes it.
pub fn remove_4to3() -> Epochs {
    let mut m = base(
        "remove-4to3",
        ALL,
        vec![ent("remove(0)", 1, 0, NONE, 0, Kind::Remove(0), true), ent("o2", 2, 0, NONE, 1, Kind::Ord, true)],
    );
    m.byz = Some(0);
    m.auth = 0b1110;
    m
}

/// As remove-4to3, but governance authorized the Byzantine member, and it signs Remove{1}: an admin that turns
/// Byzantine can shrink the line below its fault tolerance (n = 3, t = 0). The boundary: agreement may break.
pub fn remove_authorized_byz() -> Epochs {
    let mut m = remove_4to3();
    m.name = "remove-auth-byz";
    m.byz_authorized = true;
    m.entries = vec![ent("remove(1)", 0, 0, NONE, 0, Kind::Remove(1), true), ent("o2", 2, 0, NONE, 1, Kind::Ord, true)];
    m
}

/// A restart at n = 3: member 1 has one entry at the start and may author another on its old chain before it
/// restarts; it signs its Rechain, and once that is final authors on its new chain.
pub fn restart_3() -> Epochs {
    let mut m = base(
        "restart-3",
        0b1110,
        vec![
            ent("o1a", 1, 0, NONE, 0, Kind::Ord, true),
            ent("o1b", 1, 0, 0, NONE, Kind::Ord, false),
            ent("rechain(1)", 1, 1, NONE, NONE, Kind::Rechain, false),
            ent("n1", 1, 1, 2, NONE, Kind::Ord, false),
        ],
    );
    m.restart = Some(1);
    m.zombie = true;
    m
}

/// A restart at n = 4 with member 0 Byzantine (for the forgetting controls).
pub fn restart_4byz() -> Epochs {
    let mut m = base(
        "restart-4byz",
        ALL,
        vec![ent("rechain(1)", 1, 1, NONE, NONE, Kind::Rechain, false), ent("o2", 2, 0, NONE, 1, Kind::Ord, true)],
    );
    m.restart = Some(1);
    m.byz = Some(0);
    m.auth = 0b1110;
    m
}

/// Two joins at once: 1 admits 3 and 2 admits 0.
pub fn burst_2to4() -> Epochs {
    base(
        "burst-2to4",
        0b0110,
        vec![
            ent("admit(3)", 1, 0, NONE, 0, Kind::Admit(3), true),
            ent("admit(0)", 2, 0, NONE, 0, Kind::Admit(0), true),
        ],
    )
}

/// The Byzantine member only votes and times out (no proposals): a smaller space for the checks whose attack is on
/// votes and timeouts.
pub fn quiet_byz(mut m: Epochs) -> Epochs {
    m.byz_proposes = false;
    m
}

/// (b): leave-3to2 with four views. Honest members leave epoch e once E is final, so a non-empty suffix becomes
/// final only when E is finalized indirectly: E certified, a view timed out, then a suffix cut and its child in
/// consecutive views.
pub fn ctl_b() -> Epochs {
    let mut m = leave_3to2().mutate(EMut::NonEmptySuffix);
    m.views = 4;
    m
}

/// (e): leave-3to2 with five views: a member that misses the certificate finalizing E's first suffix cut finalizes a
/// later one, and takes that as the epoch's end.
pub fn ctl_e() -> Epochs {
    let mut m = leave_3to2().mutate(EMut::EndAtFinalizer);
    m.views = 5;
    m
}

/// (c): remove-4to3 with the removed Byzantine member voting and timing out in epoch 1 and counted; four views.
pub fn ctl_c() -> Epochs {
    let mut m = quiet_byz(remove_4to3()).mutate(EMut::WrongEpochVote);
    m.entries.truncate(1);
    m.views = 4;
    m
}

pub fn suite() -> Vec<Row> {
    let d = 22;
    let named = |mut m: Epochs, name: &'static str| {
        m.name = name;
        m
    };
    vec![
        Row {
            model: join_1to2(),
            expect_fail: &[],
            depth: d,
            random: false,
            what: "join 1 -> 2 (q 1 -> 2; M1's join)",
        },
        Row { model: join_2to3(), expect_fail: &[], depth: d, random: false, what: "join 2 -> 3 (q 2 -> 2)" },
        Row { model: leave_3to2(), expect_fail: &[], depth: d, random: false, what: "leave 3 -> 2 (q 2 -> 2)" },
        Row {
            model: leave_2to1(),
            expect_fail: &[],
            depth: d,
            random: false,
            what: "leave 2 -> 1 (q 2 -> 1; M1 step 8)",
        },
        Row { model: join_3to4(), expect_fail: &[], depth: d, random: false, what: "join 3 -> 4 (q 2 -> 3)" },
        Row {
            model: remove_4to3(),
            expect_fail: &[],
            depth: d,
            random: false,
            what: "remove the Byzantine member 4 -> 3 (q 3 -> 2)",
        },
        Row {
            model: restart_3(),
            expect_fail: &[],
            depth: d,
            random: false,
            what: "restart in place at n = 3 (Rechain)",
        },
        Row { model: burst_2to4(), expect_fail: &[], depth: d, random: false, what: "two joins at once 2 -> 4" },
        Row {
            model: remove_authorized_byz(),
            expect_fail: &["agreement", "same members per epoch"],
            depth: d,
            random: true,
            what: "BOUNDARY: an authorized Byzantine member removes an honest one (n = 3, t = 0 after)",
        },
        Row {
            model: named(join_2to3().mutate(EMut::AtInclusion), "ctl-a-inclusion"),
            expect_fail: &["agreement", "same members per epoch"],
            depth: d,
            random: false,
            what: "(a) a change takes effect at inclusion",
        },
        Row {
            model: named(ctl_b(), "ctl-b-suffix"),
            expect_fail: &["final log ends at E"],
            depth: d,
            random: true,
            what: "(b) a non-empty suffix proposed and accepted",
        },
        Row {
            model: named(ctl_c(), "ctl-c-epochvote"),
            expect_fail: &["agreement", "same members per epoch"],
            depth: 34,
            random: true,
            what: "(c) a vote counted in another epoch",
        },
        Row {
            model: named(restart_3().mutate(EMut::OldChainOpen), "ctl-d-oldchain"),
            expect_fail: &["one live chain per key"],
            depth: d,
            random: true,
            what: "(d) the old chain stays open after E",
        },
        Row {
            model: named(ctl_e(), "ctl-e-finalizer"),
            expect_fail: &["same members per epoch", "agreement"],
            depth: d,
            random: true,
            what: "(e) the final log ends at the finalizing cut",
        },
        Row {
            model: named(quiet_byz(restart_4byz()).mutate(EMut::ForgetViews), "ctl-f1-views"),
            expect_fail: &["no honest double vote", "agreement"],
            depth: d,
            random: false,
            what: "(f1) a restart forgets its views",
        },
        Row {
            model: named(quiet_byz(restart_4byz()).mutate(EMut::ForgetHighQc), "ctl-f2-highqc"),
            expect_fail: &["timeouts keep the lock", "agreement"],
            depth: d,
            random: false,
            what: "(f2) a restart forgets its high_qc",
        },
    ]
}

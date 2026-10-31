//! Suspicion, frozen heads, eviction votes and lying-vote checks (design 10.8, review rounds 2-3).

use super::*;

impl WorldSim {
    /// Two signed entries of `m` contradict each other (`a` at seq `k`, `b` another version of it
    /// or a successor that names another version): record the proof, withdraw what it disputes,
    /// and vote to remove `m` in this epoch (design 9.3, 10.8).
    pub(super) fn on_proof(&mut self, m: MemberId, k: u32, a: EntryId, b: EntryId) -> Vec<Entry> {
        if !self.proofs.contains_key(&m) {
            self.stats.equivocations += 1;
            self.ev(format!("EQUIVOCATION proof: member {m} signed two versions of seq {k} ({a:016x} / {b:016x})"));
        }
        let (k0, a0, b0) = match self.proofs.get(&m) {
            Some(&(k1, a1, b1)) if k1 <= k => (k1, a1, b1),
            _ => (k, a, b),
        };
        self.proofs.insert(m, (k0, a0, b0));
        let mut out = Vec::new();
        if m != self.me && self.members.contains(&m) && !self.voted(m) {
            out.extend(self.freeze_and_vote(m, k0, 0));
        }
        self.reconcile(m);
        out
    }

    /// Whether this replica has voted against `m` in the current epoch.
    pub(super) fn voted(&self, m: MemberId) -> bool {
        self.votes.get(&m).is_some_and(|v| v.contains_key(&self.me))
    }

    /// Freeze what this member will ever reference of m's chain in this epoch, and say so in a
    /// vote. `k0` is the disputed seq of a proof, or 0 for a member presumed crashed or too slow.
    pub(super) fn freeze_and_vote(&mut self, m: MemberId, k0: u32, kick: EntryId) -> Option<Entry> {
        if self.withhold_vc {
            return None;
        }
        let f = self.contiguous.get(&m).copied().unwrap_or(0);
        // A faulty member (fixture) votes without freezing: it keeps processing that chain.
        if !self.byz_ally {
            self.frozen.insert(m, f);
        }
        let (f, fid) = match self.hidden.as_ref().and_then(|h| h.get(&m)) {
            Some(&(hs, hid)) if hs < f => (hs, hid),
            _ => (f, if f == 0 { 0 } else { self.by_author[&(m, f)] }),
        };
        let (f, fid) = if self.lie_in_votes { (0, 0) } else { (f, fid) };
        let v = Entry {
            member: self.me,
            seq: self.my_seq + 1,
            prev: self.my_last,
            stamp: self.frame.max(self.now),
            payload: Payload::Vote { epoch: self.epoch, target: m, seq: k0, frozen: (f, fid), kick },
            carried: None,
        };
        Some(self.publish(v))
    }

    /// Review R3-2 (a silent member) and R3-9 (a member too slow to keep up), and proofs carried
    /// into a new epoch: vote to remove members. A member none of whose entries has reached this
    /// replica, directly or fetched, for `suspect_after` frames is presumed crashed, unless another
    /// member's acks show more of its chain than this replica holds (then this replica is behind,
    /// and fetches). The view change (10.8) then decides which removals happen and their cuts. A
    /// vote stands until its epoch ends; a member heard from again is kept by a reset (P2-4).
    /// Returns the votes to broadcast.
    pub fn suspects_due(&mut self) -> Vec<Entry> {
        if !self.members.contains(&self.me) || self.closed_at.is_some() {
            return vec![];
        }
        let mut out = Vec::new();
        // D80: a pending kick outlives an epoch that did not carry it out: freeze its target again.
        let kicks: Vec<(EntryId, MemberId, MemberId)> = self
            .admin_pending
            .iter()
            .filter_map(|id| {
                let e = self.entries.get(id)?;
                match e.payload {
                    Payload::Kick { target, .. } => Some((*id, e.member, target)),
                    _ => None,
                }
            })
            .collect();
        for (id, author, t) in kicks {
            if self.members.contains(&t) && self.members.contains(&author) && self.perms_of(author) & perm::KICK != 0 && !self.voted(t) && !self.withhold_vc {
                if t == self.me {
                    out.extend(self.self_kick_vote(id));
                } else {
                    out.extend(self.freeze_and_vote(t, 0, id));
                }
            }
        }
        // A proof outlives an epoch that did not remove its member: vote again.
        let proven: Vec<(MemberId, u32)> = self
            .proofs
            .iter()
            .map(|(m, p)| (*m, p.0))
            .chain(self.liars.iter().map(|(m, p)| (*m, self.entries.get(&p.b).map_or(1, |e| e.seq))))
            .collect();
        for (m, k) in proven {
            if m != self.me && self.members.contains(&m) && !self.voted(m) {
                out.extend(self.freeze_and_vote(m, k.max(1), 0));
            }
        }
        if self.byz_ally {
            let targets: Vec<MemberId> = self.votes.keys().copied().filter(|t| *t != self.me && self.members.contains(t)).collect();
            for t in targets {
                if !self.voted(t) {
                    out.extend(self.freeze_and_vote(t, 0, 0));
                }
            }
            return out;
        }
        // Review VC-5: a member whose acks declare an epoch newer than mine, without my reaching it,
        // caps what I count of its chain; for as long as a silent member is given, I vote it out.
        if self.fixes_off & fix::VC5 == 0 {
            let members: Vec<MemberId> = self.members.iter().copied().filter(|&j| j != self.me).collect();
            for j in members {
                let newer = self.ack_epoch_from.get(&j).is_some_and(|a| a.range(self.epoch + 1..).next().is_some());
                if !newer {
                    self.newer_since.remove(&j);
                    continue;
                }
                let since = *self.newer_since.entry(j).or_insert(self.now);
                if self.now >= since + self.cfg.suspect_after && !self.voted(j) && !self.withhold_vc {
                    self.ev(format!("member {j}'s acks declare an epoch newer than {} that no decision here backs: vote to remove it", self.epoch));
                    out.extend(self.freeze_and_vote(j, 0, 0));
                }
            }
        }
        // Removal on silence needs a quorum without the removed member, so a two-member world
        // never suspects: both members pause instead (D7).
        if self.members.len() < 3 {
            return out;
        }
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for j in members {
            if j == self.me || self.voted(j) {
                continue;
            }
            let heard = self.heard.get(&j).copied().unwrap_or(0);
            // Review R3-9 (demotion, 10.9 item 6): a member whose newest validated horizon trails
            // this replica's present by more than half the speculation limit, for as long as a
            // silent member is given, is voted out the same way. Its link cannot carry the session;
            // it can follow as a spectator. A short congestion (c1: about 2 s) does not qualify.
            let h = self.chain_state.get(&j).and_then(|c| c.0).unwrap_or(0);
            let since = if self.now > h + self.cfg.window / 2 {
                *self.lagging.entry(j).or_insert(self.now)
            } else {
                self.lagging.remove(&j);
                self.now
            };
            let silent = self.now >= heard + self.cfg.suspect_after;
            let slow = self.now >= since + self.cfg.suspect_after;
            if !silent && !slow {
                continue;
            }
            let mine = self.contiguous.get(&j).copied().unwrap_or(0);
            let ahead: Vec<(MemberId, u32)> = self
                .refs
                .iter()
                .filter(|((k, a), _)| *a == j && *k != self.me && *k != j)
                .filter_map(|((k, _), r)| r.keys().next_back().filter(|&&s| s > mine).map(|&s| (*k, s)))
                .collect();
            if !ahead.is_empty() {
                for (k, s) in ahead {
                    self.want_from(k, j, mine + 1, s);
                }
                continue;
            }
            if self.withhold_vc {
                continue;
            }
            if silent {
                self.stats.suspicions += 1;
                self.ev(format!("member {j} silent for {} frames: presumed crashed; vote to remove it", self.now - heard));
            } else {
                self.stats.demotions += 1;
                self.ev(format!("member {j}'s horizon has trailed the present by over {} frames for {} frames: vote to demote it", self.cfg.window / 2, self.now - since));
            }
            out.extend(self.freeze_and_vote(j, 0, 0));
        }
        out.extend(self.withholders_due());
        out
    }

    /// P2-4: a member whose acks have referenced no more of some chain for `suspect_after` frames,
    /// while every other member (this replica included) holds and references more of it, stalls that
    /// chain's stability, and so everyone's confirmation, as surely as a silent member. It is voted
    /// out the same way. A member that acks slowly still advances, and is not.
    pub(super) fn withholders_due(&mut self) -> Vec<Entry> {
        let mut out = Vec::new();
        if self.now < self.withhold_checked + 30 || self.withhold_vc || !self.cfg.ack_heads {
            return out;
        }
        self.withhold_checked = self.now;
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        let newest = |s: &Self, k: MemberId, y: MemberId| s.refs.get(&(k, y)).and_then(|r| r.keys().next_back()).copied().unwrap_or(0);
        // Per chain y: the two lowest references to it among the other members, so "every member but
        // j" is one comparison (O(n^2) per check, not O(n^3)).
        let mut low: BTreeMap<MemberId, ((u32, MemberId), u32)> = BTreeMap::new();
        for &y in &members {
            if self.voted(y) {
                continue;
            }
            let mut best = (u32::MAX, 0);
            let mut second = u32::MAX;
            for &k in &members {
                if k == y || k == self.me {
                    continue;
                }
                let r = newest(self, k, y);
                if r < best.0 {
                    second = best.0;
                    best = (r, k);
                } else if r < second {
                    second = r;
                }
            }
            low.insert(y, (best, second));
        }
        for &j in &members {
            if j == self.me || self.voted(j) {
                continue;
            }
            let mut blocks = None;
            for (&y, &((min1, who), min2)) in &low {
                // Review VC-9: once j has voted against y, its acks no longer reference y's chain.
                if y == j || self.votes.get(&y).is_some_and(|v| v.contains_key(&j)) {
                    continue;
                }
                let mine = if y == self.me { self.my_seq } else { self.contiguous.get(&y).copied().unwrap_or(0) };
                let others = mine.min(if who == j { min2 } else { min1 });
                let theirs = newest(self, j, y);
                let still = self.ref_advanced.get(&(j, y)).map_or(self.now >= self.cfg.suspect_after, |&t| self.now >= t + self.cfg.suspect_after);
                if theirs < others && still {
                    blocks = Some(y);
                    break;
                }
            }
            if let Some(y) = blocks {
                self.stats.withholders += 1;
                self.ev(format!("member {j} has referenced no more of member {y}'s chain for {} frames while every other member has: vote to remove it", self.cfg.suspect_after));
                out.extend(self.freeze_and_vote(j, 0, 0));
            }
        }
        out
    }

    /// Which of `m`'s entries count after an equivocation proof (review NEW-2): until a view change
    /// removes it, every entry from the disputed seq on is withdrawn unless it is stable (held
    /// identically by every member, so possibly confirmed somewhere); after it, exactly the cut.
    pub(super) fn reconcile(&mut self, m: MemberId) {
        let k = self.proofs.get(&m).map(|p| p.0);
        let decided = self.evicted.get(&m).map(|p| p.0);
        let stable = self.stable_seq(m);
        let ids: Vec<(u32, EntryId)> = self.entries.iter().filter(|(_, e)| e.member == m).map(|(id, e)| (e.seq, *id)).collect();
        for (seq, id) in ids {
            let mine = self.by_author.get(&(m, seq)) == Some(&id);
            let keep = match (decided, k) {
                (Some(cut), _) => mine && seq <= cut,
                (None, Some(k)) => mine && (seq < k || seq <= stable),
                (None, None) => mine,
            };
            let changed = if keep { self.disputed.remove(&id) } else { self.disputed.insert(id) };
            if changed {
                self.set_eff(id);
            }
        }
    }

    /// Review R3-1: whether `voter`'s frozen head of `target`'s chain extends every reference to
    /// that chain the voter published before its vote. A vote that drops one is a lie, and the
    /// ack it contradicts plus the vote prove its voter faulty.
    pub(super) fn vote_check(&mut self, target: MemberId, voter: MemberId, frozen: (u32, EntryId)) -> VoteCheck {
        let refs: Vec<(u32, EntryId, EntryId)> =
            self.refs.get(&(voter, target)).map(|r| r.iter().rev().map(|(s, (id, ack))| (*s, *id, *ack)).collect()).unwrap_or_default();
        let (mut s, mut id) = frozen;
        for (rs, rid, ack) in refs {
            if rs > frozen.0 {
                return VoteCheck::Lie(ack);
            }
            while s > rs {
                let Some(e) = self.entries.get(&id) else {
                    self.want_from(voter, target, s, s);
                    return VoteCheck::Missing;
                };
                id = e.prev;
                s -= 1;
            }
            if id != rid {
                return VoteCheck::Lie(ack);
            }
        }
        VoteCheck::Valid
    }

    /// `voter`'s vote `vote` contradicts its own earlier ack `ack`: it is proven faulty.
    pub(super) fn on_lie(&mut self, voter: MemberId, ack: EntryId, vote: EntryId) {
        if self.liars.contains_key(&voter) || voter == self.me {
            return;
        }
        self.liars.insert(voter, Proof { a: ack, b: vote });
        self.stats.lying_votes += 1;
        self.ev(format!("LYING VOTE: member {voter}'s vote drops entries its own ack referenced: proven faulty"));
    }
}

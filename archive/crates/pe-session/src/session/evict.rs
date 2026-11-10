//! Suspicion, frozen heads, eviction votes and lying-vote checks (design 10.8, review rounds 2-3).

use super::*;

impl WorldSim {
    /// Two signed entries of `m` contradict each other (`a` at seq `k`, `b` another version of it
    /// or a successor that names another version): record the proof, withdraw what it disputes,
    /// and vote to remove `m` in this epoch (design 9.3, 10.8).
    pub(super) fn on_proof(&mut self, m: MemberId, k: u32, a: EntryId, b: EntryId) -> Vec<Entry> {
        if !self.vcm.proofs.contains_key(&m) {
            self.stats.equivocations += 1;
            self.ev(format!("EQUIVOCATION proof: member {m} signed two versions of seq {k} ({a:?} / {b:?})"));
        }
        let old_k = self.vcm.proofs.get(&m).map(|p| p.0);
        let (k0, a0, b0) = match self.vcm.proofs.get(&m) {
            Some(&(k1, a1, b1)) if k1 <= k => (k1, a1, b1),
            _ => (k, a, b),
        };
        self.vcm.proofs.insert(m, (k0, a0, b0));
        // Review PF-1: m's proposals already held past its fork may never link here: keep them by value.
        // Once per proof, or when it moves earlier (VC7R-6); later ones are kept as they arrive (insert).
        if self.fixes_off & fix::PF1 == 0 && old_k.is_none_or(|ok| k0 < ok) {
            let held: Vec<EntryId> = self.by_author.range((m, k0)..=(m, u32::MAX)).map(|(_, &id)| id).collect();
            for id in held {
                self.note_proposal(id);
            }
        }
        let mut out = Vec::new();
        // Review P23-11: under static membership nobody is voted out (and `voted` is never true, so
        // each later entry of the forker would author another vote).
        if m != self.me && self.vcm.members.contains(&m) && !self.is_static() && !self.voted(m) {
            out.extend(self.freeze_and_vote(m, k0, EntryId::NONE));
        }
        self.reconcile(m);
        out
    }

    /// Whether this replica has voted against `m` in the current epoch.
    pub(super) fn voted(&self, m: MemberId) -> bool {
        self.vcm.votes.get(&m).is_some_and(|v| v.contains_key(&self.me))
    }

    /// Freeze what this member will ever reference of m's chain in this epoch, and say so in a
    /// vote. `k0` is the disputed seq of a proof, or 0 for a member presumed crashed or too slow.
    pub(super) fn freeze_and_vote(&mut self, m: MemberId, k0: u32, kick: EntryId) -> Option<Entry> {
        if self.vcm.withhold_vc {
            return None;
        }
        let f = self.contiguous.get(&m).copied().unwrap_or(0);
        // A faulty member (fixture) votes without freezing: it keeps processing that chain.
        if !self.vcm.byz_ally {
            self.vcm.frozen.insert(m, f);
        }
        let (f, fid) = match self.hidden.as_ref().and_then(|h| h.get(&m)) {
            Some(&(hs, hid)) if hs < f => (hs, hid),
            _ => (f, if f == 0 { EntryId::NONE } else { self.by_author[&(m, f)] }),
        };
        let (f, fid) = if self.lie_in_votes { (0, EntryId::NONE) } else { (f, fid) };
        let v = Entry {
            member: self.me,
            seq: self.my_seq + 1,
            prev: self.my_last,
            stamp: self.frame.max(self.now),
            payload: Payload::Vote { epoch: self.vcm.epoch, target: m, seq: k0, frozen: (f, fid), kick },
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
    pub fn suspects_due(&mut self) -> Vec<SignedEntry> {
        if self.is_static() {
            return Vec::new();
        }
        let out = self.suspects_due_entries();
        self.signed_all(out)
    }

    fn suspects_due_entries(&mut self) -> Vec<Entry> {
        if !self.vcm.members.contains(&self.me) || self.vcm.closed_at.is_some() {
            return vec![];
        }
        let mut out = Vec::new();
        // D80: a pending kick outlives an epoch that did not carry it out: freeze its target again.
        let kicks: Vec<(EntryId, MemberId, MemberId, Rejoin)> = self
            .vcm.admin_pending
            .iter()
            .filter_map(|id| {
                let e = self.entries.get(id)?;
                match e.payload {
                    Payload::Kick { target, rejoin, .. } => Some((*id, e.member, target, rejoin)),
                    _ => None,
                }
            })
            .collect();
        for (id, author, t, rejoin) in kicks {
            if self.vcm.members.contains(&t) && self.vcm.members.contains(&author) && self.admin_run().kick_ok(author, t, rejoin) && !self.voted(t) && !self.vcm.withhold_vc {
                if t == self.me {
                    out.extend(self.self_kick_vote(id));
                } else {
                    out.extend(self.freeze_and_vote(t, 0, id));
                }
            }
        }
        // A proof outlives an epoch that did not remove its member: vote again.
        let proven: Vec<(MemberId, u32)> = self
            .vcm.proofs
            .iter()
            .map(|(m, p)| (*m, p.0))
            .chain(self.vcm.liars.iter().map(|(m, p)| (*m, self.entries.get(&p.b).map_or(1, |e| e.seq))))
            .collect();
        for (m, k) in proven {
            if m != self.me && self.vcm.members.contains(&m) && !self.voted(m) {
                out.extend(self.freeze_and_vote(m, k.max(1), EntryId::NONE));
            }
        }
        if self.vcm.byz_ally {
            let targets: Vec<MemberId> = self.vcm.votes.keys().copied().filter(|t| *t != self.me && self.vcm.members.contains(t)).collect();
            for t in targets {
                if !self.voted(t) {
                    out.extend(self.freeze_and_vote(t, 0, EntryId::NONE));
                }
            }
            return out;
        }
        // Trust survey #4 (user rulings, 2026-11-01): a stale or unexplained cap (VC3R-2, VC4R-1), a
        // kick vote whose kick nobody delivers (VCR-1), a vote whose frozen head nobody delivers
        // (VC4R-5), withheld references (P2-4) and acks declaring an epoch I have not reached (VC-5)
        // no longer get their authors voted out. Each holds confirmation until an admin kicks the
        // member; a kick is a view change, which needs no confirmation, and its decision ends every
        // cap, freeze and vote of the epoch. VC-5's guards stay: an ack's epoch never falls and rises
        // by at most one (chain-local, in validation), and an ack from an epoch newer than mine never
        // counts toward my confirmation.
        // Review VC4R-3, and the generator's RG-1: a member whose acks reference more of some chain
        // than I hold, I fetch the difference from. The references that count toward stability
        // (`stable_seq`) are only the verified ones: a seq I hold in the version the reference names.
        // A faulty member that references entries nobody has (a fake head, with or without a small
        // live offset) advances its raw reference but not its verified one, so its fake head never
        // caps confirmation. Here I only issue the fetches; the counting is in `stable_seq`.
        if self.fixes_off & fix::VC4R3 == 0 {
            let members: Vec<MemberId> = self.vcm.members.iter().copied().collect();
            let want: Vec<(MemberId, MemberId, u32, u32)> = self
                .refs
                .iter()
                .filter(|((k, y), _)| *k != self.me && *k != *y && members.contains(k) && members.contains(y))
                .filter_map(|((k, y), r)| {
                    let mine = self.contiguous.get(y).copied().unwrap_or(0);
                    r.keys().next_back().filter(|&&top| top > mine).map(|&top| (*k, *y, mine + 1, top))
                })
                .collect();
            for (k, y, from, to) in want {
                self.want_from(k, y, from, to);
            }
        }
        // Review VC4R-4 and VC4R-5: what is still missing is asked for again. A removed member's cut
        // branch is walked again; a standing kick vote's kick and a standing vote's frozen head are
        // fetched by id. A vote whose frozen head nobody delivers within half the window holds back no
        // reset (`fresh`, in the change proposal); its voter is left to admins (trust survey #4).
        if self.fixes_off & fix::VC4R4 == 0 {
            let pending: Vec<MemberId> = self.vcm.evicted.keys().copied().filter(|x| !self.vcm.cut_done.contains(x)).collect();
            for x in pending {
                self.adopt_cut(x);
            }
        }
        if self.fixes_off & (fix::VC4R4 | fix::VC4R5) == 0 {
            let standing: Vec<(MemberId, MemberId, VoteRec)> = self.vcm.votes.iter().flat_map(|(y, vs)| vs.iter().map(move |(v, r)| (*y, *v, *r))).collect();
            for (y, v, r) in standing {
                if r.kick != EntryId::NONE && !self.entries.contains_key(&r.kick) {
                    self.want_entry_all(r.kick, 0);
                }
                if r.frozen.0 > 0 && !self.head_held(y, r.frozen) {
                    self.want_entry(v, r.frozen.1, 15);
                    self.want_entry_all(r.frozen.1, 15);
                }
            }
        }
        // Round 3 (VCR-4), kept only to show VC3R-2: a member whose votes or caps resets kept
        // overruling was struck, and voted out at three strikes. Hostile timing by another member
        // could make that an honest one.
        if self.fixes_off & fix::VCR4 == 0 && self.fixes_off & fix::VC3R2 != 0 {
            let struck: Vec<MemberId> = self.vcm.members.iter().copied().filter(|&j| j != self.me && !self.voted(j) && self.struck_out(j)).collect();
            for j in struck {
                if !self.vcm.withhold_vc {
                    self.ev(format!("member {j}'s votes or caps were overruled in {STRIKES_OUT} of the last {STRIKE_EPOCHS} epochs: vote to remove it"));
                    out.extend(self.freeze_and_vote(j, 0, EntryId::NONE));
                }
            }
        }
        // Removal on silence needs a quorum without the removed member, so a two-member world
        // never suspects: both members pause instead (D7).
        if self.vcm.members.len() < 3 {
            return out;
        }
        let members: Vec<MemberId> = self.vcm.members.iter().copied().collect();
        for j in members {
            if j == self.me || self.voted(j) || self.held(j) {
                continue;
            }
            let heard = self.heard.get(&j).copied().unwrap_or(0);
            // Review R3-9 (demotion, 10.9 item 6): a member whose newest validated horizon trails
            // this replica's present by more than half the speculation limit, for as long as a
            // silent member is given, is voted out the same way. Its link cannot carry the session;
            // it can follow as a spectator. A short congestion (c1: about 2 s) does not qualify.
            let h = self.chain_state.get(&j).and_then(|c| c.0).unwrap_or(0);
            let since = if self.now > h + self.cfg.window / 2 {
                *self.vcm.lagging.entry(j).or_insert(self.now)
            } else {
                self.vcm.lagging.remove(&j);
                self.now
            };
            let silent = self.now >= heard + self.cfg.suspect_after;
            let slow = self.now >= since + self.cfg.suspect_after;
            if !silent && !slow {
                self.suspect_since.remove(&j);
                continue;
            }
            let suspect_first = *self.suspect_since.entry(j).or_insert(self.now);
            let mine = self.contiguous.get(&j).copied().unwrap_or(0);
            let ahead: Vec<(MemberId, u32)> = self
                .refs
                .iter()
                .filter(|((k, a), _)| *a == j && *k != self.me && *k != j)
                .filter_map(|((k, _), r)| r.keys().next_back().filter(|&&s| s > mine).map(|&s| (*k, s)))
                .collect();
            if !ahead.is_empty() {
                // Review VC4R-3: another member's reference to more of j's chain than I hold spares j
                // while I fetch the difference, but only for a silence limit from when j first fell
                // behind. Round 4 spared j for as long as any such reference stood, so a faulty member's
                // fake reference could spare a slow colluder for good; the bound is now a fixed time, so
                // references that never deliver stop sparing it.
                let bounded = self.fixes_off & fix::VC4R3 == 0;
                for (k, s) in ahead {
                    self.want_from(k, j, mine + 1, s);
                }
                if !bounded || self.now < suspect_first + self.cfg.suspect_after {
                    continue;
                }
            }
            if self.vcm.withhold_vc {
                continue;
            }
            if silent {
                self.stats.suspicions += 1;
                self.ev(format!("member {j} silent for {} frames: presumed crashed; vote to remove it", self.now - heard));
            } else {
                self.stats.demotions += 1;
                self.ev(format!("member {j}'s horizon has trailed the present by over {} frames for {} frames: vote to demote it", self.cfg.window / 2, self.now - since));
            }
            out.extend(self.freeze_and_vote(j, 0, EntryId::NONE));
        }
        out
    }

    /// Which of `m`'s entries count after an equivocation proof (review NEW-2): until a view change
    /// removes it, every entry from the disputed seq on is withdrawn unless it is stable (held
    /// identically by every member, so possibly confirmed somewhere); after it, exactly the cut.
    pub(super) fn reconcile(&mut self, m: MemberId) {
        let k = self.vcm.proofs.get(&m).map(|p| p.0);
        let decided = self.vcm.evicted.get(&m).map(|p| p.0);
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
        if self.vcm.liars.contains_key(&voter) || voter == self.me {
            return;
        }
        self.vcm.liars.insert(voter, Proof { a: ack, b: vote });
        self.stats.lying_votes += 1;
        self.ev(format!("LYING VOTE: member {voter}'s vote drops entries its own ack referenced: proven faulty"));
    }
}

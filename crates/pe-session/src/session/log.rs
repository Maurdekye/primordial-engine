//! The entry log: authoring, insertion, hash chains, validation, stable data and the confirmed frame.

use super::*;

impl WorldSim {
    // ---- log -------------------------------------------------------------------------------------
    //
    // Round-2 protocol (design 10.8; reviews NEW-1, NEW-2; user rule 14:31Z):
    // * every input to a deterministic decision is an entry in its author's hash chain:
    //   perturbations, acks (horizon, holdings, check hash) and eviction votes;
    // * an entry's frame is its stamp. Nothing is ever re-stamped, so an author never sees its own
    //   interaction corrected; lateness is paid by the late receiver's rollback;
    // * validity is chain-local: an entry is void iff its own chain breaks a rule, so every holder
    //   decides the same;
    // * confirmation uses only stable data: a seq of a chain is stable once every member's chain
    //   references it with the same id (Wuu-Bernstein's 2-D time table, over signed hash chains).

    pub fn entry(&self, id: EntryId) -> Option<&Entry> {
        self.entries.get(&id)
    }

    pub fn effective_frame(&self, id: EntryId) -> Option<u64> {
        self.eff.get(&id).copied()
    }

    pub(super) fn is_void(&self, id: EntryId) -> bool {
        self.violations.contains(&id) || self.disputed.contains(&id)
    }

    /// Apply or withdraw an entry's effect on the sim according to its current verdict.
    pub(super) fn set_eff(&mut self, id: EntryId) {
        let e = &self.entries[&id];
        // A member that asked to join acts only once admitted (10.8): until then nothing it signs
        // touches the simulation.
        let counted = e.member == LOGIC || self.members.contains(&e.member) || self.evicted.contains_key(&e.member);
        let acts = counted && !is_protocol(&e.payload);
        let open = self.closed_at.is_none_or(|c| e.stamp < c);
        let new = if acts && open && !self.is_void(id) { Some(e.stamp) } else { None };
        let old = self.eff.get(&id).copied();
        if old == new {
            return;
        }
        if let Some(o) = old {
            if let Some(s) = self.at_frame.get_mut(&o) {
                s.remove(&id);
            }
            self.eff.remove(&id);
            self.mark_dirty(o);
        }
        if let Some(n) = new {
            self.eff.insert(id, n);
            self.at_frame.entry(n).or_default().insert(id);
            self.mark_dirty(n);
        }
    }

    pub(super) fn mark_dirty(&mut self, f: u64) {
        // Review NEW-2: confirmed frames are final. Getting here below `conf` is a protocol bug,
        // never a rollback.
        assert!(
            f >= self.conf,
            "protocol invariant violated: member {} would roll back to frame {f}, below its confirmed frame {}",
            self.me,
            self.conf
        );
        if f < self.frame {
            self.dirty_from = Some(self.dirty_from.map_or(f, |d| d.min(f)));
        }
    }

    pub(super) fn publish(&mut self, e: Entry) -> Entry {
        self.my_seq += 1;
        self.my_last = e.id();
        let se = self.keys.sign(self.world_id, e.clone());
        self.sigs.insert(self.my_last, se.sig);
        let _ = self.insert_entry(e.clone());
        e
    }

    /// `e` with the signature this replica holds for it (every entry it holds came signed, or is
    /// its own).
    pub(super) fn signed(&self, e: Entry) -> SignedEntry {
        let sig = *self.sigs.get(&e.id()).expect("every held entry has its signature");
        SignedEntry { entry: e, sig }
    }

    pub(super) fn signed_all(&self, v: Vec<Entry>) -> Vec<SignedEntry> {
        v.into_iter().map(|e| self.signed(e)).collect()
    }

    /// The entry `id` with its author's signature, for relaying.
    pub fn signed_entry(&self, id: EntryId) -> Option<SignedEntry> {
        self.entries.get(&id).map(|e| self.signed(e.clone()))
    }

    /// Author an entry at the current frame. With `carry`, the entry carries the touched object's
    /// state after this frame: computed by stepping the frame once on a scratch copy.
    pub fn author(&mut self, payload: Payload, carry: Option<u32>) -> SignedEntry {
        let stamp = if carry.is_some() {
            assert!(self.dirty_from.is_none() && self.frame >= self.now, "carrying state needs a caught-up sim");
            self.frame
        } else {
            self.frame.max(self.now)
        };
        let mut e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None };
        if let Some(obj) = carry {
            let scratch = self.save();
            let tmp_id = e.id();
            self.entries.insert(tmp_id, e.clone());
            self.at_frame.entry(stamp).or_default().insert(tmp_id);
            self.eff.insert(tmp_id, stamp);
            self.step_frame(false);
            let st = self.obj_state(obj);
            self.entries.remove(&tmp_id);
            self.at_frame.get_mut(&stamp).unwrap().remove(&tmp_id);
            self.eff.remove(&tmp_id);
            self.outcomes.remove(&tmp_id);
            self.restore(&scratch, stamp);
            e.carried = st.map(|s| (obj, s));
            self.watched.insert(obj);
        }
        let e = self.publish(e);
        self.signed(e)
    }

    /// Author a state-free entry stamped `ahead` frames after the present (opt-in input delay).
    pub fn author_ahead(&mut self, payload: Payload, ahead: u64) -> SignedEntry {
        let stamp = self.frame.max(self.now) + ahead;
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None };
        let e = self.publish(e);
        self.signed(e)
    }

    /// Test fixture for review H5: author with an arbitrary (backdated) stamp.
    pub fn author_backdated(&mut self, payload: Payload, back: u64) -> SignedEntry {
        let stamp = self.frame.max(self.now).saturating_sub(back);
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None };
        let e = self.publish(e);
        self.signed(e)
    }

    /// Author this member's ack: a chain entry stating its horizon (it will author nothing stamped
    /// at or before it), the head it holds of every other chain, and its newest confirmed check
    /// hash (reviews NEW-1, H4).
    pub fn author_ack(&mut self) -> SignedEntry {
        let mut heads = Vec::new();
        for (&q, &held) in &self.contiguous {
            // A vote's frozen head is this replica's last word on that chain in the epoch (R3-1).
            let s = match self.frozen.get(&q) {
                Some(&f) if !self.evicted.contains_key(&q) => held.min(f),
                _ => held,
            };
            if s > 0 && q != self.me && self.cfg.ack_heads {
                match self.hidden.as_ref().and_then(|h| h.get(&q)) {
                    Some(&(hs, hid)) if hs < s => heads.push((q, hs, hid)),
                    _ => heads.push((q, s, self.by_author[&(q, s)])),
                }
            }
        }
        let check = if self.dirty_from.is_none() {
            self.check_hashes.range(..self.conf.min(self.frame)).next_back().map(|(f, h)| (*f, *h))
        } else {
            None
        };
        let stamp = self.frame.max(self.now);
        let horizon = self.now.saturating_sub(1 + self.horizon_lag);
        let e = Entry {
            member: self.me,
            seq: self.my_seq + 1,
            prev: self.my_last,
            stamp,
            payload: Payload::Ack { horizon, heads, check, epoch: self.fake_ack_epoch.unwrap_or(self.epoch) },
            carried: None,
        };
        let e = self.publish(e);
        self.signed(e)
    }

    /// Store a received or fetched entry, once its signature verifies (S3; a bad one is dropped and
    /// counted). Returns entries to broadcast: when it proves equivocation, both versions and this
    /// member's vote.
    pub fn insert(&mut self, se: SignedEntry) -> Vec<SignedEntry> {
        let id = se.id();
        if self.entries.contains_key(&id) {
            return vec![];
        }
        if !self.keys.verify(self.world_id, &se) {
            self.stats.bad_sigs += 1;
            return vec![];
        }
        self.sigs.insert(id, se.sig);
        let out = self.insert_entry(se.entry);
        if !self.entries.contains_key(&id) {
            self.sigs.remove(&id);
        }
        self.signed_all(out)
    }

    /// Store an entry whose signature is verified, or this replica's own.
    pub(super) fn insert_entry(&mut self, e: Entry) -> Vec<Entry> {
        let id = e.id();
        if self.entries.contains_key(&id) || (!self.known.contains(&e.member) && e.member != LOGIC) {
            return vec![];
        }
        if let Some(&other) = self.by_author.get(&(e.member, e.seq)) {
            // Two signed entries for the same (member, seq): a transferable proof (review C2).
            self.entries.insert(id, e.clone());
            self.disputed.insert(id);
            // Another version of a proposal still names a value someone may decide (P2-4).
            self.note_proposal(id);
            let mut out = vec![self.entries[&other].clone(), e.clone()];
            out.extend(self.on_proof(e.member, e.seq, other, id));
            // Review VC-2: the version a removed member's cut names may arrive as a duplicate.
            if self.evicted.contains_key(&e.member) && self.fixes_off & fix::VC2 == 0 {
                self.adopt_cut(e.member);
            }
            return out;
        }
        self.by_author.insert((e.member, e.seq), id);
        if e.member != self.me {
            self.heard.insert(e.member, self.now);
        }
        if let Some((obj, _)) = e.carried {
            self.watched.insert(obj);
        }
        if let Payload::Touch { obj, .. } = e.payload {
            self.watched.insert(obj);
        }
        // Below the confirmed frame only a horizon violation can be new (design 10.8): never apply.
        if e.stamp < self.conf && !is_protocol(&e.payload) {
            self.violations.insert(id);
        }
        let disputed = self.proofs.get(&e.member).is_some_and(|&(k, _, _)| e.seq >= k)
            || self.evicted.get(&e.member).is_some_and(|&(cut, _)| e.seq > cut);
        if disputed {
            self.disputed.insert(id);
        }
        let (member, seq) = (e.member, e.seq);
        self.entries.insert(id, e);
        self.set_eff(id);
        let mut out = Vec::new();
        self.extend_chain(member, &mut out);
        // An entry past a hole in its author's chain shows the hole: ask for it.
        let c = self.contiguous.get(&member).copied().unwrap_or(0);
        if member != self.me && seq > c + 1 {
            self.want_all(member, c + 1, seq - 1);
        }
        // A removed member's chain may still be missing entries up to its cut.
        if self.evicted.contains_key(&member) {
            self.adopt_cut(member);
        }
        out
    }

    /// Extend `m`'s verified contiguous prefix: each next entry must link to the previous by hash.
    pub(super) fn extend_chain(&mut self, m: MemberId, out: &mut Vec<Entry>) {
        // A member that asked to join is validated once admitted, when its boundary is known.
        if m != LOGIC && !self.members.contains(&m) && !self.evicted.contains_key(&m) {
            return;
        }
        loop {
            let c = self.contiguous.get(&m).copied().unwrap_or(0);
            // A removed member's chain ends at its cut. A vote only caps what the voter references
            // (stable_seq, author_ack): it keeps processing the chain, so a member wrongly
            // suspected can still take part in the reset that keeps it (P2-4).
            if self.evicted.contains_key(&m) && self.frozen.get(&m).is_some_and(|&f| c >= f) {
                break;
            }
            let Some(&id) = self.by_author.get(&(m, c + 1)) else { break };
            let prev = if c == 0 { EntryId::NONE } else { self.by_author[&(m, c)] };
            if self.entries[&id].prev != prev {
                // The chain continues from a version of seq c we do not hold: fetch it. And the
                // two signed entries already prove that m signed two versions of seq c (P2-4: a
                // member that forks its vote is proven faulty even if it never sends the other).
                self.want_all(m, c, c);
                if c > 0 && !self.evicted.contains_key(&m) && !self.proofs.contains_key(&m) {
                    let held = self.by_author[&(m, c)];
                    out.push(self.entries[&held].clone());
                    out.push(self.entries[&id].clone());
                    out.extend(self.on_proof(m, c, held, id));
                }
                break;
            }
            self.contiguous.insert(m, c + 1);
            self.validate(m, id, out);
        }
    }

    /// Chain-local rules, in chain order (reviews NEW-1, H5; user rule 14:31Z):
    /// * an ack is void if its stamp goes backwards, or its horizon lags its stamp by more than
    ///   `horizon_slack` frames (so no member can hold a backdating window open);
    /// * any other entry is void if stamped at or before a horizon its author declared earlier.
    pub(super) fn validate(&mut self, m: MemberId, id: EntryId, out: &mut Vec<Entry>) {
        let e = self.entries[&id].clone();
        // D80: an admin entry stays pending across epochs until a decision judges it.
        let admin_entry = matches!(e.payload, Payload::Kick { .. } | Payload::SetRole { .. } | Payload::DefineRole { .. });
        let (max_h, last_ack) = self.chain_state.get(&m).copied().unwrap_or((None, 0));
        let valid = match &e.payload {
            Payload::Ack { horizon, epoch, .. } => {
                // Review VC-5: an author's epoch never falls and rises by at most one per ack.
                let prev = self.ack_epoch_from.get(&m).and_then(|a| a.keys().next_back()).copied().unwrap_or(0);
                let epoch_ok = self.fixes_off & fix::VC5 != 0 || (*epoch >= prev && *epoch <= prev + 1);
                e.stamp >= last_ack && horizon + self.cfg.horizon_slack >= e.stamp && epoch_ok
            }
            // Review VC-4: a precommit's boundary caps its author's acks, so it must lie ahead of the
            // horizon the author has declared, by at most two join leads.
            Payload::Precommit { boundary, .. } if *boundary > 0 && self.fixes_off & fix::VC4 == 0 => {
                let h = max_h.unwrap_or(0);
                *boundary > h && *boundary <= h + 2 * self.cfg.join_lead
            }
            Payload::Vote { .. }
            | Payload::Propose { .. }
            | Payload::Prevote { .. }
            | Payload::Precommit { .. }
            | Payload::Kick { .. }
            | Payload::SetRole { .. }
            | Payload::DefineRole { .. } => true,
            // A member admitted by a view change acts from its boundary frame on (10.8).
            _ => max_h.is_none_or(|h| e.stamp > h) && self.admitted.get(&m).is_none_or(|&b| e.stamp >= b),
        };
        if !valid {
            self.stats.entries_rejected_violation += 1;
            self.ev(format!("violation: m{m} s{} stamped {} breaks its chain's horizon rules: void for every member", e.seq, e.stamp));
            self.violations.insert(id);
            self.set_eff(id);
            return;
        }
        match e.payload {
            Payload::Ack { horizon, heads, check, epoch } => {
                if epoch > 0 {
                    self.ack_epoch_from.entry(m).or_default().entry(epoch).or_insert(e.seq);
                }
                let h = max_h.map_or(horizon, |x| x.max(horizon));
                self.chain_state.insert(m, (Some(h), e.stamp));
                self.ack_hist.entry(m).or_default().insert(e.seq, h);
                for &(q, s, hid) in &heads {
                    if m != self.me {
                        let mine = self.contiguous.get(&q).copied().unwrap_or(0);
                        if q != self.me && s > mine {
                            self.want_from(m, q, mine + 1, s);
                        } else if self.by_author.get(&(q, s)).is_some_and(|&x| x != hid) {
                            self.want_from(m, q, s, s);
                        }
                    }
                    // Review R3-1: once m has voted against q, its vote's frozen head is its last
                    // word on q's chain in this epoch; later references never count toward stability.
                    if self.votes.get(&q).is_some_and(|v| v.contains_key(&m)) {
                        continue;
                    }
                    let newer = self.refs.get(&(m, q)).and_then(|r| r.keys().next_back()).is_none_or(|&old| s > old);
                    if newer {
                        self.ref_advanced.insert((m, q), self.now);
                    }
                    self.refs.entry((m, q)).or_default().insert(s, (hid, id));
                }
                if m != self.me {
                    if let Some((f, hh)) = check {
                        self.remote_checks.entry(f).or_default().insert(m, hh);
                    }
                }
                self.recompute_conf();
                self.evaluate_checks();
            }
            Payload::Vote { epoch, .. }
            | Payload::Propose { epoch, .. }
            | Payload::Prevote { epoch, .. }
            | Payload::Precommit { epoch, .. }
            | Payload::Kick { epoch, .. }
            | Payload::SetRole { epoch, .. }
            | Payload::DefineRole { epoch, .. } => {
                if epoch > self.epoch {
                    self.vc_later.entry(epoch).or_default().push(id);
                } else if epoch == self.epoch || admin_entry {
                    self.record_vc(m, id, out);
                }
            }
            _ => {}
        }
    }

    /// The highest seq of `m`'s chain that every member is known to hold with identical ids:
    /// each other member's newest reference at or below my own head, checked against my copy.
    /// Monotone: what was once stable stays stable (review R3-1).
    pub(super) fn stable_seq(&mut self, m: MemberId) -> u32 {
        // After voting against m, this replica references no more of m's chain than its frozen head.
        let held = self.contiguous.get(&m).copied().unwrap_or(0);
        let mine = match self.frozen.get(&m) {
            Some(&f) if !self.evicted.contains_key(&m) => held.min(f),
            _ => held,
        };
        let mut s = mine;
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for j in members {
            if j == self.me || j == m {
                continue;
            }
            let Some((rs, rid)) = self.refs.get(&(j, m)).and_then(|r| r.range(..=mine).next_back()).map(|(a, b)| (*a, b.0)) else {
                s = 0;
                break;
            };
            if self.by_author.get(&(m, rs)) != Some(&rid) {
                // j holds another version: wait for the proof (fetch it from j).
                self.want_from(j, m, rs, rs);
                s = 0;
                break;
            }
            s = s.min(rs);
        }
        let hi = self.stable_hi.entry(m).or_insert(0);
        *hi = (*hi).max(s);
        *hi
    }

    /// Confirmed frame from stable data only (reviews C1, NEW-1): frame f is confirmed once, for
    /// every member, a stable prefix of its chain declares a horizon at or after f. No decision
    /// depends on it except finality (SCM, ACL, R14 relays) and trimming.
    pub(super) fn recompute_conf(&mut self) {
        if self.adopting {
            return;
        }
        let mut c = u64::MAX;
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for j in members {
            let mut s = self.stable_seq(j);
            // An ack from an epoch newer than mine never counts toward my confirmation (10.8): its
            // author may know of members I do not.
            if let Some(&first) = self.ack_epoch_from.get(&j).and_then(|a| a.range(self.epoch + 1..).map(|(_, s)| s).min()) {
                s = s.min(first.saturating_sub(1));
            }
            let mut h = self.ack_hist.get(&j).and_then(|a| a.range(..=s).next_back()).map(|(_, h)| *h);
            // After precommitting a join, its author's acks count only below the join's boundary.
            if let (Some(hh), Some(&(at, b))) = (h, self.join_caps.get(&j)) {
                if s > at {
                    h = Some(hh.min(b.saturating_sub(1)));
                }
            }
            match (h, self.admitted.get(&j)) {
                (Some(h), Some(&b)) => c = c.min((h + 1).max(b)),
                (None, Some(&b)) => c = c.min(b),
                (Some(h), None) => c = c.min(h + 1),
                (None, None) => return,
            }
        }
        // P2-4: a removed member's entries up to its cut count, and a replica that did not vote
        // may not hold them all yet. Until it does, it confirms only the frames that the acks it
        // holds of that prefix cover: an entry stamped at or before a declared horizon must come
        // before that ack in the chain (8.2).
        for (&x, &(cut, _)) in &self.evicted {
            let held = self.contiguous.get(&x).copied().unwrap_or(0);
            let right = self.cut_ids.get(&x).is_none_or(|&cid| cut == 0 || self.by_author.get(&(x, cut)) == Some(&cid));
            if held < cut || !right {
                // Review VC-2: past the prefix verified against every member's references, my copy may
                // be another branch of a fork, so only that prefix's acks bound confirmation; the
                // robust cut contains it.
                let upto = if self.fixes_off & fix::VC2 == 0 { self.stable_hi.get(&x).copied().unwrap_or(0).min(cut) } else { held.min(cut) };
                let h = self.ack_hist.get(&x).and_then(|a| a.range(..=upto).next_back()).map_or(0, |(_, h)| h + 1);
                c = c.min(h);
            }
        }
        if c != u64::MAX && c > self.conf {
            self.conf = c;
        }
    }
}

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
        let counted = e.member == LOGIC || self.membership().members().contains(&e.member) || self.membership().removed().contains_key(&e.member);
        let acts = counted && !is_protocol(&e.payload);
        let open = self.membership().closed_at().is_none_or(|c| e.stamp < c);
        let new = if acts && open && !self.is_void(id) { Some(e.stamp) } else { None };
        let old = self.eff.get(&id).copied();
        if old == new {
            return;
        }
        // D73(a): a hand pack acts at every frame it covers.
        let extra = e.payload.extra_frames();
        if let Some(o) = old {
            for g in o..=o + extra {
                if let Some(s) = self.at_frame.get_mut(&g) {
                    s.remove(&id);
                }
            }
            self.eff.remove(&id);
            self.mark_dirty(o);
        }
        if let Some(n) = new {
            self.eff.insert(id, n);
            for g in n..=n + extra {
                self.at_frame.entry(g).or_default().insert(id);
            }
            self.mark_dirty(n);
        }
    }

    pub(super) fn mark_dirty(&mut self, f: u64) {
        // Entries stamped before a transferred state's frame are already in it (design 9.5).
        if f < self.floor {
            return;
        }
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

    /// The id of the version of `(member, seq)` this member holds and references.
    pub fn id_at(&self, member: MemberId, seq: u32) -> Option<EntryId> {
        self.by_author.get(&(member, seq)).copied()
    }

    /// The entry `id` with its author's signature on it, if this member holds one (its own entries,
    /// and entries that came signed; option 2: the others are covered by a later entry's: `run_of`).
    pub fn signed_entry(&self, id: EntryId) -> Option<SignedEntry> {
        let sig = *self.sigs.get(&id)?;
        self.entries.get(&id).map(|e| SignedEntry { entry: e.clone(), sig })
    }

    /// Entry `id` and its author's entries above it up to the first one signed on itself (its cover),
    /// bottom up: an authenticated run (option 2; design 10.8). The others' signatures are zero.
    pub fn run_of(&self, id: EntryId) -> Option<Vec<SignedEntry>> {
        let mut out = Vec::new();
        let mut cur = id;
        loop {
            let e = self.entries.get(&cur)?.clone();
            if let Some(&sig) = self.sigs.get(&cur) {
                out.push(SignedEntry { entry: e, sig });
                return Some(out);
            }
            out.push(SignedEntry { entry: e, sig: [0; 64] });
            cur = *self.run_next.get(&cur)?;
            if out.len() >= Self::RUN_MAX {
                return None;
            }
        }
    }

    /// The message that relays entry `id` with its authentication: the entry if it is signed on
    /// itself, else its run up to its cover (explicit).
    pub fn relay(&self, id: EntryId) -> Option<Msg> {
        let mut run = self.run_of(id)?;
        Some(if run.len() == 1 { Msg::Entry(run.pop().expect("one")) } else { Msg::Run { lean: false, heads: false, signed: true, entries: run } })
    }

    pub(super) fn relay_all(&self, v: Vec<Entry>) -> Vec<Msg> {
        v.iter().filter_map(|e| self.relay(e.id())).collect()
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

    /// Author an entry at `stamp` (D73(a): a hand pack, stamped at its first frame, up to k - 1 frames
    /// before the present; its author's horizon has stayed below it).
    pub fn author_stamped(&mut self, payload: Payload, stamp: u64) -> SignedEntry {
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None };
        let e = self.publish(e);
        self.signed(e)
    }

    /// Test fixture (review HO-1, round 2): author `payload` carrying a chosen object state.
    pub fn author_carrying(&mut self, payload: Payload, carried: (u32, BodyState)) -> SignedEntry {
        let stamp = self.frame.max(self.now);
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: Some(carried) };
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
            let s = match self.membership().frozen(q) {
                Some(f) if !self.membership().removed().contains_key(&q) => held.min(f),
                _ => held,
            };
            if s > 0 && q != self.me && self.cfg.ack_heads {
                match (self.hidden.as_ref().and_then(|h| h.get(&q)), self.fake_refs) {
                    (_, Some((f, n))) if f == q => heads.push((q, s + n, EntryId::fake(fnv_u64s([41, q as u64, (s + n) as u64])))),
                    (Some(&(hs, hid)), _) if hs < s => heads.push((q, hs, hid)),
                    _ => heads.push((q, s, self.by_author[&(q, s)])),
                }
            }
        }
        let mut check = if self.dirty_from.is_none() {
            self.check_hashes.range(..self.conf.min(self.frame)).next_back().map(|(f, h)| (*f, *h))
        } else {
            None
        };
        // 7.6 reduction 1: a check hash goes in an ack only when it changes. Confirmed check hashes are
        // final, so a repeat says nothing new (every chain entry reaches every member by repair).
        if let Some((f, _)) = check {
            if self.last_check_sent == Some(f) {
                check = None;
            } else {
                self.last_check_sent = Some(f);
            }
        }
        let stamp = self.frame.max(self.now);
        let mut horizon = self.now.saturating_sub(1 + self.horizon_lag);
        // D73(a): below a pending pack's first frame until the pack is sent.
        if let Some(p0) = self.hold_horizon_below {
            horizon = horizon.min(p0.saturating_sub(1));
        }
        let e = Entry {
            member: self.me,
            seq: self.my_seq + 1,
            prev: self.my_last,
            stamp,
            payload: Payload::Ack { horizon, heads, check, epoch: self.membership().ack_epoch() },
            carried: None,
        };
        let e = self.publish(e);
        self.signed(e)
    }

    /// Design 7.6: track the latest stamp of a held entry other than an ack.
    fn note_held(&mut self, e: &Entry) {
        if !matches!(e.payload, Payload::Ack { .. }) {
            // Review LN-2: a stamp far in the future (a signed entry's, valid or not) must not keep
            // the member busy for good; nothing is busier than a window ahead.
            self.busy_stamp = self.busy_stamp.max(Some(e.stamp.min(self.now + self.cfg.window)));
        }
    }

    /// Idle (design 7.6, reduction 2): every entry this replica holds, other than acks, is stamped
    /// below `conf` (at or before its last confirmed frame), and it holds no unsent hand pack (D73(a):
    /// the pack's poses are not entries yet, and it goes out backdated to its first frame). Judged
    /// from its own copy; acks are left out, or each ack would keep every member busy. A new entry,
    /// its own or anyone's, is stamped at or after `conf`, so it makes the member busy until `conf`
    /// passes it.
    pub fn idle(&self) -> bool {
        // Review LN-2: lean entries other than acks waiting for an entry they name keep the member
        // busy, as they would once inserted (`note_held` skips acks), without their unverified stamps
        // noted.
        self.busy_stamp.is_none_or(|s| s < self.conf)
            && self.hold_horizon_below.is_none()
            && self.lean_wait.values().all(|w| w.entries.iter().all(|e| matches!(e.payload, Payload::Ack { .. })))
    }

    /// Store a received or fetched entry, once its signature verifies (S3; a bad one is dropped and
    /// counted). Returns entries to broadcast: when it proves equivocation, both versions and this
    /// member's vote.
    pub fn insert(&mut self, se: SignedEntry) -> Vec<Msg> {
        let id = se.id();
        if self.entries.contains_key(&id) {
            return vec![];
        }
        if !self.keys.verify(self.world_id, &se) {
            self.stats.bad_sigs += 1;
            return vec![];
        }
        self.insert_auth(se.entry, Auth::Sig(se.sig))
    }

    /// Whether this member's own entry `e` goes to `p` lean: always, unless it is an ack naming
    /// another member's chain more than [`Self::LEAN_HEAD_SLACK`] entries past what `p`'s newest ack
    /// holds of it. Such a head may never reach `p` (one past a removed member's cut does not cross a
    /// partition: bridge), so the lean ack would wait for it; the explicit form is inserted as before.
    /// A missing previous entry needs no such rule: the chain extends only past it either way.
    pub fn lean_to(&self, p: MemberId, e: &Entry) -> bool {
        let held = |q: MemberId| self.refs.get(&(p, q)).and_then(|r| r.keys().next_back()).copied().unwrap_or(0);
        match &e.payload {
            Payload::Ack { heads, .. } => heads.iter().all(|&(q, s, _)| q == p || held(q) + Self::LEAN_HEAD_SLACK >= s),
            _ => true,
        }
    }
    pub const LEAN_HEAD_SLACK: u32 = 32;

    /// Whether a lean entry of `member` at `seq` is waiting (in a run) for an entry it names.
    pub fn lean_waiting(&self, member: MemberId, seq: u32) -> bool {
        self.lean_wait.range((member, 0)..=(member, seq)).next_back().is_some_and(|(k, w)| k.1 + w.entries.len() as u32 > seq)
    }

    /// At most this many runs wait for entries they name.
    const LEAN_WAIT_MAX: usize = 1024;
    /// A run waits this long before its signed, explicit form is asked for too: well inside half the
    /// speculation window (condition (c)); a hole or a head in flight usually arrives sooner.
    pub const LEAN_HOLD_FRAMES: u64 = 15;
    /// The longest run (its count is one byte on the wire).
    pub const RUN_MAX: usize = 255;

    /// A run from `from` ([`Msg::Run`]): consecutive entries of one author, signed on the top one (or
    /// unsigned: a hole-fill reply). Rebuild what the wire left out, verify, insert. A run that names
    /// an entry this member does not hold (its author's entry below it, or an ack's head) waits for
    /// it and asks for it (the hole from all, a head from the acker, who holds it); an unsigned one
    /// waits for the entry above it that authenticates it. One still missing a head, or still
    /// unauthenticated, after [`Self::LEAN_HOLD_FRAMES`] also asks the sender for its signed, explicit
    /// form. One whose rebuilt form does not verify (this member holds another version: a fork) is
    /// fetched that way at once, so the explicit path sees what it would have seen (review conditions
    /// (b) and (c), @engine-architect, 2026-11-01).
    pub fn insert_run(&mut self, from: MemberId, lean: bool, heads: bool, signed: bool, entries: Vec<SignedEntry>) -> Vec<Msg> {
        let Some(top) = entries.last() else { return vec![] };
        let sig = signed.then_some(top.sig);
        let (m, s0) = (entries[0].member, entries[0].seq);
        if lean || heads {
            self.stats.lean_in += entries.len() as u64;
        }
        let w = LeanWait {
            from,
            first_prev: !lean,
            lean_heads: heads,
            entries: entries.into_iter().map(|e| e.entry).collect(),
            sig,
            since: self.now,
            asked: false,
        };
        let (prev_missing, heads_missing) = self.lean_missing(&w);
        let wants_gap = m != self.me && self.known.contains(&m) && (prev_missing || sig.is_none());
        if m != self.me && self.known.contains(&m) {
            for (q, s) in heads_missing {
                if q != self.me {
                    let c = self.contiguous.get(&q).copied().unwrap_or(0);
                    self.want_from(from, q, c + 1, s);
                }
            }
            // Condition (c) (S6): an author whose entries arrive is heard from even while they wait,
            // as an explicit entry past a hole is (insert_entry). The IO authenticates the sender, so
            // one withheld entry cannot make an honest member look silent.
            if from == m {
                self.heard.insert(m, self.now);
            }
        }
        // Design 7.6's idle predicate counts waiting runs without trusting their stamps (`idle`,
        // review LN-2). A newer copy of a run already waiting replaces it.
        self.lean_wait.insert((m, s0), w);
        while self.lean_wait.len() > Self::LEAN_WAIT_MAX {
            let oldest = *self.lean_wait.iter().min_by_key(|(k, w)| (w.since, **k)).map(|(k, _)| k).expect("non-empty");
            self.lean_wait.remove(&oldest);
        }
        if wants_gap {
            // A signed run past a hole asks everyone for the whole range from its author's verified
            // prefix, held entries included, as option 1 does: the answers bring any other version of
            // a held entry, the fork's proof (triage group L: plainfork, c2, adminfork, ...).
            if sig.is_some() {
                let c = self.contiguous.get(&m).copied().unwrap_or(0);
                self.want_all(m, c + 1, s0 - 1);
            } else {
                self.want_gap(m);
            }
        }
        self.resolve_lean()
    }

    /// Ask for `m`'s first missing range: from its verified prefix up past the runs that wait, to
    /// below the next entry held or waiting. The requester holds what is above the range (waiting,
    /// or verified past a hole), so the reply needs no signature; and it may be lean if the requester
    /// holds the entry below verified.
    fn want_gap(&mut self, m: MemberId) {
        let mut s = self.contiguous.get(&m).copied().unwrap_or(0) + 1;
        let mut below_verified = true;
        loop {
            if self.by_author.contains_key(&(m, s)) {
                s += 1;
                below_verified = true;
            } else if let Some(x) = self.lean_wait.get(&(m, s)) {
                s += x.entries.len() as u32;
                below_verified = false;
            } else {
                break;
            }
        }
        let waiting = self.lean_wait.range((m, s)..=(m, u32::MAX)).next().map(|(k, _)| k.1);
        let held = self.by_author.range((m, s)..=(m, u32::MAX)).next().map(|(k, _)| k.1);
        let Some(end) = [waiting, held].into_iter().flatten().min() else { return };
        let mut bits = 0;
        if self.lean_wire && below_verified && std::env::var("PE_LEANREP").map_or(true, |v| v != "0") {
            bits |= HELD_BELOW;
        }
        if self.run_wire {
            bits |= HELD_ABOVE;
        }
        let known: Vec<MemberId> = self.known.iter().copied().collect();
        for j in known {
            self.want_from_held(j, m, s, end - 1, bits);
        }
    }

    /// What a waiting run names that this member does not hold: its author's entry below it (when
    /// left out), and the (q, s) of each ack head left out.
    fn lean_missing(&self, w: &LeanWait) -> (bool, Vec<(MemberId, u32)>) {
        let e0 = &w.entries[0];
        let prev = !w.first_prev && e0.seq > 1 && !self.by_author.contains_key(&(e0.member, e0.seq - 1));
        let mut heads = Vec::new();
        if w.lean_heads {
            for e in &w.entries {
                if let Payload::Ack { heads: hs, .. } = &e.payload {
                    heads.extend(hs.iter().filter(|&&(q, s, _)| s > 0 && !self.by_author.contains_key(&(q, s))).map(|&(q, s, _)| (q, s)));
                }
            }
        }
        (prev, heads)
    }

    /// Whether a waiting run is authenticated: signed, or its author's entry above it is held.
    fn lean_authenticated(&self, w: &LeanWait) -> bool {
        let top = w.entries.last().expect("non-empty");
        w.sig.is_some() || self.by_author.contains_key(&(top.member, top.seq + 1))
    }

    /// Waiting runs that can go in now; unsigned runs joined to the run above them; and, for runs
    /// still waiting past [`Self::LEAN_HOLD_FRAMES`] on a head or on authentication, a request for
    /// their signed, explicit form. Called on every receipt and tick. Returns what to broadcast.
    pub fn resolve_lean(&mut self) -> Vec<Msg> {
        let mut out = Vec::new();
        loop {
            let mut progress = false;
            let keys: Vec<(MemberId, u32)> = self.lean_wait.keys().copied().collect();
            for k in keys {
                let Some(w) = self.lean_wait.get(&k) else { continue };
                let (p, h) = self.lean_missing(w);
                if !p && h.is_empty() && self.lean_authenticated(w) {
                    let w = self.lean_wait.remove(&k).expect("listed");
                    if w.since < self.now {
                        self.stats.lean_waited += w.entries.len() as u64;
                        self.stats.lean_wait_frames += (self.now - w.since) * w.entries.len() as u64;
                        self.stats.lean_wait_max = self.stats.lean_wait_max.max(self.now - w.since);
                    }
                    out.extend(self.insert_rebuilt(w));
                    progress = true;
                    continue;
                }
                // An unsigned run is authenticated by the run above it: join them.
                if w.sig.is_none() {
                    let top = w.entries.last().expect("non-empty");
                    let up = (top.member, top.seq + 1);
                    let n = w.entries.len();
                    if self.lean_wait.get(&up).is_some_and(|u| u.entries.len() + n <= Self::RUN_MAX) {
                        let u = self.lean_wait.remove(&up).expect("checked");
                        let w = self.lean_wait.get_mut(&k).expect("listed");
                        w.entries.extend(u.entries);
                        w.sig = u.sig;
                        w.from = u.from;
                        w.lean_heads |= u.lean_heads;
                        w.since = w.since.min(u.since);
                        w.asked = false;
                        progress = true;
                    }
                }
            }
            if !progress {
                break;
            }
        }
        // A missing entry below is already asked for, and the explicit form could not extend the
        // chain past it either (extend_chain); only a missing head or authentication falls back.
        // Review LN-1 (ruled 2026-11-01): no fallback for a hole; on a link that keeps up the wait is
        // one refetch, and on a saturated one the explicit form only adds bytes (docs/p2-3-r18-bytes.md).
        let now = self.now;
        let late: Vec<(MemberId, u32)> = self
            .lean_wait
            .iter()
            .filter(|(_, w)| !w.asked && now >= w.since + Self::LEAN_HOLD_FRAMES && (!self.lean_missing(w).1.is_empty() || !self.lean_authenticated(w)))
            .map(|(k, _)| *k)
            .collect();
        for k in late {
            let w = self.lean_wait.get_mut(&k).expect("listed");
            w.asked = true;
            let (from, top) = (w.from, w.entries.last().expect("non-empty").seq);
            self.stats.lean_fallbacks += 1;
            self.want_from_held(from, k.0, k.1, top, 0);
        }
        out
    }

    /// Rebuild a run's ids from what this member holds, link its entries, verify, and insert it.
    fn insert_rebuilt(&mut self, mut w: LeanWait) -> Vec<Msg> {
        let (m, s0) = (w.entries[0].member, w.entries[0].seq);
        let top_seq = w.entries.last().expect("non-empty").seq;
        if !w.first_prev {
            w.entries[0].prev = if s0 <= 1 { EntryId::NONE } else { self.by_author[&(m, s0 - 1)] };
        }
        if w.lean_heads {
            for e in w.entries.iter_mut() {
                if let Payload::Ack { heads, .. } = &mut e.payload {
                    for (q, s, id) in heads.iter_mut() {
                        *id = if *s == 0 { EntryId::NONE } else { self.by_author[&(*q, *s)] };
                    }
                }
            }
        }
        let mut ids: Vec<EntryId> = Vec::with_capacity(w.entries.len());
        for i in 0..w.entries.len() {
            if i > 0 {
                w.entries[i].prev = ids[i - 1];
            }
            ids.push(w.entries[i].id());
        }
        let top_id = *ids.last().expect("non-empty");
        let above = self.by_author.get(&(m, top_seq + 1)).copied();
        let ok = match w.sig {
            Some(sig) => self.keys.verify(self.world_id, &SignedEntry { entry: w.entries.last().expect("non-empty").clone(), sig }),
            None => above.and_then(|a| self.entries.get(&a)).is_some_and(|e| e.prev == top_id),
        };
        if !ok {
            self.stats.lean_fallbacks += 1;
            self.want_from_held(w.from, m, s0, top_seq, 0);
            // Most likely its author holds another version of the entry below (a fork): ask everyone
            // for theirs now, as the explicit form would make this member ask (extend_chain), so the
            // proof does not wait a round trip more.
            if !w.first_prev && s0 > 1 && m != self.me {
                self.want_all(m, s0 - 1, s0 - 1);
            }
            return vec![];
        }
        let mut out = Vec::new();
        let n = w.entries.len();
        for (i, e) in w.entries.into_iter().enumerate() {
            let auth = if i + 1 < n {
                Auth::Next(ids[i + 1])
            } else {
                match w.sig {
                    Some(sig) => Auth::Sig(sig),
                    None => Auth::Next(above.expect("authenticated by the entry above")),
                }
            };
            out.extend(self.insert_auth(e, auth));
        }
        out
    }

    /// Store an authenticated entry: signed on itself, or covered by the next entry of its run.
    fn insert_auth(&mut self, e: Entry, auth: Auth) -> Vec<Msg> {
        let id = e.id();
        let had = self.entries.contains_key(&id);
        match auth {
            Auth::Sig(sig) => {
                self.sigs.insert(id, sig);
                self.run_next.remove(&id);
            }
            Auth::Next(n) => {
                if !self.sigs.contains_key(&id) {
                    self.run_next.entry(id).or_insert(n);
                }
            }
        }
        if had {
            return vec![];
        }
        let out = self.insert_entry(e);
        if !self.entries.contains_key(&id) {
            self.sigs.remove(&id);
            self.run_next.remove(&id);
        }
        self.relay_all(out)
    }

    /// Store an entry whose signature is verified, or this replica's own.
    pub(super) fn insert_entry(&mut self, e: Entry) -> Vec<Entry> {
        let id = e.id();
        if self.entries.contains_key(&id) || (!self.known.contains(&e.member) && e.member != LOGIC) {
            return vec![];
        }
        // Review PF-1: every signed precommit for a value, any version, linked into its author's chain
        // or not (a proven member's counts from here, `precommitters`), with its fact signature (S3c),
        // which the epoch certificate then carries; one whose signature fails is not held.
        if let Payload::Precommit { epoch, round, value, ref sig, .. } = e.payload {
            if value != 0 && epoch >= self.vcm.epoch && self.keys.public.fact_ok(e.member, self.world_id, vc_fact(epoch, round, value), sig) {
                self.pc_held.entry((epoch, round, value)).or_default().insert(e.member, *sig);
            }
        }
        // Review VC8R-1: and every signed prevote for a value, the same way.
        if let Payload::Prevote { epoch, round, value } = e.payload {
            if value != 0 && epoch >= self.vcm.epoch {
                self.pv_held.entry((epoch, round, value)).or_default().insert(e.member);
            }
        }
        if let Some(&other) = self.by_author.get(&(e.member, e.seq)) {
            // Two signed entries for the same (member, seq): a transferable proof (review C2).
            self.note_held(&e);
            self.entries.insert(id, e.clone());
            self.disputed.insert(id);
            // Another version of a proposal still names a value someone may decide (P2-4).
            self.note_proposal(id);
            // Review VC4R-1: another version of a prevote may be the one a capped precommit counted.
            if let Payload::Prevote { epoch, round, value } = e.payload {
                if epoch == self.vcm.epoch && self.vcm.members.contains(&e.member) {
                    self.vcm.vc.any_prevotes.entry((round, value)).or_default().insert(e.member);
                }
            }
            let mut out = vec![self.entries[&other].clone(), e.clone()];
            out.extend(self.on_proof(e.member, e.seq, other, id));
            // Review VC-2: the version a removed member's cut names may arrive as a duplicate.
            if self.membership().removed().contains_key(&e.member) && self.fixes_off & fix::VC2 == 0 {
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
        let disputed = self.membership().proof_seq(e.member).is_some_and(|k| e.seq >= k)
            || self.membership().removed().get(&e.member).is_some_and(|&(cut, _)| e.seq > cut);
        if disputed {
            self.disputed.insert(id);
        }
        let (member, seq) = (e.member, e.seq);
        self.note_held(&e);
        self.entries.insert(id, e);
        self.set_eff(id);
        // Review PF-1: a proposal of a member proven to have forked may never link here; keep it by
        // value now, as any other version of a proposal is kept.
        if self.fixes_off & fix::PF1 == 0 && self.vcm.proofs.contains_key(&member) {
            self.note_proposal(id);
        }
        let mut out = Vec::new();
        self.extend_chain(member, &mut out);
        // An entry past a hole in its author's chain shows the hole: ask for it.
        let c = self.contiguous.get(&member).copied().unwrap_or(0);
        if member != self.me && seq > c + 1 {
            self.want_all(member, c + 1, seq - 1);
        }
        // A removed member's chain may still be missing entries up to its cut.
        if self.membership().removed().contains_key(&member) {
            self.adopt_cut(member);
        }
        out
    }

    /// Extend `m`'s verified contiguous prefix: each next entry must link to the previous by hash.
    pub(super) fn extend_chain(&mut self, m: MemberId, out: &mut Vec<Entry>) {
        // A member that asked to join is validated once admitted, when its boundary is known.
        if m != LOGIC && !self.membership().members().contains(&m) && !self.membership().removed().contains_key(&m) {
            return;
        }
        loop {
            let c = self.contiguous.get(&m).copied().unwrap_or(0);
            // A removed member's chain ends at its cut. A vote only caps what the voter references
            // (stable_seq, author_ack): it keeps processing the chain, so a member wrongly
            // suspected can still take part in the reset that keeps it (P2-4).
            if self.membership().removed().contains_key(&m) && self.membership().frozen(m).is_some_and(|f| c >= f) {
                break;
            }
            let Some(&id) = self.by_author.get(&(m, c + 1)) else { break };
            let prev = if c == 0 { EntryId::NONE } else { self.by_author[&(m, c)] };
            if self.entries[&id].prev != prev {
                // The chain continues from a version of seq c we do not hold: fetch it. And the
                // two signed entries already prove that m signed two versions of seq c (P2-4: a
                // member that forks its vote is proven faulty even if it never sends the other).
                self.want_all(m, c, c);
                if c > 0 && !self.membership().removed().contains_key(&m) && self.membership().proof_seq(m).is_none() {
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

    /// Chain-local rules, in chain order (reviews NEW-1, H5; user rule 14:31Z; D73(a)):
    /// * an ack is void if its stamp goes backwards, or its horizon lags its stamp by more than
    ///   `horizon_slack + PACK_MAX - 1` frames (the room a pending hand pack needs, D73(a));
    /// * any other entry is void if stamped at or before a horizon its author declared earlier;
    /// * and, so that only hand packs get the wider window, a perturbation or admin entry is void
    ///   unless its last frame (stamp + a pack's extra frames) is within `horizon_slack` frames of
    ///   its author's last ack's stamp, and a pack holds at most `PACK_MAX` targets.
    pub(super) fn validate(&mut self, m: MemberId, id: EntryId, out: &mut Vec<Entry>) {
        let e = self.entries[&id].clone();
        // D80: an admin entry stays pending across epochs until a decision judges it.
        let admin_entry = matches!(e.payload, Payload::Kick { .. } | Payload::SetRole { .. } | Payload::DefineRole { .. } | Payload::Ownership { .. } | Payload::Ban { .. });
        let (max_h, last_ack) = self.chain_state.get(&m).copied().unwrap_or((None, 0));
        let valid = match &e.payload {
            Payload::Ack { horizon, epoch, .. } => {
                // Review VC-5: an author's epoch never falls and rises by at most one per ack. Review
                // VC3R-6: a joiner's first acks may declare the epoch it was admitted into, so it need
                // not have acked every epoch since genesis (its chain is validated only once admitted).
                let prev = self.membership().last_ack_epoch(m);
                let adm = if self.fixes_off & fix::VC3R6 == 0 { self.admitted_epoch.get(&m).copied().unwrap_or(0) } else { 0 };
                let epoch_ok = self.fixes_off & fix::VC5 != 0 || (*epoch >= prev && (*epoch <= prev + 1 || *epoch <= adm));
                e.stamp >= last_ack && horizon + self.cfg.horizon_slack + PACK_MAX - 1 >= e.stamp && epoch_ok
            }
            // Review VC-4: a precommit's boundary caps its author's acks, so it must lie ahead of the
            // horizon the author has declared: a cap never takes back what its acks already confirmed.
            // Reviews VCR-12 and VC3R-5: no upper bound. A far cap restrains only its own author, and
            // round 2's bound (two join leads past that horizon) voided a lagging member's precommits.
            Payload::Precommit { boundary, .. } if *boundary > 0 && self.fixes_off & fix::VC4 == 0 => {
                let h = max_h.unwrap_or(0);
                *boundary > h && (self.fixes_off & fix::VCR12 == 0 || *boundary <= h + 2 * self.cfg.join_lead)
            }
            // Review VCR-9: an admin entry's stamp orders it (commit order), so it obeys its author's
            // horizon like any entry: no backdating past what its author has declared.
            Payload::Kick { .. } | Payload::SetRole { .. } | Payload::DefineRole { .. } | Payload::Ownership { .. } | Payload::Ban { .. } if self.fixes_off & fix::VCR9 == 0 => {
                max_h.is_none_or(|h| e.stamp > h) && e.stamp + self.cfg.horizon_slack > last_ack
            }
            Payload::Vote { .. }
            | Payload::Propose { .. }
            | Payload::Prevote { .. }
            | Payload::Precommit { .. }
            | Payload::Kick { .. }
            | Payload::SetRole { .. }
            | Payload::DefineRole { .. }
            | Payload::Ownership { .. }
            | Payload::Ban { .. } => true,
            // A member admitted by a view change acts from its boundary frame on (10.8).
            _ => {
                let extra = e.payload.extra_frames();
                max_h.is_none_or(|h| e.stamp > h)
                    && e.stamp + extra + self.cfg.horizon_slack > last_ack
                    && extra < PACK_MAX
                    && self.membership().admitted(m).is_none_or(|b| e.stamp >= b)
            }
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
                    self.vcm.note_ack_epoch(m, epoch, e.seq);
                }
                let h = max_h.map_or(horizon, |x| x.max(horizon));
                self.chain_state.insert(m, (Some(h), e.stamp));
                self.ack_hist.entry(m).or_default().insert(e.seq, h);
                self.ack_stamps.entry(m).or_default().insert(e.seq, e.stamp);
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
                    if self.membership().voted_against(m, q) {
                        continue;
                    }
                    let newer = self.refs.get(&(m, q)).and_then(|r| r.keys().next_back()).is_none_or(|&old| s > old);
                    if newer {
                        self.vcm.ref_advanced.insert((m, q), self.now);
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
            | Payload::DefineRole { epoch, .. }
            | Payload::Ownership { epoch, .. }
            | Payload::Ban { epoch, .. } => {
                if epoch > self.vcm.epoch {
                    self.vcm.vc_later.entry(epoch).or_default().push(id);
                } else if epoch == self.vcm.epoch || admin_entry {
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
        let mine = match self.membership().frozen(m) {
            Some(f) if !self.membership().removed().contains_key(&m) => held.min(f),
            _ => held,
        };
        let mut s = mine;
        let members: Vec<MemberId> = self.membership().members().iter().copied().collect();
        let matching = self.fixes_off & fix::VC4R3 == 0;
        for j in members {
            if j == self.me || j == m {
                continue;
            }
            if matching {
                // Review VC5R-2: j's contribution is the newest of m's chain it has referenced that I
                // hold in the version it names. A reference to another version (a fake id, or a fork) is
                // skipped and fetched for the proof, but it does not zero j's contribution, so a faulty
                // member alternating a fake head with a stale real one cannot cap m's stability.
                let Some(r) = self.refs.get(&(j, m)) else {
                    s = 0;
                    break;
                };
                let mut matched = None;
                let unmatched: Vec<u32> = r
                    .range(..=mine)
                    .rev()
                    .take_while(|(rs, (rid, _))| {
                        if self.by_author.get(&(m, **rs)) == Some(rid) {
                            matched = Some(**rs);
                            false
                        } else {
                            true
                        }
                    })
                    .map(|(rs, _)| *rs)
                    .collect();
                for rs in unmatched {
                    self.want_from(j, m, rs, rs);
                }
                match matched {
                    Some(rs) => s = s.min(rs),
                    None => {
                        s = 0;
                        break;
                    }
                }
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
        if self.membership().adopting() {
            return;
        }
        let mut c = u64::MAX;
        let members: Vec<MemberId> = self.membership().members().iter().copied().collect();
        for j in members {
            let mut s = self.stable_seq(j);
            // An ack from an epoch newer than mine never counts toward my confirmation (10.8): its
            // author may know of members I do not.
            if let Some(first) = self.membership().first_newer_ack(j) {
                s = s.min(first.saturating_sub(1));
            }
            let mut h = self.ack_hist.get(&j).and_then(|a| a.range(..=s).next_back()).map(|(_, h)| *h);
            // After precommitting a join, its author's acks count only below the join's boundary.
            if let (Some(hh), Some((at, b))) = (h, self.membership().join_cap(j)) {
                if s > at {
                    h = Some(hh.min(b.saturating_sub(1)));
                }
            }
            match (h, self.membership().admitted(j)) {
                (Some(h), Some(b)) => c = c.min((h + 1).max(b)),
                (None, Some(b)) => c = c.min(b),
                (Some(h), None) => c = c.min(h + 1),
                (None, None) => return,
            }
        }
        // P2-4: a removed member's entries up to its cut count, and a replica that did not vote
        // may not hold them all yet. Until it does, it confirms only the frames that the acks it
        // holds of that prefix cover: an entry stamped at or before a declared horizon must come
        // before that ack in the chain (8.2).
        for (&x, &(cut, _)) in self.membership().removed() {
            let held = self.contiguous.get(&x).copied().unwrap_or(0);
            let right = self.membership().cut_id(x).is_none_or(|cid| cut == 0 || self.by_author.get(&(x, cut)) == Some(&cid));
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

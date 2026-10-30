//! Membership: epochs, quorum rules, equivocation proofs and the view change (P2-4, design 10.8).

use super::*;

/// `valid_round` of a proposal that no quorum has prevoted before.
pub const NO_ROUND: u32 = u32::MAX;

/// Frames a proposer waits, from the start of a round, for votes on every member it suspects.
pub const VC_GATHER: u64 = 6;

/// Two signed entries of one member that contradict each other (design 10.8): two versions of one
/// seq; an entry whose `prev` names a version of its predecessor other than `a`; or an ack `a` and a
/// later vote `b` whose frozen head drops an entry that ack referenced. Anyone holding both entries
/// (and, for a vote, the target's chain) can check it, so it travels in certificates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proof {
    pub a: EntryId,
    pub b: EntryId,
}

/// One member removed by a view change: the votes that justify it (each names its voter's frozen
/// head of the member's chain) and the cut those votes determine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    pub member: MemberId,
    pub votes: Vec<EntryId>,
    pub cut: (u32, EntryId),
}

/// A membership change from epoch `epoch` to `epoch + 1` (design 10.8). `excluded` are the members
/// this decision treats as proven faulty: they count neither in its quorums nor in their size. An
/// empty change is a reset: same members, every vote of the epoch cleared.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Change {
    pub epoch: u32,
    pub remove: Vec<Removal>,
    pub add: Vec<MemberId>,
    /// First frame of the next epoch: added members count from it (10.8, joins), and facts about
    /// frames from it on are the next epoch's to certify. Set on every change.
    pub boundary: u64,
    pub excluded: Vec<(MemberId, Proof)>,
}

impl Change {
    /// The value the view change votes on: a hash of the whole change, never 0 (0 is nil).
    pub fn value(&self) -> u64 {
        let mut w = Wr(Vec::with_capacity(64));
        w.change(self);
        fnv_bytes(&w.0) | 1
    }
}

/// D80 (open): how many members a view change needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuorumRule {
    /// A strict majority of the members not excluded by proof (option a): survives crashes and
    /// partitions, but tolerates no faulty member that has not been proven so.
    Majority,
    /// A Byzantine quorum (option b, recommended): more than (m + t) / 2 of the m members not
    /// excluded by proof, which tolerates t = floor((m - 1) / 3) faulty members not yet proven.
    Bft,
}

/// `(t, q)` for `m` members not excluded by proof: unproven faulty members tolerated, and the
/// quorum size. Any two quorums then share an honest member while at most t of the m are faulty.
pub fn quorum(rule: QuorumRule, m: usize) -> (usize, usize) {
    let t = match rule {
        QuorumRule::Majority => 0,
        QuorumRule::Bft => m.saturating_sub(1) / 3,
    };
    (t, (m + t + 2) / 2)
}

/// Spike model of a member's signature over its precommit of `value` in `round` of the view change
/// that ends epoch `epoch` of `world`: the signatures an epoch certificate carries.
pub fn vc_sig(world: u32, member: MemberId, epoch: u32, round: u32, value: u64) -> u64 {
    member_sig(world, member, fnv_u64s([3, epoch as u64, round as u64, value]))
}

/// An epoch certificate (design 10.8, review R3-8b): what another world needs to follow this
/// world's membership from `epoch - 1` to `epoch`. The precommits sign the change's value, so the
/// change itself (removals, joins, exclusions and the boundary frame) is what is certified.
#[derive(Clone, Debug, PartialEq)]
pub struct EpochCert {
    pub world: u32,
    /// The epoch this certificate starts (`change.epoch + 1`).
    pub epoch: u32,
    pub change: Change,
    pub round: u32,
    /// The members of `epoch`: the previous epoch's, minus the removed, plus the added.
    pub members: Vec<MemberId>,
    /// Precommit signatures over `(world, epoch - 1, round, change value)`.
    pub precommits: Vec<(MemberId, u64)>,
    /// The two entries of each exclusion proof, so a verifier needs no chain of this world.
    pub proofs: Vec<Entry>,
}

impl EpochCert {
    /// First frame of the epoch: facts about frames from here on are this epoch's to sign.
    pub fn from_frame(&self) -> u64 {
        self.change.boundary
    }

    pub fn hash(&self) -> u64 {
        let mut v = vec![self.world as u64, self.epoch as u64, self.change.value(), self.round as u64];
        v.extend(self.members.iter().map(|m| *m as u64));
        for (m, s) in &self.precommits {
            v.extend([*m as u64, *s]);
        }
        v.extend(self.proofs.iter().map(|e| e.id()));
        fnv_u64s(v)
    }
}

/// Whether `p` proves `m` faulty from its two entries alone (design 10.8): two versions of one seq;
/// an entry whose `prev` names another version of its predecessor; or an ack that references a
/// seq of some chain beyond the frozen head of a later vote by the same member against it.
pub fn proof_self_contained(m: MemberId, p: Proof, a: &Entry, b: &Entry) -> bool {
    if a.member != m || b.member != m || a.id() != p.a || b.id() != p.b {
        return false;
    }
    if a.seq == b.seq {
        return p.a != p.b;
    }
    if b.seq == a.seq + 1 {
        return b.prev != p.a;
    }
    match (&a.payload, &b.payload) {
        (Payload::Ack { heads, .. }, Payload::Vote { target, frozen, .. }) => {
            a.seq < b.seq && heads.iter().any(|h| h.0 == *target && h.1 > frozen.0)
        }
        _ => false,
    }
}

/// Check `cert` against the members of the previous epoch: every exclusion proved by the entries
/// it carries; precommits from a quorum of the members not excluded; the member list following
/// from the change.
pub fn epoch_cert_ok(rule: QuorumRule, prev: &[MemberId], cert: &EpochCert) -> bool {
    let c = &cert.change;
    if cert.epoch == 0 || c.epoch + 1 != cert.epoch {
        return false;
    }
    let by_id: BTreeMap<EntryId, &Entry> = cert.proofs.iter().map(|e| (e.id(), e)).collect();
    let mut excluded = BTreeSet::new();
    for &(m, p) in &c.excluded {
        let (Some(a), Some(b)) = (by_id.get(&p.a), by_id.get(&p.b)) else { return false };
        if !prev.contains(&m) || !excluded.insert(m) || !proof_self_contained(m, p, a, b) {
            return false;
        }
    }
    let base: Vec<MemberId> = prev.iter().copied().filter(|m| !excluded.contains(m)).collect();
    let (_, q) = quorum(rule, base.len());
    let value = c.value();
    let signers: BTreeSet<MemberId> = cert
        .precommits
        .iter()
        .filter(|(m, s)| base.contains(m) && *s == vc_sig(cert.world, *m, c.epoch, cert.round, value))
        .map(|(m, _)| *m)
        .collect();
    let mut expect: BTreeSet<MemberId> = prev.iter().copied().collect();
    for r in &c.remove {
        expect.remove(&r.member);
    }
    expect.extend(c.add.iter().copied());
    signers.len() >= q && expect.into_iter().collect::<Vec<_>>() == cert.members
}

impl WorldSim {
    // ---- view change (P2-4, design 10.8) ---------------------------------------------------------

    /// Record a validated membership entry of the current epoch, in chain order.
    pub(super) fn record_vc(&mut self, m: MemberId, id: EntryId, out: &mut Vec<Entry>) {
        if !self.members.contains(&m) {
            return;
        }
        let e = self.entries[&id].clone();
        match e.payload {
            Payload::Vote { target, frozen, .. } => {
                if target == m || !self.members.contains(&target) || self.votes.get(&target).is_some_and(|v| v.contains_key(&m)) {
                    return;
                }
                let rec = VoteRec { frozen, stamp: e.stamp, id };
                self.votes.entry(target).or_default().insert(m, rec);
                if let VoteCheck::Lie(ack) = self.vote_check(target, m, frozen) {
                    self.on_lie(m, ack, id);
                    if self.members.contains(&self.me) && !self.voted(m) && m != self.me {
                        out.extend(self.freeze_and_vote(m, e.seq));
                    }
                }
            }
            Payload::Propose { round, valid_round, ref change, .. } => {
                self.note_proposal(id);
                if m == self.proposer(self.epoch, round) {
                    self.vc.proposals.entry(round).or_insert((change.value(), valid_round));
                }
            }
            Payload::Prevote { round, value, .. } => {
                self.vc.prevotes.entry(round).or_default().entry(m).or_insert(value);
            }
            Payload::Precommit { round, value, boundary, .. } => {
                self.vc.precommits.entry(round).or_default().entry(m).or_insert(value);
                // HELD (P2-3): ported as at b01ebdf for digest equivalence; design-reviewer/review-p2-4.md VC-4: any
                // precommit with a boundary records a join cap. Replace with @engine-architect's fix.
                if boundary > 0 {
                    self.join_caps.entry(m).or_insert((e.seq, boundary));
                }
            }
            _ => {}
        }
    }

    /// Keep the change a proposal entry carries, by value, if its author proposes its round in the
    /// current epoch; any version counts (an equivocating proposer's other version may be decided).
    pub(super) fn note_proposal(&mut self, id: EntryId) {
        let e = &self.entries[&id];
        let Payload::Propose { epoch, round, ref change, .. } = e.payload else { return };
        let v = change.value();
        self.proposal_ids.entry(v).or_insert(id);
        if epoch == self.epoch && change.epoch == epoch && e.member == self.proposer(epoch, round) {
            let c = change.clone();
            self.vc.changes.entry(v).or_insert(c);
        }
    }

    /// The proposer of `round` of the view change that ends `epoch`: a seeded rotation over the
    /// epoch's members, so no member can choose to propose.
    pub(super) fn proposer(&self, epoch: u32, round: u32) -> MemberId {
        let mut order: Vec<MemberId> = self.members.iter().copied().collect();
        let salt = self.cfg.seed ^ ((self.world_id as u64) << 40) ^ ((epoch as u64) << 8);
        order.sort_by_key(|&m| (splitmix64(salt ^ (m as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)), m));
        order[round as usize % order.len()]
    }

    /// Members this replica holds a proof against.
    pub(super) fn proven_set(&self) -> BTreeSet<MemberId> {
        self.proofs.keys().chain(self.liars.keys()).copied().filter(|m| self.members.contains(m)).collect()
    }

    pub(super) fn proof_of(&self, m: MemberId) -> Proof {
        match self.proofs.get(&m) {
            Some(&(_, a, b)) => Proof { a, b },
            None => self.liars[&m],
        }
    }

    /// This epoch's members outside `excluded`, and (t, q) over them.
    pub(super) fn base(&self, excluded: &BTreeSet<MemberId>) -> (BTreeSet<MemberId>, usize, usize) {
        let b: BTreeSet<MemberId> = self.members.iter().copied().filter(|m| !excluded.contains(m)).collect();
        let (t, q) = quorum(self.cfg.quorum, b.len());
        (b, t, q)
    }

    /// The members a change excludes from its quorums.
    pub(super) fn excluded_of(&self, v: u64) -> Option<BTreeSet<MemberId>> {
        self.vc.changes.get(&v).map(|c| c.excluded.iter().map(|(m, _)| *m).collect())
    }

    /// Whether a quorum (counted as the change `v` defines it) sent `v` in `round` of `votes`.
    pub(super) fn quorum_for(&self, votes: &BTreeMap<u32, BTreeMap<MemberId, u64>>, round: u32, v: u64) -> bool {
        let Some(xs) = self.excluded_of(v) else { return false };
        let (base, _, q) = self.base(&xs);
        votes.get(&round).is_some_and(|r| r.iter().filter(|&(m, &x)| x == v && base.contains(m)).count() >= q)
    }

    /// Members of `base` that sent anything (`nil_only`: nil) in `round` of `votes`.
    pub(super) fn count_round(votes: &BTreeMap<u32, BTreeMap<MemberId, u64>>, round: u32, base: &BTreeSet<MemberId>, nil_only: bool) -> usize {
        votes.get(&round).map_or(0, |r| r.iter().filter(|&(m, &x)| base.contains(m) && (!nil_only || x == 0)).count())
    }

    /// The deepest point of `x`'s chain that at least `k` of the given frozen heads extend
    /// (design 10.8). With at most t faulty voters among them and `k` = their number minus t,
    /// every honest head extends it, so it keeps every entry of `x` stable anywhere; and since
    /// they number more than 2t, two branches can never both reach `k`. None while an entry
    /// needed to walk back is missing (it is fetched from the voter).
    pub(super) fn robust_cut(&mut self, x: MemberId, heads: &[(MemberId, (u32, EntryId))], k: usize) -> Option<(u32, EntryId)> {
        let mut cur: Vec<(MemberId, u32, EntryId)> = heads.iter().map(|(v, (s, id))| (*v, *s, *id)).collect();
        let mut c = cur.iter().map(|h| h.1).max().unwrap_or(0);
        loop {
            if c == 0 || k == 0 {
                return Some((0, 0));
            }
            for h in cur.iter_mut() {
                while h.1 > c {
                    let Some(e) = self.entries.get(&h.2) else {
                        let (voter, s) = (h.0, h.1);
                        self.want_from(voter, x, s, s);
                        return None;
                    };
                    h.2 = e.prev;
                    h.1 -= 1;
                }
            }
            let mut groups: BTreeMap<EntryId, usize> = BTreeMap::new();
            for h in &cur {
                if h.1 == c {
                    *groups.entry(h.2).or_default() += 1;
                }
            }
            if let Some((&id, _)) = groups.iter().find(|&(_, &n)| n >= k) {
                return Some((c, id));
            }
            c -= 1;
        }
    }

    /// Whether `p` proves `m` faulty (see [`Proof`]). None while an entry it names is missing.
    pub(super) fn verify_proof(&mut self, m: MemberId, p: Proof) -> Option<bool> {
        let (a, b) = (self.entries.get(&p.a)?.clone(), self.entries.get(&p.b)?.clone());
        if a.member != m || b.member != m {
            return Some(false);
        }
        if a.seq == b.seq && p.a != p.b {
            return Some(true);
        }
        if b.seq == a.seq + 1 && b.prev != p.a {
            return Some(true);
        }
        if let (Payload::Ack { heads, .. }, Payload::Vote { target, frozen, .. }) = (&a.payload, &b.payload) {
            if a.seq < b.seq {
                if let Some(&(_, s, rid)) = heads.iter().find(|h| h.0 == *target) {
                    if s > frozen.0 {
                        return Some(true);
                    }
                    let (mut fs, mut fid) = *frozen;
                    while fs > s {
                        fid = self.entries.get(&fid)?.prev;
                        fs -= 1;
                    }
                    return Some(fid != rid);
                }
            }
        }
        Some(false)
    }

    /// What this replica proposes when it is the round's proposer: remove every member that a
    /// quorum of usable votes names (each with its cut), excluding from the quorum the members it
    /// holds proofs against; with nothing to remove, a reset if a vote of this epoch stands
    /// against a member this replica hears from (it was wrongly suspected, or came back).
    pub(super) fn build_change(&mut self) -> Option<Change> {
        // Only proofs that their two entries show alone, so other worlds can check the certificate.
        let mut proven = BTreeSet::new();
        let mut excluded = Vec::new();
        for m in self.proven_set() {
            let p = self.proof_of(m);
            if let (Some(a), Some(b)) = (self.entries.get(&p.a), self.entries.get(&p.b)) {
                if proof_self_contained(m, p, a, b) {
                    proven.insert(m);
                    excluded.push((m, p));
                }
            }
        }
        let (base, t, q) = self.base(&proven);
        let mut remove = Vec::new();
        let targets: Vec<MemberId> = self.votes.keys().copied().collect();
        for x in targets {
            let recs: Vec<(MemberId, VoteRec)> = self.votes[&x].iter().filter(|(v, _)| base.contains(v)).map(|(v, r)| (*v, *r)).collect();
            let mut usable = Vec::new();
            for (v, r) in recs {
                if self.vote_check(x, v, r.frozen) == VoteCheck::Valid {
                    usable.push((v, r));
                }
            }
            if usable.len() < q {
                continue;
            }
            let heads: Vec<(MemberId, (u32, EntryId))> = usable.iter().map(|(v, r)| (*v, r.frozen)).collect();
            let Some(cut) = self.robust_cut(x, &heads, usable.len() - t) else { continue };
            remove.push(Removal { member: x, votes: usable.iter().map(|(_, r)| r.id).collect(), cut });
        }
        let add: Vec<MemberId> = self.join_requests.iter().copied().filter(|j| !self.members.contains(j) && !self.evicted.contains_key(j)).collect();
        let last = self.decided.last().map_or(0, |d| d.0.boundary);
        let boundary = if add.is_empty() { self.now.max(last) } else { (self.now + self.cfg.join_lead).max(last) };
        if remove.is_empty() && add.is_empty() {
            let now = self.now;
            let reset = self.votes.iter().any(|(y, vs)| {
                let heard = self.heard.get(y).copied().unwrap_or(0);
                !proven.contains(y) && now < heard + self.cfg.suspect_after && vs.get(&self.me).is_none_or(|r| heard > r.stamp)
            });
            if !reset {
                return None;
            }
        }
        Some(Change { epoch: self.epoch, remove, add, boundary, excluded })
    }

    /// Whether `c` is a valid change for this epoch. Deterministic once this replica holds every
    /// entry it names (and has validated the voters' chains up to their votes); None until then.
    pub(super) fn change_valid(&mut self, c: &Change) -> Option<bool> {
        let last = self.decided.last().map_or(0, |d| d.0.boundary);
        if c.epoch != self.epoch || c.boundary < last || c.boundary > self.now + 2 * self.cfg.join_lead {
            return Some(false);
        }
        // Joins: requests this replica holds, from non-members, with a boundary still ahead of it
        // and not too far ahead (10.8).
        let mut joiners = BTreeSet::new();
        for &j in &c.add {
            if self.members.contains(&j) || self.evicted.contains_key(&j) || !joiners.insert(j) {
                return Some(false);
            }
            if !self.join_requests.contains(&j) {
                return None;
            }
        }
        // HELD (P2-3): ported as at b01ebdf for digest equivalence; design-reviewer/review-p2-4.md VC-3: join
        // validity depends on time, which a Tendermint lock cannot tolerate. Replace with the fix.
        if !c.add.is_empty() && c.boundary <= self.now + 1 {
            return Some(false);
        }
        for &(m, p) in &c.excluded {
            let (Some(a), Some(b)) = (self.entries.get(&p.a), self.entries.get(&p.b)) else { return None };
            if !proof_self_contained(m, p, a, b) {
                return Some(false);
            }
        }
        let mut xs = BTreeSet::new();
        for &(m, p) in &c.excluded {
            if !self.members.contains(&m) || !xs.insert(m) || !self.verify_proof(m, p)? {
                return Some(false);
            }
        }
        let (base, t, q) = self.base(&xs);
        let mut seen = BTreeSet::new();
        for r in &c.remove {
            let x = r.member;
            if !self.members.contains(&x) || !seen.insert(x) || r.votes.len() < q {
                return Some(false);
            }
            let mut voters = BTreeSet::new();
            let mut heads = Vec::new();
            for &vid in &r.votes {
                let e = self.entries.get(&vid)?.clone();
                let Payload::Vote { epoch, target, frozen, .. } = e.payload else { return Some(false) };
                if epoch != self.epoch || target != x || !base.contains(&e.member) || !voters.insert(e.member) {
                    return Some(false);
                }
                // The voter's first vote against x in this epoch, in the copy of its chain I hold.
                match self.votes.get(&x).and_then(|v| v.get(&e.member)) {
                    Some(rec) if rec.id == vid => {}
                    Some(_) => return Some(false),
                    None => return None,
                }
                match self.vote_check(x, e.member, frozen) {
                    VoteCheck::Valid => {}
                    VoteCheck::Missing => return None,
                    VoteCheck::Lie(ack) => {
                        self.on_lie(e.member, ack, vid);
                        return Some(false);
                    }
                }
                heads.push((e.member, frozen));
            }
            if self.robust_cut(x, &heads, heads.len() - t)? != r.cut {
                return Some(false);
            }
        }
        Some(true)
    }

    pub(super) fn author_vc(&mut self, payload: Payload) -> Entry {
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp: self.frame.max(self.now), payload, carried: None };
        self.publish(e)
    }

    pub(super) fn start_round(&mut self, r: u32) {
        self.vc.round = r;
        self.vc.step = Step::Propose;
        self.vc.round_at = self.now;
        self.vc.proposed = false;
        self.stats.max_round = self.stats.max_round.max(r);
    }

    pub(super) fn prevote(&mut self, v: u64, out: &mut Vec<Entry>) {
        self.vc.step = Step::Prevote;
        if !self.withhold_vc {
            let (epoch, round) = (self.epoch, self.vc.round);
            out.push(self.author_vc(Payload::Prevote { epoch, round, value: v }));
        }
    }

    pub(super) fn precommit(&mut self, v: u64, out: &mut Vec<Entry>) {
        self.precommit_join(v, 0, out);
    }

    pub(super) fn precommit_join(&mut self, v: u64, boundary: u64, out: &mut Vec<Entry>) {
        self.vc.step = Step::Precommit;
        if !self.withhold_vc {
            let (epoch, round) = (self.epoch, self.vc.round);
            out.push(self.author_vc(Payload::Precommit { epoch, round, value: v, boundary }));
        }
    }

    /// Run this epoch's view change and return the entries to broadcast. Called every tick:
    /// Tendermint's rules (algorithm 1, lines 11-65) over chain entries, timeouts in clock frames.
    pub fn view_change_due(&mut self) -> Vec<Entry> {
        let mut out = Vec::new();
        // Everyone that holds the log follows its decisions, members or not (a joiner learns so).
        if !self.byz_ally && self.try_decide(&mut out) {
            return out;
        }
        if !self.members.contains(&self.me) {
            return out;
        }
        if self.byz_ally {
            // Prevote and precommit every proposal seen, first per round (a linear chain).
            let props: Vec<(u32, u64)> = self.vc.proposals.iter().map(|(r, (v, _))| (*r, *v)).collect();
            for (r, v) in props {
                let me = self.me;
                if !self.vc.prevotes.get(&r).is_some_and(|x| x.contains_key(&me)) {
                    let epoch = self.epoch;
                    out.push(self.author_vc(Payload::Prevote { epoch, round: r, value: v }));
                    let boundary = self.vc.changes.get(&v).filter(|c| !c.add.is_empty()).map_or(0, |c| c.boundary);
                    out.push(self.author_vc(Payload::Precommit { epoch, round: r, value: v, boundary }));
                }
            }
            // Drive every round another member gave up on: echo its nil prevote or precommit where no
            // proposal is held, so a side of a partition that is no quorum alone still reaches a round
            // one of its own members proposes in.
            let (me, epoch) = (self.me, self.epoch);
            let nil_in = |votes: &BTreeMap<u32, BTreeMap<MemberId, u64>>| -> Vec<u32> {
                votes.iter().filter(|(_, ms)| !ms.contains_key(&me) && ms.values().any(|v| *v == 0)).map(|(r, _)| *r).collect()
            };
            for r in nil_in(&self.vc.prevotes) {
                if !self.vc.proposals.contains_key(&r) {
                    out.push(self.author_vc(Payload::Prevote { epoch, round: r, value: 0 }));
                }
            }
            for r in nil_in(&self.vc.precommits) {
                if !self.vc.proposals.contains_key(&r) {
                    out.push(self.author_vc(Payload::Precommit { epoch, round: r, value: 0, boundary: 0 }));
                }
            }
            return out;
        }
        if self.vc.since.is_none() {
            let joins = self.join_requests.iter().any(|j| !self.members.contains(j) && !self.evicted.contains_key(j));
            let busy = joins || !self.votes.is_empty() || !self.vc.proposals.is_empty() || !self.vc.prevotes.is_empty() || !self.vc.precommits.is_empty();
            if !busy {
                return out;
            }
            self.vc.since = Some(self.now);
            self.start_round(0);
        }
        for _ in 0..16 {
            let before = (self.vc.round, self.vc.step, self.vc.proposed, self.vc.polka_done.len(), self.vc.prevote_timer.len(), self.vc.precommit_timer.len());
            self.vc_rules(&mut out);
            if self.try_decide(&mut out) {
                return out;
            }
            if before == (self.vc.round, self.vc.step, self.vc.proposed, self.vc.polka_done.len(), self.vc.prevote_timer.len(), self.vc.precommit_timer.len()) {
                break;
            }
        }
        out
    }

    pub(super) fn vc_rules(&mut self, out: &mut Vec<Entry>) {
        let (e, r) = (self.epoch, self.vc.round);
        let proven = self.proven_set();
        let (any_base, t_any, q_any) = self.base(&proven);
        // Lines 55-56: more than t members are in a later round: go there.
        let mut hi: BTreeMap<MemberId, u32> = BTreeMap::new();
        for votes in [&self.vc.prevotes, &self.vc.precommits] {
            for (&rr, ms) in votes.range(r + 1..) {
                for m in ms.keys().filter(|m| any_base.contains(m)) {
                    let x = hi.entry(*m).or_insert(rr);
                    *x = (*x).max(rr);
                }
            }
        }
        let mut later: Vec<u32> = hi.into_values().collect();
        later.sort_unstable_by(|a, b| b.cmp(a));
        if later.len() > t_any {
            self.start_round(later[t_any]);
            return;
        }
        // Lines 14-19: the round's proposer proposes its valid value, or a new one.
        if self.vc.step == Step::Propose && !self.vc.proposed && !self.withhold_vc && self.proposer(e, r) == self.me {
            let (vr, change) = match self.vc.valid {
                Some((v, vr)) => (vr, self.vc.changes.get(&v).cloned()),
                None => (NO_ROUND, self.build_change()),
            };
            // Members that fail together are suspected within a few frames of each other: give the
            // votes on every member this replica itself suspects a few frames to arrive, so that one
            // change removes them all.
            let me = self.me;
            let gathering = vr == NO_ROUND
                && self.now < self.vc.round_at + VC_GATHER
                && change.as_ref().is_some_and(|c| self.votes.iter().any(|(y, vs)| vs.contains_key(&me) && !c.remove.iter().any(|x| x.member == *y)));
            if let Some(c) = change.filter(|_| !gathering) {
                self.vc.proposed = true;
                self.ev(format!("view change: propose in round {r}: remove {:?}, excluded {:?}", c.remove.iter().map(|x| (x.member, x.cut.0)).collect::<Vec<_>>(), c.excluded.iter().map(|x| x.0).collect::<Vec<_>>()));
                out.push(self.author_vc(Payload::Propose { epoch: e, round: r, valid_round: vr, change: c }));
            }
        }
        // Lines 22-33: prevote on the round's proposal.
        if self.vc.step == Step::Propose {
            if let Some(&(v, vr)) = self.vc.proposals.get(&r) {
                let c = self.vc.changes.get(&v).cloned();
                let ok = match c {
                    Some(c) => self.change_valid(&c),
                    None => Some(false),
                };
                let decision = match ok {
                    None => None,
                    Some(ok) if vr == NO_ROUND => Some(ok && self.vc.locked.is_none_or(|(lv, _)| lv == v)),
                    Some(ok) if vr < r && self.quorum_for(&self.vc.prevotes, vr, v) => {
                        Some(ok && self.vc.locked.is_none_or(|(lv, lr)| lr <= vr || lv == v))
                    }
                    Some(_) => None,
                };
                if let Some(yes) = decision {
                    self.prevote(if yes { v } else { 0 }, out);
                }
            }
        }
        // Lines 34-35: a quorum of prevotes of any kind arms the prevote timeout.
        if self.vc.step == Step::Prevote && !self.vc.prevote_timer.contains_key(&r) && Self::count_round(&self.vc.prevotes, r, &any_base, false) >= q_any {
            self.vc.prevote_timer.insert(r, self.now);
        }
        // Lines 36-43: a quorum prevoted the round's proposal: lock it and precommit it.
        if self.vc.step != Step::Propose && !self.vc.polka_done.contains(&r) {
            if let Some(&(v, _)) = self.vc.proposals.get(&r) {
                if self.quorum_for(&self.vc.prevotes, r, v) {
                    let c = self.vc.changes.get(&v).cloned().unwrap_or_default();
                    let in_time = c.add.is_empty() || c.boundary > self.now;
                    if self.vc.changes.contains_key(&v) && in_time && self.change_valid(&c) == Some(true) {
                        self.vc.polka_done.insert(r);
                        if self.vc.step == Step::Prevote {
                            self.vc.locked = Some((v, r));
                            self.precommit_join(v, if c.add.is_empty() { 0 } else { c.boundary }, out);
                        }
                        self.vc.valid = Some((v, r));
                    }
                }
            }
        }
        // Lines 44-46: a quorum prevoted nil.
        if self.vc.step == Step::Prevote && Self::count_round(&self.vc.prevotes, r, &any_base, true) >= q_any {
            self.precommit(0, out);
        }
        // Lines 47-48: a quorum of precommits of any kind arms the precommit timeout; a quorum of
        // nil precommits ends the round at once (no value can be decided in it).
        if Self::count_round(&self.vc.precommits, r, &any_base, true) >= q_any {
            self.start_round(r + 1);
            return;
        }
        if !self.vc.precommit_timer.contains_key(&r) && Self::count_round(&self.vc.precommits, r, &any_base, false) >= q_any {
            self.vc.precommit_timer.insert(r, self.now);
        }
        // Lines 57-65: timeouts, lengthening with the round.
        let to = self.cfg.vc_timeout + self.cfg.vc_timeout_step * r as u64;
        if self.vc.step == Step::Propose && self.now >= self.vc.round_at + to {
            self.prevote(0, out);
        }
        if self.vc.step == Step::Prevote && self.vc.prevote_timer.get(&r).is_some_and(|&a| self.now >= a + to) {
            self.precommit(0, out);
        }
        if self.vc.precommit_timer.get(&r).is_some_and(|&a| self.now >= a + to) {
            self.start_round(r + 1);
        }
        let _ = t_any;
    }

    /// Lines 49-54: a quorum precommitted a value in some round: decide it and apply it. A value
    /// this replica holds no proposal for is fetched from those that precommitted it.
    pub(super) fn try_decide(&mut self, out: &mut Vec<Entry>) -> bool {
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for (&rr, ms) in &self.vc.precommits {
            let mut vals: BTreeMap<u64, Vec<MemberId>> = BTreeMap::new();
            for (&m, &v) in ms {
                if v != 0 {
                    vals.entry(v).or_default().push(m);
                }
            }
            for (v, ms) in vals {
                if !self.vc.changes.contains_key(&v) {
                    unknown.push((v, ms));
                } else if self.quorum_for(&self.vc.precommits, rr, v) {
                    candidates.push((rr, v));
                }
            }
        }
        for (v, ms) in unknown {
            for m in ms {
                if m != self.me && self.value_asked.insert((m, v)) {
                    self.value_wants.push((m, v));
                }
            }
        }
        for (rr, v) in candidates {
            // A change's exclusions set its quorum: decide it only once every one is proved, or a
            // faulty minority could exclude honest members and decide alone (P2-4).
            let ex = self.vc.changes[&v].excluded.clone();
            let mut proved = true;
            for (m, p) in ex {
                if self.verify_proof(m, p) != Some(true) {
                    proved = false;
                }
            }
            if proved {
                let c = self.vc.changes[&v].clone();
                self.apply_change(c, rr, out);
                return true;
            }
        }
        false
    }

    /// Apply a decided change: the next epoch's members, each removed member's cut, a new epoch
    /// with no votes, and the epoch certificate's material.
    pub(super) fn apply_change(&mut self, c: Change, round: u32, out: &mut Vec<Entry>) {
        let v = c.value();
        let xs: BTreeSet<MemberId> = c.excluded.iter().map(|(m, _)| *m).collect();
        let sigs: Vec<(MemberId, u64)> = self
            .vc
            .precommits
            .get(&round)
            .map(|ms| ms.iter().filter(|&(m, &x)| x == v && !xs.contains(m)).map(|(&m, _)| (m, vc_sig(self.world_id, m, self.epoch, round, v))).collect())
            .unwrap_or_default();
        let removed: Vec<MemberId> = c.remove.iter().map(|r| r.member).collect();
        for r in &c.remove {
            self.members.remove(&r.member);
            self.frozen.insert(r.member, r.cut.0);
            self.cut_ids.insert(r.member, r.cut.1);
            self.evicted.insert(r.member, (r.cut.0, 0));
        }
        for &j in &c.add {
            self.members.insert(j);
            self.known.insert(j);
            self.admitted.insert(j, c.boundary);
            self.followers.remove(&j);
            self.join_requests.remove(&j);
        }
        self.join_caps.clear();
        let from = self.epoch;
        self.decided.push((c.clone(), round, sigs));
        self.epoch += 1;
        self.vc_log.push((self.epoch, self.now, round, removed.clone()));
        self.stats.view_changes += 1;
        if removed.is_empty() && c.add.is_empty() {
            self.stats.resets += 1;
        }
        self.ev(format!(
            "VIEW CHANGE decided (epoch {from} -> {}, round {round}): removed {:?} with cuts {:?}, excluded by proof {:?}, added {:?} from frame {}; members {:?}",
            self.epoch,
            removed,
            c.remove.iter().map(|r| r.cut.0).collect::<Vec<_>>(),
            xs,
            c.add,
            c.boundary,
            self.members
        ));
        // A new epoch: the old one's votes, freezes and view-change state end.
        self.votes.clear();
        self.vc = Vc::default();
        let unfreeze: Vec<MemberId> = self.frozen.keys().copied().filter(|m| !self.evicted.contains_key(m)).collect();
        for m in unfreeze {
            self.frozen.remove(&m);
        }
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for m in members {
            self.extend_chain(m, out);
        }
        for x in removed {
            self.adopt_cut(x);
        }
        // An admitted member's entries act from now on (those stamped before its boundary are void).
        let joined: Vec<EntryId> = self.entries.iter().filter(|(_, e)| c.add.contains(&e.member)).map(|(id, _)| *id).collect();
        for id in joined {
            self.set_eff(id);
        }
        if let Some(ids) = self.vc_later.remove(&self.epoch) {
            for id in ids {
                let m = self.entries[&id].member;
                self.record_vc(m, id, out);
            }
        }
        self.recompute_conf();
    }

    /// Make the version of removed member `x`'s chain that ends at its cut the one this replica
    /// holds: walk back from the cut id, fetch what is missing, replace any other version (8.2
    /// verdicts are re-derived along it), and keep exactly the cut (P2-4).
    pub(super) fn adopt_cut(&mut self, x: MemberId) {
        if self.cut_done.contains(&x) {
            return;
        }
        let (Some(&(cut, _)), Some(&cid)) = (self.evicted.get(&x), self.cut_ids.get(&x)) else { return };
        let held = self.contiguous.get(&x).copied().unwrap_or(0);
        if cut > 0 && !(held >= cut && self.by_author.get(&(x, cut)) == Some(&cid)) {
            let mut ids = Vec::with_capacity(cut as usize);
            let (mut s, mut id) = (cut, cid);
            loop {
                let Some(e) = self.entries.get(&id) else {
                    self.want_all(x, s, s);
                    return;
                };
                ids.push((s, id));
                if s == 1 {
                    break;
                }
                id = e.prev;
                s -= 1;
            }
            let mut changed = false;
            for &(s, id) in &ids {
                if self.by_author.get(&(x, s)) != Some(&id) {
                    self.by_author.insert((x, s), id);
                    changed = true;
                }
            }
            if changed {
                self.contiguous.insert(x, 0);
                self.chain_state.remove(&x);
                self.ack_hist.remove(&x);
            }
            let mut out = Vec::new();
            self.extend_chain(x, &mut out);
        }
        self.reconcile(x);
        if self.contiguous.get(&x).copied().unwrap_or(0) >= cut {
            let v = self.ack_hist.get(&x).and_then(|a| a.range(..=cut).next_back()).map_or(0, |(_, h)| h + 1);
            self.evicted.insert(x, (cut, v));
            self.cut_done.insert(x);
            self.ev(format!("member {x} evicted (epoch {}): its chain is cut at seq {cut}; it no longer counts from frame {v}", self.epoch));
        }
    }

    /// The certificate of the view change that started `epoch` (R3-8b).
    pub fn epoch_cert(&self, epoch: u32) -> Option<EpochCert> {
        let (c, round, sigs) = self.decided.get(epoch.checked_sub(1)? as usize)?;
        let mut proofs = Vec::new();
        for (_, p) in &c.excluded {
            proofs.push(self.entries.get(&p.a)?.clone());
            proofs.push(self.entries.get(&p.b)?.clone());
        }
        Some(EpochCert { world: self.world_id, epoch, change: c.clone(), round: *round, members: self.members_at(epoch)?, precommits: sigs.clone(), proofs })
    }

    /// This world's certificates for epochs 1..=`epoch`: what a fact signed in `epoch` carries so
    /// any world can check it from this world's genesis (P2-5).
    pub fn epoch_certs_upto(&self, epoch: u32) -> Option<Vec<EpochCert>> {
        (1..=epoch).map(|e| self.epoch_cert(e)).collect()
    }

    /// The epoch of this world active at `frame`, by the boundaries of the decided changes.
    pub fn epoch_at(&self, frame: u64) -> u32 {
        self.decided.iter().take_while(|d| d.0.boundary <= frame).count() as u32
    }

    /// A certificate for `fact`, a fact about `frame`: Attest entries of a strict majority of the
    /// members of the epoch active at that frame (P2-5's epoch-at-frame rule). Members that the next
    /// epoch excluded by proof do not count in that majority's size.
    pub fn cert_at(&self, fact: u64, frame: u64) -> Option<(u32, Vec<(MemberId, u64)>)> {
        let e = self.epoch_at(frame);
        let mut members: BTreeSet<MemberId> = self.members_at(e)?.into_iter().collect();
        if let Some((next, _, _)) = self.decided.get(e as usize) {
            for (m, _) in &next.excluded {
                members.remove(m);
            }
        }
        let mut c: BTreeMap<MemberId, u64> = BTreeMap::new();
        for (id, en) in &self.entries {
            if let Payload::Attest { fact: f, sig } = en.payload {
                if f == fact && !self.is_void(*id) && members.contains(&en.member) {
                    c.insert(en.member, sig);
                }
            }
        }
        (c.len() * 2 > members.len()).then(|| (e, c.into_iter().collect()))
    }

    /// The members of `epoch`, replayed from the genesis members and the decided changes.
    pub fn members_at(&self, epoch: u32) -> Option<Vec<MemberId>> {
        if epoch > self.epoch {
            return None;
        }
        let mut m: BTreeSet<MemberId> = self.genesis.iter().copied().collect();
        for (c, _, _) in self.decided.iter().take(epoch as usize) {
            for r in &c.remove {
                m.remove(&r.member);
            }
            for a in &c.add {
                m.insert(*a);
            }
        }
        Some(m.into_iter().collect())
    }

    /// A non-member asks to join (P2-4): keep its request, store its entries, send it the log.
    pub fn on_join(&mut self, j: MemberId) {
        if !self.members.contains(&j) && !self.evicted.contains_key(&j) && j != self.me {
            self.join_requests.insert(j);
            self.follow(j);
        }
    }

    /// Send `j` this replica's entries and store its own (a member about to join, in the spike).
    pub fn follow(&mut self, j: MemberId) {
        if !self.members.contains(&j) && j != self.me {
            self.known.insert(j);
            self.followers.insert(j);
        }
    }

    /// Who this replica sends its entries to: the members and the followers.
    pub fn audience(&self) -> Vec<MemberId> {
        self.members.iter().chain(self.followers.iter()).copied().filter(|m| *m != self.me).collect::<BTreeSet<_>>().into_iter().collect()
    }

    /// Test fixture (D80 bridge): from now on, acks and votes report no more of every other
    /// member's chain than this replica holds now, as a faulty member hiding what it hears. It keeps
    /// processing everything. `unfreeze_others` ends it.
    pub fn freeze_others(&mut self) {
        let heads: BTreeMap<MemberId, (u32, EntryId)> = self
            .contiguous
            .iter()
            .filter(|(m, s)| **m != self.me && **s > 0)
            .map(|(m, s)| (*m, (*s, self.by_author[&(*m, *s)])))
            .collect();
        self.hidden = Some(heads);
    }

    pub fn unfreeze_others(&mut self) {
        self.hidden = None;
    }

    /// Diagnostics: this replica's view-change state and how much of each chain it holds.
    pub fn vc_debug(&self) -> String {
        let pv: Vec<String> = self.vc.prevotes.iter().map(|(r, m)| format!("r{r}:{:?}", m.iter().map(|(k, v)| (*k, *v != 0)).collect::<Vec<_>>())).collect();
        let pc: Vec<String> = self.vc.precommits.iter().map(|(r, m)| format!("r{r}:{:?}", m.iter().map(|(k, v)| (*k, *v != 0)).collect::<Vec<_>>())).collect();
        format!(
            "now {} epoch {} round {} step {:?} since {:?} proposals {:?} prevotes [{}] precommits [{}] votes {:?} contiguous {:?} heard {:?}",
            self.now,
            self.epoch,
            self.vc.round,
            self.vc.step,
            self.vc.since,
            self.vc.proposals.keys().collect::<Vec<_>>(),
            pv.join(" "),
            pc.join(" "),
            self.votes.iter().map(|(t, v)| (*t, v.keys().copied().collect::<Vec<_>>())).collect::<Vec<_>>(),
            self.contiguous,
            self.heard
        )
    }

    /// The proposer of `round` of the view change ending `epoch`, if `epoch` is the current one.
    pub fn proposer_of(&self, epoch: u32, round: u32) -> Option<MemberId> {
        (epoch == self.epoch).then(|| self.proposer(epoch, round))
    }

    /// Answer `Msg::FetchValue`: the proposal entry carrying `value`, any version.
    pub fn on_fetch_value(&self, value: u64) -> Vec<Msg> {
        self.proposal_ids.get(&value).and_then(|id| self.entries.get(id)).map(|e| vec![Msg::Entry(e.clone())]).unwrap_or_default()
    }
}

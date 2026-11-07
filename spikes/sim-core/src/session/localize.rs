//! Thin links, Part B (thin-link plan revision 16, as confirmed by the user): localizing a thin link's
//! effects to its own player in sessions of 4 or more members.
//!
//! Section 3, the deadline's inputs, from logged data only: each author's base delay L, the session's
//! queue tolerance Q, the echo delay E and the cap C_max, and rule 2's race window G. Every value comes
//! from echo-stable acks of settled windows, so every honest replica computes the same ones.
//!
//! Spike deviation: the values live in session state, not in the check-hashed world state the plan puts
//! them in, since the spike has no 9.5 joiner that would need them without the log. A replica computing
//! other values would still show, in the world digests and the check hashes.

use std::collections::{BTreeMap, BTreeSet};

use super::{quorum, Entry, EntryId, MemberId, Payload, QuorumRule, WorldSim, CKPT_FRAMES};

/// How a decided change removed a member (ruling 1, m5b983ab01446): Q1's finality leaves out only members removed on
/// suspicion, which the fault model charges against t.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemovalKind {
    /// Its own leave request.
    Leave,
    /// An admin's kick.
    Kick,
    /// Votes: silence, a floor or latency vote, a proof.
    Suspicion,
}

/// Rule 1's scope: perturbations and hand entries. Acks are the evidence; view-change, admin and R14
/// entries keep their own rules.
pub(super) fn judged(p: &Payload) -> bool {
    matches!(p, Payload::Touch { .. } | Payload::Hand { .. })
}

/// (spike diagnostics) A Part B removal's cut as this replica finalized it: when; the epoch whose vouch is the cut
/// (carry-over: the largest final vouch over the removed member's epochs); each other member of that epoch with the
/// seq its evidence was read to (its echo-stable prefix there), that evidence, and whether it was still a member then;
/// the cut that epoch's evidence gives without the members that had left; and its last epoch's vouch alone (the cut
/// before carry-over).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutRecord {
    pub at_frame: u64,
    pub cut: u32,
    pub epoch: u32,
    pub voters: Vec<(MemberId, Option<u32>, u32, bool)>,
    pub without_left: u32,
    pub last_epoch: u32,
}

/// (spike diagnostics: PE_CUT_TRACE=1 prints the evidence behind each Part B cut, and the vouched prefix behind each
/// counted entry of a removed member or an older epoch)
/// (spike) PE_WINDOW_NOW=1: a 3-member window's mode is read for the current epoch only, and an earlier epoch's entry
/// is judged by rule 1, as before the window-mode note's fix (m5b983ab01446).
fn window_now() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("PE_WINDOW_NOW").is_ok())
}

/// (spike) PE_OLD_FINAL=1: the final term reads each member's acks up to its final raw prefix and matches references
/// against held entries, as the build before Q2's rulings did (with one level, that build: the premise-1 cell bites).
fn old_final() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("PE_OLD_FINAL").is_ok())
}

fn verdict_author() -> Option<MemberId> {
    static V: std::sync::OnceLock<Option<MemberId>> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("PE_VERDICT_AUTHOR").ok().and_then(|v| v.parse().ok()))
}

fn cut_trace() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("PE_CUT_TRACE").is_ok())
}

/// (spike diagnostics: PE_MODE_TRACE=<window>) Once a second, while that 3-member window's mode is unknown here: the
/// member whose first ack in it blocks the mode, and each other member's matched reference to its chain.
fn mode_trace() -> Option<u64> {
    static V: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("PE_MODE_TRACE").ok().and_then(|v| v.parse().ok()))
}

/// F2: one epoch's membership, as rule 1 judges that epoch's entries (revision 21 (a): a removed member's entries
/// with the members and quorum of the epoch it was last a member of).
#[derive(Clone, Debug)]
pub(super) struct Ctx {
    pub epoch: u32,
    pub members: Vec<MemberId>,
    pub q: usize,
}

impl Ctx {
    /// The rank, among the others' references sorted newest first, that echo-stability and vouching read: q − 2
    /// (q − 1 others), and at 3 players both others (S-3).
    fn rank(&self) -> usize {
        if self.members.len() == 3 { 1 } else { self.q.saturating_sub(2) }
    }
}

/// Section 3: windows are aligned blocks of this many frames; window k's values come from window k − 2.
pub const WINDOW: u64 = 300;
/// L_m is the minimum over the last 2 minutes: windows k − 25 to k − 2.
const L_WINDOWS: u64 = 24;

/// One window's values (section 3), all in frames.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowVals {
    /// Each member's base delay L_m(k).
    pub l: BTreeMap<MemberId, u64>,
    /// The queue tolerance Q(k).
    pub q: u64,
    /// Q's computed floor Q_min'(k).
    pub q_floor: u64,
    /// The P95 echo delay E(k), from an ack's stamp to its q-th echoing reference.
    pub e: u64,
    /// The deadline cap C_max(k).
    pub c_max: u64,
    /// Each member's largest gap between consecutive echo-stable acks in window k − 2 (P_m).
    pub p: BTreeMap<MemberId, u64>,
    /// Rule 3 while n − f < q (P1 (a)): C_floor(k), and each fed member's (E_T, U_T) from window k − 2; the fed
    /// members over the latency floor (32 + E_T + U_T + 4 > W − 15 − C_floor). Empty otherwise.
    pub c_floor: u64,
    pub fed_terms: BTreeMap<MemberId, (u64, u64)>,
    pub over_floor: Vec<MemberId>,
}

/// Per window: what its settled data gives the windows after it.
#[derive(Clone, Debug, Default)]
pub(super) struct WindowData {
    /// Per member: the minimum d(e) over its entries stamped in the window.
    min_d: BTreeMap<MemberId, u64>,
    /// Per member: every d(e) of its entries stamped in the window.
    d: BTreeMap<MemberId, Vec<u64>>,
    /// Per member: the echo delay of each of its acks stamped in the window (the (q − 1)-th smallest δ over
    /// the other members; the author is the q-th).
    echo: BTreeMap<MemberId, Vec<u64>>,
    /// Per member: its largest gap between consecutive echo-stable acks, the later stamped in the window.
    gap: BTreeMap<MemberId, u64>,
    /// Members whose acks in the window carry an anchor (fed), and each one's echo delays E_T (section 3): for each
    /// checkpoint its acks stamped in the window name for the first time, the ack's stamp minus the checkpoint's.
    fed: BTreeSet<MemberId>,
    e_t: BTreeMap<MemberId, Vec<u64>>,
}

/// The P95 of `v` (index ceil(0.95 × count) − 1, ascending), or None if empty.
fn p95(v: &mut [u64]) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[(v.len() * 95).div_ceil(100) - 1])
}

/// The lower median: the element at index floor((count − 1) / 2), ascending.
fn lower_median(v: &mut [u64]) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[(v.len() - 1) / 2])
}

impl WorldSim {
    /// Section 3's values of every window computed so far.
    pub fn window_values(&self) -> &BTreeMap<u64, WindowVals> {
        &self.windows
    }

    /// Part B's rules apply: switched on, and 4 or more members (2 and 3 keep today's rules, section 7).
    pub fn localized(&self) -> bool {
        self.cfg.localize && self.members.len() >= 4
    }

    /// Part B at 3 players (plan section 7): the state feed and fed mode, with Part B switched on.
    pub fn fed_mode_capable(&self) -> bool {
        self.cfg.localize && self.members.len() == 3
    }

    /// Part B at 3 players (S-1, S-3): window k's mode. Some(true) in fed mode: some member's latest ack stamped
    /// before the window's start carries an anchor (fed mode starts at the first window starting after a member's
    /// first anchored ack and ends at the first window starting after its first ack without one). None until it is
    /// known: every member's first ack stamped at or after the window's start lies in its prefix both others
    /// reference (D54 stability, or echo-stability with both others, F by its echo), so only one version of each
    /// switch can count. Some(false) outside 3-player sessions with Part B.
    pub fn window_mode(&self, k: u64) -> Option<bool> {
        if !self.fed_mode_capable() {
            return Some(false);
        }
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        self.window_mode_in(k, &members)
    }

    /// The window-mode note (@coordinator-opus m5b983ab01446, @design-reviewer m3a3ca7b15bac): window k's mode in a
    /// 3-member epoch with `members`, read from the chains as for the current epoch, so an entry of an earlier epoch is
    /// judged by its own epoch's mode whenever a replica judges it (judged under rule 1 once its epoch had ended, a D54
    /// window's entry could be void at one replica and count at another). A member removed since, whose acks up to its
    /// B (held) end before the window, has nothing left to fix: its last ack before the window is final.
    pub(super) fn window_mode_in(&self, k: u64, members: &[MemberId]) -> Option<bool> {
        let start = k * WINDOW;
        let mut fed = false;
        for &m in members {
            let acks = self.ack_by_stamp.get(&m)?;
            let ended = self.removed_in.contains_key(&m).then(|| self.bound.get(&m).copied()).flatten().filter(|&b| self.contiguous.get(&m).copied().unwrap_or(0) >= b);
            match (acks.range(start..).next().map(|(_, &s)| s), ended) {
                (Some(first_after), b) if b.is_none_or(|b| first_after <= b) => {
                    // The prefix of m's chain both others reference, in the version I hold; as in Q1's finality at n = 3
                    // (m8399b581a416), without a member since removed whose acks up to its B never name m's chain at or
                    // past this ack: the other alone fixes it. (Without that, a leaver's silence from its request on left
                    // the later windows' modes unknown for good: leave-s4-rejoin's conf stopped at 603.)
                    let stable = members.iter().filter(|&&y| y != m && !self.silent_removed(y, m, first_after)).map(|&y| self.matched_ref(y, m)).min().unwrap_or(0);
                    if first_after > stable {
                        if mode_trace() == Some(k) && self.now % 60 == 0 {
                            let refs: Vec<(MemberId, u32, bool)> = members.iter().filter(|&&y| y != m).map(|&y| (y, self.matched_ref(y, m), self.silent_removed(y, m, first_after))).collect();
                            eprintln!(
                                "MODE m{} now {} conf {} window {k}: m{m}'s first ack at or after {start} is s{first_after}, past what both others reference; (member, matched ref to m{m}, silent-removed) {refs:?}; I hold m{m} to s{}",
                                self.me,
                                self.now,
                                self.conf,
                                self.contiguous.get(&m).copied().unwrap_or(0)
                            );
                        }
                        return None;
                    }
                }
                (_, Some(_)) => {}
                _ => {
                    if mode_trace() == Some(k) && self.now % 60 == 0 {
                        let last = acks.iter().next_back().map(|(&st, &s)| (s, st));
                        eprintln!("MODE m{} now {} conf {} window {k}: m{m} has no ack at or after {start} that I hold; its newest (seq, stamp) {last:?}; I hold m{m} to s{}", self.me, self.now, self.conf, self.contiguous.get(&m).copied().unwrap_or(0));
                    }
                    return None;
                }
            }
            if let Some((_, &last_before)) = acks.range(..start).rev().find(|&(_, &s)| ended.is_none_or(|b| s <= b)) {
                fed |= self.anchored_acks.get(&m).is_some_and(|a| a.contains(&last_before));
            }
        }
        Some(fed)
    }

    /// Member m's echo as of its last anchored ack before `seq` (empty if none): which block of each chain it had
    /// already named, so E_T counts each checkpoint at its first echo.
    fn prev_echo(&self, m: MemberId, seq: u32) -> BTreeMap<MemberId, u64> {
        let mut s = seq;
        while s > 1 {
            s -= 1;
            let Some(e) = self.by_author.get(&(m, s)).and_then(|id| self.entries.get(id)) else { break };
            if let super::Payload::Ack { fed: Some(fa), .. } = &e.payload {
                return fa.echo.iter().copied().collect();
            }
            if seq - s > 256 {
                break;
            }
        }
        BTreeMap::new()
    }

    fn quorum_q(&self) -> usize {
        quorum(QuorumRule::Bft, self.members.len()).1
    }

    /// F2: the epoch of `m`'s entry `seq`, by its chain: the epoch of m's latest ack at or before it (an ack's epoch
    /// rises by at most one, VC-5). Every replica holding the chain reads the same whenever the decision reached it;
    /// by frame it could differ, since conf can pass a removal's boundary before the decision.
    pub(super) fn entry_epoch(&self, m: MemberId, seq: u32) -> u32 {
        let base = self.admitted_epoch.get(&m).copied().unwrap_or(0);
        self.ack_epoch_from.get(&m).and_then(|a| a.iter().filter(|&(_, &s)| s <= seq).map(|(&ep, _)| ep).max()).map_or(base, |ep| ep.max(base))
    }

    /// F2: Part B's rules apply to entry `e`: its epoch (by its author's chain) had 4 or more members. Rule 2 reads
    /// this rather than the current membership, so a frame re-simulated after a removal, and the offline replay,
    /// apply the rules the frame's entries were judged by.
    pub(super) fn part_b_entry(&self, e: &Entry) -> bool {
        self.cfg.localize && self.ctx(self.entry_epoch(e.member, e.seq)).is_some_and(|c| c.members.len() >= 4)
    }

    /// F2: epoch `epoch`'s members and quorum. The current epoch's from the live membership.
    pub(super) fn ctx(&self, epoch: u32) -> Option<Ctx> {
        let members: Vec<MemberId> = if epoch == self.epoch { self.members.iter().copied().collect() } else { self.members_at(epoch)? };
        let q = quorum(QuorumRule::Bft, members.len()).1;
        Some(Ctx { epoch, members, q })
    }

    /// F2: the last of `y`'s acks that counts toward removed member `x`'s chain: y's acks from x's last epoch
    /// (revision 21 (a): no ack from an epoch x isn't a member of counts). u32::MAX for a current member.
    pub(super) fn ref_cap(&self, y: MemberId, x: MemberId) -> u32 {
        match self.removed_in.get(&x) {
            Some(e) => self.ack_epoch_from.get(&y).and_then(|a| a.get(e)).map_or(u32::MAX, |&s| s.saturating_sub(1)),
            None => u32::MAX,
        }
    }

    /// F2: `voter`'s vote against `target` in `epoch`: (the vote's seq, its frozen head's seq). The current epoch's
    /// from the recorded votes; an earlier one's from the chains (the recorded votes end with their epoch).
    pub(super) fn vote_on(&self, voter: MemberId, target: MemberId, epoch: u32) -> Option<(u32, u32)> {
        if epoch == self.epoch && self.members.contains(&target) {
            let rec = self.votes.get(&target)?.get(&voter)?;
            return Some((self.entries.get(&rec.id)?.seq, rec.frozen.0));
        }
        self.vote_hist.get(&(target, voter))?.iter().find(|r| r.0 == epoch).map(|r| (r.1, r.2))
    }

    /// The newest seq of `x`'s chain that `y`'s acks have referenced in the version I hold, at or below my
    /// own held head (as `stable_seq` matches references, review VC5R-2), capped at y's own vote's frozen head on x
    /// in the epoch they're read in (R3-1 in every reader, m5b983ab01446; x's last epoch if removed). 0 if none.
    /// A fed member's echo (rule 1, TLP-15) counts here as its references: `echo_refs`.
    fn matched_ref(&self, y: MemberId, x: MemberId) -> u32 {
        let v = self.matched_ref_raw(y, x);
        let ep = self.removed_in.get(&x).map_or(self.epoch, |&r| r.saturating_sub(1));
        self.vote_on(y, x, ep).map_or(v, |(_, f)| v.min(f))
    }

    fn matched_ref_raw(&self, y: MemberId, x: MemberId) -> u32 {
        let held = self.contiguous.get(&x).copied().unwrap_or(0);
        // F2: toward a removed member's chain, only y's acks from x's last epoch count (by ack seq, which `refs`
        // doesn't keep).
        if self.removed_in.contains_key(&x) {
            let cap = self.ref_cap(y, x);
            let mine = |s: u32, hid: EntryId| s <= held && self.by_author.get(&(x, s)) == Some(&hid);
            let r = self.ack_head_hist.get(&(y, x)).and_then(|h| h.range(..=cap).rev().find(|(_, v)| mine(v.0, v.1))).map_or(0, |(_, v)| v.0);
            let e = self
                .echo_refs
                .get(&(y, x))
                .and_then(|r| r.range(..=held).rev().find(|(s, (hid, aseq))| *aseq <= cap && mine(**s, *hid)))
                .map_or(0, |(s, _)| *s);
            return r.max(e);
        }
        let r = self
            .refs
            .get(&(y, x))
            .and_then(|r| r.range(..=held).rev().find(|(s, (hid, _))| self.by_author.get(&(x, **s)) == Some(hid)))
            .map_or(0, |(s, _)| *s);
        let e = self
            .echo_refs
            .get(&(y, x))
            .and_then(|r| r.range(..=held).rev().find(|(s, (hid, _))| self.by_author.get(&(x, **s)) == Some(hid)))
            .map_or(0, |(s, _)| *s);
        r.max(e)
    }

    /// Carry-over (rev 22 part 3): `y`'s ack seqs in epoch `e`, [lo, hi]: from its first ack of e (seq 1 in its first
    /// epoch) to the ack before its first of e + 1; for a member the change ending e removes, to its B (hole 2), and
    /// likewise for one removed later that I hold no ack of e + 1 from (it crashed in e: its acks end at its B); open
    /// (u32::MAX) while e is its newest. None before y's first epoch, or while I hold no ack of y declaring e. Acks I
    /// hold are validated in chain order, and an ack's epoch rises by at most one (VC-5), so without an ack of e + 1
    /// every ack I hold from lo on is of e.
    pub(super) fn ack_range(&self, y: MemberId, e: u32) -> Option<(u32, u32)> {
        let first_epoch = self.admitted_epoch.get(&y).copied().unwrap_or(0);
        if e < first_epoch {
            return None;
        }
        let from = self.ack_epoch_from.get(&y);
        let lo = if e == first_epoch { 1 } else { *from?.get(&e)? };
        let b = || self.bound.get(&y).copied().unwrap_or(0);
        let hi = match (self.removed_in.get(&y), from.and_then(|a| a.get(&(e + 1)))) {
            (Some(&r), _) if r == e + 1 => b(),
            (_, Some(&s)) => s.saturating_sub(1),
            (Some(_), None) => b(),
            (None, None) => u32::MAX,
        };
        Some((lo, hi))
    }

    /// Carry-over: the newest seq of `x`'s chain named by `y`'s acks of epoch `e` up to ack seq `upto`, in my
    /// version, at or below my held head of x; y's fed echo from those acks likewise.
    fn ref_in(&self, y: MemberId, x: MemberId, e: u32, upto: u32) -> u32 {
        let Some((lo, hi)) = self.ack_range(y, e) else { return 0 };
        self.refs_between(y, x, lo, hi.min(upto))
    }

    /// Carry-over, finality (Q1): the newest seq of `x`'s chain named by `y`'s acks from its first ack of epoch `e` on,
    /// of that epoch or any later one; a removed member's up to its B.
    fn ref_from(&self, y: MemberId, x: MemberId, e: u32) -> u32 {
        let Some((lo, _)) = self.ack_range(y, e) else { return 0 };
        let hi = if self.removed_in.contains_key(&y) { self.bound.get(&y).copied().unwrap_or(0) } else { u32::MAX };
        self.refs_between(y, x, lo, hi)
    }

    /// The newest seq of `x`'s chain named by `y`'s acks with seq in [lo, hi], in my version, at or below my held head
    /// of x; y's fed echo from those acks likewise.
    fn refs_between(&self, y: MemberId, x: MemberId, lo: u32, hi: u32) -> u32 {
        self.ref_lookups.set(self.ref_lookups.get() + 1);
        if hi < lo {
            return 0;
        }
        let held = self.contiguous.get(&x).copied().unwrap_or(0);
        let mine = |s: u32, hid: EntryId| s <= held && self.by_author.get(&(x, s)) == Some(&hid);
        let r = self.ack_head_hist.get(&(y, x)).and_then(|h| h.range(lo..=hi).rev().find(|(_, v)| mine(v.0, v.1))).map_or(0, |(_, v)| v.0);
        // An echo stands until a newer one (a fed member's ack repeats its echo by saying nothing new), so one first
        // carried by an ack at or before hi is still y's reference in [lo, hi] (lo is an ack of y's own).
        let ech = self
            .echo_refs
            .get(&(y, x))
            .and_then(|r| r.range(..=held).rev().find(|(s, (hid, aseq))| *aseq <= hi && mine(**s, *hid)))
            .map_or(0, |(s, _)| *s);
        r.max(ech)
    }

    /// R3-1: `z`'s references to `y`'s chain in epoch `e` (its acks up to `upto`), capped at the frozen head of z's vote
    /// on y there, in echo-stability as in vouching. An honest voter never references past its freeze, so this changes
    /// nothing for one. A faulty voter's acks after its vote can't raise y's prefix past what its vote, and so y's B,
    /// covers; before its vote they can't either, since a vote dropping them is a provable lie. Gap 2 (b) needs it: a
    /// leave's B comes from q - 1 of the others' votes, which share 2q - n - 1 members with an echo-stable ack's q - 1
    /// referencers, one at n = 4, possibly the faulty one (a kick listing its target's own vote likewise).
    fn capped_ref(&self, z: MemberId, y: MemberId, e: u32, upto: u32) -> u32 {
        let r = self.ref_in(z, y, e, upto);
        match self.vote_on(z, y, e) {
            Some((_, f)) => r.min(f),
            None => r,
        }
    }

    /// Carry-over, one level (direction 1): `y`'s echo-stable prefix in epoch `ctx`: the rank statistic of its
    /// members over their raw references to y's chain in their acks of that epoch (each capped at its vote, R3-1).
    pub(super) fn es_in(&self, y: MemberId, ctx: &Ctx) -> u32 {
        let mut v: Vec<u32> = ctx.members.iter().filter(|&&z| z != y).map(|&z| self.capped_ref(z, y, ctx.epoch, u32::MAX)).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v.get(ctx.rank()).copied().unwrap_or(0)
    }

    /// Carry-over, finality (Q1, ruled m0813b922e026): `y`'s first ack of epoch `next` is fixed once `next`'s members,
    /// with `next`'s quorum, reference y's chain at or past it, read from their acks of `next` or any later epoch (a
    /// removed member's up to its B). One membership keeps rule 1's one-version argument (two quorums of it share an
    /// honest member, which references one version); later epochs' acks still close a short epoch. Read with a later
    /// epoch's membership instead, an equivocator could be fixed on two versions at two replicas.
    fn es_from(&self, y: MemberId, next: &Ctx, first: u32) -> u32 {
        // Ruling 1 as amended (@coordinator-opus m8399b581a416, adopting @design-reviewer m00b2a6407dba): leave out the
        // members that were removed and whose acks up to their B never name y's chain at or past `first` (they never
        // will), with the rest's quorum and rank.
        // - At n >= 4 only members removed on suspicion, which the fault model charges: with d left out, two reduced
        //   quorums of the others share at least t - d members, so an honest one while crashed + faulty <= t. Never a
        //   member removed by its leave or a kick, which t doesn't bound.
        // - At n = 3 a removal of any kind: the one other left fixes the version at every replica, safe against one
        //   liar (S-3's both-others rule is for an equivocator at n = 3). kickliar-fed2: S-4's leaver on the transition
        //   never acked epoch 1, and epoch 0 never closed; q1corner: a member crashed after its first ack of epoch 1.
        let members: Vec<MemberId> = next.members.iter().copied().filter(|&z| z == y || !self.dropped_for(z, y, next, first)).collect();
        let reduced = if members.len() == next.members.len() { next.clone() } else { Ctx { epoch: next.epoch, q: quorum(QuorumRule::Bft, members.len()).1, members } };
        let mut v: Vec<u32> = reduced.members.iter().filter(|&&z| z != y).map(|&z| self.ref_from(z, y, reduced.epoch)).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v.get(reduced.rank()).copied().unwrap_or(0)
    }

    /// Ruling 1 as amended: whether `z`, a member of `next`, is left out of fixing `y`'s first ack of it (seq `first`): a
    /// decided removal (at n >= 4 on suspicion only), its chain held up to its B, and its acks up to B never naming y's
    /// chain at or past `first`.
    fn dropped_for(&self, z: MemberId, y: MemberId, next: &Ctx, first: u32) -> bool {
        let Some(&kind) = self.removed_kind.get(&z) else { return false };
        if next.members.len() != 3 && kind != RemovalKind::Suspicion {
            return false;
        }
        self.silent_removed(z, y, first)
    }

    /// Whether `z`'s removal has decided, I hold its chain up to its B, and its acks up to B never name `y`'s chain at or
    /// past seq `s`: it never will.
    fn silent_removed(&self, z: MemberId, y: MemberId, s: u32) -> bool {
        if !self.removed_in.contains_key(&z) {
            return false;
        }
        let Some(&b) = self.bound.get(&z) else { return false };
        if self.contiguous.get(&z).copied().unwrap_or(0) < b {
            return false;
        }
        self.named_upto(z, y, b) < s
    }

    /// The newest seq of `y`'s chain that `z`'s acks up to seq `b` name (ruling 2: as named, not matched against held
    /// entries); a fed echo's block resolves through my checkpoints, and one past the part of y's chain I hold names past
    /// all of it.
    fn named_upto(&self, z: MemberId, y: MemberId, b: u32) -> u32 {
        let named = self.ack_head_hist.get(&(z, y)).and_then(|h| h.range(..=b).next_back()).map_or(0, |(_, v)| v.0);
        let echo = self.echo_named.get(&(z, y)).and_then(|h| h.range(..=b).next_back()).map_or(0, |(_, &blk)| match self.ckpt_by_block.get(&(y, blk)) {
            Some(&(s, _, _)) => s,
            None if self.ckpt.get(&y).is_some_and(|&(nb, _)| nb >= blk) => 0,
            None => u32::MAX,
        });
        named.max(echo)
    }

    /// Carry-over: `x`'s vouched prefix in epoch `ctx` (vouch_E): the rank statistic of its members over their
    /// references to x's chain in their acks of that epoch up to their own echo-stable prefix there (`es`), each
    /// capped at its vote's frozen head in that epoch (R3-1).
    pub(super) fn vouch_in(&self, x: MemberId, ctx: &Ctx, es: &BTreeMap<MemberId, u32>) -> u32 {
        let mut v: Vec<u32> = ctx.members.iter().filter(|&&y| y != x).map(|&y| self.capped_ref(y, x, ctx.epoch, es.get(&y).copied().unwrap_or(0))).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v.get(ctx.rank()).copied().unwrap_or(0)
    }

    /// Carry-over: whether epoch `ctx`'s acks are all in here (hole 2): each of its members has a held first ack of the
    /// next epoch, echo-stable; or a decided removal (by the change ending the epoch, or a later one) with its chain
    /// held here up to its B. A crashed member that never acks the next epoch holds the epoch open until its removal
    /// decides. The first ack's echo-stability only fixes where the member's acks of the epoch end (rule 1: one version
    /// of its chain up to it), so references from any later epoch it was a member of establish it. Read in the next
    /// epoch alone, a short one (the next change deciding before the others' acks of it reference that first ack)
    /// never closed the epoch (cutfix-shrink, -left, -move).
    pub(super) fn epoch_final(&self, ctx: &Ctx) -> bool {
        let Some(next) = (ctx.epoch < self.epoch).then(|| self.ctx(ctx.epoch + 1)).flatten() else { return false };
        ctx.members.iter().all(|&y| {
            let removed = self.removed_in.get(&y).copied();
            let ended_at_b = removed.is_some_and(|r| r > ctx.epoch) && self.bound.get(&y).is_some_and(|&b| self.contiguous.get(&y).copied().unwrap_or(0) >= b);
            match self.ack_epoch_from.get(&y).and_then(|a| a.get(&(ctx.epoch + 1))) {
                _ if removed == Some(ctx.epoch + 1) => ended_at_b,
                Some(&first) => ended_at_b || self.es_from(y, &next, first) >= first,
                None => ended_at_b,
            }
        })
    }

    /// (spike diagnostics, PE_CUT_TRACE) The members holding epoch `ctx` open, each with why: (removed in, its B, held,
    /// its first ack of the next epoch, the best echo-stable prefix of it in a later epoch).
    fn epoch_blockers(&self, ctx: &Ctx) -> Vec<String> {
        ctx.members
            .iter()
            .filter_map(|&y| {
                let removed = self.removed_in.get(&y).copied();
                let held = self.contiguous.get(&y).copied().unwrap_or(0);
                let b = self.bound.get(&y).copied();
                let ended_at_b = removed.is_some_and(|r| r > ctx.epoch) && b.is_some_and(|b| held >= b);
                let first = self.ack_epoch_from.get(&y).and_then(|a| a.get(&(ctx.epoch + 1))).copied();
                let es = self.ctx(ctx.epoch + 1).map(|c| self.es_from(y, &c, first.unwrap_or(0)));
                let ok = match first {
                    _ if removed == Some(ctx.epoch + 1) => ended_at_b,
                    Some(f) => ended_at_b || es.is_some_and(|v| v >= f),
                    None => ended_at_b,
                };
                (!ok).then(|| format!("m{y} (removed in {removed:?}, B {b:?}, held {held}, first ack of {} {first:?}, referenced to {es:?} with {}'s quorum)", ctx.epoch + 1, ctx.epoch + 1))
            })
            .collect()
    }

    /// Carry-over, the final term (Q2, ruled m0813b922e026): how far each member's acks of epoch `ctx` are read: all of
    /// them, to its B if removed and to its end of the epoch otherwise (`ack_range`), never to a final raw prefix. Every
    /// input a vouch used before (each referencer's acks up to its raw prefix there, at most B by the voter overlap) is among
    /// these, so no counted entry falls past the cut; and at 3 players an entry D54 counted and confirmed (both others'
    /// raw acks name it) stays within it (leave-s4-rejoin: read to the final raw prefix, the cut fell below one, since
    /// the leaver's acks up to its B made the fed member's later acks unstable). (spike: PE_OLD_FINAL=1 reads to the
    /// final raw prefix, as the build before Q2's rulings did.)
    pub(super) fn final_reads(&self, ctx: &Ctx) -> BTreeMap<MemberId, u32> {
        if old_final() {
            ctx.members.iter().map(|&y| (y, self.es_in(y, ctx))).collect()
        } else {
            ctx.members.iter().map(|&y| (y, u32::MAX)).collect()
        }
    }

    /// Carry-over, the final term's reference (ruling 2, m5b983ab01446, @design-reviewer m3a3ca7b15bac): from `y`'s acks
    /// of epoch `e` (to its B, or its end of e), the seq of x's head they name, capped at B_x, without matching it to
    /// entries I hold; a fed echo's checkpoint block past B_x's own block counts as B_x, an earlier one resolves through
    /// my checkpoints (I hold x's chain that far). Every replica holds these acks once e is final, and none needs x's
    /// chain past B_x, so the term is the same everywhere. The rank commutes with min(., B_x), a member's named head only
    /// grows, and every vouch is at most B_x by the voter overlap, so the term is at least every vouch. (Matched against
    /// held entries, a non-voter's reference past B_x read as its older ack at a replica holding less of x.)
    fn final_ref(&self, y: MemberId, x: MemberId, e: u32) -> u32 {
        let Some((lo, hi)) = self.ack_range(y, e) else { return 0 };
        if hi < lo {
            return 0;
        }
        let b = self.bound.get(&x).copied().unwrap_or(u32::MAX);
        let named = self.ack_head_hist.get(&(y, x)).and_then(|h| h.range(lo..=hi).next_back()).map_or(0, |(_, v)| v.0);
        // An echo stands until a newer one: the newest carried at or before hi.
        let echo = self.echo_named.get(&(y, x)).and_then(|h| h.range(..=hi).next_back()).map_or(0, |(_, &blk)| self.echo_block_seq(x, blk, b));
        named.max(echo).min(b)
    }

    /// The seq a fed echo naming checkpoint block `blk` of `x`'s chain stands for in the final term: `b` (B_x) past B_x's
    /// own block, else that block's checkpoint in my version, its first entry stamped there (0 if my version has none:
    /// no reference).
    fn echo_block_seq(&self, x: MemberId, blk: u64, b: u32) -> u32 {
        let b_block = self.by_author.get(&(x, b)).and_then(|id| self.entries.get(id)).map_or(u64::MAX, |e| e.stamp / CKPT_FRAMES);
        if blk > b_block {
            return b;
        }
        self.ckpt_by_block.get(&(x, blk)).map_or(0, |&(s, _, _)| s)
    }

    /// Carry-over, the final term: `x`'s vouch in epoch `ctx` from its members' final references (`final_ref`), each
    /// capped at its own vote's frozen head there (R3-1).
    pub(super) fn vouch_final(&self, x: MemberId, ctx: &Ctx) -> u32 {
        let mut v: Vec<u32> = ctx
            .members
            .iter()
            .filter(|&&y| y != x)
            .map(|&y| {
                let r = self.final_ref(y, x, ctx.epoch);
                self.vote_on(y, x, ctx.epoch).map_or(r, |(_, f)| r.min(f))
            })
            .collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v.get(ctx.rank()).copied().unwrap_or(0)
    }

    /// Carry-over: removed member `x`'s cut, the largest final vouch over its epochs, once every one is final (and
    /// the epoch achieving it, for the record).
    pub(super) fn carry_cut(&mut self, x: MemberId) -> Option<(u32, u32)> {
        let first = self.admitted_epoch.get(&x).copied().unwrap_or(0);
        let last = self.removed_in.get(&x)?.checked_sub(1)?;
        let mut best = (0, first);
        for e in first..=last {
            let v = match self.final_vouch.get(&(x, e)) {
                Some(&v) => v,
                None => {
                    let ctx = self.ctx(e)?;
                    if !ctx.members.contains(&x) {
                        continue;
                    }
                    if !self.epoch_final(&ctx) {
                        return None;
                    }
                    let v = if old_final() {
                        let es = self.final_reads(&ctx);
                        self.vouch_in(x, &ctx, &es)
                    } else {
                        self.vouch_final(x, &ctx)
                    };
                    self.final_vouch.insert((x, e), v);
                    v
                }
            };
            if v > best.0 {
                best = (v, e);
            }
        }
        Some(best)
    }

    /// Rule 1 (TLP-9): the echo-stable prefix of `x`'s chain. Its acks up to this seq have been referenced
    /// by q − 1 members other than `x`, in the version I hold; `x` itself is the q-th, as the author of its
    /// own chain (rule 1's guarantee: the honest members alone make q, "n = 4: 3 of 3"). So they are evidence
    /// every honest replica shares: two versions would need q − 1 of the others each, which overlap in at
    /// least t of them, and an equivocating author leaves at most t − 1 liars among those.
    pub(super) fn echo_stable(&self, x: MemberId) -> u32 {
        let q = self.quorum_q();
        let mut v: Vec<u32> = self.members.iter().filter(|&&y| y != x).map(|&y| self.matched_ref(y, x)).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        // At 3 players (fed mode, S-3): both others, the fed member by its echo.
        let i = if self.members.len() == 3 { 1 } else { q - 2 };
        v.get(i).copied().unwrap_or(0)
    }

    /// Part B: record `x`'s ack `aseq` (stamped `stamp`) naming `head` of `m`'s chain with id `hid`. The
    /// first ack to reach each head is kept (`refs` keeps the latest), so δ reads the first reference.
    pub(super) fn note_first_ref(&mut self, x: MemberId, aseq: u32, stamp: u64, m: MemberId, head: u32, hid: EntryId) {
        let r = self.first_ref.entry((x, m)).or_default();
        if r.keys().next_back().is_none_or(|&h| head > h) {
            r.insert(head, (aseq, stamp, hid));
        }
    }

    /// δ_x(e) (section 3): the stamp of `x`'s first echo-stable ack that references e (author `m`, `seq`,
    /// stamped `stamp`) in my version, minus e's stamp, capped at `cap`: Ok(exact). Err(lower bound) while
    /// unproven, from `x`'s newest echo-stable ack, which doesn't reference it. `es` is x's echo-stable seq.
    fn delta(&self, x: MemberId, m: MemberId, seq: u32, stamp: u64, es: u32, cap: u64) -> Result<u64, u64> {
        if let Some((&h, &(aseq, astamp, hid))) = self.first_ref.get(&(x, m)).and_then(|r| r.range(seq..).next()) {
            if aseq <= es && self.by_author.get(&(m, h)) == Some(&hid) {
                return Ok(astamp.saturating_sub(stamp).min(cap));
            }
        }
        let lb = self.ack_stamps.get(&x).and_then(|a| a.range(..=es).next_back()).map_or(0, |(_, &t)| t.saturating_sub(stamp));
        if lb >= cap { Ok(cap) } else { Err(lb) }
    }

    /// The `rank`-th smallest δ over e's receivers (every member but its author), once decided: the
    /// receivers not yet proven are proven no faster. d(e) is rank q − 1 (the author is the q-th holder), and
    /// so is an ack's echo delay (the author is the q-th reference to its own ack, as in `echo_stable`).
    fn ranked_delay(&self, e: &Entry, rank: usize, es: &BTreeMap<MemberId, u32>, cap: u64) -> Option<u64> {
        let mut exact = Vec::new();
        let mut lbs = Vec::new();
        for &x in self.members.iter().filter(|&&x| x != e.member) {
            match self.delta(x, e.member, e.seq, e.stamp, es.get(&x).copied().unwrap_or(0), cap) {
                Ok(d) => exact.push(d),
                Err(lb) => lbs.push(lb),
            }
        }
        exact.sort_unstable();
        let v = *exact.get(rank.checked_sub(1)?)?;
        lbs.iter().all(|&lb| lb >= v).then_some(v)
    }

    /// The seq of `m`'s first ack stamped after frame `end` that declares a horizon at or past it, if I hold
    /// one. Every entry of m stamped at or before `end` precedes it in m's chain, or is void there by the
    /// chain-local horizon rule.
    fn closing_ack(&self, m: MemberId, end: u64) -> Option<u32> {
        let from = self.ack_by_stamp.get(&m)?.range(..=end).next_back().map_or(0, |(_, &s)| s + 1);
        let hist = self.ack_hist.get(&m)?;
        self.ack_stamps.get(&m)?.range(from..).find(|&(s, &t)| t > end && hist.get(s).is_some_and(|&h| h >= end)).map(|(&s, _)| s)
    }

    /// Window `w`'s settled data, once conf has passed the window, every member's entries stamped in it are
    /// fixed, and each has its d(e). None until then.
    ///
    /// Fixed means: the member's closing ack for the window (`closing_ack`) is in its echo-stable prefix, and
    /// only entries before it count. Conf alone doesn't fix them: rule 3's deadline bound passes frames
    /// whose late entries may still be on their way, void, and a replica that already holds one would
    /// otherwise measure another set than one that doesn't.
    fn window_data(&self, w: u64, cap: u64) -> Option<WindowData> {
        let end = (w + 1) * WINDOW - 1;
        if self.conf <= end {
            return None;
        }
        let q = self.quorum_q();
        let es: BTreeMap<MemberId, u32> = self.members.iter().map(|&x| (x, self.echo_stable(x))).collect();
        let mut data = WindowData::default();
        for &m in &self.members {
            let close = self.closing_ack(m, end)?;
            if es[&m] < close {
                return None;
            }
            let Some(seqs) = self.win_seqs.get(&(w, m)) else { continue };
            let mut last_ack: Option<u64> = None;
            for &seq in seqs.range(..close) {
                let id = self.by_author.get(&(m, seq))?;
                let e = &self.entries[id];
                let d = self.ranked_delay(e, q - 1, &es, cap)?;
                data.d.entry(m).or_default().push(d);
                let mn = data.min_d.entry(m).or_insert(d);
                *mn = (*mn).min(d);
                if let super::Payload::Ack { fed: Some(fa), .. } = &e.payload {
                    data.fed.insert(m);
                    let before = self.prev_echo(m, seq);
                    for &(x, b) in &fa.echo {
                        if before.get(&x).is_some_and(|&b0| b0 >= b) {
                            continue;
                        }
                        let cst = self.ckpt_by_block.get(&(x, b)).and_then(|&(cs, _, _)| self.by_author.get(&(x, cs))).and_then(|cid| self.entries.get(cid)).map(|c| c.stamp);
                        if let Some(cst) = cst {
                            data.e_t.entry(m).or_default().push(e.stamp.saturating_sub(cst));
                            if std::env::var("PE_ET_DEBUG").ok().and_then(|v| v.parse::<MemberId>().ok()) == Some(self.me) {
                                eprintln!("ET m{} window {w}: fed {m} ack seq {seq} stamp {} echoes ({x}, {b}) checkpoint stamp {cst} -> {}", self.me, e.stamp, e.stamp.saturating_sub(cst));
                            }
                        }
                    }
                }
                if matches!(e.payload, super::Payload::Ack { .. }) {
                    // E is measured up to W, not C_max: capped at C_max it would hold C_max where it is.
                    data.echo.entry(m).or_default().push(self.ranked_delay(e, q - 1, &es, self.cfg.window)?);
                    // The gap from this member's previous ack, which may lie in the window before.
                    let prev = last_ack.or_else(|| self.ack_stamps.get(&m).and_then(|a| a.range(..seq).next_back()).map(|(_, &t)| t));
                    if let Some(p) = prev {
                        let g = data.gap.entry(m).or_insert(0);
                        *g = (*g).max(e.stamp.saturating_sub(p));
                    }
                    last_ack = Some(e.stamp);
                }
            }
        }
        Some(data)
    }

    /// Window `k`'s values (section 3), computing every earlier window first. None while window k − 2's
    /// data isn't settled yet; the caller then speculates with the newest final values.
    pub fn window_vals(&mut self, k: u64) -> Option<WindowVals> {
        if let Some(v) = self.windows.get(&k) {
            return Some(v.clone());
        }
        let from = self.windows.keys().next_back().map_or(0, |&j| j + 1);
        for j in from..=k {
            let v = self.compute_window(j)?;
            self.ev(format!("localize: window {j}: Q {} (floor {}), E {}, C_max {}, L {:?}, P {:?}", v.q, v.q_floor, v.e, v.c_max, v.l, v.p));
            if !v.fed_terms.is_empty() {
                self.ev(format!("localize: window {j}: fed (E_T, U_T) {:?}, C_floor {}, over the latency floor {:?}", v.fed_terms, v.c_floor, v.over_floor));
            }
            if v.c_max < self.cfg.floor_f {
                self.ev(format!("localize: window {j}: C_max {} is below the floor F = {}", v.c_max, self.cfg.floor_f));
            }
            self.windows.insert(j, v);
        }
        self.windows.get(&k).cloned()
    }

    /// Window `k`'s values from window k − 2's settled data and window k − 1's values.
    fn compute_window(&mut self, k: u64) -> Option<WindowVals> {
        let w = self.cfg.window;
        // No member has an L yet: a deadline then reads L = 0, and Q_max covers it.
        let lenient = WindowVals {
            l: BTreeMap::new(),
            q: self.cfg.q_max,
            q_floor: self.cfg.q_min,
            e: 0,
            c_max: (w / 2).min(w.saturating_sub(15)),
            p: BTreeMap::new(),
            c_floor: 0,
            fed_terms: BTreeMap::new(),
            over_floor: Vec::new(),
        };
        // Q(0) = Q(1) = Q_max: lenient at the start, with no data yet.
        if k < 2 {
            return Some(lenient);
        }
        let prev = self.windows.get(&(k - 1)).cloned().unwrap_or(lenient.clone());
        // δ is capped at the C_max in force for the entries measured, window k − 2's.
        let cap = self.windows.get(&(k - 2)).map_or(lenient.c_max, |v| v.c_max);
        if !self.window_data_cache.contains_key(&(k - 2)) {
            let d = self.window_data(k - 2, cap)?;
            self.window_data_cache.insert(k - 2, d);
        }
        let data = &self.window_data_cache[&(k - 2)];
        // L_m(k): the minimum d(e) over the last 2 minutes, rising faster when the last 3 windows all sit
        // above it (TLP-16); kept while the member has no entries (TLP-18).
        let mut l = BTreeMap::new();
        for &m in &self.members {
            let mins: Vec<Option<u64>> = (k.saturating_sub(1 + L_WINDOWS)..=k - 2)
                .map(|j| self.window_data_cache.get(&j).and_then(|d| d.min_d.get(&m).copied()))
                .collect();
            let base = mins.iter().flatten().min().copied();
            let last3: Vec<Option<u64>> = mins.iter().rev().take(3).copied().collect();
            let v = match base {
                Some(b) if last3.len() == 3 && last3.iter().all(|x| x.is_some_and(|x| x > b)) => last3.iter().flatten().min().copied(),
                Some(b) => Some(b),
                None => prev.l.get(&m).copied(),
            };
            if let Some(v) = v {
                l.insert(m, v);
            }
        }
        // A member with no L yet takes the median of the others'.
        let mut known: Vec<u64> = l.values().copied().collect();
        let first = lower_median(&mut known).unwrap_or(0);
        for &m in &self.members {
            l.entry(m).or_insert(first);
        }
        // Q(k): from the lower median of the members' P95 queue delays, rising by at most 5 frames and
        // falling by at most 1 per window (TLP-12), within [Q_min', Q_max].
        let mut gaps: Vec<u64> = data.gap.values().copied().collect();
        let q_floor = self.cfg.q_min.max(lower_median(&mut gaps).map_or(0, |g| g + 2));
        let mut per_member: Vec<u64> = data
            .d
            .iter()
            .filter_map(|(m, ds)| {
                let lm = l[m];
                let mut qd: Vec<u64> = ds.iter().map(|&d| d.saturating_sub(lm)).collect();
                p95(&mut qd)
            })
            .collect();
        let q = match lower_median(&mut per_member) {
            Some(med) => {
                let target = med * 3 / 2;
                let step = (target as i64 - prev.q as i64).clamp(-1, 5);
                ((prev.q as i64 + step).max(0) as u64).clamp(q_floor, self.cfg.q_max.max(q_floor))
            }
            // Members without entries are skipped; with none at all, Q stays.
            None => prev.q,
        };
        // E(k) and C_max(k) (rule 3, with nobody on the feed): the cap keeps deadline + echo under W. E is
        // each member's P95 echo delay, then the lower median over the members with acks in window k − 2, as
        // for Q (@coordinator-opus, 2026-11-01 15:21Z). Over all acks at once, one slow member's own acks
        // (1/n of them, over 5% at n ≤ 20) set the P95: a 2.5 s uplink made C_max 0 and voided everyone's
        // actions, and a liar sending its acks late could do the same.
        let mut per_member_e: Vec<u64> = data.echo.values().filter_map(|v| p95(&mut v.clone())).collect();
        let e = lower_median(&mut per_member_e).unwrap_or(prev.e);
        let mut c_max = (w / 2).min(w.saturating_sub(e + 15));
        // Rule 3 while n − f < q (P1 (a); rev 16/17, TLP-28, TLP-33): C_floor(k) = min(W/2, max(F, the largest L_m among
        // members not fed, plus Q_min')); C_max(k) = max(C_floor, min(W/2, W − 15 − max over fed T of (32 + E_T + U_T +
        // 4))), E_T and U_T T's own P95s from window k − 2 (its echo delays; d(e) over its own entries). A fed member
        // whose echo path needs more than W − 15 − C_floor is over the latency floor (rule 4 removes it).
        let fed: Vec<MemberId> = data.fed.iter().copied().filter(|m| self.members.contains(m)).collect();
        let (mut c_floor, mut fed_terms, mut over_floor) = (0, BTreeMap::new(), Vec::new());
        // At n = 3 whenever someone is fed (section 7: F's echo is always necessary there).
        if !fed.is_empty() && (self.members.len() - fed.len() < self.quorum_q() || self.members.len() == 3) {
            let l_max = l.iter().filter(|(m, _)| !fed.contains(m)).map(|(_, &x)| x).max().unwrap_or(0);
            c_floor = (w / 2).min(self.cfg.floor_f.max(l_max + q_floor));
            let mut worst = 0;
            for &t in &fed {
                let e_t = data.e_t.get(&t).and_then(|v| p95(&mut v.clone())).unwrap_or(0);
                let u_t = data.d.get(&t).and_then(|v| p95(&mut v.clone())).unwrap_or(0);
                let need = 32 + e_t + u_t + 4;
                worst = worst.max(need);
                fed_terms.insert(t, (e_t, u_t));
                if need > w.saturating_sub(15 + c_floor) {
                    over_floor.push(t);
                }
            }
            c_max = c_floor.max((w / 2).min(w.saturating_sub(15 + worst)));
        }
        // P_m: a member without acks in window k − 2 keeps its last gap.
        let mut p = prev.p.clone();
        p.extend(data.gap.iter().map(|(&m, &g)| (m, g)));
        Some(WindowVals { l, q, q_floor, e, c_max, p, c_floor, fed_terms, over_floor })
    }

    /// Rule 2's race window G_{m,a}(k) (section 3): both writers' base delays plus the jitter limit, at least
    /// the acceptable delay, plus m's ack gap, one re-fetch and 4 frames; capped at C_max.
    pub fn race_window(v: &WindowVals, cfg: &super::SimConfig, m: MemberId, a: MemberId) -> u64 {
        let lm = v.l.get(&m).copied().unwrap_or(0);
        let la = v.l.get(&a).copied().unwrap_or(0);
        let pm = v.p.get(&m).copied().unwrap_or(0);
        v.c_max.min((lm + la + cfg.j_acc).max(cfg.y_acc) + pm + cfg.refetch_r + 4)
    }

    /// Rule 1's deadline offset (section 3): min(max(L_m + Q, F), C_max) frames past an entry's stamp.
    pub fn deadline_offset(v: &WindowVals, cfg: &super::SimConfig, m: MemberId) -> u64 {
        let lm = v.l.get(&m).copied().unwrap_or(0);
        (lm + v.q).max(cfg.floor_f).min(v.c_max)
    }
}

impl WorldSim {
    /// Window `k`'s values for a decision now: final if they are, else the newest final ones, noting that
    /// window k decided provisionally (`finalize_windows` re-simulates it if the final values differ).
    pub(super) fn vals_for(&mut self, k: u64) -> WindowVals {
        if let Some(v) = self.windows.get(&k) {
            return v.clone();
        }
        // The offline replay computes nothing: it reads the live replica's values (`replay_sim_like`).
        if !self.replaying {
            if let Some(v) = self.window_vals(k) {
                return v;
            }
        }
        // A window's provisional values are fixed at their first use, so every decision in it reads the same.
        if let Some(v) = self.provisional.get(&k) {
            return v.clone();
        }
        let v = self.windows.values().next_back().cloned().unwrap_or_default();
        self.provisional.insert(k, v.clone());
        v
    }

    /// Called before conf rises: compute the next window whose data has settled, and resolve every window
    /// that decided with provisional values and is final now. One whose final values differ is re-simulated
    /// from its first frame, which conf hasn't passed: `recompute_conf` never confirms past the first window
    /// that isn't final, nor past one with unresolved provisional decisions.
    pub(super) fn finalize_windows(&mut self) {
        self.window_vals(self.conf / WINDOW + 1);
        let done: Vec<u64> = self.provisional.keys().copied().filter(|j| self.windows.contains_key(j)).collect();
        for j in done {
            let used = self.provisional.remove(&j).unwrap();
            if self.windows.get(&j) != Some(&used) {
                self.ev(format!("localize: window {j} decided with provisional values; re-simulating from frame {}", j * WINDOW));
                self.mark_dirty(j * WINDOW);
            }
        }
    }

    /// Rule 2: e's carried state for `b` is stale if another member a wrote `b` more than G_{m,a} frames
    /// before e, in a write e's author hadn't seen: its latest ack before e (fixed by e's prev chain) named
    /// a head of a's chain below that write. Anchors (rule 5) are not built yet.
    pub(super) fn stale_write(&mut self, e: &Entry, b: u32) -> bool {
        let Some(writers) = self.t.writes.get(&b) else { return false };
        let mut unseen = Vec::new();
        for (&a, ws) in writers {
            if a == e.member {
                continue;
            }
            let head = self.ack_head_hist.get(&(e.member, a)).and_then(|h| h.range(..e.seq).next_back()).map_or(0, |(_, &(h, _))| h);
            // a's first write to b that e's author hadn't seen; later ones are unseen too.
            if let Some((_, &stamp)) = ws.range(head + 1..).next() {
                unseen.push((a, stamp));
            }
        }
        if unseen.is_empty() {
            return false;
        }
        let v = self.vals_for(e.stamp / WINDOW);
        unseen.iter().any(|&(a, stamp)| stamp + Self::race_window(&v, &self.cfg, e.member, a) < e.stamp)
    }

    /// Rule 2: record a counting write to `b`, keeping 2 speculation windows of them per author.
    pub(super) fn note_write(&mut self, b: u32, m: MemberId, seq: u32, stamp: u64) {
        let keep = stamp.saturating_sub(2 * self.cfg.window);
        let ws = self.t.writes.entry(b).or_default().entry(m).or_default();
        ws.insert(seq, stamp);
        ws.retain(|_, &mut t| t >= keep);
    }
}

impl WorldSim {
    /// Rule 1: member `x`'s status for entry (`m`, `seq`) at `deadline`: Some(true) holder, Some(false)
    /// lacker, None while unproven. It comes from x's latest echo-stable ack stamped at or before the
    /// deadline, proven latest by an echo-stable ack stamped after it; x holds the entry if that ack's head of
    /// m's chain reaches `seq` in the version I hold. `newest` is the stamp of x's newest echo-stable ack.
    fn status(&self, x: MemberId, newest: Option<u64>, own_cap: u32, m: MemberId, seq: u32, deadline: u64, epoch: u32) -> Option<bool> {
        if newest? <= deadline {
            return None;
        }
        // Carry-over: `own_cap` is u32::MAX for a member; a removed member's acks count up to its echo-stable prefix in
        // its last epoch, which lies within its B by the overlap (`removed_caps`).
        let Some((_, &k)) = self.ack_by_stamp.get(&x).and_then(|a| a.range(..=deadline).rev().find(|(_, s)| **s <= own_cap)) else {
            return Some(false);
        };
        // F2: toward a removed member's chain, only x's acks from its last epoch count.
        let k = k.min(self.ref_cap(x, m));
        // Revision 21 (a): once x has voted against m, earlier in its chain than this ack, its vote's frozen head of
        // m's chain is its last word for holder status too (R3-1 covered stability only). Ordered by x's chain, so
        // every replica reads it alike. The vote of the entry's own epoch (F2).
        if let Some((vseq, frozen)) = self.vote_on(x, m, epoch) {
            if vseq < k && seq > frozen {
                return Some(false);
            }
        }
        let head = self.ack_head_hist.get(&(x, m)).and_then(|h| h.range(..=k).next_back()).map(|(_, &hh)| hh);
        match head {
            // A head of m's chain I don't hold yet: whether it is my version is unproven, so wait (fetching it).
            // Taken for a lacker, it voided entries at a replica behind on m's chain that counted elsewhere
            // (partbrand seed 11: a member back from the feed, and a distant one).
            Some((h, _)) if h >= seq && !self.by_author.contains_key(&(m, h)) => None,
            _ => Some(head.is_some_and(|(h, hid)| h >= seq && self.by_author.get(&(m, h)) == Some(&hid))),
        }
    }

    /// (measurement) Ack heads kept for rule 1 (`ack_head_hist`): the spike keeps every one; production keeps the newest
    /// per (referencer, author, epoch) while an entry of that epoch is pending.
    pub fn ack_heads_kept(&self) -> usize {
        self.ack_head_hist.values().map(|h| h.len()).sum()
    }

    /// Diagnostics: how much of `m`'s chain I hold contiguously, and the newest seq of it I hold at all.
    pub fn held_of(&self, m: MemberId) -> (u32, u32) {
        (self.contiguous.get(&m).copied().unwrap_or(0), self.by_author.range((m, 0)..=(m, u32::MAX)).next_back().map_or(0, |(&(_, s), _)| s))
    }

    /// Diagnostics: the earliest pending entry, its deadline and every other member's status for it.
    pub fn debug_pending(&self) -> String {
        let Some(&(stamp, id)) = self.pending.iter().next() else { return "no pending entry".to_string() };
        let Some(e) = self.entries.get(&id) else { return format!("pending {stamp} not held") };
        let Some(v) = self.windows.get(&(e.stamp / WINDOW)) else { return format!("m{} s{} stamp {}: window {} not computed", e.member, e.seq, e.stamp, e.stamp / WINDOW) };
        let deadline = e.stamp + Self::deadline_offset(v, &self.cfg, e.member);
        let newest: BTreeMap<MemberId, u64> =
            self.members.iter().filter_map(|&x| { let es = self.echo_stable(x); self.ack_stamps.get(&x).and_then(|a| a.range(..=es).next_back()).map(|(_, &t)| (x, t)) }).collect();
        let st: Vec<String> = self
            .members
            .iter()
            .filter(|&&x| x != e.member)
            .map(|&x| format!("m{x}:{:?}(newest {:?}, es {}, held {:?})", self.status(x, newest.get(&x).copied(), u32::MAX, e.member, e.seq, deadline, self.epoch), newest.get(&x), self.echo_stable(x), self.contiguous.get(&x)))
            .collect();
        format!("m{} s{} stamp {} deadline {}: {}", e.member, e.seq, e.stamp, deadline, st.join(" "))
    }

    /// Rule 1: what e's statuses decide: Some(false) void at q proven lackers, Some(true) at n − q + 1 proven holders
    /// (the author holds its own), which counts once its author's vouched prefix covers it (carry-over, checked by the
    /// caller). None while undecided, or while the deadline's window isn't final. `caps`: removed members' ack caps.
    fn verdict_of(&self, e: &Entry, ctx: &Ctx, newest: &BTreeMap<MemberId, u64>, caps: &BTreeMap<MemberId, u32>) -> Option<bool> {
        let v = self.windows.get(&(e.stamp / WINDOW))?;
        let deadline = e.stamp + Self::deadline_offset(v, &self.cfg, e.member);
        // F2: with the members and quorum of the entry's own epoch.
        let q = ctx.q;
        let n = ctx.members.len();
        let (mut holders, mut lackers) = (1, 0);
        for &x in ctx.members.iter().filter(|&&x| x != e.member) {
            match self.status(x, newest.get(&x).copied(), caps.get(&x).copied().unwrap_or(u32::MAX), e.member, e.seq, deadline, ctx.epoch) {
                Some(true) => holders += 1,
                Some(false) => lackers += 1,
                None => {}
            }
        }
        if lackers >= q {
            Some(false)
        } else if holders + q > n {
            Some(true)
        } else {
            None
        }
    }

    /// Carry-over: each removed member's ack cap, its echo-stable prefix in its last epoch, and the stamp of its newest
    /// ack within it: u64::MAX once that epoch's acks are all in here (its last echo-stable ack is then its last).
    fn removed_caps(&self) -> (BTreeMap<MemberId, u32>, BTreeMap<MemberId, u64>) {
        let (mut caps, mut newest) = (BTreeMap::new(), BTreeMap::new());
        for &x in self.evicted.keys().filter(|x| !self.members.contains(x)) {
            let Some(ctx) = self.removed_in.get(&x).and_then(|&r| self.ctx(r - 1)) else { continue };
            let cap = self.es_in(x, &ctx);
            caps.insert(x, cap);
            if self.epoch_final(&ctx) {
                newest.insert(x, u64::MAX);
            } else if let Some((_, &t)) = self.ack_stamps.get(&x).and_then(|a| a.range(..=cap).next_back()) {
                newest.insert(x, t);
            }
        }
        (caps, newest)
    }

    /// Carry-over (rev 22 part 3; ruled by @coordinator-opus mbdc2c5ae906c, the shape @design-reviewer's): apply each
    /// Part B removal's cut once final. Removed member x's cut is the largest final vouch over its epochs
    /// (`carry_cut`): every vouch that counted one of x's entries anywhere is one epoch's statistic over a subset of
    /// that epoch's acks, so at most that epoch's final value, so at most the cut. No counted entry is voided; entries
    /// past it are void. It replaces revision 21 (a)'s cut with the last epoch's members, the left-member rule
    /// (m6426247221b1) and the interim guard: each epoch's acks end in the log (first acks of the next epoch, or B),
    /// so no term depends on a later change or another member's cut.
    pub(super) fn finalize_cuts(&mut self) {
        let xs: Vec<MemberId> = self.pb_pending.iter().copied().collect();
        for x in xs {
            // `adopt_cut` recomputes confirmation, which comes back here: one finalized meanwhile is done.
            if !self.pb_pending.contains(&x) {
                continue;
            }
            let Some((cut, at)) = self.carry_cut(x) else {
                if cut_trace() {
                    let first = self.admitted_epoch.get(&x).copied().unwrap_or(0);
                    let last = self.removed_in.get(&x).map_or(0, |&r| r - 1);
                    let open: Vec<u32> = (first..=last).filter(|&e| !self.final_vouch.contains_key(&(x, e)) && self.ctx(e).is_some_and(|c| c.members.contains(&x) && !self.epoch_final(&c))).collect();
                    let why: Vec<String> = open.iter().filter_map(|&e| self.ctx(e).map(|c| format!("{e}: {}", self.epoch_blockers(&c).join(", ")))).collect();
                    eprintln!("CUTWAIT m{} now {} conf {} epoch {}: m{x}'s epochs {first}..={last}, not final yet {open:?}; held open by [{}]", self.me, self.now, self.conf, self.epoch, why.join("; "));
                }
                continue;
            };
            // (spike diagnostics: the record, and PE_CUT_TRACE=1 prints each member's evidence behind a cut)
            {
                let (per, without_left) = match self.ctx(at) {
                    Some(c) => {
                        let es = self.final_reads(&c);
                        let per: Vec<(MemberId, Option<u32>, u32, bool)> = c
                            .members
                            .iter()
                            .filter(|&&y| y != x)
                            .map(|&y| {
                                let r = if old_final() { self.ref_in(y, x, at, es[&y]) } else { self.final_ref(y, x, at) };
                                let r = self.vote_on(y, x, at).map_or(r, |(_, f)| r.min(f));
                                (y, Some(es[&y]), r, self.members.contains(&y))
                            })
                            .collect();
                        let mut v: Vec<u32> = per.iter().map(|p| if p.3 { p.2 } else { 0 }).collect();
                        v.sort_unstable_by(|a, b| b.cmp(a));
                        let w = v.get(c.rank()).copied().unwrap_or(0);
                        (per, w)
                    }
                    None => (Vec::new(), 0),
                };
                let last_epoch = self.removed_in.get(&x).and_then(|&r| self.final_vouch.get(&(x, r - 1))).copied().unwrap_or(0);
                if cut_trace() {
                    eprintln!(
                        "CUT m{} now {} conf {}: m{x} removed in epoch {:?}: cut {cut}, the vouch of epoch {at}; (member, es, evidence, member now) {per:?}; without members that left {without_left}; its last epoch alone {last_epoch}",
                        self.me, self.now, self.conf, self.removed_in.get(&x)
                    );
                }
                self.cut_records.insert(x, CutRecord { at_frame: self.now, cut, epoch: at, voters: per, without_left, last_epoch });
            }
            let cid = if cut == 0 { 0 } else { self.by_author.get(&(x, cut)).copied().unwrap_or(0) };
            self.pb_pending.remove(&x);
            self.frozen.insert(x, cut);
            self.cut_ids.insert(x, cid);
            self.evicted.insert(x, (cut, 0));
            self.ev(format!("carry-over: member {x}'s removal is final; its chain is cut at seq {cut}, the vouch of epoch {at}"));
            self.adopt_cut(x);
        }
    }

    /// (spike diagnostics) A removed member's entries this replica counted (verdict true) that lie past its final cut,
    /// which coverage says can't exist: (seq, stamp) each. Empty until the cut is final and held here.
    pub fn counted_past_cut(&self, x: MemberId) -> Vec<(u32, u64)> {
        let Some(&(cut, _)) = self.evicted.get(&x).filter(|_| self.cut_done.contains(&x)) else { return Vec::new() };
        self.entries.iter().filter(|(id, e)| e.member == x && e.seq > cut && self.verdicts.get(id) == Some(&true)).map(|(_, e)| (e.seq, e.stamp)).collect()
    }

    /// (spike diagnostics) The newest seq of `x`'s chain counted here (rule 1's verdict true), and the first seq of it
    /// stamped at or after `frame`.
    pub fn counted_upto(&self, x: MemberId, frame: u64) -> (u32, Option<u32>) {
        let counted = self.entries.iter().filter(|(id, e)| e.member == x && self.verdicts.get(id) == Some(&true)).map(|(_, e)| e.seq).max().unwrap_or(0);
        let from = self.entries.values().filter(|e| e.member == x && e.stamp >= frame).map(|e| e.seq).min();
        (counted, from)
    }

    /// (spike diagnostics) Whether epoch `e` is final here: None before it ends, else the members holding it open (empty
    /// once final), each with why (`epoch_blockers`).
    pub fn epoch_open(&self, e: u32) -> Option<Vec<String>> {
        let ctx = self.ctx(e)?;
        if e >= self.epoch {
            return None;
        }
        Some(if self.epoch_final(&ctx) { Vec::new() } else { self.epoch_blockers(&ctx) })
    }

    /// (spike diagnostics) A removed member's final cut here, once held (`cut_done`).
    pub fn final_cut(&self, x: MemberId) -> Option<u32> {
        self.evicted.get(&x).filter(|_| self.cut_done.contains(&x)).map(|&(c, _)| c)
    }

    /// Rule 1: decide every pending entry whose statuses are known. A void entry leaves the simulation at its
    /// frame, which conf hasn't passed: rule 3 never confirms past an entry without a verdict.
    pub(super) fn decide_verdicts(&mut self) {
        self.stats.verdict_passes += 1;
        // Each member's newest echo-stable ack stamp. A status is known only for a deadline before it, and
        // a deadline lies past its entry's stamp, so entries stamped past all of them can't be decided yet.
        let newest: BTreeMap<MemberId, u64> = self
            .members
            .iter()
            .filter_map(|&x| {
                let es = self.echo_stable(x);
                self.ack_stamps.get(&x)?.range(..=es).next_back().map(|(_, &t)| (x, t))
            })
            .collect();
        let horizon = newest.values().copied().max().unwrap_or(0);
        // Carry-over: a removed member's acks count up to its echo-stable prefix in its last epoch, known for good once
        // that epoch's acks are all in here.
        let mut newest = newest;
        let (caps, rnewest) = self.removed_caps();
        newest.extend(rnewest);
        // Carry-over: per epoch, each member's echo-stable prefix there (one level: the others' raw references, each
        // capped at its own vote), and each author's vouch there from them. Q2's second level was dropped
        // (@coordinator-opus mfeac029a05b0): with the final term reading every member's acks to its B or its end of the
        // epoch, one level's inputs are always among the final term's (es_raw(y) <= B_y by the voter overlap), and the
        // second level paused twofed-long's full members 54 frames after the shrink.
        let mut es_at: BTreeMap<u32, BTreeMap<MemberId, u32>> = BTreeMap::new();
        let mut vouch_at: BTreeMap<(MemberId, u32), u32> = BTreeMap::new();
        let mut ctxs: BTreeMap<u32, Option<Ctx>> = BTreeMap::new();
        let pend: Vec<(u64, EntryId)> = self.pending.iter().copied().collect();
        for (stamp, id) in pend {
            let Some(e) = self.entries.get(&id).cloned() else {
                self.pending.remove(&(stamp, id));
                continue;
            };
            // A removed member's entry past its final cut is void for good (`reconcile` disputed it), held behind a hole
            // in its chain or not. Its chain is frozen at the cut, so the hole never fills, and waiting below for the
            // entry's epoch held conf at the boundary for good (cutfix-same: member 2's entries reached 5, 6 and 7 late,
            // past its cut; their conf stopped at 404 while 1's and 4's went on).
            if self.cut_done.contains(&e.member) && self.evicted.get(&e.member).is_some_and(|&(cut, _)| e.seq > cut) {
                self.pending.remove(&(stamp, id));
                self.verdicts.insert(id, false);
                continue;
            }
            // F2: judged with the members and quorum of the entry's own epoch, by its author's chain. One of an
            // epoch I haven't reached waits (its members aren't known to me yet).
            let ep = self.entry_epoch(e.member, e.seq);
            if ep > self.epoch {
                continue;
            }
            // Held but not validated yet (behind a hole in its author's chain): its epoch is the validated prefix's
            // unless an ack in between, held but unvalidated, names a later one; then it waits.
            if e.seq > self.contiguous.get(&e.member).copied().unwrap_or(0) {
                let from = self.contiguous.get(&e.member).copied().unwrap_or(0) + 1;
                let later = self
                    .by_author
                    .range((e.member, from)..(e.member, e.seq))
                    .any(|(_, hid)| matches!(self.entries.get(hid).map(|x| &x.payload), Some(Payload::Ack { epoch, .. }) if *epoch > ep));
                if later {
                    continue;
                }
            }
            if !ctxs.contains_key(&ep) {
                let c = self.ctx(ep);
                ctxs.insert(ep, c);
            }
            let Some(ctx) = ctxs[&ep].clone() else { continue };
            // A removed member's entry: judged as any (carry-over: vouched in its epochs up to its last). A non-member
            // that was never one isn't validated.
            if !self.members.contains(&e.member) && !self.evicted.contains_key(&e.member) {
                self.pending.remove(&(stamp, id));
                continue;
            }
            // One its author made in an epoch it isn't a member of (by its chain: after the change that removed it) lies
            // past its B, so past its cut: it never counts, and waits for the cut, which voids it (above). The D54
            // shortcuts below counted such entries (premise1: member 4's s179 in the 3-member epoch after its kick).
            if !ctx.members.contains(&e.member) {
                continue;
            }
            // An epoch of 2 members (or Part B off) keeps D54: rule 1 voids nothing there.
            if !self.cfg.localize || ctx.members.len() < 3 {
                self.pending.remove(&(stamp, id));
                self.verdicts.insert(id, !self.is_void(id));
                continue;
            }
            // At 3 players: an entry in a D54 window counts (D54 voids nothing; it waits for stability), one in a
            // fed window is judged by rule 1, and one whose window's mode isn't known yet waits; the mode of the
            // entry's own epoch, whatever mine is now (the window-mode note, m5b983ab01446).
            if ctx.members.len() == 3 && !(window_now() && ep != self.epoch) {
                match self.window_mode_in(stamp / WINDOW, &ctx.members) {
                    None => continue,
                    Some(false) => {
                        if cut_trace() && verdict_author() == Some(e.member) {
                            eprintln!("VERDICT m{} now {} conf {} epoch {}: m{} s{} stamp {} counts in a D54 window ({})", self.me, self.now, self.conf, self.epoch, e.member, e.seq, stamp, stamp / WINDOW);
                        }
                        self.pending.remove(&(stamp, id));
                        self.verdicts.insert(id, !self.is_void(id));
                        continue;
                    }
                    Some(true) => {}
                }
            }
            // Already void by a chain-local rule or a proof: nothing to decide.
            let decided = if self.is_void(id) {
                Some(false)
            } else if stamp >= horizon {
                None
            } else {
                match self.verdict_of(&e, &ctx, &newest, &caps) {
                    // Carry-over: it counts once vouch_E covers it for some epoch E from its own to mine (its author's
                    // last, if removed), its author a member of E. Otherwise it waits: a later epoch's acks can vouch it.
                    Some(true) => {
                        let last = self.removed_in.get(&e.member).map_or(self.epoch, |&r| r - 1).min(self.epoch);
                        let mut by = None;
                        for ee in ep..=last {
                            let v = match self.final_vouch.get(&(e.member, ee)).or_else(|| vouch_at.get(&(e.member, ee))) {
                                Some(&v) => Some(v),
                                None => {
                                    let c = if ee == ep { Some(ctx.clone()) } else { self.ctx(ee) };
                                    match c {
                                        Some(c) if c.members.contains(&e.member) => {
                                            if !es_at.contains_key(&ee) {
                                                let esr: BTreeMap<MemberId, u32> = c.members.iter().map(|&y| (y, self.es_in(y, &c))).collect();
                                                es_at.insert(ee, esr);
                                            }
                                            let v = self.vouch_in(e.member, &c, &es_at[&ee]);
                                            vouch_at.insert((e.member, ee), v);
                                            Some(v)
                                        }
                                        _ => None,
                                    }
                                }
                            };
                            if v.is_some_and(|v| v >= e.seq) {
                                by = Some(ee);
                                break;
                            }
                        }
                        if cut_trace() && by.is_some() && verdict_author() == Some(e.member) {
                            let ee = by.unwrap();
                            let evid: Vec<(MemberId, u32)> = es_at.get(&ee).map(|m| m.iter().map(|(&y, &v)| (y, self.ref_in(y, e.member, ee, v))).collect()).unwrap_or_default();
                            eprintln!(
                                "VERDICT m{} now {} conf {} epoch {}: m{} s{} stamp {} (epoch {}) counts, vouched in epoch {ee} by {:?}; es there {:?}; evidence {evid:?}",
                                self.me, self.now, self.conf, self.epoch, e.member, e.seq, stamp, ctx.epoch, vouch_at.get(&(e.member, ee)).or(self.final_vouch.get(&(e.member, ee))), es_at.get(&ee)
                            );
                        } else if cut_trace() && by.is_some() && (self.evicted.contains_key(&e.member) || ctx.epoch < self.epoch || by != Some(ep)) {
                            eprintln!(
                                "VERDICT m{} now {} conf {} epoch {}: m{} s{} stamp {} (epoch {}) counts, vouched in epoch {:?}",
                                self.me, self.now, self.conf, self.epoch, e.member, e.seq, stamp, ctx.epoch, by
                            );
                        }
                        by.map(|_| true)
                    }
                    x => x,
                }
            };
            let Some(counts) = decided else { continue };
            self.pending.remove(&(stamp, id));
            self.verdicts.insert(id, counts);
            if !counts && !self.is_void(id) {
                self.ev(format!("rule 1: m{} s{} stamped {} void: q members lacked it at its deadline", e.member, e.seq, stamp));
                self.late_void.insert(id);
                self.set_eff(id);
            }
        }
    }

    /// Rule 3: the last frame f for which anything `j` could still author stamped at or before f would be past
    /// its deadline at q members: f + offset(f) < the q-th newest echo-stable ack stamp among the others. None
    /// without q such acks or final window values.
    ///
    /// A member counts only if its acks reference no more of j's chain than I hold. Otherwise it may hold
    /// entries of j that I don't, which could count; I fetch them and wait.
    pub(super) fn deadline_bound(&self, j: MemberId) -> Option<u64> {
        let ctx = self.ctx(self.epoch)?;
        self.deadline_bound_in(j, &ctx)
    }

    /// F2: rule 3's bound for entries of `j` judged with `ctx`'s members and quorum. A removed member counts once its
    /// cut is final (its last ack within the cut is its latest, so it lacks anything later), and not before.
    pub(super) fn deadline_bound_in(&self, j: MemberId, ctx: &Ctx) -> Option<u64> {
        let q = ctx.q;
        let held = self.contiguous.get(&j).copied().unwrap_or(0);
        let mut t: Vec<u64> = ctx
            .members
            .iter()
            .filter(|&&x| x != j)
            .filter_map(|&x| {
                let (es, last) = if self.members.contains(&x) {
                    (self.echo_stable(x), None)
                } else {
                    // Carry-over: a removed member counts once its last epoch's acks are all in here: its acks up to its
                    // echo-stable prefix there, the last of them its latest.
                    match self.removed_in.get(&x).and_then(|&r| self.ctx(r - 1)) {
                        Some(c) if self.epoch_final(&c) => (self.es_in(x, &c), Some(u64::MAX)),
                        _ => return None,
                    }
                };
                let cap = es.min(self.ref_cap(x, j));
                let head = self.ack_head_hist.get(&(x, j)).and_then(|h| h.range(..=cap).next_back()).map(|(_, &hh)| hh);
                // Skipped, not counted, if I can't place its head of j's chain: beyond my held prefix, or (revision
                // 21, RV20-2) another version at a position I hold. Counted as a lacker, it let a version I don't
                // hold be vouched with a stamp below a frame this bound confirmed.
                if let Some((h, hid)) = head {
                    if h > held || (h > 0 && self.by_author.get(&(j, h)) != Some(&hid)) {
                        return None;
                    }
                }
                if last.is_some() {
                    return last;
                }
                self.ack_stamps.get(&x).and_then(|a| a.range(..=es).next_back()).map(|(_, &st)| st)
            })
            .collect();
        t.sort_unstable_by(|a, b| b.cmp(a));
        let tq = *t.get(q - 1)?;
        // The first frame s whose deadline, s + the offset of s's own window, isn't before tq; the bound is the
        // frame before it. Every frame of a window without final values counts as such a frame. An entry
        // stamped late in an earlier window can have a longer offset than tq's window and be the first, but
        // only within C_max ≤ W/2 (the speculation window's half) of tq.
        let last = *self.windows.keys().next_back()?;
        let mut s0 = (last + 1) * WINDOW;
        for k in tq.saturating_sub(self.cfg.window / 2) / WINDOW..=(tq / WINDOW).min(last) {
            let Some(v) = self.windows.get(&k) else {
                s0 = s0.min(k * WINDOW);
                break;
            };
            let first = (k * WINDOW).max(tq.saturating_sub(Self::deadline_offset(v, &self.cfg, j)));
            if first < (k + 1) * WINDOW {
                s0 = s0.min(first);
            }
        }
        s0.checked_sub(1)
    }
}

#[cfg(test)]
mod holdings {
    //! Ruling 2's holdings test (@coordinator-opus m0de2cc2c987b, m9d93b4a24a48): the final term must not depend on how
    //! much of the removed member's chain a replica holds. The cell form (partb `holdings`) never got the kick decided
    //! at the replica holding less, so the two replicas are built here from the state the final term reads.
    use super::super::{SceneKind, SimConfig};
    use super::*;

    const X: MemberId = 4;
    const B: u32 = 20;
    const FED: MemberId = 5;

    /// An entry of `m`'s chain at `seq`, stamped 10 frames per seq (x's seq 20 lies in checkpoint block 6, seq 25 in 7).
    fn entry(m: MemberId, seq: u32) -> Entry {
        Entry { member: m, seq, prev: 0, stamp: 10 * seq as u64, payload: Payload::Touch { obj: m, impulse: [0.0; 3], grab: false }, carried: None, link: 0 }
    }

    fn hold(w: &mut WorldSim, m: MemberId, upto: u32) {
        for s in 1..=upto {
            let e = entry(m, s);
            let id = e.id();
            w.by_author.insert((m, s), id);
            w.entries.insert(id, e);
        }
        w.contiguous.insert(m, upto);
    }

    fn ckpts(w: &mut WorldSim, m: MemberId, upto: u32) {
        let mut newest = (0, 0);
        for s in 1..=upto {
            let e = entry(m, s);
            let blk = e.stamp / CKPT_FRAMES;
            if !w.ckpt_by_block.contains_key(&(m, blk)) {
                w.ckpt_by_block.insert((m, blk), (s, e.id(), e.rest_hash()));
                newest = (blk, e.id());
            }
        }
        w.ckpt.insert(m, newest);
    }

    /// Member 1's replica of a 5-member session after epoch 0's change removed x = 4 by a kick: x's chain held to
    /// `held` (B or past it), and every input the final term and epoch 0's finality read.
    /// - Voters 1 and 2 froze x's chain at 17 and 20 (B = 20, the larger); their acks of epoch 0 name it that far.
    /// - Member 3, a non-voter, has an ack naming x's seq 15 and then one naming seq 22, past B.
    /// - Member 5, fed, has an echo naming x's checkpoint block 4 and then one naming block 7, past B's block (6). A
    ///   replica holding x only to B can't resolve block 7: its matched echo stays at block 4.
    /// - Each survivor's acks of epoch 0 end at seq 19; its first ack of epoch 1 is seq 20, and every other survivor's
    ///   ack 21 names its seq 21 (epoch 0 is final).
    fn replica(held: u32) -> WorldSim {
        let all = [1, 2, 3, 4, 5];
        let survivors = [1, 2, 3, FED];
        let mut w = WorldSim::new(1, 1, &all, SceneKind::Empty, SimConfig { localize: true, ..SimConfig::default() });
        w.epoch = 1;
        w.members = survivors.iter().copied().collect();
        w.removed_in.insert(X, 1);
        w.removed_kind.insert(X, RemovalKind::Kick);
        w.bound.insert(X, B);
        hold(&mut w, X, held);
        ckpts(&mut w, X, held);
        let xid = |s: u32| entry(X, s).id();
        for (y, named) in [(1, vec![(10, 17)]), (2, vec![(10, 20)]), (3, vec![(9, 15), (10, 22)])] {
            let h = w.ack_head_hist.entry((y, X)).or_default();
            for (aseq, s) in named {
                h.insert(aseq, (s, xid(s)));
            }
        }
        for (voter, frozen) in [(1, 17), (2, 20)] {
            w.vote_hist.entry((X, voter)).or_default().push((0, 11, frozen));
        }
        w.echo_named.insert((FED, X), [(9, 4), (10, 7)].into_iter().collect());
        // The echoes as matched at receipt (the old final term's reading): block 7 only where its checkpoint is held.
        let er = w.echo_refs.entry((FED, X)).or_default();
        for (aseq, blk) in [(9u32, 4u64), (10, 7)] {
            if let Some(&(s, id, _)) = w.ckpt_by_block.get(&(X, blk)) {
                er.insert(s, (id, aseq));
            }
        }
        for &y in &survivors {
            hold(&mut w, y, 21);
            w.ack_epoch_from.entry(y).or_default().insert(1, 20);
            for &z in survivors.iter().filter(|&&z| z != y) {
                let yid = entry(y, 21).id();
                w.ack_head_hist.entry((z, y)).or_default().insert(21, (21, yid));
            }
        }
        w
    }

    #[test]
    fn the_final_term_is_the_same_at_every_holding() {
        let (mut a, mut b) = (replica(B), replica(B + 5));
        let ctx = a.ctx(0).expect("epoch 0's membership");
        assert_eq!(ctx.members, vec![1, 2, 3, 4, 5]);
        assert!(a.epoch_final(&ctx) && b.epoch_final(&ctx), "epoch 0 is final at both");
        // The scenario bites the matched reading: member 3's ack past B and member 5's echo past B's block resolve only
        // at the replica holding more of x's chain.
        assert_eq!((a.refs_between(3, X, 1, 19), b.refs_between(3, X, 1, 19)), (15, 22));
        assert_ne!(a.refs_between(FED, X, 1, 19), b.refs_between(FED, X, 1, 19));
        // The ruled final term: named heads and echo blocks capped at B, unmatched.
        for y in [1, 2, 3, FED] {
            assert_eq!(a.final_ref(y, X, 0), b.final_ref(y, X, 0), "member {y}'s final reference");
        }
        assert_eq!((a.final_ref(1, X, 0), a.final_ref(2, X, 0), a.final_ref(3, X, 0), a.final_ref(FED, X, 0)), (17, 20, 20, 20));
        assert_eq!(a.echo_block_seq(X, 7, B), B, "a block past B's block counts as B");
        assert_eq!(a.vouch_final(X, &ctx), b.vouch_final(X, &ctx));
        assert_eq!(a.carry_cut(X), Some((B, 0)));
        assert_eq!(b.carry_cut(X), Some((B, 0)), "identical cuts");
        // Matched against held entries over the same acks (the reading before ruling 2), the cuts would differ: the
        // third highest of (17, 20, 15, 13) against (17, 20, 22, 23).
        let all_acks: BTreeMap<MemberId, u32> = ctx.members.iter().map(|&y| (y, u32::MAX)).collect();
        assert_eq!((a.vouch_in(X, &ctx, &all_acks), b.vouch_in(X, &ctx, &all_acks)), (15, 20));
    }
}

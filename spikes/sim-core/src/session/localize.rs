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

use std::collections::BTreeMap;

use super::{quorum, Entry, EntryId, MemberId, Payload, QuorumRule, WorldSim};

/// Rule 1's scope: perturbations and hand entries. Acks are the evidence; view-change, admin and R14
/// entries keep their own rules.
pub(super) fn judged(p: &Payload) -> bool {
    matches!(p, Payload::Touch { .. } | Payload::Hand { .. })
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

    fn quorum_q(&self) -> usize {
        quorum(QuorumRule::Bft, self.members.len()).1
    }

    /// The newest seq of `x`'s chain that `y`'s acks have referenced in the version I hold, at or below my
    /// own held head (as `stable_seq` matches references, review VC5R-2). 0 if none.
    /// A fed member's echo (rule 1, TLP-15) counts here as its references: `echo_refs`.
    fn matched_ref(&self, y: MemberId, x: MemberId) -> u32 {
        let held = self.contiguous.get(&x).copied().unwrap_or(0);
        let r = self
            .refs
            .get(&(y, x))
            .and_then(|r| r.range(..=held).rev().find(|(s, (hid, _))| self.by_author.get(&(x, **s)) == Some(hid)))
            .map_or(0, |(s, _)| *s);
        let e = self
            .echo_refs
            .get(&(y, x))
            .and_then(|r| r.range(..=held).rev().find(|(s, hid)| self.by_author.get(&(x, **s)) == Some(*hid)))
            .map_or(0, |(s, _)| *s);
        r.max(e)
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
        v.get(q - 2).copied().unwrap_or(0)
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
        let c_max = (w / 2).min(w.saturating_sub(e + 15));
        // P_m: a member without acks in window k − 2 keeps its last gap.
        let mut p = prev.p.clone();
        p.extend(data.gap.iter().map(|(&m, &g)| (m, g)));
        Some(WindowVals { l, q, q_floor, e, c_max, p })
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
    fn status(&self, x: MemberId, newest: Option<u64>, m: MemberId, seq: u32, deadline: u64) -> Option<bool> {
        if newest? <= deadline {
            return None;
        }
        let Some((_, &k)) = self.ack_by_stamp.get(&x).and_then(|a| a.range(..=deadline).next_back()) else {
            return Some(false);
        };
        let head = self.ack_head_hist.get(&(x, m)).and_then(|h| h.range(..=k).next_back()).map(|(_, &hh)| hh);
        match head {
            // A head of m's chain I don't hold yet: whether it is my version is unproven, so wait (fetching it).
            // Taken for a lacker, it voided entries at a replica behind on m's chain that counted elsewhere
            // (partbrand seed 11: a member back from the feed, and a distant one).
            Some((h, _)) if h >= seq && !self.by_author.contains_key(&(m, h)) => None,
            _ => Some(head.is_some_and(|(h, hid)| h >= seq && self.by_author.get(&(m, h)) == Some(&hid))),
        }
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
            .map(|&x| format!("m{x}:{:?}(newest {:?}, es {}, held {:?})", self.status(x, newest.get(&x).copied(), e.member, e.seq, deadline), newest.get(&x), self.echo_stable(x), self.contiguous.get(&x)))
            .collect();
        format!("m{} s{} stamp {} deadline {}: {}", e.member, e.seq, e.stamp, deadline, st.join(" "))
    }

    /// Rule 1: e's verdict once its statuses decide it: Some(false) void at q proven lackers, Some(true)
    /// counting at n − q + 1 proven holders (the author holds its own). None while undecided, or while the
    /// deadline's window isn't final.
    fn verdict_of(&self, e: &Entry, newest: &BTreeMap<MemberId, u64>) -> Option<bool> {
        let v = self.windows.get(&(e.stamp / WINDOW))?;
        let deadline = e.stamp + Self::deadline_offset(v, &self.cfg, e.member);
        let q = self.quorum_q();
        let n = self.members.len();
        let (mut holders, mut lackers) = (1, 0);
        for &x in self.members.iter().filter(|&&x| x != e.member) {
            match self.status(x, newest.get(&x).copied(), e.member, e.seq, deadline) {
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

    /// Rule 1: decide every pending entry whose statuses are known. A void entry leaves the simulation at its
    /// frame, which conf hasn't passed: rule 3 never confirms past an entry without a verdict.
    pub(super) fn decide_verdicts(&mut self) {
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
        let pend: Vec<(u64, EntryId)> = self.pending.iter().copied().collect();
        for (stamp, id) in pend {
            let Some(e) = self.entries.get(&id).cloned() else {
                self.pending.remove(&(stamp, id));
                continue;
            };
            // From a member removed since: its cut decides instead (P2-4: its entries up to the cut count, the
            // rest are disputed). Spike simplification: rule 1 doesn't judge across a membership change.
            if !self.members.contains(&e.member) {
                self.pending.remove(&(stamp, id));
                continue;
            }
            // Already void by a chain-local rule or a proof: nothing to decide.
            let decided = if self.is_void(id) {
                Some(false)
            } else if stamp >= horizon {
                None
            } else {
                self.verdict_of(&e, &newest)
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
        let q = self.quorum_q();
        let held = self.contiguous.get(&j).copied().unwrap_or(0);
        let mut t: Vec<u64> = self
            .members
            .iter()
            .filter(|&&x| x != j)
            .filter_map(|&x| {
                let es = self.echo_stable(x);
                let head = self.ack_head_hist.get(&(x, j)).and_then(|h| h.range(..=es).next_back()).map_or(0, |(_, &(h, _))| h);
                if head > held {
                    return None;
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

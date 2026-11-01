//! Admin indicators, the session side (design 10.8, "Admin indicators"): whom confirmation is
//! waiting on. Read-only: nothing here changes the replica, queues a fetch or sends anything (review
//! IND-6), and nothing acts on the result. `SessionCore::indicators` adds the threshold, the admin
//! gate and the late-acks hint.

use super::*;

/// Why a member's term holds the confirmed frame (design 10.8, "Holding the session", in the order
/// the attribution tries them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hold {
    /// The replica is adopting a decision (`adopting()` freezes confirmation).
    Adopting,
    /// The term is capped by a join, an ack from a newer epoch, or a removed member's prefix not yet
    /// held. Names no member: it ends with the view change.
    Membership { of: MemberId },
    /// `voter` voted against `of`, so its later references to `of` never count (R3-1); `voter` is
    /// this replica when its own vote froze its copy. Waits for the view change (review IND-1).
    Voted { voter: MemberId, of: MemberId },
    /// `of` has no applied ack and isn't admitted (review IND-5).
    NoAckYet { of: MemberId },
    /// This replica holds an entry of `of` beyond a gap: its own copy is what waits (review IND-4).
    ReplicaGap { of: MemberId },
    /// `of`'s newest applied ack declares no horizon at or past the confirmed frame; `frames` is the
    /// present minus that horizon.
    Behind { of: MemberId, frames: u64 },
    /// `by`'s newest reference to `of`'s chain is below `of`'s seq `r`, the lowest whose ack declares
    /// a horizon at or past the confirmed frame, though `by`'s own acks show it has had time to hold
    /// it (review IND-2's guard). `frames` is the present minus r's stamp.
    NotReferencing { by: MemberId, of: MemberId, frames: u64 },
    /// More than t members are each `NotReferencing` `of` (or, at three players, the one other
    /// member is): the common cause is `of`'s outbound link.
    NotHeard { of: MemberId, frames: u64 },
}

impl WorldSim {
    /// [`WorldSim::stable_seq`] without its side effects: no fetch requests and no `stable_hi`
    /// update (review IND-6). Same value: `stable_hi` only ever holds a value `stable_seq` returned.
    pub(super) fn stable_seq_ro(&self, m: MemberId) -> u32 {
        let held = self.contiguous.get(&m).copied().unwrap_or(0);
        let mine = match self.membership().frozen(m) {
            Some(f) if !self.membership().removed().contains_key(&m) => held.min(f),
            _ => held,
        };
        let mut s = mine;
        let matching = self.fixes_off & fix::VC4R3 == 0;
        for &j in self.membership().members() {
            if j == self.me || j == m {
                continue;
            }
            let Some(r) = self.refs.get(&(j, m)) else {
                s = 0;
                break;
            };
            let found = if matching {
                r.range(..=mine).rev().find(|(rs, (rid, _))| self.by_author.get(&(m, **rs)) == Some(rid)).map(|(rs, _)| *rs)
            } else {
                r.range(..=mine).next_back().and_then(|(rs, (rid, _))| (self.by_author.get(&(m, *rs)) == Some(rid)).then_some(*rs))
            };
            match found {
                Some(rs) => s = s.min(rs),
                None => {
                    s = 0;
                    break;
                }
            }
        }
        self.stable_hi.get(&m).copied().unwrap_or(0).max(s)
    }

    /// Member `j`'s term of the confirmed frame, as `recompute_conf` computes it, with whether a
    /// membership cap (join cap or newer-epoch ack) bounds it.
    fn conf_term(&self, j: MemberId) -> Option<(u64, bool)> {
        let mut s = self.stable_seq_ro(j);
        let mut capped = false;
        if let Some(first) = self.membership().first_newer_ack(j) {
            if first.saturating_sub(1) < s {
                capped = true;
            }
            s = s.min(first.saturating_sub(1));
        }
        let mut h = self.ack_hist.get(&j).and_then(|a| a.range(..=s).next_back()).map(|(_, h)| *h);
        if let (Some(hh), Some((at, b))) = (h, self.membership().join_cap(j)) {
            if s > at && b.saturating_sub(1) < hh {
                capped = true;
                h = Some(hh.min(b.saturating_sub(1)));
            }
        }
        match (h, self.membership().admitted(j)) {
            (Some(h), Some(b)) => Some(((h + 1).max(b), capped)),
            (None, Some(b)) => Some((b, capped)),
            (Some(h), None) => Some((h + 1, capped)),
            (None, None) => None,
        }
    }

    /// Whom the confirmed frame is waiting on, at present frame `present` (design 10.8). Empty when
    /// nothing holds it beyond the terms' minimum (the caller applies the lag threshold).
    pub fn holding(&self, present: u64) -> Vec<Hold> {
        let ms = self.membership();
        if ms.adopting() {
            return vec![Hold::Adopting];
        }
        let members: Vec<MemberId> = ms.members().iter().copied().collect();
        let mut out = Vec::new();
        let terms: Vec<(MemberId, Option<(u64, bool)>)> = members.iter().map(|&j| (j, self.conf_term(j))).collect();
        // A member with no ack that isn't admitted: recompute_conf returns with no bound at all.
        for (j, t) in &terms {
            if t.is_none() {
                out.push(Hold::NoAckYet { of: *j });
            }
        }
        if !out.is_empty() {
            return out;
        }
        let mut c = terms.iter().filter_map(|(_, t)| t.map(|x| x.0)).min().unwrap_or(u64::MAX);
        // A removed member's prefix not yet held bounds it too.
        let mut removed_bound = Vec::new();
        for (&x, &(cut, _)) in ms.removed() {
            let held = self.contiguous.get(&x).copied().unwrap_or(0);
            let right = ms.cut_id(x).is_none_or(|cid| cut == 0 || self.by_author.get(&(x, cut)) == Some(&cid));
            if held < cut || !right {
                let upto = if self.fixes_off & fix::VC2 == 0 { self.stable_hi.get(&x).copied().unwrap_or(0).min(cut) } else { held.min(cut) };
                let h = self.ack_hist.get(&x).and_then(|a| a.range(..=upto).next_back()).map_or(0, |(_, h)| h + 1);
                removed_bound.push((x, h));
                c = c.min(h);
            }
        }
        for (x, h) in removed_bound {
            if h == c {
                out.push(Hold::Membership { of: x });
            }
        }
        let n_others = members.len().saturating_sub(2);
        let t = (members.len().saturating_sub(1)) / 3;
        for (j, term) in terms {
            let Some((v, capped)) = term else { continue };
            if v != c {
                continue;
            }
            // 1. Membership change: a cap, or a vote freezing references to j.
            if capped {
                out.push(Hold::Membership { of: j });
                continue;
            }
            let mut voted = false;
            if ms.frozen(j).is_some() && !ms.removed().contains_key(&j) {
                out.push(Hold::Voted { voter: self.me, of: j });
                voted = true;
            }
            for &k in &members {
                if k != self.me && k != j && ms.voted_against(k, j) {
                    out.push(Hold::Voted { voter: k, of: j });
                    voted = true;
                }
            }
            if voted {
                continue;
            }
            // 3. This replica's own gap in j's chain (2, no ack yet, returned above).
            let held = self.contiguous.get(&j).copied().unwrap_or(0);
            if self.by_author.range((j, held + 1)..=(j, u32::MAX)).next().is_some() {
                out.push(Hold::ReplicaGap { of: j });
                continue;
            }
            // 4. Behind: j's newest applied ack declares no horizon at or past C.
            let hist = self.ack_hist.get(&j);
            let newest = hist.and_then(|a| a.iter().next_back()).map(|(_, h)| *h);
            if newest.is_none_or(|h| h + 1 <= c) {
                out.push(Hold::Behind { of: j, frames: present.saturating_sub(newest.unwrap_or(0)) });
                continue;
            }
            // 5. Not referencing: r is the lowest seq of j whose ack declares a horizon >= C.
            let Some(r) = hist.and_then(|a| a.iter().find(|(_, h)| **h >= c).map(|(s, _)| *s)) else { continue };
            let r_stamp = self.ack_stamps.get(&j).and_then(|a| a.get(&r)).copied().unwrap_or(0);
            let frames = present.saturating_sub(r_stamp);
            let guard = (1.0 / self.cfg.dt as f64).round() as u64; // 1 s, the show threshold
            let mut lagging = Vec::new();
            for &k in &members {
                if k == self.me || k == j {
                    continue;
                }
                let refd = self
                    .refs
                    .get(&(k, j))
                    .and_then(|m| m.range(..=held).rev().find(|(rs, (rid, _))| self.by_author.get(&(j, **rs)) == Some(rid)).map(|(rs, _)| *rs))
                    .unwrap_or(0);
                if refd >= r {
                    continue;
                }
                let k_newest = self.ack_stamps.get(&k).and_then(|a| a.iter().next_back()).map(|(_, st)| *st);
                match k_newest {
                    Some(st) if st >= r_stamp + guard => lagging.push(k),
                    // IND-7: recent but early: nothing yet. Old: k is behind, or a gap here.
                    Some(st) if present.saturating_sub(st) < guard => {}
                    _ => {
                        let kh = self.contiguous.get(&k).copied().unwrap_or(0);
                        if self.by_author.range((k, kh + 1)..=(k, u32::MAX)).next().is_some() {
                            out.push(Hold::ReplicaGap { of: k });
                        } else {
                            out.push(Hold::Behind { of: k, frames: present.saturating_sub(k_newest.unwrap_or(0)) });
                        }
                    }
                }
            }
            // 6. Not heard: more than t of the others (at three players, the one other: both shown).
            if n_others == 1 && lagging.len() == 1 {
                out.push(Hold::NotHeard { of: j, frames });
                out.push(Hold::NotReferencing { by: lagging[0], of: j, frames });
            } else if n_others > 1 && lagging.len() > t {
                out.push(Hold::NotHeard { of: j, frames });
            } else {
                out.extend(lagging.into_iter().map(|k| Hold::NotReferencing { by: k, of: j, frames }));
            }
        }
        out.dedup();
        out
    }

    /// Whether `x` is a current member (only members' acks are judged for late acks).
    pub fn is_member(&self, x: MemberId) -> bool {
        self.membership().members().contains(&x) && !self.membership().removed().contains_key(&x)
    }

    /// Whether `x` voted against this replica (its references to this chain are frozen, so the
    /// late-acks hint leaves it out, review LA-4).
    pub fn voted_against_me(&self, x: MemberId) -> bool {
        self.membership().voted_against(x, self.me)
    }

    /// `x`'s admission boundary, if it joined (a joiner catching up is left out of the late-acks
    /// hint until its acks declare a horizon at or past it, review LA-4).
    pub fn admitted_at(&self, x: MemberId) -> Option<u64> {
        self.membership().admitted(x)
    }

    /// The id of this replica's own entry `seq`, if it holds it.
    pub fn own_id(&self, seq: u32) -> Option<EntryId> {
        self.by_author.get(&(self.me, seq)).copied()
    }
}

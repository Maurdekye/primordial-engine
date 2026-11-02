//! Thin links, Part B, rule 5 (thin-link plan revision 19): the state feed, the fed member's side.
//!
//! A member whose downlink can't carry the full entry stream is fed: the others stop sending it entries, and
//! one feeder sends it present-frame body states instead. It writes them into its own replica only (never
//! the log), re-simulates to its present, and keeps acting: its own entries still go out. It holds no log
//! while fed, so it is a lacker for every entry (its acks' heads stay where they were), it can't confirm,
//! and it judges no one. When its link recovers it fetches the log again and becomes a full member.
//!
//! The feed never enters the shared log. The harness (the spike's net model) decides who is fed.

use std::collections::BTreeMap;

use super::{echo_digest, entry_id, BodyState, Entry, EntryId, FedAck, MemberId, Msg, WorldSim};

/// Item 7's bandwidth floor as rule 4 (D) reads it, total over a member's peers: about 0.625 kbit/s per member (2.5 at
/// n = 4, 5 at n = 8). A voter's share is this over n − 1. (Revision 22 restates the floor itself, pending the user's
/// confirmation; (D) only has to catch a dead uplink, so its threshold stays at the stated figure.)
pub const FLOOR_BPS_PER_MEMBER: u64 = 625;

/// One body in a feed packet, quantized: position in 0.1 mm, orientation as a unit quaternion scaled to
/// i16, linear velocity in cm/s. 30 bytes on the wire with its object id. Angular velocity isn't sent: the
/// next packet corrects the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedBody {
    pub obj: u32,
    pub pos: [i32; 3],
    pub rot: [i16; 4],
    pub lin: [i16; 3],
}

impl FeedBody {
    pub fn from_state(obj: u32, s: &BodyState) -> Self {
        let p = s.pos_f64();
        let q = s.rot();
        let v = s.lin();
        let qi = |x: f32| (x.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        let vi = |x: f32| (x * 100.0).round().clamp(-32767.0, 32767.0) as i16;
        FeedBody {
            obj,
            pos: [(p[0] * 1e4).round() as i32, (p[1] * 1e4).round() as i32, (p[2] * 1e4).round() as i32],
            rot: [qi(q.v.x), qi(q.v.y), qi(q.v.z), qi(q.s)],
            lin: [vi(v.x), vi(v.y), vi(v.z)],
        }
    }

    pub fn to_state(&self) -> BodyState {
        let p = [self.pos[0] as f64 / 1e4, self.pos[1] as f64 / 1e4, self.pos[2] as f64 / 1e4];
        let mut q = [self.rot[0] as f32, self.rot[1] as f32, self.rot[2] as f32, self.rot[3] as f32];
        let n = q.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n > 0.0 {
            q.iter_mut().for_each(|x| *x /= n);
        } else {
            q = [0.0, 0.0, 0.0, 1.0];
        }
        let lin = [self.lin[0] as f32 / 100.0, self.lin[1] as f32 / 100.0, self.lin[2] as f32 / 100.0];
        BodyState::new(p, q, lin, [0.0; 3])
    }
}

/// The fed member's state (never logged, never hashed into the world).
#[derive(Clone, Debug, Default)]
pub struct Fed {
    /// Feed packets by the frame they describe: the bodies' states after that frame.
    pub anchors: BTreeMap<u64, Vec<FeedBody>>,
    /// The frame the member went on the feed at.
    pub since: u64,
    /// The digest of the echo my last ack carried (0: none yet on this stay).
    pub echoed: u64,
    /// Revision 21 (RV20-3): when each member's checkpoints last reached me (my present), and my downlink delay
    /// measured on the feed (frames from a packet's frame to my present when it arrived).
    pub ckpt_heard: BTreeMap<MemberId, u64>,
    pub delay_frames: u64,
}

impl WorldSim {
    /// Whether this replica is on the feed.
    pub fn is_fed(&self) -> bool {
        self.fed.is_some()
    }

    /// Go on the feed or come off it (the harness decides). Coming off drops the anchors and re-simulates
    /// from the first of them, so the replica follows the log again as the fetched entries arrive. The caller
    /// fetches the log (`recovery_fetches`).
    pub fn set_fed(&mut self, on: bool) {
        match (on, self.fed.is_some()) {
            (true, false) => {
                self.ev(format!("feed: member {} goes on the state feed at frame {}", self.me, self.frame));
                let heard: BTreeMap<MemberId, u64> = self.members.iter().map(|&m| (m, self.now)).collect();
                self.fed = Some(Fed { anchors: BTreeMap::new(), since: self.frame, echoed: 0, ckpt_heard: heard, delay_frames: 0 });
                // TLP-24: each chain's echo starts at the newest checkpoint my own last references cover (I held
                // that log), unless I echo a newer one already (a re-entry).
                let others: Vec<MemberId> = self.members.iter().copied().filter(|&x| x != self.me).collect();
                for x in others {
                    if let Some(c) = self.covered_ckpt(self.me, x) {
                        if self.echo_last.get(&x).is_none_or(|&(b0, _)| c.0 > b0) {
                            self.echo_last.insert(x, c);
                            if let Some(&(s, _, _)) = self.ckpt_by_block.get(&(x, c.0)) {
                                self.echo_seq.insert(x, s);
                            }
                        }
                    }
                }
            }
            (false, true) => {
                let fed = self.fed.take().unwrap();
                self.ev(format!("feed: member {} comes off the state feed at frame {} (on it since {})", self.me, self.frame, fed.since));
                // It heard no entries while fed, so it has no evidence of anyone's silence or lag: the clocks
                // restart (a stale `heard` voted out live members on leaving).
                let now = self.now;
                let ms: Vec<MemberId> = self.members.iter().copied().collect();
                for m in ms {
                    self.heard.insert(m, now);
                }
                self.suspect_since.clear();
                self.lagging.clear();
                // Re-simulate from the frame it went on the feed (its state before that was the log's): anchors are
                // pruned to the last two windows, so the first kept one left older anchors' writes in the state.
                let f = fed.since.max(self.conf);
                if f < self.frame && !fed.anchors.is_empty() {
                    self.mark_dirty(f);
                }
            }
            _ => {}
        }
    }

    /// A feed packet: the feeder's states of `bodies` after `frame`. Written at that frame on the next
    /// re-simulation, if it lies within what this replica simulated (older ones are dropped).
    pub fn on_feed(&mut self, frame: u64, bodies: Vec<FeedBody>) {
        let Some(since) = self.fed.as_ref().map(|f| f.since) else { return };
        // My downlink delay, from this packet (revision 21: F's silence threshold allows for it).
        let now = self.now;
        if let Some(f) = self.fed.as_mut() {
            f.delay_frames = now.saturating_sub(frame);
        }
        if frame < since || frame < self.conf {
            return;
        }
        // Keep about two speculation windows of packets.
        let keep = self.frame.saturating_sub(2 * self.cfg.window);
        let fed = self.fed.as_mut().unwrap();
        fed.anchors.entry(frame).or_default().extend(bodies);
        fed.anchors.retain(|&f, _| f >= keep);
        if frame < self.frame {
            self.mark_dirty(frame);
        }
    }

    /// The feeder's side: the states, after the last simulated frame, of the bodies to send, most relevant
    /// first. Hands, then moving bodies, then the rest from `rr` on (round robin), within `budget` bytes of
    /// message (datagram overhead excluded).
    pub fn feed_packet(&self, budget: usize, rr: &mut usize) -> Option<Msg> {
        let frame = self.frame.checked_sub(1)?;
        // 13 bytes of message header (tag, frame, count), 30 per body.
        let n = (budget.saturating_sub(13) / 30).max(1);
        let live: Vec<(u32, BodyState)> =
            self.t.objs.iter().enumerate().filter_map(|(i, o)| o.map(|o| (i as u32, BodyState::read(&self.world, o.body)))).collect();
        if live.is_empty() {
            return None;
        }
        let hands: Vec<u32> = self.hand_owners.keys().copied().collect();
        let mut picked: Vec<FeedBody> = Vec::new();
        for (obj, s) in &live {
            if picked.len() < n && hands.contains(obj) {
                picked.push(FeedBody::from_state(*obj, s));
            }
        }
        for (obj, s) in &live {
            let v = s.lin();
            if picked.len() < n && !hands.contains(obj) && (v.x * v.x + v.y * v.y + v.z * v.z) > 1e-6 {
                picked.push(FeedBody::from_state(*obj, s));
            }
        }
        let k = live.len();
        for i in 0..k {
            if picked.len() >= n {
                break;
            }
            let (obj, s) = &live[(*rr + i) % k];
            if !picked.iter().any(|b| b.obj == *obj) {
                picked.push(FeedBody::from_state(*obj, s));
            }
        }
        *rr = (*rr + n) % k;
        Some(Msg::Feed { frame, bodies: picked })
    }

    /// Coming off the feed: fetch every chain from what this replica holds, from `holder`.
    pub fn recovery_fetches(&self) -> Vec<Msg> {
        self.members
            .iter()
            .filter(|&&m| m != self.me)
            .map(|&m| Msg::Fetch { author: m, from: self.contiguous.get(&m).copied().unwrap_or(0) + 1, to: u32::MAX })
            .collect()
    }

    /// Rule 4's exception (TLP-25, TLP-29): if `j` is fed (its newest ack carries an anchor) and its echo of my chain
    /// has trailed my oldest checkpoint it hasn't echoed for at least X = max(W − C_max, suspect_after) frames: that
    /// lag, and X. Judged from j's own acks as I received them; a short dropout stays under X.
    pub(super) fn floor_breach(&self, j: MemberId) -> Option<String> {
        let &echoed = self.fed_peers.get(&j)?;
        let after = echoed.or_else(|| self.covered_ckpt(j, self.me).map(|(b, _)| b));
        let lo = after.map_or(0, |b| b + 1);
        let (&(_, b), _) = self.ckpt_by_block.range((self.me, lo)..=(self.me, u64::MAX)).next()?;
        // From when I first sent it that checkpoint (an echo can't be timely about one it was never sent), to j's newest
        // ack I hold: the evidence is j's own acks, X frames after I sent it, still without the echo. A replica behind
        // on j's chain (back from the feed, catching up) sees only older acks, and doesn't judge.
        let sent = *self.ckpt_first_sent.get(&(j, b))?;
        let c_max = self.windows.values().next_back().map_or(self.cfg.window / 2, |v| v.c_max);
        let x = self.cfg.window.saturating_sub(c_max).max(self.cfg.suspect_after);
        if let Some(acked) = self.ack_stamps.get(&j).and_then(|a| a.values().next_back().copied()) {
            let lag = acked.saturating_sub(sent);
            if lag >= x {
                return Some(format!("its echo of my chain has trailed my checkpoints for {lag} frames (X = {x})"));
            }
        }
        self.uplink_breach(j, x)
    }

    /// Revision 22: what a fed member checks of a change, from what it holds (view-change entries by value, its clock,
    /// the decided changes). Every removal needs its own evidence: its own vote (silence) or a proof it holds. A join or
    /// any admin entry gets its nil (it can't check join rules without the log; a kick needs the kicker's chain
    /// segment, not built), so removals are proposed on their own. The boundary is checked against its own decided
    /// boundaries only (condition 3); the change's cut isn't checked (under revision 21 a Part B removal is cut at the
    /// final vouched prefix, whatever the value says).
    pub(super) fn fed_change_valid(&self, c: &super::Change) -> Option<bool> {
        let last = self.decided.last().map_or(0, |d| d.0.boundary);
        if c.epoch != self.epoch || c.boundary < last || !c.add.is_empty() || !c.admin.is_empty() {
            return Some(false);
        }
        for r in &c.remove {
            let mine = self.votes.get(&r.member).is_some_and(|v| v.contains_key(&self.me));
            let proof = self.proofs.contains_key(&r.member) || self.liars.contains_key(&r.member);
            let leave = self.leaves.get(&r.member).is_some_and(|id| r.votes == [*id]);
            if !mine && !proof && !leave {
                return Some(false);
            }
        }
        for (x, _) in &c.excluded {
            if !self.proofs.contains_key(x) {
                return Some(false);
            }
        }
        Some(true)
    }

    /// Rule 4's floor test, the uplink half (D) (revision 22): the bytes of fed member j's own datagrams that reached
    /// me directly over the last X frames, against my share of the floor, floor / (n − 1) (D-1). Only a fed j is
    /// judged (D-2; `floor_breach` is reached only for one); a fed voter doesn't judge (its downlink carries only echo
    /// inputs by design), and neither does a voter that hasn't watched j fed for a whole window yet.
    fn uplink_breach(&self, j: MemberId, x: u64) -> Option<String> {
        if self.fed.is_some() {
            return None;
        }
        let since = *self.fed_seen.get(&j)?;
        if self.now < since + x {
            return None;
        }
        let got: u64 = self.rx_bytes.get(&j).map_or(0, |b| b.range(self.now.saturating_sub(x)..).map(|(_, v)| *v).sum());
        let n = self.members.len().max(2) as u64;
        let share_bps = FLOOR_BPS_PER_MEMBER * n / (n - 1);
        let need = (share_bps as f64 * x as f64 * self.cfg.dt as f64 / 8.0) as u64;
        (got < need).then(|| format!("only {got} B of its own datagrams reached me in {x} frames, under my share of the floor ({need} B)"))
    }

    /// Rule 4 (D): bytes of one datagram that reached me directly from `from` (its own, not relayed), counted by my
    /// present's frame. The transport reports them (the spike's Net, on delivery).
    pub fn note_rx(&mut self, from: MemberId, bytes: u64) {
        let now = self.now;
        let keep = now.saturating_sub(4 * self.cfg.window);
        let b = self.rx_bytes.entry(from).or_default();
        *b.entry(now).or_insert(0) += bytes;
        while b.keys().next().is_some_and(|&f| f < keep) {
            b.pop_first();
        }
    }

    /// TLP-24: the newest checkpoint of `x`'s chain that `y`'s newest ack I hold references (its block and id).
    fn covered_ckpt(&self, y: MemberId, x: MemberId) -> Option<(u64, EntryId)> {
        let head = self.ack_head_hist.get(&(y, x)).and_then(|h| h.values().next_back()).map_or(0, |&(s, _)| s);
        self.ckpt_by_block.range((x, 0)..=(x, u64::MAX)).rev().find(|(_, v)| v.0 <= head).map(|(k, v)| (k.1, v.1))
    }

    /// What a fed replica's ack adds (rule 5, rule 1's echo): its newest anchor frame, and its echo of every
    /// other member's chain if that changed since my previous ack. None when not fed.
    pub(super) fn fed_ack(&mut self) -> Option<FedAck> {
        let echoed: Vec<(MemberId, u64, EntryId)> =
            self.echo_last.iter().filter(|(x, _)| **x != self.me && self.members.contains(x)).map(|(&x, &(b, id))| (x, b, id)).collect();
        let digest = echo_digest(echoed.iter().copied());
        let f = self.fed.as_mut()?;
        let anchor = f.anchors.keys().next_back().copied().unwrap_or(f.since);
        if echoed.is_empty() || f.echoed == digest {
            return Some(FedAck { anchor, echo: Vec::new(), digest: 0 });
        }
        f.echoed = digest;
        Some(FedAck { anchor, echo: echoed.iter().map(|&(x, b, _)| (x, b)).collect(), digest })
    }

    /// TLP-23, the fed side: `member`'s checkpoints, from `from` (only the chain's author is followed). The
    /// chain is computed forward from the checkpoint I echo: each id from the previous one and the rest-hash.
    /// A list that doesn't link up with mine (it starts past it) is ignored; the author resends from my echo.
    pub fn on_ckpts(&mut self, from: MemberId, member: MemberId, after: Option<u64>, list: Vec<(u64, u32, u64)>) {
        if self.fed.is_none() || from != member || member == self.me {
            return;
        }
        let now = self.now;
        if let Some(f) = self.fed.as_mut() {
            f.ckpt_heard.insert(member, now);
        }
        let mut cur = self.echo_last.get(&member).copied();
        let mut it = list.into_iter().peekable();
        if after != cur.map(|(b, _)| b) {
            // The list starts before my checkpoint: it must name it, and I go on after it.
            let Some((mine, _)) = cur else { return };
            let mut found = false;
            while let Some((b, _, _)) = it.next() {
                if b == mine {
                    found = true;
                    break;
                }
                if b > mine {
                    break;
                }
            }
            if !found {
                return;
            }
        }
        let mut seq = None;
        for (b, s, rest) in it {
            if cur.is_some_and(|(b0, _)| b <= b0) {
                break;
            }
            cur = Some((b, entry_id(cur.map_or(0, |(_, id)| id), rest)));
            seq = Some(s);
        }
        if let Some(c) = cur {
            self.echo_last.insert(member, c);
        }
        if let Some(s) = seq {
            self.echo_seq.insert(member, s);
        }
    }

    /// TLP-23, the members' side: for each member my log shows fed, my checkpoints since its echo of my chain
    /// (or, before its first echo, since the newest one its own references cover, TLP-24). Sent when there is
    /// one it hasn't been sent, and again every 64 frames while its echo trails. (to, message).
    pub fn ckpt_msgs(&mut self) -> Vec<(MemberId, Msg)> {
        const RESEND_FRAMES: u64 = 2 * super::CKPT_FRAMES;
        let mut out = Vec::new();
        // A fed member sends its own checkpoints too (it holds its own chain).
        if !self.members.contains(&self.me) {
            return out;
        }
        let fs: Vec<(MemberId, Option<u64>)> = self.fed_peers.iter().map(|(&f, &b)| (f, b)).collect();
        for (f, echoed) in fs {
            if !self.members.contains(&f) {
                continue;
            }
            let after = echoed.or_else(|| self.covered_ckpt(f, self.me).map(|(b, _)| b));
            let lo = after.map_or(0, |b| b + 1);
            let list: Vec<(u64, u32, u64)> = self.ckpt_by_block.range((self.me, lo)..=(self.me, u64::MAX)).map(|(&(_, b), &(s, _, r))| (b, s, r)).collect();
            let Some(&(newest, _, _)) = list.last() else { continue };
            let due = self.ckpt_sent.get(&f).is_none_or(|&(sb, at)| newest > sb || self.frame >= at + RESEND_FRAMES);
            if due {
                self.ckpt_sent.insert(f, (newest, self.frame));
                for &(b, _, _) in &list {
                    self.ckpt_first_sent.entry((f, b)).or_insert(self.frame);
                }
                out.push((f, Msg::Ckpts { member: self.me, after, list }));
            }
        }
        out
    }

    /// Rule 1's echo (TLP-15), the receivers' side: check each fed member's newest echo against my version
    /// once I hold every checkpoint it names. A match credits the echoed checkpoints as that member's
    /// references, for echo-stability only. A mismatch (my version has another checkpoint there, or none) is
    /// counted; production asks for the full signed heads (7.6).
    pub(super) fn retry_echoes(&mut self) {
        let fs: Vec<MemberId> = self.echo_pending.keys().copied().collect();
        for f in fs {
            let (echo, digest, aseq) = self.echo_pending[&f].clone();
            let mut triples: Vec<(MemberId, u64, u32, EntryId)> = Vec::with_capacity(echo.len());
            let (mut wait, mut differ) = (false, false);
            for &(x, b) in &echo {
                match self.ckpt_by_block.get(&(x, b)) {
                    Some(&(s, id, _)) => triples.push((x, b, s, id)),
                    // My version of x's chain is past block b with no checkpoint there: another line.
                    None if self.ckpt.get(&x).is_some_and(|&(nb, _)| nb > b) => differ = true,
                    None => wait = true,
                }
            }
            if wait && !differ {
                continue;
            }
            self.echo_pending.remove(&f);
            if differ || echo_digest(triples.iter().map(|&(x, b, _, id)| (x, b, id))) != digest {
                self.stats.echo_mismatches += 1;
                continue;
            }
            self.stats.echoes_matched += 1;
            if !self.cfg.echo {
                continue;
            }
            for (x, _, s, id) in triples {
                // Review R3-1: a member's vote against x is its last word on x's chain in this epoch.
                if self.votes.get(&x).is_some_and(|v| v.contains_key(&f)) {
                    continue;
                }
                self.echo_refs.entry((f, x)).or_default().insert(s, (id, aseq));
            }
        }
    }

    /// Revision 21 (RV20-3): a fed replica's votes, on evidence it can check without a log. Here silence: a member
    /// whose checkpoints haven't reached me for suspect_after plus one checkpoint period plus my own downlink delay
    /// (so a thin F doesn't suspect healthy members as a matter of course). Its vote alone removes no one: a removal
    /// needs q votes. Its frozen head is its last word, as for any voter.
    pub(super) fn fed_suspects(&mut self) -> Vec<Entry> {
        let mut out = Vec::new();
        if !self.members.contains(&self.me) || self.closed_at.is_some() || self.members.len() < 3 || self.withhold_vc {
            return out;
        }
        let Some(f) = self.fed.as_ref() else { return out };
        let limit = self.cfg.suspect_after + super::CKPT_FRAMES + f.delay_frames;
        let silent: Vec<(MemberId, u64)> = f
            .ckpt_heard
            .iter()
            .filter(|(m, _)| **m != self.me && self.members.contains(m))
            .filter(|(_, t)| self.now >= **t + limit)
            .map(|(m, t)| (*m, self.now - *t))
            .collect();
        for (j, quiet) in silent {
            if self.voted(j) {
                continue;
            }
            self.stats.suspicions += 1;
            self.ev(format!("fed: member {j}'s checkpoints haven't reached me for {quiet} frames (limit {limit}): presumed crashed; vote to remove it"));
            out.extend(self.freeze_and_vote(j, 0, 0));
        }
        out
    }

    /// Write this frame's feed states, after the world step (like carried states).
    pub(super) fn write_anchors(&mut self, f: u64) {
        let Some(bodies) = self.fed.as_ref().and_then(|fed| fed.anchors.get(&f)).cloned() else { return };
        for b in bodies {
            if let Some(o) = self.live(b.obj) {
                b.to_state().write(&mut self.world, o.body);
            }
        }
    }
}

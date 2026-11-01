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

use super::{BodyState, Msg, WorldSim};

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
                self.fed = Some(Fed { anchors: BTreeMap::new(), since: self.frame });
            }
            (false, true) => {
                let fed = self.fed.take().unwrap();
                self.ev(format!("feed: member {} comes off the state feed at frame {} (on it since {})", self.me, self.frame, fed.since));
                if let Some(&f) = fed.anchors.keys().next() {
                    if f >= self.conf && f < self.frame {
                        self.mark_dirty(f);
                    }
                }
            }
            _ => {}
        }
    }

    /// A feed packet: the feeder's states of `bodies` after `frame`. Written at that frame on the next
    /// re-simulation, if it lies within what this replica simulated (older ones are dropped).
    pub fn on_feed(&mut self, frame: u64, bodies: Vec<FeedBody>) {
        let Some(since) = self.fed.as_ref().map(|f| f.since) else { return };
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

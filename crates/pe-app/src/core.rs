//! The peer core, without IO: one member's replica of one world and the session protocol around it.
//! Messages and time go in (`receive`, `tick`); addressed messages and events come out (`outbox`,
//! `events`). The node (`node.rs`) owns the socket and the clock.
//!
//! This is the shape agreed with @engine-architect for pe-session (P2-3), which will own this core:
//! * every time is on the session's agreed timeline (the node applies the clock-sync offset);
//! * the core resolves recipients itself (`WorldSim::audience`, which changes with epochs);
//! * each outgoing message has a class, so the IO can defer repair traffic on a long queue;
//! * `next_deadline` tells the IO when it must tick again.
//!
//! The per-tick order is `spikes/session-sim`'s `tick_peer` at viewchange b01ebdf: advance, planned
//! actions, ack, attests, suspicions, view change, fetches.

use std::collections::BTreeMap;

use pe_sim_core::session::{Entry, EntryId, MemberId, Msg, Outcome, Payload, SceneKind, SimConfig, WorldSim};

/// What an outgoing message is, so the IO can prioritise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Authored entries and acks.
    Live,
    /// Votes, view-change entries and equivocation proofs.
    Control,
    /// Answers to fetches: may wait while the uplink queue is long (review R3-9).
    Repair,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Out {
    pub to: Vec<MemberId>,
    pub class: Class,
    pub msg: Msg,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// One of my entries got a (new) verdict, or became final.
    Outcome { entry: EntryId, frame: u64, outcome: Outcome, is_final: bool },
    /// Every frame below `conf` is confirmed.
    Confirmed { conf: u64 },
    /// A membership decision: the epoch reached and the members removed.
    Epoch { epoch: u32, removed: Vec<MemberId> },
    Paused { frame: u64 },
    Resumed { frame: u64 },
    /// A canonical check hash differed from another member's: divergence.
    CheckFailed { total: u64 },
}

/// Authoring refusals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorError {
    /// The session has not started, or the sim is behind its clock or paused at its speculation
    /// limit (non-hand payloads need a caught-up sim at the present frame).
    NotReady,
    /// A hand that is not this member's.
    NotOwner,
}

// `me`, `next_deadline`, `set_link` and the queue delay are the agreed pe-session API (P2-3); the node
// ticks every millisecond in M1 and needs none of them yet.
#[allow(dead_code)]
pub struct PeerCore {
    pub sim: WorldSim,
    epoch_us: Option<u64>,
    dt_us: u64,
    last_frame_seen: u64,
    next_ack_us: u64,
    ack_every_us: u64,
    /// R18 congestion slot: the uplink queue delay the IO measured. M1 on a LAN stays at level 0.
    queue_delay_us: u64,
    out: Vec<Out>,
    events: Vec<Event>,
    // event bookkeeping
    mine: BTreeMap<EntryId, (Option<(u64, Outcome)>, bool)>,
    last_conf: u64,
    last_epoch_log: usize,
    paused: bool,
    last_checks_bad: u64,
}

#[allow(dead_code)]
impl PeerCore {
    pub fn new(world: u32, me: MemberId, members: &[MemberId], scene: SceneKind, cfg: SimConfig) -> Self {
        let dt_us = (cfg.dt as f64 * 1e6).round() as u64;
        PeerCore {
            sim: WorldSim::new(world, me, members, scene, cfg),
            epoch_us: None,
            dt_us,
            last_frame_seen: u64::MAX,
            next_ack_us: 0,
            ack_every_us: 2 * dt_us,
            queue_delay_us: 0,
            out: Vec::new(),
            events: Vec::new(),
            mine: BTreeMap::new(),
            last_conf: 0,
            last_epoch_log: 0,
            paused: false,
            last_checks_bad: 0,
        }
    }

    pub fn me(&self) -> MemberId {
        self.sim.me
    }

    pub fn dt_us(&self) -> u64 {
        self.dt_us
    }

    pub fn started(&self) -> bool {
        self.epoch_us.is_some()
    }

    /// The session starts at `epoch_us` (session timeline). Later calls are ignored.
    pub fn start(&mut self, epoch_us: u64) {
        self.epoch_us.get_or_insert(epoch_us);
    }

    /// The clock frame at `now_us` (None before the epoch).
    pub fn clock_frame(&self, now_us: u64) -> Option<u64> {
        let e = self.epoch_us?;
        (now_us >= e).then(|| (now_us - e) / self.dt_us)
    }

    /// When the IO must call `tick` next: the next frame boundary or the ack timer, whichever is
    /// sooner. None before the session starts.
    pub fn next_deadline(&self) -> Option<u64> {
        let e = self.epoch_us?;
        let next_frame = match self.last_frame_seen {
            u64::MAX => e,
            f => e + (f + 1) * self.dt_us,
        };
        Some(next_frame.min(self.next_ack_us.max(e)))
    }

    /// R18 slot: the measured uplink queue delay. Unused on a LAN (level 0).
    pub fn set_link(&mut self, queue_delay_us: u64) {
        self.queue_delay_us = queue_delay_us;
    }

    fn send(&mut self, class: Class, msg: Msg) {
        let to = self.sim.audience();
        if !to.is_empty() {
            self.out.push(Out { to, class, msg });
        }
    }

    fn send_to(&mut self, to: MemberId, class: Class, msg: Msg) {
        if to != self.sim.me {
            self.out.push(Out { to: vec![to], class, msg });
        }
    }

    fn fetches(&mut self, now_us: u64) {
        for (dest, f) in self.sim.take_fetches(now_us / 1000) {
            self.send_to(dest, Class::Control, f);
        }
    }

    /// A message from member `from`, already authenticated by the IO.
    pub fn receive(&mut self, from: MemberId, m: Msg, now_us: u64) {
        match m {
            Msg::Entry(e) => {
                // Proofs of equivocation come back with this member's vote: broadcast them.
                for v in self.sim.insert(e) {
                    self.send(Class::Control, Msg::Entry(v));
                }
                self.fetches(now_us);
            }
            Msg::Fetch { author, from: a, to: b } => {
                for r in self.sim.on_fetch(author, a, b) {
                    self.send_to(from, Class::Repair, r);
                }
            }
            Msg::FetchValue { value } => {
                for r in self.sim.on_fetch_value(value) {
                    self.send_to(from, Class::Repair, r);
                }
            }
            // Static membership in M1: joins are not admitted.
            Msg::Join { .. } => {}
            Msg::Batch(v) => {
                for x in v {
                    self.receive(from, x, now_us);
                }
            }
        }
        self.note_events();
    }

    /// Advance to the clock frame of `now_us`; ack, vote, change views and repair as due.
    pub fn tick(&mut self, now_us: u64) {
        let Some(frame) = self.clock_frame(now_us) else { return };
        self.sim.now = frame;
        if frame != self.last_frame_seen || !self.sim.caught_up() {
            self.last_frame_seen = frame;
            self.sim.advance(frame);
        }
        if now_us >= self.next_ack_us {
            self.next_ack_us = now_us + self.ack_every_us;
            let a = self.sim.author_ack();
            self.send(Class::Live, Msg::Entry(a));
        }
        for e in self.sim.attest_due() {
            self.send(Class::Control, Msg::Entry(e));
        }
        // Review R3-2: vote out silent members (and, since P2-4, members withholding references).
        for e in self.sim.suspects_due() {
            self.send(Class::Control, Msg::Entry(e));
        }
        for e in self.sim.view_change_due() {
            self.send(Class::Control, Msg::Entry(e));
        }
        self.fetches(now_us);
        self.note_events();
    }

    /// Whether a non-hand payload can be authored now.
    pub fn ready(&self) -> bool {
        self.started() && self.sim.caught_up() && self.sim.frame >= self.sim.now
    }

    /// Author a perturbation at the present frame (the player and agent path); it goes out with the
    /// next outbox. Hand poses stream even while the sim is behind, stamped at the present.
    pub fn author(&mut self, payload: Payload) -> Result<Entry, AuthorError> {
        if !self.started() {
            return Err(AuthorError::NotReady);
        }
        let e = match payload {
            Payload::Hand { obj, .. } => {
                if self.sim.hand_owners.get(&obj) != Some(&self.sim.me) {
                    return Err(AuthorError::NotOwner);
                }
                self.sim.author_ahead(payload, 0)
            }
            p => {
                if !self.ready() {
                    return Err(AuthorError::NotReady);
                }
                let carry = match p {
                    Payload::Touch { obj, .. } => Some(obj),
                    _ => None,
                };
                self.sim.author(p, carry)
            }
        };
        self.mine.insert(e.id(), (None, false));
        self.send(Class::Live, Msg::Entry(e.clone()));
        Ok(e)
    }

    pub fn outbox(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.out)
    }

    pub fn events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    fn note_events(&mut self) {
        let s = &self.sim;
        for (id, (last, fin)) in self.mine.iter_mut() {
            if *fin {
                continue;
            }
            let now = s.outcomes.get(id).copied();
            let is_final = s.entry(*id).is_some_and(|e| e.stamp < s.conf);
            if now != *last || is_final {
                *last = now;
                *fin = is_final;
                if let Some((frame, outcome)) = now {
                    self.events.push(Event::Outcome { entry: *id, frame, outcome, is_final });
                }
            }
        }
        if s.conf != self.last_conf {
            self.last_conf = s.conf;
            self.events.push(Event::Confirmed { conf: s.conf });
        }
        for (epoch, _, _, removed) in &s.vc_log[self.last_epoch_log..] {
            self.events.push(Event::Epoch { epoch: *epoch, removed: removed.clone() });
        }
        self.last_epoch_log = s.vc_log.len();
        let paused = s.frame < s.now;
        if paused != self.paused {
            self.paused = paused;
            self.events.push(if paused { Event::Paused { frame: s.frame } } else { Event::Resumed { frame: s.frame } });
        }
        if s.stats.checks_bad != self.last_checks_bad {
            self.last_checks_bad = s.stats.checks_bad;
            self.events.push(Event::CheckFailed { total: s.stats.checks_bad });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two cores wired back to back with no loss: they start, exchange acks, confirm frames, and a
    /// touch by member 1 reaches member 2 with equal history.
    #[test]
    fn two_cores_confirm_and_agree() {
        let cfg = SimConfig::default();
        let scene = SceneKind::Pile { side: 2, layers: 2 };
        let mut a = PeerCore::new(1, 1, &[1, 2], scene, cfg);
        let mut b = PeerCore::new(1, 2, &[1, 2], scene, cfg);
        a.start(0);
        b.start(0);
        let mut touched = None;
        let mut events = vec![];
        for ms in 0..3000u64 {
            let now = ms * 1000;
            a.tick(now);
            b.tick(now);
            if ms == 500 {
                touched = Some(a.author(Payload::Touch { obj: 1, impulse: [0.0, 5.0, 0.0], grab: false }).unwrap().id());
            }
            for (src, dst) in [(1u32, 2u32), (2, 1)] {
                let outs = if src == 1 { a.outbox() } else { b.outbox() };
                for o in outs {
                    assert_eq!(o.to, vec![dst]);
                    if dst == 2 { b.receive(1, o.msg, now) } else { a.receive(2, o.msg, now) }
                }
            }
            events.extend(a.events());
        }
        let conf = a.sim.conf.min(b.sim.conf);
        assert!(conf > 150, "conf {conf}");
        assert!((0..conf).all(|f| a.sim.hashes.get(&f) == b.sim.hashes.get(&f)));
        let id = touched.unwrap();
        assert!(events.iter().any(|e| matches!(e, Event::Outcome { entry, outcome: Outcome::Accepted, is_final: true, .. } if *entry == id)));
        assert!(events.iter().any(|e| matches!(e, Event::Confirmed { .. })));
        let d = a.next_deadline().unwrap();
        assert!(d > 2_999_000 && d <= 3_000_000 + a.dt_us(), "{d}");
    }

    #[test]
    fn authoring_rules() {
        let cfg = SimConfig::default();
        let mut a = PeerCore::new(1, 1, &[1, 2], SceneKind::BallPit { balls: 10, hands: 2 }, cfg);
        assert_eq!(a.author(Payload::Touch { obj: 1, impulse: [0.0; 3], grab: false }).unwrap_err(), AuthorError::NotReady);
        a.start(0);
        a.tick(0);
        let mine = *a.sim.hand_owners.iter().find(|(_, m)| **m == 1).unwrap().0;
        let theirs = *a.sim.hand_owners.iter().find(|(_, m)| **m == 2).unwrap().0;
        let hand = |obj| Payload::Hand { obj, target_mm: [0, 500, 0], active: true, span: 1 };
        assert_eq!(a.author(hand(theirs)).unwrap_err(), AuthorError::NotOwner);
        assert!(a.author(hand(mine)).is_ok());
        assert!(a.author(Payload::Touch { obj: 1, impulse: [0.0, 1.0, 0.0], grab: false }).is_ok());
    }
}

//! The session core without IO (P2-3 S6): one member's replica of one world and the protocol
//! around it. Messages and time go in (`receive`, `begin_tick`/`end_tick` or `tick`); addressed
//! messages and events come out (`outbox`, `events`). The IO layer owns sockets, datagram framing
//! and batching, the datagram-level authentication of its peers, and the clock.
//!
//! The shape is the one agreed for P2-10 and first written as `pe-app`'s `PeerCore` (ab54526 on
//! p2/app), with these differences:
//! * **Signatures:** entries arrive as [`SignedEntry`]s and the core verifies each one before it
//!   stores it (`WorldSim::insert`); the IO need not.
//! * **Time:** every call takes the IO's local monotonic time in µs. Frames follow the session
//!   timeline, local time plus the clock-sync offset (`set_offset`); the ack timer and fetch backoff
//!   run on local time, so a correction of the offset never makes them jump.
//! * **Ticks in two halves**, `begin_tick` (advance to the present frame) and `end_tick` (ack,
//!   attest, suspect, change views, fetch), so entries authored at the start of a frame precede
//!   that tick's ack in its datagram. `tick` is both halves for an IO that authors at other times.
//! * **R18 lives here:** the IO reports its uplink queue delay (`set_link`); with `adapt` on, the
//!   core raises and lowers its congestion level, which sets the ack interval and the hand-pose
//!   stride (a pose that is not due returns `AuthorError::Stride`). R18 is on by default (design
//!   10.9, D56: it is how the engine works); `adapt` and `ack_on_frames` off are for control runs.
//! * **Background worlds** (design 22.3) advance in bursts every `throttle_us`; `begin_tick`'s
//!   `burst` forces one, for a member about to author something that carries state.
//!
//! The order of everything is `spikes/session-sim`'s `tick_peer`, so the scenario harness
//! (`crates/pe-session-harness`), which drives its peers through this core, reproduces the spike's
//! digests.

use std::collections::{BTreeMap, BTreeSet};

use crate::session::{EntryId, MemberId, Msg, Outcome, Payload, SignedEntry, TransferError, WorldSim};

/// R18: frames between hand poses, and between acks, at each congestion level. Review R3-9 added
/// levels 4 and 5 for links of about 8-16 kbit/s; both are aligned to the same frame ticks, so a
/// tick's hand pose shares the ack's datagram.
pub const HAND_STRIDE: [u64; 6] = [1, 2, 3, 4, 6, 8];
pub const ACK_FRAMES: [u64; 6] = [2, 2, 3, 4, 6, 8];
/// R18's thresholds (design 10.9): raise above 40 ms at most every 100 ms; lower after 300 ms below
/// 10 ms.
const RAISE_US: u64 = 40_000;
const CALM_US: u64 = 10_000;
const RAISE_EVERY_US: u64 = 100_000;
const LOWER_AFTER_US: u64 = 300_000;

/// What an outgoing message is, so the IO can prioritise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Authored entries and acks.
    Live,
    /// Votes, view-change entries, equivocation proofs, fetch requests and the view change's
    /// fetches by value or id.
    Control,
    /// Answers to fetches of chain ranges: the IO may drop them while its uplink queue to the
    /// requester is long (review R3-9); the requester asks again. One fetch's answer is a run of
    /// consecutive `Repair` messages to the same member: the IO sends or drops the run whole,
    /// deciding at its first message (cutting a reply short starves repair on a busy link).
    Repair,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Out {
    /// In audience order, never this member.
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
    /// V3 (user 08:31Z): no admin is left, so the session closed at frame `at`: every member listed was
    /// removed, and nothing stamped at or after `at` acts.
    Closed { at: u64, removed: Vec<MemberId> },
    Paused { frame: u64 },
    Resumed { frame: u64 },
    /// A canonical check hash differed from another member's: divergence. `total` counts them.
    CheckFailed { total: u64 },
    /// A state transfer from `from` for check frame `frame` was restored, or refused (design 9.5). On a
    /// refusal, ask another member ([`SessionCore::request_state`]).
    Transfer { from: MemberId, frame: u64, result: Result<(), TransferError> },
}

/// Authoring refusals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorError {
    /// The session has not started, or the sim is behind its clock or paused at its speculation
    /// limit (anything but a hand pose needs a caught-up sim at the present frame).
    NotReady,
    /// A hand that is not this member's (8.4).
    NotOwner,
    /// A second pose for a hand in a frame that already has one.
    Stride,
    /// Accepted, nothing to send yet: under R18 thinning the pose waits in this member's pending
    /// pack (D73(a)) and already moves its hand here; it goes out with the pack.
    Pending,
}

/// D73(a): a hand's poses not yet sent, for consecutive frames from `p0`, at stride `k`.
struct Pack {
    p0: u64,
    k: u64,
    targets: Vec<[i32; 3]>,
}

struct TickState {
    clock_frame: u64,
    new_frame: bool,
    audience: Vec<MemberId>,
}

pub struct SessionCore {
    /// The replica. Read-only for the IO; its fields are not a stable API (M1).
    pub sim: WorldSim,
    started: bool,
    /// Frame 0 on the session timeline, µs.
    pub epoch_us: u64,
    pub dt_us: u64,
    /// Session time minus local time, µs (clock sync).
    pub offset_us: i64,
    /// Background mode (design 22.3): advance only every `throttle_us`, in one burst, from
    /// `throttle_from_us` (local time).
    pub throttle_us: Option<u64>,
    pub throttle_from_us: u64,
    last_burst_us: u64,
    next_ack_us: u64,
    pub ack_every_us: u64,
    /// Acks on frame ticks (a multiple of the ack interval) rather than by timer: for an IO that
    /// batches a tick's messages into one datagram (R18, viz-4). On by default.
    pub ack_on_frames: bool,
    last_frame_seen: u64,
    /// R18: adapt the hand-pose stride and ack interval to the uplink queue delay. On by default.
    pub adapt: bool,
    pub level: usize,
    level_t: u64,
    calm_since: u64,
    queue_delay_us: Option<u64>,
    /// D56's memory (design 10.9): after congestion began at level L, lowering into L or below
    /// needs `recall_calm_us` of calm instead of 300 ms, until `forget_us` pass with a signal and no
    /// congestion. [proposal: 5 s and 30 s, tuned by the 80 kbit/s run.]
    pub recall_calm_us: u64,
    pub forget_us: u64,
    /// The highest level at which congestion began (the signal crossed 40 ms upward), if remembered.
    pub congested_at: Option<usize>,
    over: bool,
    quiet_us: u64,
    last_adapt_us: Option<u64>,
    hand_last: BTreeMap<u32, u64>,
    /// D73(a): per hand, the poses waiting for their pack while R18 thins the stream.
    packs: BTreeMap<u32, Pack>,
    /// Ticks spent at each congestion level.
    pub level_ms: [u64; 6],
    tick: Option<TickState>,
    out: Vec<Out>,
    events: Vec<Event>,
    /// My entries whose outcome is not final yet, with the last outcome reported.
    mine: BTreeMap<EntryId, Option<(u64, Outcome)>>,
    last_conf: u64,
    last_epoch_log: usize,
    told_closed: bool,
    paused: bool,
    last_checks_bad: u64,
    /// Review P23-3: donors asked for a state ([`SessionCore::request_state`]) and not answered yet.
    state_asked: BTreeSet<MemberId>,
    /// Review P23-4: when each requester last got a state from this member.
    state_served: BTreeMap<MemberId, u64>,
}

/// Review P23-4: a member answers one `FetchState` per requester per this interval.
pub const STATE_ANSWER_INTERVAL_US: u64 = 1_000_000;

impl SessionCore {
    pub fn new(sim: WorldSim) -> Self {
        let dt_us = (sim.cfg.dt as f64 * 1e6).round() as u64;
        SessionCore {
            sim,
            started: false,
            epoch_us: 0,
            dt_us,
            offset_us: 0,
            throttle_us: None,
            throttle_from_us: 0,
            last_burst_us: 0,
            next_ack_us: 0,
            // Saturating: hz = 0 (dt = inf) must reach a validator rather than overflow in a debug build.
            ack_every_us: dt_us.saturating_mul(2),
            ack_on_frames: true,
            last_frame_seen: u64::MAX,
            adapt: true,
            level: 0,
            level_t: 0,
            calm_since: 0,
            queue_delay_us: None,
            recall_calm_us: 5_000_000,
            forget_us: 30_000_000,
            congested_at: None,
            over: false,
            quiet_us: 0,
            last_adapt_us: None,
            hand_last: BTreeMap::new(),
            packs: BTreeMap::new(),
            level_ms: [0; 6],
            tick: None,
            out: Vec::new(),
            events: Vec::new(),
            mine: BTreeMap::new(),
            last_conf: 0,
            last_epoch_log: 0,
            told_closed: false,
            paused: false,
            last_checks_bad: 0,
            state_asked: BTreeSet::new(),
            state_served: BTreeMap::new(),
        }
    }

    pub fn me(&self) -> MemberId {
        self.sim.me
    }

    pub fn started(&self) -> bool {
        self.started
    }

    /// The session's frame 0 is at `epoch_us` on the session timeline (after clock sync). Later
    /// calls are ignored.
    pub fn start(&mut self, epoch_us: u64) {
        if !self.started {
            self.started = true;
            self.epoch_us = epoch_us;
        }
    }

    /// Clock sync: session time = local time + `offset_us`.
    pub fn set_offset(&mut self, offset_us: i64) {
        self.offset_us = offset_us;
    }

    /// Idle (design 7.6, reduction 2): nothing but acks is unconfirmed in this member's own copy.
    /// R18's congested-level memory (D56) and idle thinning gate on it.
    pub fn idle(&self) -> bool {
        self.sim.idle()
    }

    /// R18: the uplink queue delay the IO measured (the longest towards any member), µs, or `None`
    /// when it has no fresh measurement. `None` holds the level: it is neither congestion nor calm,
    /// and D56's forget timer stops (review of the pe-app link signal, question 1).
    pub fn set_link(&mut self, queue_delay_us: Option<u64>) {
        self.queue_delay_us = queue_delay_us;
    }

    fn session_us(&self, now_us: u64) -> u64 {
        (now_us as i64 + self.offset_us).max(0) as u64
    }

    /// The clock frame at local time `now_us` (None before the session's frame 0).
    pub fn clock_frame(&self, now_us: u64) -> Option<u64> {
        let t = self.session_us(now_us);
        (self.started && t >= self.epoch_us).then(|| (t - self.epoch_us) / self.dt_us)
    }

    /// When the IO must tick again (local time): the next frame boundary or the ack timer,
    /// whichever is sooner. None before the session starts.
    pub fn next_deadline(&self) -> Option<u64> {
        if !self.started {
            return None;
        }
        let frame_session = match self.last_frame_seen {
            u64::MAX => self.epoch_us,
            f => self.epoch_us + (f + 1) * self.dt_us,
        };
        let frame_local = (frame_session as i64 - self.offset_us).max(0) as u64;
        Some(frame_local.min(self.next_ack_us))
    }

    /// Whether a payload other than a hand pose can be authored now.
    pub fn ready(&self) -> bool {
        self.started && self.sim.caught_up() && self.sim.frame == self.sim.now
    }

    fn audience(&self) -> Vec<MemberId> {
        match &self.tick {
            Some(t) => t.audience.clone(),
            None => self.sim.audience(),
        }
    }

    fn send_to_all(&mut self, to: &[MemberId], class: Class, msg: Msg) {
        let to: Vec<MemberId> = to.iter().copied().filter(|&m| m != self.sim.me).collect();
        if !to.is_empty() {
            self.out.push(Out { to, class, msg });
        }
    }

    fn send(&mut self, class: Class, msg: Msg) {
        let to = self.audience();
        self.send_to_all(&to, class, msg);
    }

    /// A joiner asks member `from` for its newest confirmed check-frame state (design 9.5). The answer
    /// is restored only against a check hash a quorum signed in the acks this replica holds (P22S-1), so
    /// it needs the log first; [`Event::Transfer`] reports the outcome.
    /// Only the first answer from that member, to this request, is considered (review P23-3), so an
    /// answer refused for any reason (a tampered state: `Hash`) uses up that request; ask again.
    ///
    /// Precondition (review P23R-1): the replica must not simulate past the donor's check frame before
    /// the answer arrives. A state never rewinds a replica (`Behind`), so a joiner that has replayed
    /// the log beyond that frame refuses every state. Hold its simulation until [`Event::Transfer`]
    /// (the harness's statejoin sets `throttle_us`); holding it in the core is a follow-up.
    pub fn request_state(&mut self, from: MemberId) {
        self.state_asked.insert(from);
        self.send_to(from, Class::Control, Msg::FetchState);
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

    /// A message from member `from`. Entries are verified here; the IO has authenticated `from`.
    pub fn receive(&mut self, from: MemberId, m: Msg, now_us: u64) {
        let audience = self.sim.audience();
        self.receive_one(from, m, now_us, &audience);
        self.note_events();
    }

    fn receive_one(&mut self, from: MemberId, m: Msg, now_us: u64, audience: &[MemberId]) {
        match m {
            Msg::Entry(e) => {
                // Proofs of equivocation come back with this member's vote: broadcast them.
                for v in self.sim.insert(e) {
                    self.send_to_all(audience, Class::Control, Msg::Entry(v));
                }
                self.fetches(now_us);
            }
            // Review P23-4: requests are answered for the members and followers only (`audience`),
            // and every answer is a repair, which the IO may drop under a long queue.
            Msg::Fetch { .. } | Msg::FetchValue { .. } | Msg::FetchEntry { .. } | Msg::FetchState if !audience.contains(&from) => {}
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
            Msg::FetchEntry { id, back } => {
                for r in self.sim.on_fetch_entry(id, back) {
                    self.send_to(from, Class::Repair, r);
                }
            }
            Msg::Join { member, attests, sig, expires } => self.sim.on_join(member, attests, sig, expires),
            // Design 9.5: the state is large, so it travels as a repair (the IO may drop it under a
            // long queue; the joiner asks again).
            Msg::FetchState => {
                // Review P23-4: one answer per requester per interval.
                if self.state_served.get(&from).is_some_and(|&t| now_us < t + STATE_ANSWER_INTERVAL_US) {
                    return;
                }
                if let Some(st) = self.sim.export_state() {
                    self.state_served.insert(from, now_us);
                    self.send_to(from, Class::Repair, Msg::State(Box::new(st)));
                }
            }
            Msg::State(st) => {
                // Review P23-3: only the answer to this member's own request, from that donor.
                let result = if self.state_asked.remove(&from) { self.sim.import_state(&st, from) } else { Err(TransferError::Unrequested) };
                if let Err(e) = result {
                    self.sim.stats.transfers_refused += 1;
                    self.sim.stats.last_refusal = Some(e);
                }
                self.events.push(Event::Transfer { from, frame: st.frame, result });
            }
            // Review P23-1: one level (the decoder refuses a batch inside a batch); a nested batch built
            // in process is ignored rather than recursed into.
            Msg::Batch(v) => {
                for x in v {
                    if !matches!(x, Msg::Batch(_)) {
                        self.receive_one(from, x, now_us, audience);
                    }
                }
            }
        }
    }

    /// The first half of a tick at local time `now_us`: run R18, then advance the sim to the
    /// present frame (unless the world is background and not due; `burst` forces it). Returns
    /// the clock frame if the sim was advanced, which is when the IO authors that frame's actions.
    pub fn begin_tick(&mut self, now_us: u64, burst: bool) -> Option<u64> {
        let Some(clock_frame) = self.clock_frame(now_us) else {
            self.tick = None;
            return None;
        };
        self.sim.now = clock_frame;
        let audience = self.sim.audience();
        if self.adapt {
            // R18: up fast, down slowly. The signal is this member's own uplink queue delay. `level`
            // is public: an out-of-range value is clamped, never indexed (review P23-14).
            self.level = self.level.min(HAND_STRIDE.len() - 1);
            let dt = self.last_adapt_us.map_or(0, |t| now_us.saturating_sub(t));
            self.last_adapt_us = Some(now_us);
            match self.queue_delay_us {
                Some(b) if b > RAISE_US => {
                    // D56: remember the level at which congestion began, not the levels the
                    // controller climbs through while the queue drains.
                    if !self.over {
                        self.over = true;
                        self.congested_at = self.congested_at.max(Some(self.level));
                    }
                    self.quiet_us = 0;
                    self.calm_since = 0;
                    if now_us >= self.level_t + RAISE_EVERY_US && self.level < HAND_STRIDE.len() - 1 {
                        self.level += 1;
                        self.level_t = now_us;
                    }
                }
                Some(b) => {
                    self.over = false;
                    self.quiet_us += dt;
                    if self.quiet_us >= self.forget_us {
                        self.congested_at = None;
                    }
                    if b < CALM_US {
                        if self.calm_since == 0 {
                            self.calm_since = now_us;
                        }
                    } else {
                        self.calm_since = 0;
                    }
                }
                // No signal: hold. Not calm, and the forget timer stops.
                None => self.calm_since = 0,
            }
            if self.level > 0 && self.calm_since > 0 {
                let into = self.level - 1;
                let calm = if self.congested_at.is_some_and(|c| into <= c) { self.recall_calm_us } else { LOWER_AFTER_US };
                if now_us >= self.calm_since + calm && now_us >= self.level_t + LOWER_AFTER_US {
                    self.level = into;
                    self.level_t = now_us;
                    self.calm_since = now_us;
                }
            }
            self.ack_every_us = ACK_FRAMES[self.level] * self.dt_us;
            self.level_ms[self.level] += 1;
        }
        // Simulate once per new clock frame (coalescing a frame's late arrivals into one rollback);
        // a background world advances in periodic bursts.
        let throttled = !burst && self.throttle_us.is_some_and(|p| now_us >= self.throttle_from_us && now_us < self.last_burst_us + p);
        let mut new_frame = false;
        let mut advanced = false;
        if !throttled && (clock_frame != self.last_frame_seen || !self.sim.caught_up()) {
            new_frame = clock_frame != self.last_frame_seen;
            self.last_frame_seen = clock_frame;
            if self.throttle_us.is_some() && now_us >= self.throttle_from_us {
                self.last_burst_us = now_us;
            }
            self.sim.advance(clock_frame);
            advanced = true;
        }
        self.tick = Some(TickState { clock_frame, new_frame, audience });
        advanced.then_some(clock_frame)
    }

    /// The second half of a tick: ack when due, then attestations, suspicions, the view change and
    /// fetch requests.
    pub fn end_tick(&mut self, now_us: u64) {
        let Some(t) = self.tick.take() else { return };
        let ack_due = if self.ack_on_frames {
            let every = (self.ack_every_us / self.dt_us).max(1);
            t.new_frame && t.clock_frame % every == 0
        } else {
            now_us >= self.next_ack_us
        };
        let send = |s: &mut Self, v: Vec<SignedEntry>, class| {
            for e in v {
                s.send_to_all(&t.audience, class, Msg::Entry(e));
            }
        };
        // Test fixture (review VC3R-6): a joiner that starts from a state transfer acks only once admitted.
        if ack_due && !(self.sim.quiet_until_member && !self.sim.membership().members().contains(&self.sim.me)) {
            self.next_ack_us = now_us + self.ack_every_us;
            let a = self.sim.author_ack();
            send(self, vec![a], Class::Live);
        }
        let v = self.sim.attest_due();
        send(self, v, Class::Control);
        // Review R3-2: vote out silent members; P2-4: decide removals.
        let v = self.sim.suspects_due();
        send(self, v, Class::Control);
        let v = self.sim.view_change_due();
        send(self, v, Class::Control);
        self.fetches(now_us);
        self.note_events();
    }

    /// A whole tick, for an IO that authors outside it.
    pub fn tick(&mut self, now_us: u64) {
        self.begin_tick(now_us, false);
        self.end_tick(now_us);
    }

    /// Author a perturbation at the present frame (the player and agent path); it goes out with
    /// the next outbox. Hand poses stream even while the sim is behind, stamped `ahead` frames past
    /// the present, at the congestion level's stride; anything else needs `ready()`. A touch
    /// carries its object's resulting state.
    pub fn author(&mut self, payload: Payload, ahead: u64) -> Result<SignedEntry, AuthorError> {
        if !self.started {
            return Err(AuthorError::NotReady);
        }
        let e = match payload {
            Payload::Hand { obj, target_mm, active, .. } => {
                if self.sim.hand_owners.get(&obj) != Some(&self.sim.me) {
                    return Err(AuthorError::NotOwner);
                }
                let k = if self.adapt { HAND_STRIDE[self.level.min(HAND_STRIDE.len() - 1)] } else { 1 };
                // The frame this pose is for: the stamp an entry authored now would get.
                let s = self.sim.frame.max(self.sim.now) + ahead;
                if !active {
                    self.flush_pack(obj);
                    self.sim.author_ahead(Payload::Hand { obj, target_mm, active, span: 1, more: Vec::new() }, ahead)
                } else {
                    if self.hand_last.get(&obj).is_some_and(|&l| s <= l) {
                        return Err(AuthorError::Stride);
                    }
                    self.hand_last.insert(obj, s);
                    if k == 1 {
                        self.flush_pack(obj);
                        self.sim.author_ahead(Payload::Hand { obj, target_mm, active, span: 1, more: Vec::new() }, ahead)
                    } else {
                        // D73(a): under R18 thinning every frame keeps its own target. The pose moves
                        // this member's hand at once (`own_pending`), and goes out in a pack of the
                        // frames of one stride window, [m k, m k + k - 1], stamped at its first frame:
                        // one entry every k frames, the author's hand exact. A stride change or a gap
                        // sends the pack early.
                        if self.packs.get(&obj).is_some_and(|p| p.k != k || p.p0 + p.targets.len() as u64 != s) {
                            self.flush_pack(obj);
                        }
                        let p = self.packs.entry(obj).or_insert(Pack { p0: s, k, targets: Vec::new() });
                        p.targets.push(target_mm);
                        let full = (s + 1) % k == 0 || p.targets.len() as u64 >= k;
                        self.sim.own_pending.insert((s, obj), target_mm);
                        self.sim.hold_horizon_below = self.packs.values().map(|p| p.p0).min();
                        if !full {
                            return Err(AuthorError::Pending);
                        }
                        return Ok(self.flush_pack(obj).expect("a pack with a pose"));
                    }
                }
            }
            p => {
                if !self.ready() {
                    return Err(AuthorError::NotReady);
                }
                match p {
                    Payload::Touch { obj, .. } => self.sim.author(p, Some(obj)),
                    Payload::Kick { target, .. } => self.sim.kick(target),
                    Payload::SetRole { member, role, .. } => self.sim.set_role(member, role),
                    Payload::DefineRole { role, perms, .. } => self.sim.define_role(role, perms),
                    p => self.sim.author(p, None),
                }
            }
        };
        Ok(self.sent(e))
    }

    /// D73(a): send hand `obj`'s pending pack, if any: one entry stamped at its first frame holding a
    /// target per frame. Its poses stop being local-only; the pack replaces them (its insertion rolls
    /// this replica back to the first frame, which re-simulates with the same targets).
    fn flush_pack(&mut self, obj: u32) -> Option<SignedEntry> {
        let p = self.packs.remove(&obj)?;
        for j in 0..p.targets.len() as u64 {
            self.sim.own_pending.remove(&(p.p0 + j, obj));
        }
        self.sim.hold_horizon_below = self.packs.values().map(|p| p.p0).min();
        let mut ts = p.targets.into_iter();
        let target_mm = ts.next().expect("a pack holds a pose");
        let e = self.sim.author_stamped(Payload::Hand { obj, target_mm, active: true, span: 1, more: ts.collect() }, p.p0);
        Some(self.sent(e))
    }

    /// Broadcast an entry this member authored on the sim directly (scenario fixtures: backdated
    /// entries, an adversarial member moving another's hand).
    pub fn sent(&mut self, e: SignedEntry) -> SignedEntry {
        self.mine.insert(e.id(), None);
        self.send(Class::Live, Msg::Entry(e.clone()));
        e
    }

    pub fn outbox(&mut self) -> Vec<Out> {
        std::mem::take(&mut self.out)
    }

    pub fn events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    pub fn view(&self) -> &WorldSim {
        &self.sim
    }

    fn note_events(&mut self) {
        let s = &self.sim;
        let mut done = Vec::new();
        for (id, last) in self.mine.iter_mut() {
            let now = s.outcomes.get(id).copied();
            // Review P23-7: final only once its frame is confirmed and simulated with no rollback pending.
            let is_final = s.entry(*id).is_some_and(|e| s.settled(e.stamp));
            if now != *last || is_final {
                *last = now;
                if let Some((frame, outcome)) = now {
                    self.events.push(Event::Outcome { entry: *id, frame, outcome, is_final });
                }
            }
            if is_final {
                done.push(*id);
            }
        }
        for id in done {
            self.mine.remove(&id);
        }
        if s.conf != self.last_conf {
            self.last_conf = s.conf;
            self.events.push(Event::Confirmed { conf: s.conf });
        }
        if let (Some(at), false) = (s.vcm.closed_at, self.told_closed) {
            self.told_closed = true;
            self.events.push(Event::Closed { at, removed: s.closed_members.clone() });
        }
        for (epoch, _, _, removed) in &s.vcm.vc_log[self.last_epoch_log..] {
            self.events.push(Event::Epoch { epoch: *epoch, removed: removed.clone() });
        }
        self.last_epoch_log = s.vcm.vc_log.len();
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
    use crate::session::{KeyRing, SceneKind, SimConfig};

    fn core(me: MemberId, members: &[MemberId], scene: SceneKind) -> SessionCore {
        SessionCore::new(WorldSim::new(1, me, members, scene, SimConfig::default(), KeyRing::test(me)))
    }

    /// Two cores wired back to back with no loss: they start, exchange acks, confirm frames, and a
    /// touch by member 1 reaches member 2 with equal history.
    #[test]
    fn two_cores_confirm_and_agree() {
        let scene = SceneKind::Pile { side: 2, layers: 2 };
        let (mut a, mut b) = (core(1, &[1, 2], scene), core(2, &[1, 2], scene));
        a.start(0);
        b.start(0);
        let mut touched = None;
        let mut events = vec![];
        for ms in 0..3000u64 {
            let now = ms * 1000;
            a.tick(now);
            b.tick(now);
            if ms == 500 {
                touched = Some(a.author(Payload::Touch { obj: 1, impulse: [0.0, 5.0, 0.0], grab: false }, 0).unwrap().id());
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
        assert!(d > 2_999_000 && d <= 3_000_000 + a.dt_us, "{d}");
    }

    #[test]
    fn authoring_rules() {
        let mut a = core(1, &[1, 2], SceneKind::BallPit { balls: 10, hands: 2 });
        assert_eq!(a.author(Payload::Touch { obj: 1, impulse: [0.0; 3], grab: false }, 0).unwrap_err(), AuthorError::NotReady);
        a.start(0);
        a.tick(0);
        let mine = *a.sim.hand_owners.iter().find(|(_, m)| **m == 1).unwrap().0;
        let theirs = *a.sim.hand_owners.iter().find(|(_, m)| **m == 2).unwrap().0;
        let hand = |obj| Payload::Hand { obj, target_mm: [0, 500, 0], active: true, span: 1, more: Vec::new() };
        assert_eq!(a.author(hand(theirs), 0).unwrap_err(), AuthorError::NotOwner);
        assert!(a.author(hand(mine), 0).is_ok());
        // One pose per frame at level 0: a second one in the same frame waits for the next.
        assert_eq!(a.author(hand(mine), 0).unwrap_err(), AuthorError::Stride);
        assert!(a.author(Payload::Touch { obj: 1, impulse: [0.0, 1.0, 0.0], grab: false }, 0).is_ok());
    }

    /// A forged or tampered entry from the wire is dropped by the core, not stored.
    #[test]
    fn the_core_verifies_entries() {
        let scene = SceneKind::Pile { side: 2, layers: 1 };
        let (mut a, mut b) = (core(1, &[1, 2], scene), core(2, &[1, 2], scene));
        a.start(0);
        b.start(0);
        a.tick(0);
        b.tick(0);
        let mut e = a.author(Payload::Touch { obj: 1, impulse: [0.0, 1.0, 0.0], grab: false }, 0).unwrap();
        e.sig[3] ^= 0x40;
        b.receive(1, Msg::Entry(e.clone()), 0);
        assert_eq!(b.sim.stats.bad_sigs, 1);
        assert!(b.sim.entry(e.id()).is_none());
    }
}

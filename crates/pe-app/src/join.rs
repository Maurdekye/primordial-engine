//! Joining from a cut (app.md 4, "Joining from a cut"): A opens the world alone ([`crate::net::Endpoint::open_alone`])
//! and plays. B joins later at a reconfiguring cut E, the cut that makes A's `Admit` of B effective. At n = 2 nothing
//! becomes final in the new epoch until B votes, so A writes the `Admit` only once B has shown it can start.
//!
//! - **pe-app's messages** ([`AppMsg`]) ride the Protocol lane under [`TAG`]:
//!   - `JoinReady`, B to A, once B's checks on the welcome passed and its clock is ready;
//!   - `JoinStart`, A to B: pe-agree's start record, once E is final at A;
//!   - `JoinImported`, B to A, once B holds E's state.
//!
//!   pe-agree's wire messages start with a one-byte tag from 1 up, far below the tag's first byte, so neither can be
//!   read as the other. The simulation thread offers each Protocol message to the hook first
//!   ([`crate::sim::NetHook::claim`]), so agreement never sees pe-app's.
//! - **The inviter** ([`Inviter`]) runs one join at a time, on the simulation thread through
//!   [`crate::net::MemberHook`]. Its steps ([`Step`]):
//!   1. the reply a person pastes;
//!   2. B's hello and the welcome;
//!   3. `JoinReady`;
//!   4. the `Admit`, written by the simulation thread;
//!   5. E final here, then `JoinStart`;
//!   6. B's import, then the new epoch's first final cut.
//!
//!   Each step starts `T_join` ([`T_JOIN`]) again. The join fails when a step takes longer, or at once when B's
//!   link closes. Before the `Admit` a failure costs nothing: A closes B's link, and another reply may come. After
//!   it the session can't confirm changes until that cut is final.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use pe_net::lifecycle::{CloseCode, Refusal};
use pe_net_crypto::identity::{Invite, Reply};
use pe_seams::agree::CutRef;
use pe_seams::{Admit, Agreement, HostTime, LineId, MemberKey, SubmitError};

use crate::genesis::Role;
use crate::leave::{capitalized, player};
use crate::world::DEV_MEMBER;

/// How long a join may take one step (app.md 4): its steps are the `Admit`, `JoinStart` sent, `JoinImported` and the
/// new epoch's first final cut, so the fetch and the catch-up each get a whole `T_join`.
pub const T_JOIN: Duration = Duration::from_secs(30);

/// How long the inviter holds a join request it hasn't decided, from a key that may be the next reply's. pe-net closes
/// such a connection at `T_welcome` (10 s, code 8), which the joiner waits through, and the inviter's own dial carries
/// the join then. A held request older than that only welcomes a closed connection, which does nothing.
const HELD: Duration = Duration::from_secs(30);

/// Why a join into a world with scripts is refused, to both players (app.md 4's M1 limit, @coordinator-opus): a joiner's import carries no script archive, and a joiner that builds every active version first
/// isn't built, so it would step without them and split.
pub const SCRIPTS: &str = "this world has scripts; joining it comes after M1";

/// Why a join is refused once the session's members changed since the invite was made: its pin no longer holds, so no
/// joiner's check of the welcome could pass (app.md 4's M1 limits). After B's join that refuses every third player.
pub const PLAYERS_CHANGED: &str = "the players changed since the invite was made";

/// The tag pe-app's own Protocol-lane messages start with (module doc).
pub const TAG: &[u8] = b"pe.app/";

const K_READY: u8 = 1;
const K_START: u8 = 2;
const K_IMPORTED: u8 = 3;

/// pe-app's messages on the Protocol lane (module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppMsg {
    /// B to A: the welcome passed B's checks and B's clock is ready (transport.md 5.4).
    JoinReady,
    /// A to B: pe-agree's start record of the epoch that admitted B (S3's `epoch_start`), once E is final at A.
    JoinStart(Vec<u8>),
    /// B to A: B holds E's state (pe-diverge's join is `Joined`).
    JoinImported,
}

impl AppMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = TAG.to_vec();
        match self {
            AppMsg::JoinReady => b.push(K_READY),
            AppMsg::JoinStart(start) => {
                b.push(K_START);
                b.extend_from_slice(start);
            }
            AppMsg::JoinImported => b.push(K_IMPORTED),
        }
        b
    }

    /// `None` if `bytes` aren't pe-app's (no [`TAG`]); `Some(None)` if they carry the tag but are no message this
    /// build knows.
    pub fn decode(bytes: &[u8]) -> Option<Option<AppMsg>> {
        let rest = bytes.strip_prefix(TAG)?;
        Some(match rest.split_first() {
            Some((&K_READY, [])) => Some(AppMsg::JoinReady),
            Some((&K_START, start)) => Some(AppMsg::JoinStart(start.to_vec())),
            Some((&K_IMPORTED, [])) => Some(AppMsg::JoinImported),
            _ => None,
        })
    }
}

/// The largest `JoinStart` payload: pe-net's largest Protocol message, less the tag and the kind.
pub fn max_start_len() -> usize {
    pe_net::lanes::LaneParams::default().l_msg - TAG.len() - 1
}

/// What the inviter reads of agreement once a tick ([`Inviter::tick`]).
#[derive(Clone, Debug, Default)]
pub(crate) struct AgreeAt {
    pub(crate) members: Vec<MemberKey>,
    pub(crate) epoch: u64,
    pub(crate) final_height: u64,
    pub(crate) final_cut: Option<CutRef>,
}

/// A join's steps at the inviter, in order (module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// The reply was taken and pe-net dials the joiner: its hello is awaited.
    Dialing,
    /// Welcomed at the transport: its `JoinReady` is awaited.
    Welcomed,
    /// Ready: the simulation thread is to write the `Admit` ([`Inviter::take_entry`]).
    Ready,
    /// The `Admit` is in this member's chain: E, the cut that makes it effective, is awaited here.
    Admitted,
    /// E is final here and `JoinStart` went out: the joiner's import is awaited.
    Started,
    /// The joiner holds E's state: its first vote, the new epoch's first final cut, is awaited.
    Imported,
}

/// What the inviter asks of the network; [`crate::net::MemberHook`] turns each into a pe-net command.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    /// Dial the joiner this reply names.
    TakeReply(Reply),
    /// Welcome this key at the transport, naming the current epoch's members.
    Welcome(MemberKey),
    /// Close this key's connection, saying why.
    Refuse(MemberKey, Refusal),
    /// One of pe-app's messages to this key, on the Protocol lane.
    Send(MemberKey, AppMsg),
    /// The clock's members: every member but this machine (transport.md 5.3).
    ClockMembers(Vec<MemberKey>),
}

/// The inviter's counts, for tests and diagnostics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JoinStats {
    /// Replies taken, and replies refused (another invite's, expired, a member's, or a join already in flight).
    pub replies: u64,
    pub replies_refused: u64,
    /// Joins welcomed at the transport, admitted (the `Admit` written), started (`JoinStart` sent), joined (the new
    /// epoch's first cut final) and failed.
    pub welcomed: u64,
    pub admitted: u64,
    pub started: u64,
    pub joined: u64,
    pub failed: u64,
}

/// The one join in flight.
#[derive(Clone, Debug)]
struct Join {
    key: MemberKey,
    step: Step,
    /// When it last took a step: `T_join` runs from here.
    since: HostTime,
    /// The final height when E became final here: the new epoch's first final cut is above it.
    e_height: Option<u64>,
    /// It failed after its `Admit` was written: the new epoch's first final cut may still come, and ends it.
    failed: bool,
}

/// The inviter's side of joins (module doc): its invite, the replies a person pastes, and the one join in flight.
pub struct Inviter {
    me: MemberKey,
    /// The session's line: the invite's pin hashes the members under it.
    line: LineId,
    invite: Invite,
    replies: Receiver<Reply>,
    /// How long a join step may take before the join fails, ns. Shared ([`Inviter::t_join_handle`]), so a test can run
    /// its setup under the product's `T_join` and shorten it once its cut is final.
    t_join: Arc<AtomicU64>,
    join: Option<Join>,
    /// Join requests not decided yet, from keys that may be the next reply's, with when each came.
    held: BTreeMap<MemberKey, HostTime>,
    /// The `Admit` for the simulation thread to write, taken once.
    entry: Option<Vec<u8>>,
    acts: Vec<Act>,
    notices: Vec<String>,
    pub stats: JoinStats,
}

impl Inviter {
    /// The join timeout, shared: a test shortens it after its setup (net.rs's held-join test). The inviter reads it at
    /// each check, so a change applies to the step in flight.
    pub fn t_join_handle(&self) -> Arc<AtomicU64> {
        self.t_join.clone()
    }

    /// The inviter of `invite`, made in the session on `line`, taking replies from `replies`.
    pub fn new(me: MemberKey, line: LineId, invite: Invite, replies: Receiver<Reply>, t_join: Duration) -> Inviter {
        Inviter {
            me,
            line,
            invite,
            replies,
            t_join: Arc::new(AtomicU64::new(u64::try_from(t_join.as_nanos()).unwrap_or(u64::MAX))),
            join: None,
            held: BTreeMap::new(),
            entry: None,
            acts: Vec::new(),
            notices: Vec::new(),
            stats: JoinStats::default(),
        }
    }

    /// The join in flight: its key, its step, and whether it failed after its `Admit`.
    pub fn join(&self) -> Option<(MemberKey, Step, bool)> {
        self.join.as_ref().map(|j| (j.key, j.step, j.failed))
    }

    /// The network's part of what the inviter decided since the last call.
    pub fn take_acts(&mut self) -> Vec<Act> {
        std::mem::take(&mut self.acts)
    }

    /// What the console should say since the last call.
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    /// The `Admit` the simulation thread is to write, once. A world that holds a script by now refuses the join
    /// instead (M1's limit, checked again in the tick that writes the `Admit`).
    pub fn take_entry(&mut self, holds_scripts: bool, members: &[MemberKey], now: HostTime) -> Option<Vec<u8>> {
        if holds_scripts && self.entry.is_some() {
            self.fail(SCRIPTS, members, now);
            return None;
        }
        self.entry.take()
    }

    /// Whether a join this machine admitted is in flight: its `Leave` waits (app.md 4, "A quit during a join").
    pub fn holds_leave(&self) -> bool {
        self.join.as_ref().is_some_and(|j| j.step >= Step::Admitted && !j.failed)
    }

    /// Whether a join this machine admits is past the joiner's ready and not yet over (failed after its `Admit`
    /// included, since the joiner may still come in): pe-api then answers a script commit `unavailable` (app.md 4, "A
    /// world with scripts: an M1 limit").
    pub fn holds_script_commits(&self) -> bool {
        self.join.as_ref().is_some_and(|j| j.step >= Step::Ready)
    }

    /// Whether the session is still in the epoch the invite pins, with the members it pins: only then can a joiner's
    /// check of the welcome's members pass, and pe-agree's start verify against the pinned epoch.
    fn pin_holds(&self, members: &[MemberKey], epoch: u64) -> bool {
        self.invite
            .pin
            .is_some_and(|p| p.epoch == epoch && pe_agree::core::members_hash(self.line, epoch, members).0 == p.members)
    }

    fn expired(&self) -> bool {
        self.invite.expiry_unix_s < crate::net::unix_now()
    }

    /// A reply a person pasted: take it if it answers this invite, names nobody in the session, and no join is in
    /// flight; the console says why otherwise.
    pub(crate) fn on_reply(&mut self, reply: Reply, members: &[MemberKey], epoch: u64, now: HostTime) {
        let key = MemberKey(reply.key);
        let refused = if reply.invite_id != self.invite.id || self.invite.key != self.me.0 {
            Some("That reply code answers another invite.".to_string())
        } else if self.expired() {
            Some("The invite has expired: start a new session to invite someone.".to_string())
        } else if key == self.me || key == DEV_MEMBER || members.contains(&key) {
            Some(format!("{} is in this session already.", capitalized(player(key))))
        } else if let Some(j) = &self.join {
            Some(format!("{} is joining; paste the next reply code once that join ends.", capitalized(player(j.key))))
        } else if !self.pin_holds(members, epoch) {
            Some("The players changed since the invite was made, so it can't be used: start a new session.".into())
        } else {
            None
        };
        if let Some(why) = refused {
            self.stats.replies_refused += 1;
            self.notices.push(why);
            return;
        }
        self.stats.replies += 1;
        self.acts.push(Act::TakeReply(reply));
        self.join = Some(Join { key, step: Step::Dialing, since: now, e_height: None, failed: false });
        // The joiner dialled when it printed its reply code, so its hello may be here already.
        if self.held.remove(&key).is_some() {
            self.welcome(key, now);
        }
    }

    /// Welcome the joiner, on every connection its hello comes on: one may be its early dial, already closed, and
    /// the next the inviter's own (pe-net keeps one of them).
    fn welcome(&mut self, key: MemberKey, now: HostTime) {
        if let Some(j) = &mut self.join
            && j.key == key
        {
            if j.step == Step::Dialing {
                j.step = Step::Welcomed;
                j.since = now;
                self.stats.welcomed += 1;
            }
            self.acts.push(Act::Welcome(key));
        }
    }

    /// A join request from a key that isn't a member: `true` if the inviter took it (welcomed it, holds it
    /// undecided while the invite is open and no join is in flight, or refused it itself, saying why); `false`
    /// refuses it, "not admitted". While the world holds a script, the joiner is refused with [`SCRIPTS`]; once the
    /// members changed since the invite (its pin no longer holds), with [`PLAYERS_CHANGED`], at once rather than
    /// held until pe-net's `T_welcome` closes it, since no reply could start its join (@shader-lang's Low on
    /// d27af1c1: a third player couldn't join anyway, as a member other than the inviter refuses its mesh dial).
    pub fn on_join_request(
        &mut self,
        peer: MemberKey,
        members: &[MemberKey],
        epoch: u64,
        holds_scripts: bool,
        now: HostTime,
    ) -> bool {
        let ours = match &self.join {
            Some(j) => j.key == peer && !j.failed,
            None => !self.expired() && peer != DEV_MEMBER,
        };
        if ours && holds_scripts {
            if self.join.is_some() {
                self.fail(SCRIPTS, members, now);
            } else {
                self.acts.push(Act::Refuse(peer, refusal(SCRIPTS)));
                self.notices.push(couldnt_join(peer, SCRIPTS));
            }
            return true;
        }
        match &self.join {
            Some(j) if j.key == peer && !j.failed => {
                if j.step <= Step::Welcomed && !self.pin_holds(members, epoch) {
                    self.fail(PLAYERS_CHANGED, members, now);
                } else {
                    self.welcome(peer, now);
                }
                true
            }
            Some(_) => false,
            None if !self.expired() && peer != DEV_MEMBER => {
                if self.pin_holds(members, epoch) {
                    self.held.insert(peer, now);
                } else {
                    self.acts.push(Act::Refuse(peer, refusal(PLAYERS_CHANGED)));
                    self.notices.push(couldnt_join(peer, PLAYERS_CHANGED));
                }
                true
            }
            None => false,
        }
    }

    /// One of pe-app's messages from `from`.
    pub fn on_message(&mut self, from: MemberKey, msg: AppMsg, members: &[MemberKey], now: HostTime) {
        let me = self.me;
        let Some(j) = &mut self.join else { return };
        if j.key != from || j.failed {
            return;
        }
        match (msg, j.step) {
            (AppMsg::JoinReady, Step::Welcomed) => {
                j.step = Step::Ready;
                j.since = now;
                self.entry = Some(Admit::new(from, Role::Joiner.perms()).encode().to_vec());
                // The joiner's clock is ready, and the members it converged on are running: only now does it count
                // among this machine's clock members (transport.md 5.4, @design-reviewer's Low 3).
                let mut clock: Vec<MemberKey> = members.iter().copied().filter(|m| *m != me).collect();
                clock.push(from);
                clock.sort();
                self.acts.push(Act::ClockMembers(clock));
            }
            (AppMsg::JoinImported, Step::Started) => {
                j.step = Step::Imported;
                j.since = now;
            }
            _ => {}
        }
    }

    /// What became of the `Admit` [`Inviter::take_entry`] handed over.
    pub fn entry_written(&mut self, written: Result<(), SubmitError>, members: &[MemberKey], now: HostTime) {
        let Some(j) = &mut self.join else { return };
        if j.step != Step::Ready {
            return;
        }
        match written {
            Ok(()) => {
                j.step = Step::Admitted;
                j.since = now;
                self.stats.admitted += 1;
                self.notices.push(joining(j.key));
            }
            Err(e) => self.fail(&format!("agreement refused the Admit ({e:?})"), members, now),
        }
    }

    /// The joiner's link went down: a join past its welcome fails at once.
    pub fn on_link_down(&mut self, peer: MemberKey, members: &[MemberKey], now: HostTime) {
        if self.join.as_ref().is_some_and(|j| j.key == peer && j.step >= Step::Welcomed) {
            self.fail("their connection closed", members, now);
        }
    }

    /// Once a tick: the replies pasted since, E becoming final, the new epoch's first final cut, and `T_join`.
    ///
    /// `verified` is the cut this machine's state is verified at (pe-diverge's Cv). E's start goes out only once it is
    /// E: the joiner fetches E's state from this machine, and pe-diverge serves only a root it verified, answering
    /// `NotHeld` before then, which would fail a join whose one source this is. A replica still catching up after
    /// the start (500 balls built, then stepped) verifies E a little after agreement makes it final (the first
    /// two-process demo, 2026-11-18).
    pub fn on_tick(&mut self, agree: &dyn Agreement, verified: Option<CutRef>, now: HostTime) {
        let at = AgreeAt {
            members: agree.members(),
            epoch: agree.epoch(),
            final_height: agree.stats().final_height,
            final_cut: agree.final_cut().map(|fc| fc.cut.at),
        };
        self.tick(&at, verified, &|| agree.epoch_start(), now);
    }

    /// [`Inviter::on_tick`] on what it reads of agreement: `start` is S3's `epoch_start`, asked only when needed.
    pub(crate) fn tick(
        &mut self,
        at: &AgreeAt,
        verified: Option<CutRef>,
        start: &dyn Fn() -> Option<Vec<u8>>,
        now: HostTime,
    ) {
        let (members, epoch, final_height) = (&at.members[..], at.epoch, at.final_height);
        while let Ok(reply) = self.replies.try_recv() {
            self.on_reply(reply, members, epoch, now);
        }
        let held = i64::try_from(HELD.as_nanos()).unwrap_or(i64::MAX);
        self.held.retain(|_, at| now.0.saturating_sub(at.0) < held);
        let Some(j) = &mut self.join else { return };
        if j.step == Step::Admitted
            && members.contains(&j.key)
            && let Some(e) = at.final_cut
            && verified == Some(e)
        {
            // E, the cut that makes the `Admit` effective, is final here, and this machine's state is verified there.
            j.e_height = Some(final_height);
            match start() {
                Some(start) if start.len() <= max_start_len() => {
                    j.step = Step::Started;
                    j.since = now;
                    self.stats.started += 1;
                    self.acts.push(Act::Send(j.key, AppMsg::JoinStart(start)));
                }
                start => {
                    let key = j.key;
                    let why = match start {
                        Some(s) => format!("the start record ({} bytes) is too large for a message", s.len()),
                        None => "this machine no longer held the epoch's certified cut".into(),
                    };
                    self.fail(&why, members, now);
                    self.acts.push(Act::Refuse(key, refusal(&why)));
                    return;
                }
            }
        }
        let Some(j) = &mut self.join else { return };
        if let Some(e) = j.e_height
            && final_height > e
        {
            // The new epoch's first final cut: the joiner voted.
            self.notices.push(if j.failed { joined_after_all(j.key) } else { joined(j.key) });
            self.stats.joined += 1;
            self.join = None;
            return;
        }
        let ns = self.t_join.load(Ordering::Relaxed);
        let t_join = i64::try_from(ns).unwrap_or(i64::MAX);
        if !j.failed && now.0.saturating_sub(j.since.0) > t_join {
            let why = format!("the join took no step for {} s", Duration::from_nanos(ns).as_secs());
            self.fail(&why, members, now);
        }
    }

    /// The join failed (module doc). Before its `Admit`: the joiner's link closes, the clock's members are set back,
    /// and another reply may come. After it: the session can't confirm changes until the new epoch's first cut is
    /// final, which ends the join after all.
    fn fail(&mut self, why: &str, members: &[MemberKey], _now: HostTime) {
        let me = self.me;
        let Some(j) = &mut self.join else { return };
        if j.failed {
            return;
        }
        self.stats.failed += 1;
        if j.step >= Step::Admitted {
            j.failed = true;
            self.notices.push(join_failed(j.key));
            return;
        }
        let key = j.key;
        if j.step == Step::Ready {
            self.entry = None;
            self.acts.push(Act::ClockMembers(members.iter().copied().filter(|m| *m != me).collect()));
        }
        self.acts.push(Act::Refuse(key, refusal(why)));
        self.notices.push(couldnt_join(key, why));
        self.join = None;
    }
}

fn refusal(why: &str) -> Refusal {
    Refusal { code: CloseCode::NotAdmitted, detail: format!("not admitted: {why}") }
}

/// The inviter's console, from the `Admit` on (app.md 4).
pub fn joining(m: MemberKey) -> String {
    format!("{} is joining…", capitalized(player(m)))
}

/// The inviter's console, when a join fails before its `Admit`: nothing is lost.
pub fn couldnt_join(m: MemberKey, why: &str) -> String {
    format!("{} couldn't join: {why}.", capitalized(player(m)))
}

/// The inviter's console, when a join fails after its `Admit` (app.md 4).
pub fn join_failed(m: MemberKey) -> String {
    format!("{} couldn't join. This session can't confirm changes any more: start a new one.", capitalized(player(m)))
}

/// The inviter's console, at the new epoch's first final cut.
pub fn joined(m: MemberKey) -> String {
    format!("{} joined the session.", capitalized(player(m)))
}

/// The joiner's console while its import runs (app.md 4: pe-api answers not-ready until the replica reaches the
/// present).
pub fn joining_here() -> String {
    "Joining the session…".into()
}

/// The joiner's console once it holds E's state: it simulates on to the present, then votes.
pub fn imported_here() -> String {
    "Joined: catching up to the present.".into()
}

/// The joiner's status line in the window, from the join's start until its replica first reaches the present (the
/// import, then the catch-up; render.md 8.1's status line).
pub fn catching_up_status() -> String {
    "Joining: catching up…".into()
}

/// The joiner's console when its import failed: it quits.
pub fn couldnt_join_here(why: &str) -> String {
    format!("Couldn't join the session: {why}.")
}

/// The inviter's console, when that cut is final after the join had failed (app.md 4).
pub fn joined_after_all(m: MemberKey) -> String {
    format!("{} joined after all; changes are confirmed again.", capitalized(player(m)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pe_app_s_messages_round_trip_and_pe_agree_never_reads_them() {
        for m in
            [AppMsg::JoinReady, AppMsg::JoinStart(vec![1, 2, 3]), AppMsg::JoinStart(Vec::new()), AppMsg::JoinImported]
        {
            let bytes = m.encode();
            assert_eq!(AppMsg::decode(&bytes), Some(Some(m.clone())));
            // pe-agree's wire tags are single bytes from 1: the tag's first byte is none of them.
            assert!(pe_agree::codec::decode(&bytes).is_err(), "pe-agree reads {m:?}");
        }
        assert_eq!(AppMsg::decode(b"pe.app/\x09"), Some(None), "the tag, then no message this build knows");
        assert_eq!(AppMsg::decode(b"pe.app/\x01x"), Some(None), "JoinReady carries nothing");
        assert_eq!(AppMsg::decode(&[2, 0, 0]), None, "not pe-app's");
    }

    const A: MemberKey = MemberKey([0xa1; 32]);
    const B: MemberKey = MemberKey([0x0b; 32]);
    const LINE: LineId = LineId(pe_seams::Hash([9; 32]));
    const T0: HostTime = HostTime(1_000_000_000);

    /// A's inviter in epoch 0, A alone, with B's reply to its invite.
    fn inviter() -> (Inviter, Reply) {
        let pin = pe_net_crypto::identity::Pin { epoch: 0, members: pe_agree::core::members_hash(LINE, 0, &[A]).0 };
        let invite = Invite {
            id: [4; 8],
            key: A.0,
            expiry_unix_s: crate::net::unix_now() + 600,
            addrs: vec!["127.0.0.1:1".parse().unwrap()],
            pin: Some(pin),
        };
        let reply = Reply { invite_id: invite.id, key: B.0, addrs: vec!["127.0.0.1:2".parse().unwrap()] };
        let (_, replies) = std::sync::mpsc::channel();
        (Inviter::new(A, LINE, invite, replies, T_JOIN), reply)
    }

    /// The inviter, B's reply taken, B welcomed and ready: the `Admit` waits to be written.
    fn ready() -> Inviter {
        let (mut inv, reply) = inviter();
        inv.on_reply(reply.clone(), &[A], 0, T0);
        assert!(inv.on_join_request(B, &[A], 0, false, T0), "B's hello is the join's");
        inv.on_message(B, AppMsg::JoinReady, &[A], T0);
        assert_eq!(inv.join(), Some((B, Step::Ready, false)));
        assert_eq!(
            inv.take_acts(),
            vec![Act::TakeReply(reply), Act::Welcome(B), Act::ClockMembers(vec![B])],
            "dial B, welcome it, and count its ready clock"
        );
        inv
    }

    /// The path to the `Admit`: written once, with the joiner's role, and from then A's `Leave` waits for the join.
    #[test]
    fn the_inviter_writes_the_admit_once_b_is_ready_and_holds_its_leave_meanwhile() {
        let mut inv = ready();
        assert!(!inv.holds_leave(), "nothing admitted yet");
        assert!(inv.holds_script_commits(), "B is ready: a script commit now would race the Admit (app.md 4)");
        let entry = inv.take_entry(false, &[A], T0).expect("the Admit");
        assert_eq!(Admit::decode(&entry), Some(Some(Admit::new(B, Role::Joiner.perms()))));
        assert_eq!(inv.take_entry(false, &[A], T0), None, "taken once");
        inv.entry_written(Ok(()), &[A], T0);
        assert_eq!(inv.join(), Some((B, Step::Admitted, false)));
        assert!(inv.holds_leave(), "a join A admitted is in flight");
        assert_eq!(inv.take_notices(), vec![joining(B)]);
        // B's link closes before E: the join fails, the session is stranded, and the Leave may go.
        inv.on_link_down(B, &[A], T0);
        assert_eq!(inv.join(), Some((B, Step::Admitted, true)));
        assert!(!inv.holds_leave());
        assert!(inv.holds_script_commits(), "B may still join after all: script commits wait for the join's end");
        assert_eq!(inv.take_notices(), vec![join_failed(B)]);
    }

    /// E's start goes out only once this machine's state is verified at E, so it can serve E to the joiner: before
    /// that pe-diverge answers `NotHeld`, which failed the first two-process demo's join (its one source).
    #[test]
    fn the_start_waits_until_the_inviter_s_state_is_verified_at_e() {
        let mut inv = ready();
        inv.take_entry(false, &[A], T0).expect("the Admit");
        inv.entry_written(Ok(()), &[A], T0);
        let _ = (inv.take_acts(), inv.take_notices());
        let e = CutRef { line: LINE, height: 3, frame: pe_seams::Frame(18) };
        let at = AgreeAt { members: vec![B, A], epoch: 1, final_height: 3, final_cut: Some(e) };
        let start = || Some(vec![7u8; 10]);
        let before = CutRef { height: 2, frame: pe_seams::Frame(12), ..e };
        inv.tick(&at, Some(before), &start, T0);
        inv.tick(&at, None, &start, T0);
        assert_eq!(
            (inv.join(), inv.take_acts()),
            (Some((B, Step::Admitted, false)), vec![]),
            "E isn't verified here yet"
        );
        inv.tick(&at, Some(e), &start, T0);
        assert_eq!(inv.join(), Some((B, Step::Started, false)));
        assert_eq!(inv.take_acts(), vec![Act::Send(B, AppMsg::JoinStart(vec![7; 10]))]);
        // Script commits wait past E, until the new epoch's first final cut ends the join (app.md 4).
        assert!(inv.holds_script_commits(), "E is final, the join isn't over");
        let after = AgreeAt { members: vec![B, A], epoch: 1, final_height: 4, final_cut: None };
        inv.tick(&after, None, &start, T0);
        assert_eq!(inv.join(), None);
        assert!(!inv.holds_script_commits(), "B is in: script commits go ahead");
    }

    /// M1's limit (@coordinator-opus): a world with scripts refuses the join, saying so to both
    /// players, at B's hello and again in the tick that would write the `Admit`. Neither writes an `Admit`.
    #[test]
    fn a_world_with_scripts_refuses_the_join_at_the_hello_and_at_the_admit() {
        let refused =
            |acts: &[Act]| acts.iter().any(|a| matches!(a, Act::Refuse(k, r) if *k == B && r.detail.contains(SCRIPTS)));
        // At the hello, before any reply: B learns why, and so does A's console.
        let (mut inv, _) = inviter();
        assert!(inv.on_join_request(B, &[A], 0, true, T0));
        assert!(refused(&inv.take_acts()), "B is told why");
        assert_eq!(inv.take_notices(), vec![couldnt_join(B, SCRIPTS)]);
        assert_eq!(inv.join(), None);
        // A script committed after B was ready: the Admit's tick refuses it, and A's clock members are set back.
        let mut inv = ready();
        assert_eq!(inv.take_entry(true, &[A], T0), None, "no Admit");
        let acts = inv.take_acts();
        assert!(refused(&acts), "B is told why: {acts:?}");
        assert!(acts.contains(&Act::ClockMembers(Vec::new())), "B is no clock member any more: {acts:?}");
        assert_eq!(inv.take_notices(), vec![couldnt_join(B, SCRIPTS)]);
        assert_eq!((inv.join(), inv.holds_leave(), inv.holds_script_commits()), (None, false, false));
    }

    /// Only B's reply to this invite starts a join, one at a time, and nobody else's hello is taken meanwhile.
    #[test]
    fn a_reply_to_another_invite_a_member_s_or_a_second_join_is_refused() {
        let (mut inv, reply) = inviter();
        inv.on_reply(Reply { invite_id: [5; 8], ..reply.clone() }, &[A], 0, T0);
        inv.on_reply(Reply { key: A.0, ..reply.clone() }, &[A], 0, T0);
        inv.on_reply(reply.clone(), &[A], 1, T0);
        assert_eq!(inv.join(), None, "another invite's, this machine's own, and the pin's epoch gone");
        assert_eq!(inv.stats.replies_refused, 3);
        inv.on_reply(reply.clone(), &[A], 0, T0);
        assert_eq!(inv.join(), Some((B, Step::Dialing, false)));
        let c = MemberKey([0xcc; 32]);
        inv.on_reply(Reply { key: c.0, ..reply }, &[A], 0, T0);
        assert_eq!(inv.join(), Some((B, Step::Dialing, false)), "a second join waits");
        assert!(!inv.on_join_request(c, &[A], 0, false, T0), "C's hello is refused while B joins");
        assert_eq!(inv.stats.replies_refused, 4);
    }

    /// After B's join the invite's pin no longer holds (it pins epoch 0's members), so a third player's hello is
    /// refused at once, saying why to both, instead of held until pe-net's `T_welcome`; and its reply starts no join
    /// (@shader-lang's Low on d27af1c1).
    #[test]
    fn once_the_players_changed_a_third_player_s_hello_is_refused_saying_why() {
        let (mut inv, reply) = inviter();
        let c = MemberKey([0xcc; 32]);
        assert!(inv.on_join_request(c, &[A, B], 1, false, T0), "the inviter answers it itself");
        assert_eq!(inv.take_acts(), vec![Act::Refuse(c, refusal(PLAYERS_CHANGED))], "C is told why");
        assert_eq!(inv.take_notices(), vec![couldnt_join(c, PLAYERS_CHANGED)], "and so is A's console");
        inv.on_reply(Reply { key: c.0, ..reply }, &[A, B], 1, T0);
        assert_eq!((inv.join(), inv.stats.replies_refused), (None, 1), "its reply starts no join");
        // While the pin holds, an early hello is held for the reply that names it.
        let (mut inv, _) = inviter();
        assert!(inv.on_join_request(c, &[A], 0, false, T0));
        assert_eq!((inv.take_acts(), inv.take_notices()), (vec![], vec![]), "held, undecided");
    }
}

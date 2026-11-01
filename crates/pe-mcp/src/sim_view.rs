//! Views of one member's `WorldSim` as tool outputs, and the shared interaction path. Both backends
//! use these: the session simulator (`sim_backend`) and a live peer (`pe-app`), so a tool returns the
//! same shapes whichever drives it.

use pe_session::peer::SessionCore;
use pe_session::session::{CheckHash, EntryId, Outcome, Payload, SignedEntry, WorldSim};

use crate::backend::*;
use crate::model::*;

pub fn kind_name(k: u8) -> &'static str {
    match k {
        0 => "box",
        1 => "sphere",
        2 => "capsule",
        3 => "ball",
        _ => "hand",
    }
}

pub fn hex(h: u64) -> String {
    format!("{h:016x}")
}

/// A check hash (BLAKE3, 32 bytes) as 64 hex digits.
pub fn check_hex(h: &CheckHash) -> String {
    h.0.iter().map(|b| format!("{b:02x}")).collect()
}

fn summary(s: &WorldSim, i: u32) -> Option<ObjectSummary> {
    let o = s.live(i)?;
    let st = s.obj_state(i)?;
    Some(ObjectSummary { object: i, kind: kind_name(o.kind).into(), position: st.pos(), linear_velocity: st.lin() })
}

fn claim(s: &WorldSim, obj: u32) -> Option<ClaimInfo> {
    s.claim(obj).filter(|c| c.until > s.frame).map(|c| ClaimInfo { object: obj, member: c.member, until_frame: c.until })
}

pub fn state(s: &WorldSim, offset: u32, limit: u32) -> WorldState {
    let count = s.obj_slots();
    let end = offset.saturating_add(limit).min(count);
    let objects = (offset.min(count)..end).filter_map(|i| summary(s, i)).collect();
    let claims = s.claims().filter_map(|(o, _)| claim(s, o)).collect();
    WorldState {
        frame: s.frame,
        conf: s.conf,
        state_hash: s.frame.checked_sub(1).and_then(|f| s.hashes.get(&f)).map(|&h| hex(h)),
        object_count: count,
        objects,
        claims,
        next_offset: (end < count).then_some(end),
    }
}

pub fn object(s: &WorldSim, object: u32) -> ToolResult<ObjectState> {
    let unknown = || {
        ToolError::new(ErrorCode::UnknownObject, format!("no live object {object}"))
            .with_details(serde_json::json!({ "object_count": s.obj_slots() }))
    };
    let o = s.live(object).ok_or_else(unknown)?;
    let st = s.obj_state(object).ok_or_else(unknown)?;
    Ok(ObjectState {
        object,
        global_id: hex(o.global),
        kind: kind_name(o.kind).into(),
        position: st.pos(),
        rotation: st.rot(),
        linear_velocity: st.lin(),
        angular_velocity: st.ang(),
        state_hash: hex(st.hash()),
        claim: claim(s, object),
    })
}

/// Everything a peer checks before authoring an interaction for its member `me`.
pub fn check_interaction(s: &WorldSim, i: &Interaction, me: MemberId) -> ToolResult<()> {
    let (object, is_hand) = match *i {
        Interaction::Touch { object, .. } => (object, false),
        Interaction::Hand { object, .. } => (object, true),
    };
    let o = s.live(object).ok_or_else(|| {
        ToolError::new(ErrorCode::UnknownObject, format!("no live object {object}")).with_details(serde_json::json!({ "object_count": s.obj_slots() }))
    })?;
    match (is_hand, o.kind == 4) {
        (false, true) => return Err(ToolError::arg("action", format!("object {object} is a kinematic hand: use action \"hand\""))),
        (true, false) => return Err(ToolError::arg("action", format!("object {object} is not a hand ({})", kind_name(o.kind)))),
        _ => {}
    }
    // A hand is its member's alone (review of 358cfb7, finding 1). Every replica refuses another
    // member's hand entry (RejectedNotOwner, design 8.4); this checks the same map before authoring,
    // so the agent gets a clear error instead of a refused entry.
    if is_hand {
        let owner = s.hand_owners.get(&object).copied();
        if owner != Some(me) {
            let mine = s.hand_owners.iter().find(|&(_, &m)| m == me).map(|(&h, _)| h);
            let (o, y) = (owner.map_or("no member".into(), |m| format!("member {m}")), mine.map_or("none".into(), |h| format!("object {h}")));
            return Err(ToolError::new(ErrorCode::NotOwner, format!("object {object} is {o}'s hand; yours is {y}"))
                .with_details(serde_json::json!({ "owner": owner, "yours": mine })));
        }
    }
    if !s.caught_up() || s.frame < s.now {
        return Err(ToolError::new(ErrorCode::NotReady, "the driven peer is paused at its speculation limit or behind its clock; step and retry")
            .with_details(serde_json::json!({ "frame": s.frame, "now": s.now, "conf": s.conf, "window": s.cfg.window })));
    }
    Ok(())
}

/// An agent's hand target, held until a new target or `active: false` (D52 ruling (a), 2026-10-31).
/// A hand moves only while its author keeps sending (design 6.10: it follows a pose over its span,
/// coasts at most `hand_predict` frames, then holds), so the backend re-authors the target at every
/// pose the congestion stride allows, as a held controller would.
#[derive(Default)]
pub struct HandHold {
    target: Option<(u32, [i32; 3])>,
    /// The first pose sent for the current target: what `propose` returns.
    first: Option<SignedEntry>,
}

impl HandHold {
    pub fn set(&mut self, obj: u32, target_mm: [i32; 3]) {
        self.target = Some((obj, target_mm));
        self.first = None;
    }
    pub fn release(&mut self) {
        self.target = None;
        self.first = None;
    }
    /// Send the held pose if one is due at this frame (call it every tick); returns it.
    pub fn tick(&mut self, core: &mut SessionCore) -> Option<SignedEntry> {
        let (obj, target_mm) = self.target?;
        let e = core.author(Payload::Hand { obj, target_mm, active: true, span: 1, more: Vec::new() }, 0).ok()?;
        self.first.get_or_insert_with(|| e.clone());
        Some(e)
    }
    pub fn first(&self) -> Option<&SignedEntry> {
        self.first.as_ref()
    }
}

/// How long `propose` waits for the first pose of a new hand target: a pose slot comes at least
/// every 8 frames (R18's widest stride), so this only runs out on a stalled peer.
pub const HOLD_WAIT_MS: u64 = 1000;

/// Author a checked interaction at the present frame: the same `WorldSim` calls a player's input uses.
pub fn author(s: &mut WorldSim, i: Interaction) -> SignedEntry {
    match i {
        Interaction::Touch { object, impulse, grab } => s.author(Payload::Touch { obj: object, impulse, grab }, Some(object)),
        Interaction::Hand { object, target_mm, active } => s.author_ahead(Payload::Hand { obj: object, target_mm, active, span: 1, more: Vec::new() }, 0),
    }
}

pub fn status(s: &WorldSim, entry: EntryId, me: MemberId) -> Option<EntryStatus> {
    let e = s.entry(entry).filter(|e| e.member == me)?;
    let is_final = e.stamp < s.conf;
    let mut st = EntryStatus { stamp: e.stamp, state: InteractionState::Pending, reason: None, by_member: None, until_frame: None, is_final };
    if s.void_ids().contains(&entry) {
        st.state = InteractionState::Void;
        return Some(st);
    }
    let Some(&(_, outcome)) = s.outcomes.get(&entry) else { return Some(st) };
    let reject = |r: &str| (InteractionState::Rejected, Some(r.to_string()));
    let ((state, reason), by, until) = match outcome {
        Outcome::Accepted => ((InteractionState::Accepted, None), None, None),
        Outcome::RejectedClaimed { by, until } => (reject("claimed"), Some(by), Some(until)),
        Outcome::RejectedCooldown { until } => (reject("cooldown"), None, Some(until)),
        Outcome::RejectedGone => (reject("gone"), None, None),
        Outcome::RejectedPortalClosed => (reject("portal_closed"), None, None),
        Outcome::RejectedDuplicate => (reject("duplicate"), None, None),
        Outcome::RejectedNoProof => (reject("no_proof"), None, None),
        Outcome::RejectedInvalid => (reject("invalid"), None, None),
        Outcome::RejectedNoRoute => (reject("no_route"), None, None),
        Outcome::RejectedLate => (reject("late"), None, None),
        Outcome::RejectedLiveId => (reject("live_id"), None, None),
        Outcome::RejectedDeadline => (reject("deadline"), None, None),
        Outcome::RejectedNoProvenance => (reject("no_provenance"), None, None),
        Outcome::RejectedFork => (reject("fork"), None, None),
        Outcome::RejectedUnsettled => (reject("unsettled"), None, None),
        Outcome::RejectedPolicy => (reject("policy"), None, None),
        Outcome::RejectedNotOwner => (reject("not_owner"), None, None),
    };
    st.state = state;
    st.reason = reason;
    st.by_member = by;
    st.until_frame = until;
    Some(st)
}

pub fn counters(s: &WorldSim) -> Counters {
    Counters { rollbacks: s.stats.rollbacks, resim_frames: s.stats.resim_frames }
}

pub fn events_since(s: &WorldSim, n: usize) -> (Vec<String>, usize) {
    (s.events.get(n..).unwrap_or_default().to_vec(), s.events.len())
}

pub fn hashes(s: &WorldSim, from: u64, to: u64) -> Hashes {
    let fh = |(f, h): (&u64, &u64)| FrameHash { frame: *f, hash: hex(*h) };
    let ch = |(f, h): (&u64, &CheckHash)| FrameHash { frame: *f, hash: check_hex(h) };
    Hashes { conf: s.conf, frames: s.hashes.range(from..to).map(fh).collect(), checks: s.check_hashes.range(from..to).map(ch).collect() }
}

/// `members` is the session's genesis member list (evicted members stay listed, with their eviction).
pub fn members(s: &WorldSim, me: MemberId, members: &[MemberId]) -> Members {
    let ev = s.members_evicted();
    Members {
        me,
        epoch: s.membership().epoch(),
        members: members
            .iter()
            .map(|&m| MemberInfo { member: m, me: m == me, evicted: ev.get(&m).map(|&(cut_seq, from_frame)| Eviction { cut_seq, from_frame }) })
            .collect(),
        suspicions: s.stats.suspicions,
        demotions: s.stats.demotions,
        view_changes: s.stats.view_changes,
        resets: s.stats.resets,
        equivocations: s.stats.equivocations,
        lying_votes: s.stats.lying_votes,
        decisions: s
            .vcm
            .vc_log
            .iter()
            .map(|(epoch, frame, round, removed)| Decision { epoch: *epoch, frame: *frame, round: *round, removed: removed.clone() })
            .collect(),
    }
}

pub fn sync(s: &WorldSim) -> SyncStatus {
    SyncStatus {
        frame: s.frame,
        now: s.now,
        conf: s.conf,
        speculation: s.now.saturating_sub(s.conf),
        window: s.cfg.window,
        paused: s.frame < s.now,
        caught_up: s.caught_up(),
        checks_ok: s.stats.checks_ok,
        checks_bad: s.stats.checks_bad,
        latest_check: s.check_hashes.iter().next_back().map(|(f, h)| FrameHash { frame: *f, hash: check_hex(h) }),
        rollbacks: s.stats.rollbacks,
        max_rollback_depth: s.stats.max_depth,
        resim_frames: s.stats.resim_frames,
        paused_frames: s.stats.paused_frames,
        nonfinite: s.stats.nonfinite,
    }
}

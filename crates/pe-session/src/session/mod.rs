//! Session protocol core for the phase-1 spikes, revised after design review C1/C2/H4/H5 and the
//! user rulings of 2026-10-30: perturbations travel as **state + canonical frame**; same-frame
//! conflicts use a stable, non-grindable tie-break; programmable systems run locally and sync
//! only on perturbation; R14 cross-world transfers.
//!
//! * [`Entry`]: one perturbation (touch, hand target, transfer step, …). Its identity is the hash
//!   of its content, never `(member, seq)` alone. Each member's entries form a hash chain, so two
//!   different entries with the same `(member, seq)` are a transferable proof of equivocation.
//! * [`Ack`]: every member periodically states its *horizon* (it will author nothing stamped at
//!   or before it), the entries it holds (per author: contiguous seq and id), and the entries that
//!   reached it late. The **confirmed frame** is computed from acks only, so admission never
//!   depends on local arrival time (C1).
//! * [`WorldSim`]: one world's deterministic simulation with keyframe snapshots, rollback to the
//!   earliest changed frame, carried-state application, claims, and R14 escrow/arrival tables.
//!
//! Frame convention as in the rest of sim-core: the state at `frame` is the state at the *start*
//! of that frame; `hashes[f]` is the hash after frame `f` was stepped. `conf` is exclusive: every
//! frame `< conf` is confirmed and can never be rolled back.
//!
//! Layout (P2-3 S1, a structure-preserving port of the spike core at b01ebdf): this module holds
//! the state types and [`WorldSim`]; each submodule adds the methods for one concern:
//! * `entry`: entries, messages and their byte encoding; the same-frame tie rank.
//! * `membership`: membership: epochs, quorum rules, equivocation proofs and the view change (P2-4, design 10.8).
//! * `log`: the entry log: authoring, insertion, hash chains, validation, stable data and the confirmed frame.
//! * `evict`: suspicion, frozen heads, eviction votes and lying-vote checks (design 10.8, review rounds 2-3).
//! * `fetch`: fetch repair: requests for missing chain ranges, with backoff, and their answers.
//! * `check`: sync checks: comparing check hashes at confirmed, fully simulated frames.
//! * `r14`: r14 cross-world transfers: facts, attestations, certificates and P2-5 provenance.
//! * `sim`: scenes, keyframes and state hashes, frame application and stepping, rollback.
//! * `replay`: the offline reference: re-simulating a world from a final log.

use std::collections::{BTreeMap, BTreeSet};

use box3d_rust::body::{
    body_apply_linear_impulse_to_center, body_get_angular_velocity, body_get_linear_velocity,
    body_get_transform, body_set_angular_velocity, body_set_linear_velocity,
    body_set_target_transform, body_set_transform, create_body, destroy_body,
};
use box3d_rust::geometry::{Capsule, Sphere};
use box3d_rust::hull::make_box_hull;
use box3d_rust::id::BodyId;
use box3d_rust::math_functions::{Pos, Quat, Transform, Vec3, QUAT_IDENTITY};
use box3d_rust::recording::{
    deserialize_into_shell, hash_world_state, serialize_world, GeometryRegistry, RecBuffer,
    RegistrySlot,
};
use box3d_rust::shape::{create_capsule_shape, create_hull_shape, create_sphere_shape};
use box3d_rust::types::{default_body_def, default_shape_def, BodyType};
use box3d_rust::world::World;

use crate::{fnv_u64s, splitmix64, CLAIM_FRAMES, SUBSTEPS};
pub use crate::provenance::{
    birth_world, chain_hash, check_chain, fact_committed, fork_proof, ChainError, EpochBook, EpochCerts, Export, ForkProof, Hop, ImportPolicy, Seen,
};

mod entry;
mod membership;
mod log;
mod evict;
mod fetch;
mod check;
mod r14;
mod sim;
mod replay;

pub use entry::*;
pub use membership::*;
pub use r14::*;
pub use sim::*;
pub use replay::*;

// ---- rollback state ------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    pub member: MemberId,
    pub until: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscrowStatus {
    Pending,
    Committed,
    Returned,
    DepartRejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Escrow {
    pub obj: u32,
    pub record: Option<ObjRecord>,
    pub status: EscrowStatus,
    pub depart_frame: u64,
    pub depart: EntryId,
    pub dest: u32,
    /// A frame of `dest`: an arrival stamped later is refused there, and that certified refusal
    /// returns the object (review R3-6).
    pub deadline: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    Accepted { obj: u32, frame: u64 },
    /// `until` is the transfer's deadline: past it, any replay is late, so the entry can go.
    Rejected { frame: u64, until: u64 },
}

/// Everything that rolls back together. The Box3D world image covers bodies, contacts, islands
/// and id pools; the tables cover engine state.
#[derive(Clone, Default)]
pub struct Tables {
    pub objs: Vec<Option<Obj>>,
    pub claims: BTreeMap<u32, Claim>,
    /// After a claim expires, its holder may not claim the object again for CLAIM_COOLDOWN frames,
    /// so repeated impulses cannot monopolize an object (review M3).
    pub cooldown: BTreeMap<u32, (MemberId, u64)>,
    pub escrow: BTreeMap<u64, Escrow>,
    pub arrivals: BTreeMap<u64, Arrival>,
    /// P2-5: per global id, here (with its provenance) or the transfer that last took it away.
    /// Never pruned: a world's own last exit is what lets it refuse a forked chain.
    pub seen: BTreeMap<u64, Seen>,
    pub portal_open: bool,
    /// Active hands: frames since the hand's last entry (dead reckoning, `SimConfig::hand_predict`).
    pub hand_idle: BTreeMap<u32, u64>,
}

#[derive(Clone)]
struct Keyframe {
    image: Vec<u8>,
    t: Tables,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub rollbacks: u64,
    pub resim_frames: u64,
    pub max_depth: u64,
    pub resim_us: u64,
    pub step_us: u64,
    pub steps: u64,
    /// Keyframe saves and restores (counts and time), the worst single `advance` call, and the
    /// most image bytes the ring ever held.
    pub saves: u64,
    pub save_us: u64,
    pub restore_us: u64,
    pub max_advance_us: u64,
    pub advance_us: u64,
    pub max_ring_bytes: u64,
    pub overwrites: u64,
    pub paused_frames: u64,
    pub entries_rejected_violation: u64,
    pub equivocations: u64,
    pub checks_ok: u64,
    pub checks_bad: u64,
    /// Check frames at which some body held a non-finite position, rotation or velocity (review L2).
    pub nonfinite: u64,
    /// Eviction votes found to contradict their voter's own published references (review R3-1).
    pub lying_votes: u64,
    /// Members this replica presumed crashed and voted to remove (review R3-2).
    pub suspicions: u64,
    /// Members this replica voted to demote because their link could not keep up (review R3-9).
    pub demotions: u64,
    /// View changes this replica decided, those that changed no member (resets), and the highest
    /// round any of them needed (P2-4).
    pub view_changes: u64,
    pub resets: u64,
    pub max_round: u32,
    /// Members this replica voted out for withholding references (P2-4).
    pub withholders: u64,
    /// D80: kicks and role entries the decided changes applied, and those they refused.
    pub admin_applied: u64,
    pub admin_refused: u64,
}

/// Which world images a member keeps between the base and its present (design 6.7). Purely local:
/// images are a cache of states the log already determines, so no setting changes any result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapPolicy {
    /// A keyframe at every multiple of `k` frames (the 3d0f98a behaviour with k = 4).
    Every(u64),
    /// Dense near the present (every `k` frames for the newest `4k`), then thinned so the spacing
    /// grows with age: about two keyframes per doubling of age.
    Log(u64),
    /// The base (newest image at or before the confirmed frame) plus one candidate, saved at the
    /// first multiple of `k` after the confirmed frame, that becomes the next base.
    Base(u64),
    /// The base plus keyframes every `k` frames for the newest `4k` frames only: a rollback
    /// deeper than that re-simulates from the base.
    Window(u64),
}

impl SnapPolicy {
    pub fn label(&self) -> String {
        match self {
            SnapPolicy::Every(k) => format!("every{k}"),
            SnapPolicy::Log(k) => format!("log{k}"),
            SnapPolicy::Base(k) => format!("base{k}"),
            SnapPolicy::Window(k) => format!("window{k}"),
        }
    }
}

/// Keyframe spacing kept at `age` frames behind the present under `SnapPolicy::Log(k)`.
pub fn log_grid(k: u64, age: u64) -> u64 {
    let r = age / k;
    if r < 4 {
        k
    } else {
        k << (62 - r.leading_zeros() as u64)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SimConfig {
    pub dt: f32,
    /// Speculation limit: a member stops advancing when `frame - conf` reaches this (frames).
    pub window: u64,
    pub snap: SnapPolicy,
    /// How far an ack's horizon may lag its own stamp, in frames: bounds backdating (review H5,
    /// user rule 14:31Z). Replaces the arrival-based late allowance; nothing is re-stamped.
    pub horizon_slack: u64,
    pub seed: u64,
    pub check_every: u64,
    /// Remote-hand dead reckoning: an active hand with no entry for a frame keeps its last
    /// commanded velocity for up to this many frames, then holds. 0 = hold at once.
    pub hand_predict: u64,
    /// A member none of whose entries has arrived for this many frames is presumed crashed and
    /// voted out (review R3-2). 1.5x the speculation limit, so a congested member is not.
    pub suspect_after: u64,
    /// The view change's quorum rule (D80, open; the default is the recommendation).
    pub quorum: QuorumRule,
    /// View-change timeouts (frames): a round's steps time out after `vc_timeout + round *
    /// vc_timeout_step` frames, so rounds lengthen until messages arrive in time.
    pub vc_timeout: u64,
    pub vc_timeout_step: u64,
    /// Acks list the heads held of every other chain. Off only for membership timing at scale, where
    /// full heads cost n^3 memory across the simulated replicas: nothing then becomes stable.
    pub ack_heads: bool,
    /// How far ahead of its present a proposer puts a join's boundary frame (P2-4).
    pub join_lead: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            dt: 1.0 / 60.0,
            window: 120,
            snap: SnapPolicy::Every(4),
            horizon_slack: 2,
            seed: 0x5052_494D_4F52_4449,
            check_every: 30,
            hand_predict: 0,
            suspect_after: 180,
            quorum: QuorumRule::Bft,
            vc_timeout: 20,
            vc_timeout_step: 10,
            ack_heads: true,
            join_lead: 60,
        }
    }
}

/// Verdict of one entry when its frame was (re)simulated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    RejectedClaimed { by: MemberId, until: u64 },
    RejectedCooldown { until: u64 },
    RejectedGone,
    RejectedPortalClosed,
    RejectedDuplicate,
    /// R14: a relay without a valid certificate from the other world (reviews NEW-3, NEW-4).
    RejectedNoProof,
    /// R14: a certified record that is not a valid body state (review NEW-21).
    RejectedInvalid,
    /// R14: the other world is not on this world's agreed route list (reviews R3-4 to R3-6).
    RejectedNoRoute,
    /// R14: the arrival is stamped after the source's deadline (review R3-6).
    RejectedLate,
    /// R14: an object with this global id is already live here (review R3-5).
    RejectedLiveId,
    /// R14: the deadline is more than `MAX_DEADLINE_FRAMES` away (round 3, low).
    RejectedDeadline,
    /// P2-5: the chain does not prove the object's way from its birth into the source world.
    RejectedNoProvenance,
    /// P2-5: the object left this world by a transfer the arrival's chain does not contain.
    RejectedFork,
    /// P2-5: the object arrived here and the departure lacks its certified hop in.
    RejectedUnsettled,
    /// P25-1: this world's import policy refuses the object's birth world, chain or kind.
    RejectedPolicy,
    /// A `Hand` entry for a hand that is not its author's: a member moves only its own hands (P2-8
    /// review, 8.4).
    RejectedNotOwner,
}

/// One member's vote to remove another, as read from its chain (reviews R3-1, R3-2).
#[derive(Clone, Copy, Debug)]
struct VoteRec {
    /// The voter's frozen head of the target's chain.
    frozen: (u32, EntryId),
    /// The frame the vote was stamped at.
    stamp: u64,
    /// The vote entry's id: a view change names the votes it relies on.
    id: EntryId,
    /// The kick it answers (0: none, D80).
    kick: EntryId,
}

/// Result of checking a vote against its voter's own earlier acks (review R3-1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VoteCheck {
    Valid,
    /// The vote drops an entry this ack of the voter referenced.
    Lie(EntryId),
    /// An entry needed to tell is missing (it is fetched).
    Missing,
}

/// Steps of a view-change round (Tendermint's propose, prevote and precommit).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Step {
    #[default]
    Propose,
    Prevote,
    Precommit,
}

/// The view change of the current epoch as this replica runs it (P2-4, design 10.8): Tendermint's
/// algorithm for one decision (Buchman, Kwon and Milosevic 2018, algorithm 1), whose messages are
/// chain entries, so they are signed, ordered per member, fetched like any entry, and a member
/// that sends two different ones to two members has equivocated.
#[derive(Clone, Debug, Default)]
struct Vc {
    /// Clock frame at which this replica began the instance (round 0); None while idle.
    since: Option<u64>,
    round: u32,
    step: Step,
    /// Clock frame at which the current round began (the propose timeout runs from it).
    round_at: u64,
    proposed: bool,
    /// (value, round) this replica is locked on, and the latest value it saw a quorum prevote.
    locked: Option<(u64, u32)>,
    valid: Option<(u64, u32)>,
    /// Round -> (value, valid round) of the first proposal by that round's proposer, in chain order.
    proposals: BTreeMap<u32, (u64, u32)>,
    /// Every change proposed in this epoch, by value, including another version of an equivocating
    /// proposer's entry.
    changes: BTreeMap<u64, Change>,
    /// Round -> member -> value (0 = nil): each member's first prevote and first precommit.
    prevotes: BTreeMap<u32, BTreeMap<MemberId, u64>>,
    precommits: BTreeMap<u32, BTreeMap<MemberId, u64>>,
    /// Rounds whose proposal this replica already saw a quorum prevote.
    polka_done: BTreeSet<u32>,
    /// Armed timeouts: round -> clock frame armed.
    prevote_timer: BTreeMap<u32, u64>,
    precommit_timer: BTreeMap<u32, u64>,
}

pub struct WorldSim {
    pub world_id: u32,
    pub me: MemberId,
    pub members: BTreeSet<MemberId>,
    pub epoch: u32,
    pub cfg: SimConfig,
    pub world: World,
    pub t: Tables,
    pub frame: u64,
    registry: GeometryRegistry,
    slots: Vec<RegistrySlot>,
    slot_count: usize,
    ring: BTreeMap<u64, Keyframe>,
    /// Scratch world for canonical check hashes (review NEW-7).
    scratch: Option<World>,
    /// R14: the worlds this one trades objects with, and their members: its agreed route list
    /// (D46 trust config, part of the world's genesis; reviews R3-4 to R3-6). Any other world can
    /// neither receive its objects nor vouch for one.
    pub peers_of: BTreeMap<u32, Vec<MemberId>>,
    /// P2-5: epoch certificates of every world a provenance chain may pass through (static stub of
    /// P2-4's interface), and whether arrivals check provenance (off only for the control run).
    pub epochs: EpochBook,
    pub provenance: bool,
    /// Number of objects this world was born with: its birth records.
    pub genesis_objects: u64,
    /// P25-1: what this world accepts beyond identity (birth worlds, chain worlds, kinds).
    pub import_policy: ImportPolicy,
    /// Hands belong to members (a hand is part of its member's avatar, 16.3): hand object -> owner.
    /// A `Hand` entry by anyone else is refused (P2-8 review, 8.4). In the ball pit, hand k is the
    /// k-th genesis member's. Fixed for the world's life; not part of the rollback state.
    pub hand_owners: BTreeMap<u32, MemberId>,
    /// P25-3: double spends this world holds transferable proof of, from confirmed frames only
    /// (keyed by world, global id, entry and the two transfers). Not part of the state.
    pub fork_proofs: BTreeMap<(u32, u64, u64, u64, u64), ForkProof>,
    /// Cross-world effects of each simulated frame, kept outside the rollback state: a re-simulation
    /// of the frame replaces them, and they are acted on only once the frame is confirmed (P25-3,
    /// P25-4): (commit facts to attest, fork proofs found).
    frame_effects: BTreeMap<u64, (Vec<u64>, Vec<ForkProof>)>,
    /// Confirmed commit facts not yet attested (P25-4: each attested once).
    commits_due: Vec<u64>,
    attested: BTreeSet<u64>,
    // ---- log (round 2: chains, stability, chain-local verdicts)
    known: BTreeSet<MemberId>,
    entries: BTreeMap<EntryId, Entry>,
    /// The version of each (member, seq) this member holds and references: the first one seen.
    by_author: BTreeMap<(MemberId, u32), EntryId>,
    /// Per author: the verified contiguous prefix (hash-linked) held.
    contiguous: BTreeMap<MemberId, u32>,
    /// Void by a chain-local rule (horizon violations): identical for every holder.
    violations: BTreeSet<EntryId>,
    /// Withdrawn by an equivocation proof or an eviction cut.
    disputed: BTreeSet<EntryId>,
    eff: BTreeMap<EntryId, u64>,
    at_frame: BTreeMap<u64, BTreeSet<EntryId>>,
    /// Per author: (max horizon declared so far, last ack stamp) along its verified prefix.
    chain_state: BTreeMap<MemberId, (Option<u64>, u64)>,
    /// Per author: ack seq -> max horizon declared up to that seq (valid acks only).
    ack_hist: BTreeMap<MemberId, BTreeMap<u32, u64>>,
    /// (referrer, author) -> seq -> (id, the referrer's ack that said so): what each member's acks
    /// said it holds of each chain, up to the referrer's vote against that author (review R3-1).
    /// Never pruned in the spike; production keeps only the newest reference per pair, because
    /// consecutive references must extend each other [I].
    refs: BTreeMap<(MemberId, MemberId), BTreeMap<u32, (EntryId, EntryId)>>,
    /// Per author: the highest seq ever found stable. Stability is permanent (chains only grow), so
    /// a later conflicting reference can never shrink it (review R3-1).
    stable_hi: BTreeMap<MemberId, u32>,
    /// Per member: my present frame when a new entry of its chain last reached me (review R3-2).
    heard: BTreeMap<MemberId, u64>,
    /// Per member: my present frame since when its newest validated horizon has trailed my present
    /// by more than half the speculation limit (review R3-9, demotion).
    lagging: BTreeMap<MemberId, u64>,
    /// Equivocators: first disputed seq and the two contradicting entries (two versions of one seq,
    /// or an entry and a successor that names another version of it).
    proofs: BTreeMap<MemberId, (u32, EntryId, EntryId)>,
    /// Members proven faulty by a vote that contradicts their own earlier ack (ack, vote). Their
    /// chain is consistent, so nothing of it is withdrawn before the view change removes them.
    liars: BTreeMap<MemberId, Proof>,
    /// Heads this member froze when it voted (per epoch), or cuts of removed members.
    frozen: BTreeMap<MemberId, u32>,
    /// This epoch's votes: target -> voter -> the voter's first vote against target.
    votes: BTreeMap<MemberId, BTreeMap<MemberId, VoteRec>>,
    /// Evicted members: (cut seq, first frame they no longer count for, from the cut prefix held).
    evicted: BTreeMap<MemberId, (u32, u64)>,
    /// Evicted members: the id at the cut, naming which version of their chain counts.
    cut_ids: BTreeMap<MemberId, EntryId>,
    /// Evicted members whose prefix up to the cut this replica holds and has reconciled.
    cut_done: BTreeSet<MemberId>,
    /// The view change of the current epoch.
    vc: Vc,
    /// View-change entries (votes, proposals, prevotes, precommits) of later epochs, processed on
    /// entering their epoch.
    vc_later: BTreeMap<u32, Vec<EntryId>>,
    /// Every proposal entry seen, by value (answers `Msg::FetchValue`).
    proposal_ids: BTreeMap<u64, EntryId>,
    /// Decided changes, one per epoch boundary: (change, round, precommit signatures).
    decided: Vec<(Change, u32, Vec<(MemberId, u64)>)>,
    /// Decisions: (epoch reached, my clock frame when I decided, round, members removed).
    pub vc_log: Vec<(u32, u64, u32, Vec<MemberId>)>,
    /// Test fixture: author no vote, proposal, prevote or precommit (a member that withholds them).
    pub withhold_vc: bool,
    /// Non-members this replica sends its entries to: members that asked to join, or followers.
    pub followers: BTreeSet<MemberId>,
    /// Members that asked to join and are not admitted yet.
    join_requests: BTreeSet<MemberId>,
    /// Admitted members: the first frame each counts for.
    pub admitted: BTreeMap<MemberId, u64>,
    /// This epoch: members that precommitted a value adding members, at seq, with its boundary. Their
    /// later acks count only below the boundary, until they reach a later epoch (10.8).
    join_caps: BTreeMap<MemberId, (u32, u64)>,
    /// Per member: the first seq of its chain whose ack declares each epoch after 0.
    ack_epoch_from: BTreeMap<MemberId, BTreeMap<u32, u32>>,
    /// Test fixture (D80 bridge): heads this member's acks and votes report at most.
    hidden: Option<BTreeMap<MemberId, (u32, EntryId)>>,
    /// (referrer, author) -> my present frame when the referrer's acks last referenced more of that
    /// author's chain (P2-4: a member that stops referencing a chain stalls everyone's confirmation).
    ref_advanced: BTreeMap<(MemberId, MemberId), u64>,
    /// The last frame withheld references were checked for.
    withhold_checked: u64,
    /// Test fixture (D80 bridge): a faulty member that backs every vote another member casts,
    /// prevotes and precommits every proposal it sees, in every round, echoes the nil votes of
    /// rounds it holds no proposal for, never proposes, and never applies a decision, so it can
    /// back both sides of a partition in one epoch.
    pub byz_ally: bool,
    /// The genesis members (epoch 0), from which every later epoch's members follow.
    genesis: Vec<MemberId>,
    /// D80: each member's role. Every genesis member is an admin unless the session says otherwise.
    pub roles: BTreeMap<MemberId, RoleId>,
    /// D80: custom roles, id -> permission set. Admin (role 0) is built in and holds every permission.
    pub role_defs: BTreeMap<RoleId, u64>,
    /// D80: who may join, and with which role.
    pub join_rules: JoinRules,
    /// The relationships each pending joiner showed.
    join_attests: BTreeMap<MemberId, Vec<Attest>>,
    /// D80 (user 22:37Z): the world's owners, signed world metadata, shareable by several people;
    /// separate from the session's admins.
    pub owners: BTreeSet<MemberId>,
    /// This epoch's admin entries from members allowed to make them, as held.
    admin_pending: BTreeSet<EntryId>,
    /// Admin entries as the decided changes judged them: applied, or refused.
    pub admin_outcomes: BTreeMap<EntryId, bool>,
    /// D80: the session closed (no admin was left) from this frame on: no entry stamped at or after
    /// it acts.
    pub closed_at: Option<u64>,
    /// Test fixture: review fixes switched off (see [`fix`]).
    pub fixes_off: u32,
    /// Test fixture (review VC-5): the epoch this member's acks declare, whatever epoch it is in.
    pub fake_ack_epoch: Option<u32>,
    /// VC-5: per member, my present when its acks first declared an epoch newer than mine.
    newer_since: BTreeMap<MemberId, u64>,
    /// VC-2: a removed member's chain is being switched to its cut's branch; confirmation waits.
    adopting: bool,
    /// Values decided by a quorum of precommits that this replica holds no proposal for:
    /// (member to ask, value), and every pair asked so far.
    value_wants: Vec<(MemberId, u64)>,
    value_asked: BTreeSet<(MemberId, u64)>,
    wants: Vec<(MemberId, MemberId, u32, u32)>,
    pub conf: u64,
    /// My clock frame (my present), set by the harness each tick.
    pub now: u64,
    my_seq: u32,
    /// Test fixture (review H5): publish horizons this many frames behind the present.
    pub horizon_lag: u64,
    /// Test fixture (review R3-1): vote with a forged frozen head of (0, 0).
    pub lie_in_votes: bool,
    my_last: EntryId,
    /// (holder, author) -> (first seq asked, when, current retry interval), all in ms.
    fetch_asked: BTreeMap<(MemberId, MemberId), (u32, u64, u64)>,
    // ---- results
    pub hashes: BTreeMap<u64, u64>,
    pub check_hashes: BTreeMap<u64, u64>,
    remote_checks: BTreeMap<u64, BTreeMap<MemberId, u64>>,
    evaluated: BTreeSet<(u64, MemberId)>,
    pub outcomes: BTreeMap<EntryId, (u64, Outcome)>,
    pub watched: BTreeSet<u32>,
    /// (frame, obj) -> object state hash after that frame, for watched objects.
    pub obj_hist: BTreeMap<(u64, u32), u64>,
    dirty_from: Option<u64>,
    /// The present a running re-simulation is heading for (keyframe thinning during re-sim).
    resim_to: u64,
    /// Record every object's position after each frame (observer measurements).
    pub track: bool,
    pub pos_hist: BTreeMap<u64, Vec<[f32; 3]>>,
    pub stats: Stats,
    pub events: Vec<String>,
}

/// Bytes needed to store `b` as a patch on `a`: every `block`-byte block of `b` that differs from
/// the block at the same offset in `a` (8 bytes of offset each), plus any tail beyond `a`.
pub fn block_delta(a: &[u8], b: &[u8], block: usize) -> u64 {
    let n = a.len().min(b.len());
    let mut bytes = 0u64;
    let mut i = 0;
    while i < n {
        let e = (i + block).min(n);
        if a[i..e] != b[i..e] {
            bytes += (e - i) as u64 + 8;
        }
        i = e;
    }
    bytes + b.len().saturating_sub(n) as u64
}

pub fn now_us() -> u64 {
    // Monotonic microseconds since first call (review L6: never SystemTime for sim clocks).
    use std::sync::OnceLock;
    use std::time::Instant;
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_micros() as u64
}

impl WorldSim {
    pub fn new(world_id: u32, me: MemberId, members: &[MemberId], scene: SceneKind, cfg: SimConfig) -> Self {
        let (world, objs) = build_scene(scene, world_id);
        let genesis_objects = objs.len() as u64;
        let hand_owners = match scene {
            SceneKind::BallPit { balls, hands } => {
                let mut ms = members.to_vec();
                ms.sort_unstable();
                (0..hands).zip(ms).map(|(k, m)| ((balls + k) as u32, m)).collect()
            }
            _ => BTreeMap::new(),
        };
        WorldSim {
            world_id,
            me,
            members: members.iter().copied().collect(),
            epoch: 0,
            cfg,
            world,
            t: Tables { objs, portal_open: true, ..Default::default() },
            frame: 0,
            registry: GeometryRegistry::new(),
            slots: Vec::new(),
            slot_count: 0,
            ring: BTreeMap::new(),
            scratch: None,
            peers_of: BTreeMap::new(),
            epochs: EpochBook::default(),
            provenance: true,
            genesis_objects,
            import_policy: ImportPolicy::default(),
            hand_owners,
            fork_proofs: BTreeMap::new(),
            frame_effects: BTreeMap::new(),
            commits_due: Vec::new(),
            attested: BTreeSet::new(),
            known: members.iter().copied().chain([me]).collect(),
            entries: BTreeMap::new(),
            by_author: BTreeMap::new(),
            contiguous: BTreeMap::new(),
            violations: BTreeSet::new(),
            disputed: BTreeSet::new(),
            eff: BTreeMap::new(),
            at_frame: BTreeMap::new(),
            chain_state: BTreeMap::new(),
            ack_hist: BTreeMap::new(),
            refs: BTreeMap::new(),
            stable_hi: BTreeMap::new(),
            heard: BTreeMap::new(),
            lagging: BTreeMap::new(),
            proofs: BTreeMap::new(),
            liars: BTreeMap::new(),
            frozen: BTreeMap::new(),
            votes: BTreeMap::new(),
            evicted: BTreeMap::new(),
            cut_ids: BTreeMap::new(),
            cut_done: BTreeSet::new(),
            vc: Vc::default(),
            vc_later: BTreeMap::new(),
            proposal_ids: BTreeMap::new(),
            decided: Vec::new(),
            vc_log: Vec::new(),
            withhold_vc: false,
            byz_ally: false,
            hidden: None,
            ref_advanced: BTreeMap::new(),
            withhold_checked: 0,
            followers: BTreeSet::new(),
            join_requests: BTreeSet::new(),
            admitted: BTreeMap::new(),
            join_caps: BTreeMap::new(),
            ack_epoch_from: BTreeMap::new(),
            genesis: members.to_vec(),
            roles: members.iter().map(|&m| (m, ADMIN)).collect(),
            role_defs: BTreeMap::new(),
            join_rules: JoinRules::default(),
            join_attests: BTreeMap::new(),
            owners: BTreeSet::new(),
            admin_pending: BTreeSet::new(),
            admin_outcomes: BTreeMap::new(),
            closed_at: None,
            fixes_off: 0,
            fake_ack_epoch: None,
            newer_since: BTreeMap::new(),
            adopting: false,
            value_wants: Vec::new(),
            value_asked: BTreeSet::new(),
            wants: Vec::new(),
            conf: 0,
            now: 0,
            my_seq: 0,
            horizon_lag: 0,
            lie_in_votes: false,
            my_last: 0,
            fetch_asked: BTreeMap::new(),
            hashes: BTreeMap::new(),
            check_hashes: BTreeMap::new(),
            remote_checks: BTreeMap::new(),
            evaluated: BTreeSet::new(),
            outcomes: BTreeMap::new(),
            watched: BTreeSet::new(),
            obj_hist: BTreeMap::new(),
            dirty_from: None,
            resim_to: 0,
            track: false,
            pos_hist: BTreeMap::new(),
            stats: Stats::default(),
            events: Vec::new(),
        }
    }

    fn ev(&mut self, s: String) {
        self.events.push(format!("[w{} m{} f{} c{}] {s}", self.world_id, self.me, self.frame, self.conf));
    }
}

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

use pe_physics::{BodyDesc, BodyKind, Geometry, ObjectId, PhysicsConfig, PhysicsWorld, Pose, Push, ShapeDesc, WorldImage};

use pe_core::Frame;
use pe_sim::{SimStore, Snapshot};

use crate::ownership::{Book, Cert, Change as OwnChange, Insert, Key, OwnError, MAX_CERT_BYTES};
use crate::{fnv_u64s, splitmix64, CLAIM_FRAMES};
pub use crate::sign::{hash64, sig_words, test_fact_sig, test_join_sig, test_sign, Fact, FactKind, FactWriter, KeyRing, PublicKeys, SessionId, Sig, SignedEntry, TEST_SESSION};
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
mod store;
mod transfer;
mod replay;
mod seam;
#[cfg(test)]
mod ownership_tests;
#[cfg(test)]
mod bans_tests;

pub use entry::*;
pub use membership::*;
pub use seam::*;
pub use r14::*;
pub use sim::*;
pub use store::*;
pub use transfer::*;
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
    pub escrow: BTreeMap<u64, Escrow>,
    pub arrivals: BTreeMap<u64, Arrival>,
    /// P2-5: per global id, here (with its provenance) or the transfer that last took it away.
    /// Never pruned: a world's own last exit is what lets it refuse a forked chain.
    pub seen: BTreeMap<u64, Seen>,
    pub portal_open: bool,
}

#[derive(Clone)]
struct Keyframe {
    image: WorldImage,
    /// The per-object engine tables: shares the store's buffers copy-on-write.
    store: Snapshot,
    t: Tables,
}

/// A run waiting in [`WorldSim::insert_run`]: its sender, its entries (consecutive seqs of one
/// author; ids left out on the wire are `EntryId::NONE` until rebuilt), whether its first entry's
/// `prev` came on the wire, whether its acks' heads must be rebuilt, the top entry's signature if
/// it came signed, since when it waits (frame), and whether its signed, explicit form has been asked
/// for.
#[derive(Clone, Debug)]
pub struct LeanWait {
    pub from: MemberId,
    pub first_prev: bool,
    pub lean_heads: bool,
    pub entries: Vec<Entry>,
    pub sig: Option<Sig>,
    pub since: u64,
    pub asked: bool,
    /// The full range below it has been asked of everyone (the hole rule's fallback).
    pub asked_all: bool,
}

/// How a stored entry is authenticated (option 2): by its author's signature on it, or by the next
/// entry of the run it arrived in, which names it (and so on up to a signature).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Auth {
    Sig(Sig),
    Next(EntryId),
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
    /// Geometry-registry compactions (P22-12), their time, and the most blobs the registry held.
    pub compactions: u64,
    /// State transfers this replica restored (design 9.5).
    pub imports: u64,
    /// State transfers refused, and the last reason.
    pub transfers_refused: u64,
    pub last_refusal: Option<TransferError>,
    pub compact_us: u64,
    pub max_registry_len: u64,
    /// Check hashes computed and their cost: the physics state's (pe-physics `check_hash`: canonical
    /// save plus BLAKE3) and the rest (the store's serialization, the world tables and BLAKE3 over them).
    pub checks: u64,
    pub check_physics_us: u64,
    pub check_rest_us: u64,
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
    /// Received entries dropped because their signature did not verify (S3).
    pub bad_sigs: u64,
    /// Lean entries ([`Msg::Run`] with its lean flag) received, and those this member could not rebuild and verify,
    /// whose explicit form it fetched from their author instead.
    pub lean_in: u64,
    pub lean_fallbacks: u64,
    /// Lean entries that waited for an entry they name, and their total wait in frames.
    pub lean_waited: u64,
    pub lean_wait_frames: u64,
    /// The longest a lean entry waited, frames.
    pub lean_wait_max: u64,
    /// Fetch requests sent (gap repair, conflicts, lean entries).
    pub fetches_sent: u64,
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

/// When a member rebuilds its physics world's geometry registry from the images it still keeps (review
/// P22-12, `pe_physics::PhysicsWorld::compact_images`). Purely local and invisible to the simulation,
/// like [`SnapPolicy`]: compaction changes which blob indices images use, never a hashed or stepped value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactPolicy {
    Never,
    /// After a ring trim, once the registry holds at least `at` blobs and at least twice as many as
    /// right after the last compaction (so the cost, one restore and one save per kept keyframe, is
    /// amortized over the registry's growth).
    Grown { at: usize },
    /// After every ring trim (tests: the most compactions a run can have).
    EveryTrim,
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
    /// commanded velocity for up to this many frames, then holds. 0 = hold at once. The default is
    /// D52's 6 frames (design 6.10, a world rule); a run that tests the hold opts out.
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
    /// Which touches claim (8.4). `D72` is the rule; `Spike` keeps the spike's earlier rule only so
    /// the P2-3 S4 comparison can show the switch is the one change. Removed after P2-3's review.
    pub claim_rule: ClaimRule,
    pub compact: CompactPolicy,
}

/// See [`SimConfig::claim_rule`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimRule {
    /// D72: only grabs claim, and only a grab can be refused.
    D72,
    /// The spike: every touch claims its object for CLAIM_FRAMES (a grab renews a held claim), and
    /// any touch can be refused.
    Spike,
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
            hand_predict: 6,
            suspect_after: 180,
            quorum: QuorumRule::Bft,
            vc_timeout: 20,
            vc_timeout_step: 10,
            ack_heads: true,
            join_lead: 60,
            claim_rule: ClaimRule::D72,
            compact: CompactPolicy::Grown { at: 1024 },
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
    /// My present when I recorded it (review VCR-1: how long a kick vote has named no live kick).
    at: u64,
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
    /// (round, value, member) -> the signature over that precommit's fact (S3c). Keyed by value too
    /// (review P23-5): a proven forker's precommits for two values each keep their own signature.
    precommit_sigs: BTreeMap<(u32, u64, MemberId), Sig>,
    /// Rounds whose proposal this replica already saw a quorum prevote.
    polka_done: BTreeSet<u32>,
    /// Armed timeouts: round -> clock frame armed.
    prevote_timer: BTreeMap<u32, u64>,
    precommit_timer: BTreeMap<u32, u64>,
    /// Review VC3R-2: value -> my present when its proposal by the round's proposer first reached me.
    seen_at: BTreeMap<u64, u64>,
    /// Review VC4R-1: (round, value) -> every member a prevote for it is held from, in any version.
    any_prevotes: BTreeMap<(u32, u64), BTreeSet<MemberId>>,
    /// Review VC4R-1: (round, member) -> the id of that member's first prevote this replica holds, so a
    /// precommitter can name the prevote ids of the polka it saw.
    prevote_id: BTreeMap<(u32, MemberId), EntryId>,
}

pub struct WorldSim {
    pub world_id: u32,
    pub me: MemberId,
    pub cfg: SimConfig,
    /// The physics world (S2: through pe-physics).
    pub world: PhysicsWorld,
    /// S5b: the per-object engine tables (session/store.rs). Object index `i` is
    /// [`object_id`]`(i)`, here and in the physics world.
    pub store: SimStore,
    /// The world-level engine tables.
    pub t: Tables,
    pub frame: u64,
    ring: BTreeMap<u64, Keyframe>,
    /// Geometry blobs right after the last registry compaction (P22-12).
    compacted_len: usize,
    /// The canonical parts of each check frame's state not yet superseded by a newer confirmed one: what
    /// this replica can serve a joiner (design 9.5).
    served: BTreeMap<u64, StateTransfer>,
    /// The first frame this replica simulated itself: a state transfer put it there (0 otherwise).
    /// Entries stamped before it are in the transferred state; it never rolls back below it.
    floor: u64,
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
    frame_effects: BTreeMap<u64, (Vec<Fact>, Vec<ForkProof>)>,
    /// Confirmed commit facts not yet attested (P25-4: each attested once).
    commits_due: Vec<Fact>,
    attested: BTreeSet<Fact>,
    // ---- log (round 2: chains, stability, chain-local verdicts)
    known: BTreeSet<MemberId>,
    entries: BTreeMap<EntryId, Entry>,
    /// The newest stamp of any entry held other than an ack (design 7.6's idle predicate): void
    /// ones, fork versions, own and protocol entries included; raised at insert, as the spike's.
    busy_stamp: Option<u64>,
    /// The version of each (member, seq) this member holds and references: the first one seen.
    by_author: BTreeMap<(MemberId, u32), EntryId>,
    /// Lean entries waiting for entries they name ([`WorldSim::insert_lean`]), by (member, seq).
    lean_wait: BTreeMap<(MemberId, u32), LeanWait>,
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
    /// Per author: ack seq -> that valid ack's stamp. With `ack_hist` and `ack_epoch_from`, the chain
    /// state at any seq of the validated prefix, from which a listed admin entry on another branch is
    /// judged (review VC7R-1).
    ack_stamps: BTreeMap<MemberId, BTreeMap<u32, u64>>,
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
    /// Review PF-1 and VC7R-6: proposal entries of later epochs kept by value before their epoch began
    /// (another version, or a proven forker's past its fork), noted on entering it.
    later_proposals: BTreeMap<u32, Vec<EntryId>>,
    /// Review PF-1: every signed precommit held for a value, any version, linked into its author's
    /// chain or not: (epoch, round, value) to the members that signed one. Only proven members are
    /// counted from it (`precommitters`); everyone else counts in chain order.
    pc_held: BTreeMap<(u32, u32, u64), BTreeMap<MemberId, Sig>>,
    /// Review VC8R-1: every signed prevote held for a value, any version, linked or not, as `pc_held`
    /// is for precommits. Only proven members are counted from it (`prevoters`).
    pv_held: BTreeMap<(u32, u32, u64), BTreeSet<MemberId>>,
    /// Review VC4R-2: per pending joiner, its signed request (signature, expiry) with the latest expiry.
    join_reqs: BTreeMap<MemberId, (Sig, u64)>,
    /// Test fixture (D80 bridge): heads this member's acks and votes report at most.
    hidden: Option<BTreeMap<MemberId, (u32, EntryId)>>,
    /// The genesis members (epoch 0), from which every later epoch's members follow.
    genesis: Vec<MemberId>,
    /// V3 (user 08:31Z): the members the close removed, every one left; everyone is told (the
    /// `SESSION CLOSED` event) and no further change is made to the world. The spike keeps its member
    /// list for its own bookkeeping and records the removal here; pe-session empties the list.
    pub closed_members: Vec<MemberId>,
    /// Test fixture: review fixes switched off (see [`fix`]).
    pub fixes_off: u64,
    /// Review VC4R-4: (holder, id) -> (when last asked, current retry interval), in ms.
    entry_backoff: BTreeMap<(MemberId, EntryId), (u64, u64)>,
    /// Review VC4R-3 (and the generator's RG-1): (claimant, author) -> (the highest seq of the author's
    /// chain the claimant has referenced that I hold in the named version, and my present when that last
    /// rose). A member faking a reference advances its raw reference but not this verified one, so the
    /// withholding rule catches it; a real backlog clears once I fetch the entries, advancing it.
    ref_seen: BTreeMap<(MemberId, MemberId), (u32, u64)>,
    /// Review VC4R-3: per member suspected silent or slow, my present when it first was, so the fetch
    /// exemption that spares it while I chase another member's claim about it is bounded in time.
    suspect_since: BTreeMap<MemberId, u64>,
    /// Review VC3R-2: per member, its first capped precommit of this epoch: (round, value, my present
    /// when I recorded it).
    cap_seen: BTreeMap<MemberId, (u32, u64, u64)>,
    /// Review VC4R-1: the prevote ids a member's capped precommit carried as its polka, and every id of
    /// them asked for so far, so the justification is fetched once before the member is voted out.
    cap_polka: BTreeMap<MemberId, Vec<EntryId>>,
    /// Review VC3R-6: the epoch each member entered the session in (genesis members: 0).
    admitted_epoch: BTreeMap<MemberId, u32>,
    /// Review VC3R-2: per member, until when I cast no suspicion vote against it, because decisions
    /// answered my last ones while no other member voted with me; and how many times in a row.
    vote_hold: BTreeMap<MemberId, u64>,
    hold_n: BTreeMap<MemberId, u32>,
    /// Test fixture (round 4): this member's acks claim `n` more entries of `member`'s chain than it
    /// holds, with an id nobody signed: (member, n).
    pub fake_refs: Option<(MemberId, u32)>,
    /// Test fixture (review VC3R-3): the change this member proposes in every round it is the
    /// proposer of, whatever it holds.
    pub byz_change: Option<Change>,
    /// Test fixture (review VC3R-6): author no ack before this member is admitted, as a joiner that
    /// starts from a state transfer rather than replaying the log would.
    pub quiet_until_member: bool,
    wants: Vec<(MemberId, MemberId, u32, u32, u8)>,
    pub conf: u64,
    /// My clock frame (my present), set by the harness each tick.
    pub now: u64,
    my_seq: u32,
    /// Test fixture (review H5): publish horizons this many frames behind the present.
    pub horizon_lag: u64,
    /// D73(a): this member's own hand poses waiting for their pack, by (frame, hand): applied at their
    /// frames on this replica only (`step_frame`). Set by `SessionCore`.
    pub own_pending: BTreeMap<(u64, u32), [i32; 3]>,
    /// D73(a): while a pack is pending, its first frame: this member's acks declare a horizon below
    /// it, so nothing it confirms can miss the pack.
    pub hold_horizon_below: Option<u64>,
    /// 7.6 reduction 1: the check frame this member's last ack carried.
    last_check_sent: Option<u64>,
    /// Test fixture (review R3-1): vote with a forged frozen head of (0, 0).
    pub lie_in_votes: bool,
    my_last: EntryId,
    /// (holder, author) -> (first seq asked, when, current retry interval), all in ms.
    fetch_asked: BTreeMap<(MemberId, MemberId), (u32, u64, u64)>,
    // ---- results
    pub hashes: BTreeMap<u64, u64>,
    pub check_hashes: BTreeMap<u64, CheckHash>,
    remote_checks: BTreeMap<u64, BTreeMap<MemberId, CheckHash>>,
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
    /// This replica's signing key and the members' public keys (S3).
    keys: KeyRing,
    /// The author's signature of each entry held signed on itself, for relaying it (option 2: an
    /// entry that came inside a run is covered by the next entry of the run instead: `run_next`).
    sigs: BTreeMap<EntryId, Sig>,
    run_next: BTreeMap<EntryId, EntryId>,
    /// The R18 below-20 item: option 1, own live entries lean (implicit ids) and fetch replies lean
    /// when the requester holds the entry below; option 2, own live entries in runs signed once, and
    /// hole-fill replies unsigned. Off: the explicit, signed-per-entry wire (measurement A/B).
    pub lean_wire: bool,
    pub run_wire: bool,
    /// The membership and view-change state (P2-3 S6b): read through [`Membership`], written by
    /// the view change.
    pub vcm: ViewChangeMembership,
    /// M1: a fixed member list; when set, the core reads it instead of `vcm` and the view change is off.
    static_ms: Option<StaticMembership>,
}

/// Everything the view change (10.8, P2-4) and the D80 admin layer keep: members, epochs, votes,
/// cuts, joins, roles. The protocol core reads it through [`Membership`]; the view change
/// (membership.rs, evict.rs) writes its fields. A plain struct the view change extends.
#[derive(Clone)]
pub struct ViewChangeMembership {
    pub members: BTreeSet<MemberId>,
    pub epoch: u32,
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
    decided: Vec<(Change, u32, Vec<(MemberId, Sig)>)>,
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
    /// Test fixture (review VCR-7): with `byz_ally`, prevote every proposal but never precommit.
    pub byz_no_precommit: bool,
    /// D80: each member's role. Every genesis member is an admin unless the session says otherwise.
    pub roles: BTreeMap<MemberId, RoleId>,
    /// D80: custom roles, id -> permission set. Admin (role 0) is built in and holds every permission.
    pub role_defs: BTreeMap<RoleId, u64>,
    /// D80: who may join, and with which role.
    pub join_rules: JoinRules,
    /// The relationships each pending joiner showed.
    join_attests: BTreeMap<MemberId, Vec<Attest>>,
    /// D80 (user 22:37Z): the world's owners, signed world metadata, shareable by several people;
    /// separate from the session's admins. A world without an ownership chain (`book`) lists them
    /// here; one with a chain takes them from it.
    pub owners: BTreeSet<MemberId>,
    /// World ownership (12.5): the world's ownership chain. Governance, like roles: changed only by
    /// the certificates decided view changes commit, never rewound by a rollback.
    pub book: Option<Book>,
    /// 12.4: this session's bans and rejoin roles, by identity key; never written into the world.
    /// Governance, like roles: changed only by decided admin entries, never rewound by a rollback,
    /// outside the check hash.
    pub bans: BTreeMap<Key, Restrict>,
    /// 12.4: the world's perma-bans (its ACL), by identity key; governance like `bans`.
    pub perma_bans: BTreeSet<Key>,
    /// This epoch's admin entries from members allowed to make them, as held.
    admin_pending: BTreeSet<EntryId>,
    /// Review OWNY-2: ownership entries not pending only because a parent certificate is not in the
    /// book yet (built on a pending one's head); judged again after each decision.
    own_held: BTreeSet<EntryId>,
    /// Admin entries as the decided changes judged them: applied, or refused.
    pub admin_outcomes: BTreeMap<EntryId, bool>,
    /// D80: the session closed (no admin was left) from this frame on: no entry stamped at or after
    /// it acts.
    pub closed_at: Option<u64>,
    /// Test fixture (review VC-5): the epoch this member's acks declare, whatever epoch it is in.
    pub fake_ack_epoch: Option<u32>,
    /// VC-5: per member, my present when its acks first declared an epoch newer than mine.
    newer_since: BTreeMap<MemberId, u64>,
    /// VC-2: a removed member's chain is being switched to its cut's branch; confirmation waits.
    adopting: bool,
    /// Review VCR-4: per member, the epochs reached by resets that overruled one of its votes or caps.
    pub strikes: BTreeMap<MemberId, Vec<u32>>,
    /// Review VCR-8: admin entries a decided change lists and this replica lacks: (member to ask, id,
    /// how many predecessors too), and every pair asked so far.
    entry_wants: Vec<(MemberId, EntryId, u8)>,
    entry_asked: BTreeSet<(MemberId, EntryId)>,
    /// Values decided by a quorum of precommits that this replica holds no proposal for:
    /// (member to ask, value), and every pair asked so far.
    value_wants: Vec<(MemberId, u64)>,
    value_asked: BTreeSet<(MemberId, u64)>,
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
    /// Whether frame `f`'s outcomes are final here (review P23-7): confirmed, simulated, and no
    /// re-simulation pending from `f` or earlier.
    pub fn settled(&self, f: u64) -> bool {
        f < self.conf && f < self.frame && self.dirty_from.is_none_or(|d| d > f)
    }

    pub fn new(world_id: u32, me: MemberId, members: &[MemberId], scene: SceneKind, cfg: SimConfig, mut keys: KeyRing) -> Self {
        assert!(!members.contains(&LOGIC), "member id 0 is reserved for world logic (review P23-10)");
        // This world's facts are signed in its own session (review P23-2). A ring without one (an
        // offline replay's) keeps what its directory says.
        if keys.session != [0; 32] {
            keys.public.add_world(world_id, keys.session);
        }
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
            static_ms: None,
            vcm: ViewChangeMembership {
                members: members.iter().copied().collect(),
                epoch: 0,
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
                byz_no_precommit: false,
                ref_advanced: BTreeMap::new(),
                withhold_checked: 0,
                followers: BTreeSet::new(),
                join_requests: BTreeSet::new(),
                admitted: BTreeMap::new(),
                join_caps: BTreeMap::new(),
                ack_epoch_from: BTreeMap::new(),
                roles: members.iter().map(|&m| (m, ADMIN)).collect(),
                role_defs: BTreeMap::new(),
                join_rules: JoinRules::default(),
                join_attests: BTreeMap::new(),
                owners: BTreeSet::new(),
                book: None,
                bans: BTreeMap::new(),
                perma_bans: BTreeSet::new(),
                admin_pending: BTreeSet::new(),
                own_held: BTreeSet::new(),
                admin_outcomes: BTreeMap::new(),
                closed_at: None,
                fake_ack_epoch: None,
                newer_since: BTreeMap::new(),
                adopting: false,
                strikes: BTreeMap::new(),
                entry_wants: Vec::new(),
                entry_asked: BTreeSet::new(),
                value_wants: Vec::new(),
                value_asked: BTreeSet::new(),
            },
            cfg,
            world,
            store: object_store(&objs),
            t: Tables { portal_open: true, ..Default::default() },
            frame: 0,
            ring: BTreeMap::new(),
            compacted_len: 0,
            served: BTreeMap::new(),
            floor: 0,
            peers_of: BTreeMap::new(),
            epochs: EpochBook { keys: keys.public.clone(), ..EpochBook::default() },
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
            busy_stamp: None,
            by_author: BTreeMap::new(),
            lean_wait: BTreeMap::new(),
            contiguous: BTreeMap::new(),
            violations: BTreeSet::new(),
            disputed: BTreeSet::new(),
            eff: BTreeMap::new(),
            at_frame: BTreeMap::new(),
            chain_state: BTreeMap::new(),
            ack_hist: BTreeMap::new(),
            ack_stamps: BTreeMap::new(),
            refs: BTreeMap::new(),
            stable_hi: BTreeMap::new(),
            heard: BTreeMap::new(),
            later_proposals: BTreeMap::new(),
            pc_held: BTreeMap::new(),
            pv_held: BTreeMap::new(),
            hidden: None,
            join_reqs: BTreeMap::new(),
            genesis: members.to_vec(),
            closed_members: Vec::new(),
            fixes_off: 0,
            entry_backoff: BTreeMap::new(),
            ref_seen: BTreeMap::new(),
            suspect_since: BTreeMap::new(),
            cap_seen: BTreeMap::new(),
            cap_polka: BTreeMap::new(),
            admitted_epoch: BTreeMap::new(),
            vote_hold: BTreeMap::new(),
            hold_n: BTreeMap::new(),
            fake_refs: None,
            byz_change: None,
            quiet_until_member: false,
            wants: Vec::new(),
            conf: 0,
            now: 0,
            my_seq: 0,
            horizon_lag: 0,
            own_pending: BTreeMap::new(),
            hold_horizon_below: None,
            last_check_sent: None,
            lie_in_votes: false,
            my_last: EntryId::NONE,
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
            keys,
            sigs: BTreeMap::new(),
            run_next: BTreeMap::new(),
            lean_wire: true,
            run_wire: true,
        }
    }

    fn ev(&mut self, s: String) {
        self.events.push(format!("[w{} m{} f{} c{}] {s}", self.world_id, self.me, self.frame, self.conf));
    }
}

#[cfg(test)]
mod d73_review {
    use super::*;
    use crate::sign::test_sign;

    /// Review D73-1 (@design-reviewer's probe): an admin entry stamped within the wider ack lag but
    /// more than horizon_slack before its author's last ack stamp is void where linked (`validate`),
    /// and `valid_on_own_branch` (change_valid, VC7R-1) agrees.
    #[test]
    fn admin_backdating_bound_agrees_on_both_paths() {
        let mut s = WorldSim::new(1, 1, &[1, 2, 3], SceneKind::Empty, SimConfig::default(), KeyRing::test(1));
        let a = Entry { member: 2, seq: 1, prev: EntryId::NONE, stamp: 20, payload: Payload::Ack { horizon: 12, heads: vec![], check: None, epoch: 0 }, carried: None };
        let sa = test_sign(1, a);
        let k = Entry { member: 2, seq: 2, prev: sa.id(), stamp: 14, payload: Payload::Kick { epoch: 0, target: 3, rejoin: Rejoin::Free }, carried: None };
        let sk = test_sign(1, k);
        let kid = sk.id();
        s.insert(sa);
        s.insert(sk);
        assert!(s.violations.contains(&kid), "void where linked");
        assert_eq!(s.valid_on_own_branch(kid), Some(false), "void on its own branch too");
    }
}

#[cfg(test)]
mod v3_review {
    use super::*;
    use crate::sign::test_sign;

    /// Review V3-1 (@design-reviewer's probe): after a close empties `vcm.members`, a former member's
    /// two versions of a current-epoch proposal reach `note_proposal`; `proposer()` must not index an
    /// empty order (it panicked, "remainder with a divisor of zero").
    #[test]
    fn a_proposal_after_a_close_does_not_panic() {
        let mut s = WorldSim::new(1, 1, &[1, 2, 3], SceneKind::Empty, SimConfig::default(), KeyRing::test(1));
        // What a close leaves (membership.rs apply_change, V3): no members.
        s.closed_members = s.vcm.members.iter().copied().collect();
        s.vcm.members.clear();
        s.vcm.closed_at = Some(10);
        let ep = s.vcm.epoch;
        let mk = |b: u64| test_sign(1, Entry { member: 2, seq: 1, prev: EntryId::NONE, stamp: 20, payload: Payload::Propose { epoch: ep, round: 0, valid_round: 0, change: Change { epoch: ep, boundary: b, ..Default::default() } }, carried: None });
        s.insert(mk(30));
        s.insert(mk(31));
        assert_eq!(s.proposer(ep, 0), LOGIC, "no one proposes in a closed session");
    }
}

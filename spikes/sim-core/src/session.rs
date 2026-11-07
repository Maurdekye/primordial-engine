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

use std::collections::{BTreeMap, BTreeSet};

use box3d_rust::body::{
    body_apply_linear_impulse_to_center, body_get_angular_velocity, body_get_linear_velocity,
    body_get_transform, body_reset_history, body_set_angular_velocity, body_set_awake,
    body_set_linear_velocity, body_set_target_transform, body_set_transform, create_body, destroy_body,
};
use box3d_rust::geometry::{Capsule, Sphere};
use box3d_rust::hull::make_box_hull;
use box3d_rust::id::BodyId;
use box3d_rust::math_functions::{Pos, Quat, Vec3, WorldTransform, QUAT_IDENTITY};
use box3d_rust::recording::{
    deserialize_into_shell, hash_world_state, serialize_world, GeometryRegistry, RecBuffer,
    RegistrySlot,
};
use box3d_rust::shape::{create_capsule_shape, create_hull_shape, create_sphere_shape};
use box3d_rust::types::{default_body_def, default_shape_def, BodyType};
use box3d_rust::world::World;

use crate::ownership::{Book, Cert, Change as OwnChange, Insert, Key, OwnError, MAX_CERT_WORDS};

mod feed;
mod localize;
pub use feed::FeedBody;
pub use localize::{CutRecord, WindowVals, WINDOW};
use crate::{fnv_u64s, pos, splitmix64, CLAIM_FRAMES, SUBSTEPS};
pub use crate::provenance::{
    birth_world, chain_hash, check_chain, fact_committed, fork_proof, ChainError, EpochBook, EpochCerts, EpochProof, Export, ForkProof, Hop, ImportPolicy, Seen,
};

pub type MemberId = u32;
pub type EntryId = u64;
/// Member 0 is world logic: its actions are computed identically on every peer, never networked,
/// and never take claims (review M3).
pub const LOGIC: MemberId = 0;

/// D73(a): the most targets one hand entry packs, the largest R18 hand stride. It also bounds how far a
/// pack's stamp may lie behind its author's present (k - 1 frames).
pub const PACK_MAX: u64 = 8;

impl Payload {
    /// The frames after its stamp that an entry also acts on: a hand pack's extra targets (D73(a)).
    pub fn extra_frames(&self) -> u64 {
        match self {
            Payload::Hand { more, .. } => more.len() as u64,
            _ => 0,
        }
    }
}

// ---- object state ------------------------------------------------------------------------------

/// Exact bit pattern of a rigid body's sim state: position, rotation, linear and angular velocity.
/// 16 words (D58): the position as three f64, each two words, low word first; then the rotation
/// and both velocities as f32 bits. An f32 build widens its positions exactly, so it keeps its bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyState(pub [u32; 16]);

#[allow(clippy::unnecessary_cast)] // Pos is f64 under double-precision, f32 without it
impl BodyState {
    pub fn read(world: &World, id: BodyId) -> Self {
        let t = body_get_transform(world, id);
        let v = body_get_linear_velocity(world, id);
        let w = body_get_angular_velocity(world, id);
        let p = |x: f64| [x.to_bits() as u32, (x.to_bits() >> 32) as u32];
        let [x, y, z] = [p(t.p.x as f64), p(t.p.y as f64), p(t.p.z as f64)];
        BodyState([
            x[0], x[1], y[0], y[1], z[0], z[1],
            t.q.v.x.to_bits(), t.q.v.y.to_bits(), t.q.v.z.to_bits(), t.q.s.to_bits(),
            v.x.to_bits(), v.y.to_bits(), v.z.to_bits(),
            w.x.to_bits(), w.y.to_bits(), w.z.to_bits(),
        ])
    }
    /// A state from its parts: position, rotation (x, y, z, s), linear and angular velocity.
    pub fn new(p: [f64; 3], q: [f32; 4], v: [f32; 3], w: [f32; 3]) -> Self {
        let mut s = [0u32; 16];
        for (k, x) in p.into_iter().enumerate() {
            s[2 * k] = x.to_bits() as u32;
            s[2 * k + 1] = (x.to_bits() >> 32) as u32;
        }
        for (k, x) in q.into_iter().chain(v).chain(w).enumerate() {
            s[6 + k] = x.to_bits();
        }
        BodyState(s)
    }
    /// This state at another position.
    pub fn with_pos(&self, p: [f64; 3]) -> Self {
        let mut s = Self::new(p, [0.0; 4], [0.0; 3], [0.0; 3]);
        s.0[6..].copy_from_slice(&self.0[6..]);
        s
    }
    pub fn pos_f64(&self) -> [f64; 3] {
        [self.d(0), self.d(2), self.d(4)]
    }
    /// Every component finite (no NaN or infinity).
    pub fn is_finite(&self) -> bool {
        self.pos_f64().iter().all(|x| x.is_finite()) && self.0[6..].iter().all(|&b| f32::from_bits(b).is_finite())
    }
    fn f(&self, i: usize) -> f32 {
        f32::from_bits(self.0[i])
    }
    fn d(&self, i: usize) -> f64 {
        f64::from_bits(self.0[i] as u64 | (self.0[i + 1] as u64) << 32)
    }
    pub fn pos(&self) -> Pos {
        Pos { x: self.d(0) as _, y: self.d(2) as _, z: self.d(4) as _ }
    }
    pub fn rot(&self) -> Quat {
        Quat { v: Vec3 { x: self.f(6), y: self.f(7), z: self.f(8) }, s: self.f(9) }
    }
    pub fn lin(&self) -> Vec3 {
        Vec3 { x: self.f(10), y: self.f(11), z: self.f(12) }
    }
    pub fn ang(&self) -> Vec3 {
        Vec3 { x: self.f(13), y: self.f(14), z: self.f(15) }
    }
    /// Overwrite the body's state with these exact bits, awake, and forget its history (design
    /// 7.3, as pe-physics' `set_state` does): the fork's `body_reset_history` destroys its contacts
    /// (recycled manifolds and warm starts), zeroes its joints' impulses and its sleep timer, so its
    /// next step depends only on its state and the rest of the world.
    pub fn write(&self, world: &mut World, id: BodyId) {
        body_set_transform(world, id, self.pos(), self.rot());
        body_set_linear_velocity(world, id, self.lin());
        body_set_angular_velocity(world, id, self.ang());
        body_set_awake(world, id, true);
        body_reset_history(world, id);
    }
    pub fn hash(&self) -> u64 {
        fnv_u64s(self.0.iter().map(|&b| b as u64))
    }
}

/// Everything another world needs to recreate an object (R14). `kind` selects the shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjRecord {
    pub global: u64,
    pub kind: u8,
    pub state: BodyState,
    pub src_world: u32,
    pub src_frame: u64,
    /// P2-5: the transfer that brought the object into `src_world` (0: born there).
    pub via: u64,
}

// ---- entries, acks, messages ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// Impulse on an object. `grab` = a held grab, which renews its claim; a plain impulse claims
    /// once and never renews (review M3).
    Touch { obj: u32, impulse: [f32; 3], grab: bool },
    /// Kinematic hand target, in millimetres (quantized so every peer sees the same bits), to be
    /// reached `span` frames after the stamp (1 normally). `active = false` parks the hand.
    /// D73(a): `more` packs the targets of the frames after the stamp, one per frame (at most
    /// [`PACK_MAX`] targets in all), so an author thinned by R18 to one entry every k frames still
    /// gives every frame its own target: target j applies at stamp + j with a span of 1, the last
    /// with `span`. Empty for an unpacked entry (wire tag 2; a pack is tag 21).
    Hand { obj: u32, target_mm: [i32; 3], active: bool, span: u8, more: Vec<[i32; 3]> },
    /// R14, source world: carry `obj` out through the portal into escrow, bound for world `dest`.
    /// The transfer's id is derived from this entry's id (review NEW-5), never chosen.
    /// P2-5: `hop` is the certified hop that brought the object here, when this world does not
    /// hold it yet (copied from a `Provenance` entry); without it an arrived object cannot leave.
    Depart { obj: u32, dest: u32, deadline: u64, hop: Option<Hop> },
    /// R14, destination world: the object arrives. `cert` is a strict majority of the source
    /// world's members' signatures over "departure `depart` of this record to this world, by
    /// this deadline, is confirmed" (reviews NEW-4, R3-8); without it the entry does nothing.
    /// `deadline` is a frame of the destination world: a later arrival is refused (R3-6).
    /// P2-5: `chain` is the object's provenance from its birth into the source world; the
    /// certificate covers its hash. `proof` names the source's epoch at the departure frame
    /// (`record.src_frame`) with its epoch certificates: the signers are that epoch's (22.7, R3-8b).
    Arrive { transfer: u64, record: ObjRecord, depart: EntryId, deadline: u64, cert: Vec<(MemberId, u64)>, chain: Vec<Hop>, proof: EpochProof },
    /// R14, source world: the destination's confirmed verdict, with a strict majority of the
    /// destination's members' signatures over it (review NEW-3). Anyone may relay it. `frame` is the
    /// destination's arrival frame, and `proof` its epoch there with its epoch certificates.
    Verdict { transfer: u64, accepted: bool, cert: Vec<(MemberId, u64)>, frame: u64, proof: EpochProof },
    /// A member's signature over a cross-world fact its replica has confirmed (R14), published as
    /// a chain entry so it is equivocation-proof and fetchable.
    Attest { fact: u64, sig: u64 },
    /// P2-5, destination world: the source's certified commit of a transfer into this world, relayed
    /// so every member holds the hop the object's next departure must carry. Never simulated.
    Provenance { hop: Hop },
    /// R14 test fixture: open or close this world's arrival portal.
    Portal { open: bool },
    /// World ownership (12.5): one signed certificate of the world's ownership chain, an admin entry
    /// (D80) like a kick. Any member may publish one: the owners' signatures in it are its authority.
    /// A view change commits it in commit order, and it applies if it is valid against the book
    /// the entries before it leave, so every replica holds the same owners after each decision.
    Ownership { epoch: u32, cert: Cert },
    /// The author's ack, as a chain entry (review NEW-1): it will author nothing stamped at or
    /// before `horizon`; per other author, the newest seq it holds contiguously and that entry's
    /// id; its newest confirmed check frame and full-state hash (review H4); the membership epoch
    /// its author is in (design 10.8: an ack from a newer epoch than the reader's never counts
    /// toward the reader's confirmation).
    /// Thin links Part B: `fed` is set while its author is on the state feed (rule 5): its newest anchor frame
    /// and its echo (rule 1).
    Ack { horizon: u64, heads: Vec<(MemberId, u32, EntryId)>, check: Option<(u64, u64)>, epoch: u32, fed: Option<FedAck> },
    /// Vote, in membership epoch `epoch`, to remove `target`: it signed two versions of its entry
    /// `seq` (or another proof), or (`seq` = 0) it has gone silent or cannot keep up (reviews
    /// R3-2, R3-9). `frozen` is the head of target's chain this voter holds and will ever
    /// reference in this epoch (review NEW-2). A vote stands until the epoch ends (P2-4).
    /// `kick` names the kick this vote answers (0: the voter's own suspicion or proof). A kick vote
    /// only freezes the chain for the cut, may come from the target itself, and counts toward no
    /// removal but that kick's (D80).
    Vote { epoch: u32, target: MemberId, seq: u32, frozen: (u32, EntryId), kick: EntryId },
    /// View change (P2-4, design 10.8): the round's proposer proposes `change` for epoch `epoch`;
    /// `valid_round` is the round in which it saw a quorum prevote this value (`NO_ROUND` if none).
    Propose { epoch: u32, round: u32, valid_round: u32, change: Change },
    /// View change: this member's first-phase vote for a proposal's value in a round (0 = nil).
    Prevote { epoch: u32, round: u32, value: u64 },
    /// View change: this member's second-phase vote; a quorum of them decides the value. A value
    /// that adds members states their first frame, `boundary` (0 if none): from this entry on, the
    /// author's acks count toward no reader's confirmation at or past it until a later epoch (10.8).
    Precommit { epoch: u32, round: u32, value: u64, boundary: u64, polka: Vec<EntryId> },
    /// D80 (user 22:13Z): a kick of `target` by a member holding the KICK permission, for the view
    /// change that ends `epoch`. It authorises the removal alone: members vote only to freeze the
    /// target's chain for the cut, never to approve it. The view change commits it in order with
    /// every other membership change. Like every admin entry it stays pending across epochs until a
    /// decision applies or refuses it; `epoch` is the first epoch whose change may include it.
    Kick { epoch: u32, target: MemberId },
    /// Give `member` the role `role`, for the view change that ends `epoch` (ROLES permission).
    /// Refused if it would leave the session without an admin.
    SetRole { epoch: u32, member: MemberId, role: RoleId },
    /// Define custom role `role` as the permission set `perms` (ROLES permission). Admin (role 0) is
    /// built in and cannot be redefined.
    DefineRole { epoch: u32, role: RoleId, perms: u64 },
}

/// `valid_round` of a proposal that no quorum has prevoted before.
pub const NO_ROUND: u32 = u32::MAX;

/// Top bit of an encoded change's epoch: the change carries D80 admin entries and kicks.
const CHANGE_EXT: u32 = 1 << 31;

/// Test fixture: P2-4 review fixes a repro can switch off (`WorldSim::fixes_off`), to show the
/// behaviour before the fix in the same binary (review-p2-4.md).
pub mod fix {
    /// VC-2: confirmation past a removed member's fork bounded by the verified prefix; the cut's
    /// branch adopted on the duplicate path.
    pub const VC2: u64 = 1 << 0;
    /// VC-3: a join's (or close's) boundary checked only for fresh proposals; late precommits.
    pub const VC3: u64 = 1 << 1;
    /// VC-4: a precommit's boundary must lie just ahead of its author's horizon; stale caps reset.
    pub const VC4: u64 = 1 << 2;
    /// VC-5: an ack's epoch rises by at most one. (An epoch newer than mine left unbacked was also voted
    /// out until trust survey #4.)
    pub const VC5: u64 = 1 << 3;
    /// Round 3, VCR-1: a reset answers a kick vote that names no held, pending kick of its target.
    pub const VCR1: u64 = 1 << 4;
    /// VCR-2: a vote on its own kick freezes exactly its author's previous entry; a cut uses only
    /// votes whose frozen heads this replica can walk.
    pub const VCR2: u64 = 1 << 5;
    /// VCR-3: a member acks as soon as it enters a new epoch, so its acks never skip one.
    pub const VCR3: u64 = 1 << 6;
    /// VCR-4: round 3 counted resets against the members whose votes and caps they overruled; that part
    /// is now `VC3R2`'s. (Its newer-epoch timers went with VC-5's vote, trust survey #4.)
    pub const VCR4: u64 = 1 << 7;
    /// VCR-5: a cut's branch is fetched from its highest missing stretch down.
    pub const VCR5: u64 = 1 << 8;
    /// VCR-6: no reconcile before a cut's branch is re-validated.
    pub const VCR6: u64 = 1 << 9;
    /// VCR-7: joiners' attestations travel in the change, and a join request is never overwritten.
    pub const VCR7: u64 = 1 << 10;
    /// VCR-8: a decision applies only once every admin entry it lists is held.
    pub const VCR8: u64 = 1 << 11;
    /// VCR-9: admin entries obey their author's horizon; a proposer lists them without skipping one.
    pub const VCR9: u64 = 1 << 12;
    /// VCR-12 and VC3R-5: a precommit's boundary need only lie ahead of its author's horizon. Round 2
    /// also bounded it from above, from that horizon, which voided a lagging member's precommits.
    pub const VCR12: u64 = 1 << 13;
    /// HO-1, round 2: a touch carries the touched object's state only.
    pub const HO1: u64 = 1 << 14;
    // Bit 15 was VC3R-1 (the newer-epoch timer's round-3 pause), retired with VC-5's vote (trust
    // survey #4).
    /// VC3R-2: no strikes. A stale cap, or a kick vote whose kick nobody delivers, is not answered by a
    /// reset (round 3), and a reset waits briefly for votes to gather. Trust survey #4: neither gets its
    /// author voted out any more either; it holds confirmation until an admin kicks it.
    pub const VC3R2: u64 = 1 << 16;
    /// VC3R-3: a change lists only admin entries this replica has validated and found not void.
    pub const VC3R3: u64 = 1 << 17;
    /// VC3R-4: a join request is signed by its joiner, and the attestations of its requests merge.
    pub const VC3R4: u64 = 1 << 18;
    /// VC3R-6: a joiner's acks may declare its admission epoch at once.
    pub const VC3R6: u64 = 1 << 19;
    /// VC3R-7: a cut's branch is fetched by entry id, down the prev links.
    pub const VC3R7: u64 = 1 << 20;
    /// Round 4, with `VC3R2`: slowness timers survive decisions (round 3 restarted every one at each
    /// decision, so a member forcing a reset every few hundred ms kept a slow one from demotion); a
    /// member alone in a vote that a decision answered holds off instead (VC-6).
    pub const LAGKEEP: u64 = 1 << 21;
    /// Round 5, VC4R-2: a change carries each joiner's signed request and is judged from it, never
    /// from a request the validator happens to hold.
    pub const VC4R2: u64 = 1 << 22;
    /// VC4R-3: a member's claim to hold more of a chain than this replica does spares that chain's
    /// author for a silence limit at most, and a reference counts toward stability only once the entry
    /// it names is held in that version.
    pub const VC4R3: u64 = 1 << 23;
    // Bit 24 was VC4R-1 (the cap rule's polka counted in any version), retired with that rule (trust
    // survey #4: a cap's author is no longer voted out).
    /// VC4R-4: entry fetches by id are retried with backoff instead of asked once.
    pub const VC4R4: u64 = 1 << 25;
    /// VC4R-5: a vote whose frozen head nobody delivers stops holding resets back. (Its voter was also
    /// voted out until trust survey #4.)
    pub const VC4R5: u64 = 1 << 26;
    /// VC4R-6: a join request's signature names the session and an expiry.
    pub const VC4R6: u64 = 1 << 27;
    /// Round 5, VC3R-3's low: a listed admin entry must be stable here (every member holds that version).
    pub const VC3R3S: u64 = 1 << 28;
    // Bit 29 was VC6R-1 (the cap rule's fetch budget), retired with that rule (trust survey #4).
    /// VC6R-3: a listed admin entry's validity is a pure function of the entry and its ancestry (held,
    /// its author's chain validated up to it, not voided). Canonical and undisputed are the proposer's
    /// choice only, so a fork arriving later cannot invalidate a value a quorum locked on.
    pub const VC6R3: u64 = 1 << 30;
    /// VC6R-4: a decision resets only the timers its cleared votes made stale (each voter's reference to
    /// its target, and my own votes' targets) and new members', not every timer.
    pub const VC6R4: u64 = 1 << 31;
    /// PF-1 (@ballpit-viz, mirroring prevotefork): a proven member's precommits count toward a
    /// decision wherever they sit in its chain, and its proposals are kept by value the same way. A
    /// replica keeps the first version of a seq it receives, so a forker's chain past its fork may
    /// never link here, and its precommit in a quorum others decided on held this replica back.
    pub const PF1: u64 = 1 << 32;
    /// VC7R-1: a listed admin entry is judged along its own ancestry (the chain-local rules replayed
    /// from the prefix this replica validated), never by which branch of its author's chain this replica
    /// linked: a horizon-void version held as a duplicate was valid here and void where it was linked.
    /// Pure only with `RECUT` on (review VC8R-2): the replay reads the ack-epoch history.
    pub const VC7R1: u64 = 1 << 33;
    // Bit 34 was VC7R-2 (the cap and frozen-head fetch budgets before their authors were voted out),
    // retired with those rules (trust survey #4).
    /// Round 8 (found while building `slowcapper`): a cut that re-points a removed member's chain
    /// re-validates the cut branch from a clean chain state, the ack-epoch history included. Without it
    /// the history of the abandoned branch made every re-validated early ack fail VC-5's epoch rule, so
    /// a replica that re-pointed voided the removed member's acks and one that did not kept them.
    /// Review VC8R-2: VC7R1's purity rests on it, since `valid_on_own_branch` reads that history.
    pub const RECUT: u64 = 1 << 35;
    /// VC8R-1: a proven forker's prevotes count toward a polka (a lock, a re-proposal's valid round)
    /// wherever they sit in its chain, in any version, as PF-1 counts its precommits toward a decision.
    /// A member that kept another version of the forked seq could not count the forker's prevote, never
    /// saw the polka the others locked on, and the locked members prevoted nil on everything else.
    pub const VC8R1: u64 = 1 << 36;
    /// Review OWNY-2 (ownership, 12.5): a certificate built on a pending one's head is held and judged
    /// again after each decision, not dropped at receipt. (A high bit, clear of P2-4's next flags.)
    pub const OWNY2: u64 = 1 << 40;
}

/// The most attestations a join request or a change carries per joiner (review VCR-11).
pub const MAX_ATTESTS: usize = 64;

/// Review VC6R-2: the most prevote ids a capped precommit carries (its polka). At least the largest
/// quorum, so honest code never exceeds it; a decode of more is refused.
pub const MAX_POLKA: usize = 512;

/// Review P25E-3: the most epoch certificates a hop or an epoch proof carries; a decode of more is
/// refused. The spike sends every certificate from the partner's genesis; production sends only the
/// ones the receiver lacks [I].
pub const MAX_EPOCH_CERTS: usize = 1024;

/// Review OWNZ-1: the most ownership entries one author has held for a missing parent (OWNY-2); past
/// it, such an entry is dropped at receipt as before, so one faulty member can't grow the set that
/// every decision re-scans.
pub const MAX_HELD_PER_AUTHOR: usize = 16;

/// Round 3 only (`fix::VC3R2` off, review VC3R-2): a member whose votes or caps resets overruled in
/// this many of the last `STRIKE_EPOCHS` epochs is voted out.
pub const STRIKES_OUT: usize = 3;
pub const STRIKE_EPOCHS: u32 = 8;

/// Frames a proposer waits, from the start of a round, for votes on every member it suspects.
pub const VC_GATHER: u64 = 6;

/// Review VC3R-2 (round 4): frames the newest standing vote must have stood before a reset is
/// proposed, so the votes of members that suspect the same member together arrive first.
pub const RESET_WAIT: u64 = 12;

/// Two signed entries of one member that contradict each other (design 10.8): two versions of one
/// seq; an entry whose `prev` names a version of its predecessor other than `a`; or an ack `a` and a
/// later vote `b` whose frozen head drops an entry that ack referenced. Anyone holding both entries
/// (and, for a vote, the target's chain) can check it, so it travels in certificates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proof {
    pub a: EntryId,
    pub b: EntryId,
}

/// One member removed by a view change: the votes that justify it (each names its voter's frozen
/// head of the member's chain) and the cut those votes determine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    pub member: MemberId,
    pub votes: Vec<EntryId>,
    pub cut: (u32, EntryId),
    /// The kick that authorises this removal (0: the voters' own suspicion or proof, D80).
    pub kick: EntryId,
}

/// A membership change from epoch `epoch` to `epoch + 1` (design 10.8). `excluded` are the members
/// this decision treats as proven faulty: they count neither in its quorums nor in their size. An
/// empty change is a reset: same members, every vote of the epoch cleared.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Change {
    pub epoch: u32,
    pub remove: Vec<Removal>,
    pub add: Vec<MemberId>,
    /// First frame of the next epoch: added members count from it (10.8, joins), and facts about
    /// frames from it on are the next epoch's to certify. Set on every change.
    pub boundary: u64,
    pub excluded: Vec<(MemberId, Proof)>,
    /// D80: the admin entries (kicks, role entries) this change commits, in commit order (stamp,
    /// author, seq), each with whether it applies. A refused one (its author lacks the permission
    /// or has left, or it would leave no admin) is listed too, so its refusal is agreed.
    pub admin: Vec<(EntryId, bool)>,
    /// D80: each added member's starting role, in `add`'s order. It travels in the change because a
    /// replica may decide from others' precommits without the join request and its attestations.
    pub add_roles: Vec<RoleId>,
    /// Review VCR-7: each added member's attestations, in `add`'s order, as the proposer holds them.
    /// Every replica judges the join rules from these, signature-checked, never from its own copy, so
    /// a joiner that sends each member something different cannot split them.
    pub add_attests: Vec<Vec<Attest>>,
    /// Review VC4R-2: each added member's signed request, (signature, expiry), in `add`'s order, so a
    /// member that never received the request validates the join all the same.
    pub add_reqs: Vec<(u64, u64)>,
}

/// Commit order of admin entries (D80, rev 22 (e1)): the stamp-effective ones (role entries, ownership) first, by stamp,
/// author, seq; then kicks, by a hash of `seed` (the previous decided change's value: unknown until it decides, so no
/// author can grind a key that sorts first; @coordinator-opus m67ddf30d63ad, @design-reviewer m4b209491ab0b) and the
/// author, then seq. No kick's stamp is read.
fn admin_key(e: &Entry, seed: u64) -> (u8, u64, MemberId, u32) {
    match e.payload {
        Payload::Kick { .. } => (1, fnv_u64s([0x5045_4b49_434b_4f52, seed, e.member as u64]), e.member, e.seq),
        _ => (0, e.stamp, e.member, e.seq),
    }
}

/// A member's ownership key (12.5). In the spike a member id is its identity (10.3); production
/// reads the key from the member's signed genesis or join entry. The one place that maps them.
pub fn key_of(m: MemberId) -> Key {
    m
}

/// The member an ownership key identifies, the inverse of [`key_of`].
pub fn member_of(k: Key) -> MemberId {
    k
}

/// The membership state an admin sequence runs against (D80).
#[derive(Clone)]
struct AdminRun {
    members: BTreeSet<MemberId>,
    roles: BTreeMap<MemberId, RoleId>,
    defs: BTreeMap<RoleId, u64>,
    excluded: BTreeSet<MemberId>,
    /// The world owners' keys (12.5; [`key_of`] maps a member to its key).
    owners: BTreeSet<Key>,
    /// The world's ownership chain (12.5), if it has one, and the session lineage that commits to it.
    book: Option<Book>,
    lineage: u32,
    /// Rev 22 (e1): the ACL as of the previous decided change (members, roles, role definitions, owners), against which
    /// a kick is judged, whatever this decision's role entries (which run first) changed.
    acl0: Option<Box<(BTreeSet<MemberId>, BTreeMap<MemberId, RoleId>, BTreeMap<RoleId, u64>, BTreeSet<Key>)>>,
}

/// Revision 22, plan (A) (@coordinator-opus, 2026-11-06 22:13Z): a latency leave carries the fed set its sender saw. Spike
/// encoding: a mask in the leave vote's `seq` (0 on every other leave), bit m − 1 for member m, the top bit set. A lying
/// leaver can only affect its own leave.
pub const LATENCY_LEAVE: u32 = 1 << 31;

pub fn latency_leave_mask(fed: &BTreeSet<MemberId>) -> u32 {
    fed.iter().filter(|&&m| (1..=31).contains(&m)).fold(LATENCY_LEAVE, |a, &m| a | 1 << (m - 1))
}

/// The fed set a leave vote's `seq` carries: None for a plain leave.
pub fn latency_leave_fed(seq: u32) -> Option<BTreeSet<MemberId>> {
    (seq & LATENCY_LEAVE != 0).then(|| (1..=31u32).filter(|&m| seq & (1 << (m - 1)) != 0).collect())
}

/// The two-fed region for a latency leave at commit: n − f < q with `members`, f the carried fed set among them.
fn latency_region(members: &BTreeSet<MemberId>, fed: &BTreeSet<MemberId>) -> bool {
    let n = members.len();
    let f = fed.iter().filter(|m| members.contains(m)).count();
    f > 0 && n - f < quorum(QuorumRule::Bft, n).1
}

impl AdminRun {
    fn perms(&self, m: MemberId) -> u64 {
        match self.roles.get(&m) {
            Some(&ADMIN) => perm::ALL,
            Some(r) => self.defs.get(r).copied().unwrap_or(0),
            None => 0,
        }
    }
    /// Whether `m` counts as an admin: its role holds the roles permission, so it can make itself Admin
    /// (V2 = b, user 08:31Z). Admin holds every permission, so this includes it. The last-admin rules
    /// count these members (@coordinator-opus's call, 09:24Z).
    fn admin_like(&self, m: MemberId) -> bool {
        self.perms(m) & perm::ROLES != 0
    }
    fn role_admin_like(&self, r: RoleId) -> bool {
        r == ADMIN || self.defs.get(&r).is_some_and(|p| p & perm::ROLES != 0)
    }
    fn has_admin(&self) -> bool {
        self.members.iter().any(|&m| self.admin_like(m))
    }
    /// Whether member `a` may kick `target` in this state: the kick permission, not itself, a member;
    /// only a co-owner kicks an owner (Q27, user 22:49Z); kicking a member who counts as an admin needs
    /// its own permission (V1, user 08:31Z). `apply` judges a kick by it in commit order, and a replica
    /// receiving a kick by it against the current state, so a kick refused now freezes no one.
    fn kick_allowed(&self, a: MemberId, target: MemberId) -> bool {
        let p = self.perms(a);
        let owner_safe = self.owners.contains(&key_of(target)) && !self.owners.contains(&key_of(a));
        let admin_safe = self.admin_like(target) && p & perm::KICK_ADMIN == 0;
        p & perm::KICK != 0 && target != a && self.members.contains(&target) && !owner_safe && !admin_safe
    }
    /// Whether admin entry `e` applies at this point of the sequence; if it does, apply it. A
    /// refused entry changes nothing.
    fn apply(&mut self, e: &Entry) -> bool {
        let a = e.member;
        if !self.members.contains(&a) || self.excluded.contains(&a) {
            return false;
        }
        let p = self.perms(a);
        match e.payload {
            Payload::Kick { target, .. } => {
                // Rev 22 (e1): judged against the previous decided ACL; the target must still be a member here.
                let allowed = match &self.acl0 {
                    Some(b) => {
                        let v = AdminRun { members: b.0.clone(), roles: b.1.clone(), defs: b.2.clone(), excluded: BTreeSet::new(), owners: b.3.clone(), book: None, lineage: self.lineage, acl0: None };
                        v.kick_allowed(a, target) && self.members.contains(&target)
                    }
                    None => self.kick_allowed(a, target),
                };
                if !allowed {
                    return false;
                }
                self.members.remove(&target);
                self.roles.remove(&target);
                true
            }
            Payload::SetRole { member, role, .. } => {
                // Q27 (user 22:49Z): an owner may give up Admin, but only a co-owner demotes an owner.
                let owner_safe = self.owners.contains(&key_of(member)) && member != a && !self.owners.contains(&key_of(a)) && role != ADMIN;
                if p & perm::ROLES == 0 || !self.members.contains(&member) || (role != ADMIN && !self.defs.contains_key(&role)) || owner_safe {
                    return false;
                }
                let old = self.roles.insert(member, role);
                // The last admin cannot give the role up without handing it on (user 22:11Z).
                if !self.has_admin() {
                    match old {
                        Some(o) => self.roles.insert(member, o),
                        None => self.roles.remove(&member),
                    };
                    return false;
                }
                true
            }
            Payload::DefineRole { role, perms, .. } => {
                if p & perm::ROLES == 0 || role == ADMIN {
                    return false;
                }
                let old = self.defs.insert(role, perms);
                // A role holding the roles permission counts as an admin (V2 = b), so taking the
                // permission from the last such role gives the last admin up too (user 22:11Z).
                if !self.has_admin() {
                    match old {
                        Some(o) => self.defs.insert(role, o),
                        None => self.defs.remove(&role),
                    };
                    return false;
                }
                true
            }
            // 12.5: valid against the book as the entries before it leave it. The outcome depends
            // on the book, the certificate and commit order only, never on the boundary (which is
            // computed after the admin entries); the entry's stamp stands in as the frame, which
            // only the session's own attestation record reads. Later entries see the new owners.
            Payload::Ownership { ref cert, .. } => {
                let Some(b) = self.book.as_mut() else { return false };
                if !matches!(b.commit_local(cert.clone(), self.lineage, e.stamp), Ok(Insert::Applied)) {
                    return false;
                }
                self.owners = b.view().owners.owners;
                true
            }
            _ => false,
        }
    }
}

/// A role in a session (D80; user rulings 22:13Z-22:18Z). Admin, role 0, is built in: it always
/// exists, holds every permission and is by default the only role. Admins define other roles as
/// subsets of the permission set. Admins are equal: none is above another.
pub type RoleId = u16;
pub const ADMIN: RoleId = 0;

/// Thin links Part B, rule 5: the longest a fed member's ack period P can be (P <= 32 frames).
pub const FED_ACK_MAX_FRAMES: u64 = 32;

/// The permissions a role grants. The catalogue grows with the engine; these are the ones the
/// membership protocol checks. Admin holds them all.
pub mod perm {
    /// Kick a member who does not count as an admin (D80: authorises the removal on its own).
    pub const KICK: u64 = 1 << 0;
    /// Define roles and give them to members. Holding it counts as being an admin, since it can grant
    /// Admin (V2 = b, user 08:31Z).
    pub const ROLES: u64 = 1 << 1;
    /// Kick a member who counts as an admin (V1, user 08:31Z): by default only Admin holds it, and a
    /// session may give it to other roles.
    pub const KICK_ADMIN: u64 = 1 << 2;
    pub const ALL: u64 = u64::MAX;
}

/// A relationship a joiner can show (D80 join rules, user 22:47Z and 22:51Z): it is the target,
/// is friends with it (mutual, both sign), is its confidant (the target named the joiner, D27a),
/// holds an invite ticket from it (for this world), or is one named player.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rel {
    Is,
    Friend,
    Confidant,
    Invited,
    Named(MemberId),
}

impl Rel {
    fn tag(self) -> u8 {
        match self {
            Rel::Is => 0,
            Rel::Friend => 1,
            Rel::Confidant => 2,
            Rel::Invited => 3,
            Rel::Named(_) => 4,
        }
    }
}

/// Whom a join rule looks at: a current player, a current admin, a current player with a given
/// role, or a world owner (present or not).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Player,
    Admin,
    Role(RoleId),
    Owner,
}

/// One join rule: if the joiner has `rel` with any (or all) of `target`, it is admitted with `role`,
/// or refused when `role` is None.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JoinRule {
    pub target: Target,
    pub all: bool,
    pub rel: Rel,
    pub role: Option<RoleId>,
}

/// Who may join and with which starting role (user rulings 22:37Z, 22:47Z, 22:48Z): first the fixed
/// row 0, "a world owner joins as Admin", which no world removes; then the first rule that matches;
/// then `fallback` (None refuses). Evaluated against the membership the change leaves at its commit,
/// so a join stands once committed (22:17Z) and "all of" means all at that commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinRules {
    pub rules: Vec<JoinRule>,
    pub fallback: Option<RoleId>,
}

impl JoinRules {
    /// Public (D21): everyone joins with `role`; every player is an admin by default (22:11Z).
    pub fn public(role: RoleId) -> Self {
        JoinRules { rules: vec![], fallback: Some(role) }
    }
    /// Friends only: friends of a current admin (22:17Z).
    pub fn friends_only(role: RoleId) -> Self {
        JoinRules { rules: vec![JoinRule { target: Target::Admin, all: false, rel: Rel::Friend, role: Some(role) }], fallback: None }
    }
    /// Friends+: friends of any current player (22:17Z).
    pub fn friends_plus(role: RoleId) -> Self {
        JoinRules { rules: vec![JoinRule { target: Target::Player, all: false, rel: Rel::Friend, role: Some(role) }], fallback: None }
    }
    /// Invite only (22:51Z): holders of an invite ticket from a current player.
    pub fn invite_only(role: RoleId) -> Self {
        JoinRules { rules: vec![JoinRule { target: Target::Player, all: false, rel: Rel::Invited, role: Some(role) }], fallback: None }
    }
    /// A home world (22:36Z): the owners' confidants join as Admin, their friends as `guest`.
    pub fn home(guest: RoleId) -> Self {
        let r = |rel, role| JoinRule { target: Target::Owner, all: false, rel, role: Some(role) };
        JoinRules { rules: vec![r(Rel::Confidant, ADMIN), r(Rel::Friend, guest)], fallback: None }
    }
}

impl Default for JoinRules {
    fn default() -> Self {
        Self::public(ADMIN)
    }
}

/// An attestation a joiner presents: `rel` with `with`, signed by `with` and by the joiner (spike
/// signature model). Relationships are between players, signed without a world; an invite ticket
/// names its world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attest {
    pub rel: Rel,
    pub with: MemberId,
    pub sig_with: u64,
    pub sig_joiner: u64,
}

pub fn fact_rel(rel: Rel, joiner: MemberId, with: MemberId, world: u32) -> u64 {
    let (a, b) = if rel == Rel::Friend { (joiner.min(with), joiner.max(with)) } else { (with, joiner) };
    let w = if rel == Rel::Invited { world } else { 0 };
    fnv_u64s([31, rel.tag() as u64, a as u64, b as u64, w as u64])
}

/// The attestation `with` and the joiner sign for `rel` (spike signature model).
pub fn attest(rel: Rel, joiner: MemberId, with: MemberId, world: u32) -> Attest {
    let f = fact_rel(rel, joiner, with, world);
    Attest { rel, with, sig_with: member_sig(0, with, f), sig_joiner: member_sig(0, joiner, f) }
}

pub fn attest_ok(a: &Attest, joiner: MemberId, world: u32) -> bool {
    let f = fact_rel(a.rel, joiner, a.with, world);
    matches!(a.rel, Rel::Friend | Rel::Confidant | Rel::Invited)
        && a.with != joiner
        && a.sig_with == member_sig(0, a.with, f)
        && a.sig_joiner == member_sig(0, joiner, f)
}

/// Round 4's join fact (review VC3R-4): "`joiner` asks to join world `world`". Replayable in any later
/// session of the world (VC4R-6); kept for the fix-off runs.
pub fn fact_join(world: u32, joiner: MemberId) -> u64 {
    fnv_u64s([37, world as u64, joiner as u64])
}

/// Round 4's join signature, over [`fact_join`].
pub fn join_sig(world: u32, joiner: MemberId) -> u64 {
    member_sig(0, joiner, fact_join(world, joiner))
}

/// A session's id: its world and its genesis members (P2-3: the session id in the key ring).
pub fn session_id(world: u32, genesis: &[MemberId]) -> u64 {
    fnv_u64s([43, world as u64].into_iter().chain(genesis.iter().map(|&m| m as u64)))
}

/// Review VC4R-6: "`joiner` asks to join session `session`, until the change admitting it has a
/// boundary past `expires`", under its own domain tag (47), so it is valid in that session only.
pub fn fact_join_in(session: u64, joiner: MemberId, expires: u64) -> u64 {
    fnv_u64s([47, session, joiner as u64, expires])
}

/// The joiner's signature on its join request (spike model of its Ed25519 key, as for attestations).
pub fn join_sig_in(session: u64, joiner: MemberId, expires: u64) -> u64 {
    member_sig(0, joiner, fact_join_in(session, joiner, expires))
}

/// A signed join request from `joiner` to session `session`.
pub fn join_msg(session: u64, joiner: MemberId, attests: Vec<Attest>, expires: u64) -> Msg {
    Msg::Join { member: joiner, attests, sig: join_sig_in(session, joiner, expires), expires }
}

impl Change {
    /// The value the view change votes on: a hash of the whole change, never 0 (0 is nil).
    pub fn value(&self) -> u64 {
        let mut w = Wr(Vec::with_capacity(64));
        w.change(self);
        fnv_bytes(&w.0) | 1
    }
}

/// D80 (open): how many members a view change needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuorumRule {
    /// A strict majority of the members not excluded by proof (option a): survives crashes and
    /// partitions, but tolerates no faulty member that has not been proven so.
    Majority,
    /// A Byzantine quorum (option b, recommended): more than (m + t) / 2 of the m members not
    /// excluded by proof, which tolerates t = floor((m - 1) / 3) faulty members not yet proven.
    Bft,
}

/// `(t, q)` for `m` members not excluded by proof: unproven faulty members tolerated, and the
/// quorum size. Any two quorums then share an honest member while at most t of the m are faulty.
pub fn quorum(rule: QuorumRule, m: usize) -> (usize, usize) {
    let t = match rule {
        QuorumRule::Majority => 0,
        QuorumRule::Bft => m.saturating_sub(1) / 3,
    };
    (t, (m + t + 2) / 2)
}

/// Spike model of a member's signature over its precommit of `value` in `round` of the view change
/// that ends epoch `epoch` of `world`: the signatures an epoch certificate carries.
pub fn vc_sig(world: u32, member: MemberId, epoch: u32, round: u32, value: u64) -> u64 {
    member_sig(world, member, fnv_u64s([3, epoch as u64, round as u64, value]))
}

/// An epoch certificate (design 10.8, review R3-8b): what another world needs to follow this
/// world's membership from `epoch - 1` to `epoch`. The precommits sign the change's value, so the
/// change itself (removals, joins, exclusions and the boundary frame) is what is certified.
#[derive(Clone, Debug, PartialEq)]
pub struct EpochCert {
    pub world: u32,
    /// The epoch this certificate starts (`change.epoch + 1`).
    pub epoch: u32,
    pub change: Change,
    pub round: u32,
    /// The members of `epoch`: the previous epoch's, minus the removed, plus the added.
    pub members: Vec<MemberId>,
    /// Precommit signatures over `(world, epoch - 1, round, change value)`.
    pub precommits: Vec<(MemberId, u64)>,
    /// The two entries of each exclusion proof, so a verifier needs no chain of this world.
    pub proofs: Vec<Entry>,
}

impl EpochCert {
    /// First frame of the epoch: facts about frames from here on are this epoch's to sign.
    pub fn from_frame(&self) -> u64 {
        self.change.boundary
    }

    pub fn hash(&self) -> u64 {
        let mut v = vec![self.world as u64, self.epoch as u64, self.change.value(), self.round as u64];
        v.extend(self.members.iter().map(|m| *m as u64));
        for (m, s) in &self.precommits {
            v.extend([*m as u64, *s]);
        }
        v.extend(self.proofs.iter().map(|e| e.id()));
        fnv_u64s(v)
    }
}

/// Whether `p` proves `m` faulty from its two entries alone (design 10.8): two versions of one seq;
/// an entry whose `prev` names another version of its predecessor; or an ack that references a
/// seq of some chain beyond the frozen head of a later vote by the same member against it.
pub fn proof_self_contained(m: MemberId, p: Proof, a: &Entry, b: &Entry) -> bool {
    if a.member != m || b.member != m || a.id() != p.a || b.id() != p.b {
        return false;
    }
    if a.seq == b.seq {
        return p.a != p.b;
    }
    if b.seq == a.seq + 1 {
        return b.prev != p.a;
    }
    match (&a.payload, &b.payload) {
        (Payload::Ack { heads, .. }, Payload::Vote { target, frozen, .. }) => {
            a.seq < b.seq && heads.iter().any(|h| h.0 == *target && h.1 > frozen.0)
        }
        _ => false,
    }
}

/// Check `cert` against the members of the previous epoch: every exclusion proved by the entries
/// it carries; precommits from a quorum of the members not excluded; the member list following
/// from the change.
pub fn epoch_cert_ok(rule: QuorumRule, prev: &[MemberId], cert: &EpochCert) -> bool {
    let c = &cert.change;
    if cert.epoch == 0 || c.epoch + 1 != cert.epoch {
        return false;
    }
    let by_id: BTreeMap<EntryId, &Entry> = cert.proofs.iter().map(|e| (e.id(), e)).collect();
    let mut excluded = BTreeSet::new();
    for &(m, p) in &c.excluded {
        let (Some(a), Some(b)) = (by_id.get(&p.a), by_id.get(&p.b)) else { return false };
        if !prev.contains(&m) || !excluded.insert(m) || !proof_self_contained(m, p, a, b) {
            return false;
        }
    }
    let base: Vec<MemberId> = prev.iter().copied().filter(|m| !excluded.contains(m)).collect();
    let (_, q) = quorum(rule, base.len());
    let value = c.value();
    let signers: BTreeSet<MemberId> = cert
        .precommits
        .iter()
        .filter(|(m, s)| base.contains(m) && *s == vc_sig(cert.world, *m, c.epoch, cert.round, value))
        .map(|(m, _)| *m)
        .collect();
    let mut expect: BTreeSet<MemberId> = prev.iter().copied().collect();
    for r in &c.remove {
        expect.remove(&r.member);
    }
    expect.extend(c.add.iter().copied());
    signers.len() >= q && expect.into_iter().collect::<Vec<_>>() == cert.members
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub member: MemberId,
    /// 1-based per member. A member that rejoins gets a new MemberId, so seqs never restart.
    pub seq: u32,
    /// Id of this member's previous entry (0 for seq 1): a hash chain.
    pub prev: EntryId,
    /// Canonical frame chosen by the author (its present frame when it acted).
    pub stamp: u64,
    pub payload: Payload,
    /// User ruling 09:30Z: the resulting state of the touched object after frame `stamp`.
    pub carried: Option<(u32, BodyState)>,
    /// Thin links Part B (TLP-19, TLP-31): the checkpoint link. A member's first entry stamped in any non-empty
    /// 32-frame block is its checkpoint, and carries the id of that member's most recent earlier checkpoint;
    /// every other entry carries 0 (as does a member's first checkpoint).
    pub link: EntryId,
}

/// Thin links Part B: a checkpoint block is 32 frames.
pub const CKPT_FRAMES: u64 = 32;

/// Thin links Part B (rule 1's echo, rule 5): what a fed member's ack adds. `anchor`: the frame of its newest
/// state anchor (on every fed ack). `echo`: per member it echoes, the block of the newest checkpoint of that
/// member's chain it computed (TLP-23); `digest`: the hash of the echoed (member, block, id) triples, which each
/// receiver recomputes from its own version (production: 32 bytes, DS-4). The echo rides only on an ack after
/// it changed (empty: as before); it is cumulative, and acks are read in chain order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FedAck {
    pub anchor: u64,
    pub echo: Vec<(MemberId, u64)>,
    pub digest: u64,
}

/// H_id (TLP-22, TLP-31): an entry's id from its checkpoint link and its rest-hash.
pub fn entry_id(link: EntryId, rest: u64) -> EntryId {
    fnv_u64s([0x5045_5f49_445f_5631, link, rest])
}

/// Rule 1's echo: the digest over the echoed (member, block, id) triples, in member order.
pub fn echo_digest(triples: impl IntoIterator<Item = (MemberId, u64, EntryId)>) -> u64 {
    fnv_u64s(std::iter::once(0x5045_5f45_4348_4f31).chain(triples.into_iter().flat_map(|(m, b, id)| [m as u64, b, id])))
}

/// Thin links Part B (TLP-19): a chain's checkpoint state after `e`, from its state before it: the newest
/// checkpoint's block and id (None before the chain's first entry). `e` is a checkpoint if it is stamped in a
/// later block than the newest checkpoint's.
pub fn ckpt_step(ck: Option<(u64, EntryId)>, e: &Entry) -> Option<(u64, EntryId)> {
    match ck {
        Some((b, _)) if e.stamp / CKPT_FRAMES <= b => ck,
        _ => Some((e.stamp / CKPT_FRAMES, e.id())),
    }
}

/// The link an entry stamped `stamp` carries after the chain state `ck`: the newest checkpoint's id if the
/// entry is a checkpoint, and 0 if it is not (or is the chain's first).
pub fn ckpt_link_for(ck: Option<(u64, EntryId)>, stamp: u64) -> EntryId {
    match ck {
        Some((b, id)) if stamp / CKPT_FRAMES > b => id,
        _ => 0,
    }
}

impl Entry {
    /// Spike: H_id(link || H_rest(rest)), two FNV variants under distinct domain prefixes (TLP-22, TLP-31), so a
    /// fed member computes a checkpoint's id from the link it holds and the rest-hash it is sent. Production:
    /// domain-separated SHA-256 / BLAKE3 over the signed bytes.
    pub fn id(&self) -> EntryId {
        entry_id(self.link, self.rest_hash())
    }

    /// H_rest: the hash of everything but the link's value (the encoding's flags still say whether there is one).
    pub fn rest_hash(&self) -> u64 {
        let mut w = Wr(Vec::with_capacity(104));
        w.0.extend_from_slice(b"pe-rest\0");
        w.entry_rest(self);
        fnv_bytes(&w.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    /// Any chain entry: a perturbation, an ack or a vote.
    Entry(Entry),
    /// Ask for `author`'s entries `from..=to` (gap or conflict detection).
    Fetch { author: MemberId, from: u32, to: u32 },
    /// Everything one member sends one peer in one tick, in one datagram (R18, viz-4).
    Batch(Vec<Msg>),
    /// View change: ask for the proposal entry carrying `value` (a member that saw a quorum of
    /// precommits for a value it holds no proposal for; the proposer may have equivocated).
    FetchValue { value: u64 },
    /// A non-member asks to join the world (P2-4), after an invite or the world's ACL admits it (12).
    /// `attests`: the relationships it shows for the world's join rules (D80). `sig`: its own
    /// signature over [`fact_join_in`] (reviews VC3R-4, VC4R-6), so no one else can make or pre-empt its
    /// request, and it is valid in this session only, until a change with a boundary past `expires`.
    Join { member: MemberId, attests: Vec<Attest>, sig: u64, expires: u64 },
    /// Review VCR-8: ask for the entry with this id (an admin entry a decided change lists), and
    /// (review VC3R-7) up to `back` of its predecessors down its prev links, so the reply is exactly
    /// that branch of a forked chain.
    FetchEntry { id: EntryId, back: u8 },
    /// Thin links Part B, rule 5: the state feed to a fed member, from its feeder. The bodies' states
    /// after `frame`, quantized (`FeedBody`). Rev 22 delta (proposal): `fed`, the members the feeder's log shows
    /// fed, so a fed member knows whom to send its own checkpoints (contact only). Never logged.
    Feed { frame: u64, fed: Vec<MemberId>, bodies: Vec<FeedBody> },
    /// Thin links Part B (TLP-23): `member`'s checkpoints for a fed member, block and rest-hash each, from the
    /// one after block `after` (the checkpoint the first one links to; None: the first is the chain's first).
    /// Only from the chain's own author. Never logged. Rev 22 delta (proposal): `fed`, the sender is fed itself, and
    /// `echo`, its own last echo of the recipient's chain (a block), from which the recipient sends its checkpoints.
    Ckpts { member: MemberId, after: Option<u64>, list: Vec<(u64, u32, u64)>, fed: bool, echo: Option<u64> },
}

pub fn fnv_bytes(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h = (h ^ x as u64).wrapping_mul(0x100_0000_01B3);
    }
    h
}

// ---- wire encoding (also used in-process, so byte counts are real) ------------------------------

pub struct Wr(pub Vec<u8>);
impl Wr {
    fn u8(&mut self, v: u8) { self.0.push(v) }
    fn u32(&mut self, v: u32) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn u64(&mut self, v: u64) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn i32(&mut self, v: i32) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn i16(&mut self, v: i16) { self.0.extend_from_slice(&v.to_le_bytes()) }
    fn state(&mut self, s: &BodyState) { for b in s.0 { self.u32(b) } }
    fn cert(&mut self, c: &[(MemberId, u64)]) { self.u32(c.len() as u32); for (m, s) in c { self.u32(*m); self.u64(*s) } }
    fn hop(&mut self, h: &Hop) {
        self.u32(h.src); self.u32(h.dest); self.u64(h.transfer); self.u64(h.via); self.u32(h.epoch); self.u64(h.frame);
        assert!(h.certs.len() <= MAX_EPOCH_CERTS, "a hop carries at most {MAX_EPOCH_CERTS} epoch certificates");
        self.u32(h.certs.len() as u32); for c in &h.certs { self.epoch_cert(c) }
        self.cert(&h.cert)
    }
    fn epoch_cert(&mut self, c: &EpochCert) {
        self.u32(c.world); self.u32(c.epoch); self.change(&c.change); self.u32(c.round);
        self.u32(c.members.len() as u32); for m in &c.members { self.u32(*m) }
        self.cert(&c.precommits);
        self.u32(c.proofs.len() as u32); for e in &c.proofs { self.entry(e) }
    }
    fn chain(&mut self, c: &[Hop]) { self.u32(c.len() as u32); for h in c { self.hop(h) } }
    fn epoch_proof(&mut self, p: &EpochProof) {
        assert!(p.certs.len() <= MAX_EPOCH_CERTS, "an epoch proof carries at most {MAX_EPOCH_CERTS} certificates");
        self.u32(p.epoch); self.u32(p.certs.len() as u32); for c in &p.certs { self.epoch_cert(c) }
    }
    fn attests(&mut self, a: &[Attest]) {
        assert!(a.len() <= MAX_ATTESTS, "{} attestations: at most {MAX_ATTESTS} travel", a.len());
        self.u32(a.len() as u32);
        for x in a { self.u8(x.rel.tag()); self.u32(x.with); self.u64(x.sig_with); self.u64(x.sig_joiner) }
    }
    fn entry(&mut self, e: &Entry) {
        self.entry_rest(e);
        if e.link != 0 {
            self.u64(e.link);
        }
    }
    /// The canonical encoding without the checkpoint link's value (H_rest's input).
    fn entry_rest(&mut self, e: &Entry) {
        self.u32(e.member);
        self.u32(e.seq);
        self.u64(e.prev);
        self.u64(e.stamp);
        match &e.payload {
            Payload::Touch { obj, impulse, grab } => {
                self.u8(1); self.u32(*obj); for c in impulse { self.u32(c.to_bits()) } self.u8(*grab as u8);
            }
            Payload::Hand { obj, target_mm, active, span, more } => {
                self.u8(if more.is_empty() { 2 } else { 21 });
                self.u32(*obj); for c in target_mm { self.i32(*c) } self.u8(*active as u8); self.u8(*span);
                if !more.is_empty() {
                    // Thin links (Part A): a pack's extra targets as deltas from the previous target, in
                    // the narrowest width that holds every axis of every step (1: i8, 2: i16, 0: whole i32).
                    // A stride-8 pack falls from 129 to 67 bytes.
                    self.u8(more.len() as u8);
                    let mut prev = *target_mm;
                    let mut maxd = 0i64;
                    for t in more {
                        for i in 0..3 {
                            maxd = maxd.max((t[i] as i64 - prev[i] as i64).abs());
                        }
                        prev = *t;
                    }
                    let mode = if maxd <= 127 { 1u8 } else if maxd <= 32767 { 2 } else { 0 };
                    self.u8(mode);
                    let mut prev = *target_mm;
                    for t in more {
                        for i in 0..3 {
                            let d = t[i].wrapping_sub(prev[i]);
                            match mode {
                                1 => self.u8(d as i8 as u8),
                                2 => self.0.extend_from_slice(&(d as i16).to_le_bytes()),
                                _ => self.i32(t[i]),
                            }
                        }
                        prev = *t;
                    }
                }
            }
            Payload::Depart { obj, dest, deadline, hop } => {
                self.u8(3); self.u32(*obj); self.u32(*dest); self.u64(*deadline);
                match hop { Some(h) => { self.u8(1); self.hop(h) } None => self.u8(0) }
            }
            Payload::Arrive { transfer, record, depart, deadline, cert, chain, proof } => {
                self.u8(4); self.u64(*transfer); self.u64(record.global); self.u8(record.kind);
                self.state(&record.state); self.u32(record.src_world); self.u64(record.src_frame); self.u64(record.via);
                self.u64(*depart); self.u64(*deadline); self.cert(cert); self.chain(chain); self.epoch_proof(proof);
            }
            Payload::Provenance { hop } => { self.u8(16); self.hop(hop) }
            Payload::Verdict { transfer, accepted, cert, frame, proof } => {
                self.u8(5); self.u64(*transfer); self.u8(*accepted as u8); self.cert(cert); self.u64(*frame); self.epoch_proof(proof);
            }
            Payload::Attest { fact, sig } => { self.u8(9); self.u64(*fact); self.u64(*sig) }
            Payload::Portal { open } => { self.u8(6); self.u8(*open as u8) }
            Payload::Ownership { epoch, cert } => {
                let w = cert.to_words();
                assert!(w.len() <= MAX_CERT_WORDS, "an ownership certificate is at most {MAX_CERT_WORDS} words");
                self.u8(20); self.u32(*epoch); self.u32(w.len() as u32); for x in w { self.u64(x) }
            }
            Payload::Ack { horizon, heads, check, epoch, fed } => {
                // Epoch 0 keeps the phase-1 layout (tag 7), so runs without a view change keep
                // their entry ids; a later epoch is tag 14 with the epoch appended. A fed member's ack
                // (Part B) is tag 22: the epoch, then its anchor and echo.
                self.u8(if fed.is_some() { 22 } else if *epoch == 0 { 7 } else { 14 }); self.u64(*horizon); self.u32(heads.len() as u32);
                for (q, s, id) in heads { self.u32(*q); self.u32(*s); self.u64(*id) }
                match check { Some((f, h)) => { self.u8(1); self.u64(*f); self.u64(*h) } None => self.u8(0) }
                if *epoch != 0 || fed.is_some() { self.u32(*epoch) }
                if let Some(fa) = fed {
                    self.u64(fa.anchor); self.u32(fa.echo.len() as u32);
                    for (m, b) in &fa.echo { self.u32(*m); self.u64(*b) }
                    if !fa.echo.is_empty() { self.u64(fa.digest) }
                }
            }
            Payload::Vote { epoch, target, seq, frozen, kick } => {
                // A vote that answers no kick keeps tag 8 (and so its id); a kick vote is tag 15.
                self.u8(if *kick == 0 { 8 } else { 15 }); self.u32(*epoch); self.u32(*target); self.u32(*seq); self.u32(frozen.0); self.u64(frozen.1);
                if *kick != 0 { self.u64(*kick) }
            }
            Payload::Kick { epoch, target } => { self.u8(17); self.u32(*epoch); self.u32(*target) }
            Payload::SetRole { epoch, member, role } => { self.u8(18); self.u32(*epoch); self.u32(*member); self.u32(*role as u32) }
            Payload::DefineRole { epoch, role, perms } => { self.u8(19); self.u32(*epoch); self.u32(*role as u32); self.u64(*perms) }
            Payload::Propose { epoch, round, valid_round, change } => {
                self.u8(11); self.u32(*epoch); self.u32(*round); self.u32(*valid_round); self.change(change)
            }
            Payload::Prevote { epoch, round, value } => { self.u8(12); self.u32(*epoch); self.u32(*round); self.u64(*value) }
            Payload::Precommit { epoch, round, value, boundary, polka } => {
                self.u8(13); self.u32(*epoch); self.u32(*round); self.u64(*value); self.u64(*boundary);
                // Review VC4R-1: a capped precommit carries the prevote ids of the polka that justifies
                // it, so a member that did not receive them (an equivocated prevote) can fetch them
                // before it votes the capper out. At most `MAX_ATTESTS` so the size is bounded.
                assert!(polka.len() <= MAX_POLKA, "a precommit carries at most {MAX_POLKA} prevote ids");
                self.u32(polka.len() as u32);
                for id in polka { self.u64(*id) }
            }
        }
        // Flags: bit 0 a carried state follows, bit 1 a checkpoint link follows (after it).
        self.u8(e.carried.is_some() as u8 | ((e.link != 0) as u8) << 1);
        if let Some((obj, s)) = &e.carried {
            self.u32(*obj);
            self.state(s);
        }
    }
    fn change(&mut self, c: &Change) {
        // A change with admin entries or kicks sets the epoch's top bit and carries them; any other
        // keeps the P2-4 layout, so its value is unchanged (D80).
        let ext = !c.admin.is_empty()
            || c.remove.iter().any(|r| r.kick != 0)
            || c.add_roles.iter().any(|&r| r != ADMIN)
            || c.add_attests.iter().any(|a| !a.is_empty())
            || !c.add_reqs.is_empty();
        // Review VCR-11: the top bit marks the layout, so an epoch must stay below it.
        assert!(c.epoch < CHANGE_EXT, "epoch {} does not fit a change's encoding", c.epoch);
        self.u32(if ext { c.epoch | CHANGE_EXT } else { c.epoch });
        self.u32(c.remove.len() as u32);
        for r in &c.remove {
            self.u32(r.member);
            self.u32(r.votes.len() as u32);
            for v in &r.votes { self.u64(*v) }
            self.u32(r.cut.0);
            self.u64(r.cut.1);
            if ext { self.u64(r.kick) }
        }
        self.u32(c.add.len() as u32);
        for m in &c.add { self.u32(*m) }
        self.u64(c.boundary);
        self.u32(c.excluded.len() as u32);
        for (m, p) in &c.excluded { self.u32(*m); self.u64(p.a); self.u64(p.b) }
        if ext {
            self.u32(c.admin.len() as u32);
            for (id, ok) in &c.admin { self.u64(*id); self.u8(*ok as u8) }
            self.u32(c.add_roles.len() as u32);
            for r in &c.add_roles { self.u32(*r as u32) }
            self.u32(c.add_attests.len() as u32);
            for a in &c.add_attests { self.attests(a) }
            self.u32(c.add_reqs.len() as u32);
            for (sig, exp) in &c.add_reqs { self.u64(*sig); self.u64(*exp) }
        }
    }
    pub fn msg(&mut self, m: &Msg) {
        match m {
            Msg::Entry(e) => { self.u8(1); self.entry(e) }
            Msg::Fetch { author, from, to } => { self.u8(3); self.u32(*author); self.u32(*from); self.u32(*to) }
            Msg::Batch(v) => { self.u8(4); self.u32(v.len() as u32); for m in v { self.msg(m) } }
            Msg::FetchValue { value } => { self.u8(5); self.u64(*value) }
            Msg::Join { member, attests, sig, expires } => {
                self.u8(if attests.is_empty() { 6 } else { 7 }); self.u32(*member); self.u64(*sig); self.u64(*expires);
                if !attests.is_empty() {
                    self.attests(attests);
                }
            }
            Msg::FetchEntry { id, back: 0 } => { self.u8(8); self.u64(*id) }
            Msg::FetchEntry { id, back } => { self.u8(9); self.u64(*id); self.u8(*back) }
            Msg::Ckpts { member, after, list, fed, echo } => {
                self.u8(11); self.u32(*member);
                match after { Some(b) => { self.u8(1); self.u64(*b) } None => self.u8(0) }
                self.u32(list.len() as u32);
                for (b, s, r) in list { self.u64(*b); self.u32(*s); self.u64(*r) }
                self.u8(u8::from(*fed) | if echo.is_some() { 2 } else { 0 });
                if let Some(b) = echo { self.u64(*b) }
            }
            Msg::Feed { frame, fed, bodies } => {
                self.u8(10); self.u64(*frame);
                self.u8(fed.len() as u8);
                for m in fed { self.u32(*m) }
                self.u32(bodies.len() as u32);
                for b in bodies {
                    self.u32(b.obj);
                    for &x in &b.pos { self.i32(x) }
                    for &x in &b.rot { self.i16(x) }
                    for &x in &b.lin { self.i16(x) }
                }
            }
        }
    }
}

pub struct Rd<'a>(pub &'a [u8], pub usize);
impl Rd<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.0.get(self.1..self.1 + n)?;
        self.1 += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> { Some(self.take(1)?[0]) }
    fn u32(&mut self) -> Option<u32> { Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
    fn u64(&mut self) -> Option<u64> { Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?)) }
    fn i32(&mut self) -> Option<i32> { Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?)) }
    fn i16(&mut self) -> Option<i16> { Some(i16::from_le_bytes(self.take(2)?.try_into().ok()?)) }
    fn f32(&mut self) -> Option<f32> { Some(f32::from_bits(self.u32()?)) }
    fn cert(&mut self) -> Option<Vec<(MemberId, u64)>> {
        let n = self.u32()? as usize;
        let mut c = Vec::with_capacity(n.min(64));
        for _ in 0..n { c.push((self.u32()?, self.u64()?)) }
        Some(c)
    }
    fn hop(&mut self) -> Option<Hop> {
        let (src, dest, transfer, via, epoch, frame) = (self.u32()?, self.u32()?, self.u64()?, self.u64()?, self.u32()?, self.u64()?);
        let n = self.u32()? as usize;
        if n > MAX_EPOCH_CERTS { return None; }
        let mut certs = Vec::with_capacity(n.min(64));
        for _ in 0..n { certs.push(self.epoch_cert()?) }
        Some(Hop { src, dest, transfer, via, epoch, frame, certs, cert: self.cert()? })
    }
    fn epoch_proof(&mut self) -> Option<EpochProof> {
        let epoch = self.u32()?;
        let n = self.u32()? as usize;
        if n > MAX_EPOCH_CERTS { return None; }
        let mut certs = Vec::with_capacity(n.min(64));
        for _ in 0..n { certs.push(self.epoch_cert()?) }
        Some(EpochProof { epoch, certs })
    }
    fn epoch_cert(&mut self) -> Option<EpochCert> {
        let (world, epoch, change, round) = (self.u32()?, self.u32()?, self.change()?, self.u32()?);
        let n = self.u32()? as usize;
        let mut members = Vec::with_capacity(n.min(256));
        for _ in 0..n { members.push(self.u32()?) }
        let precommits = self.cert()?;
        let n = self.u32()? as usize;
        let mut proofs = Vec::with_capacity(n.min(64));
        for _ in 0..n { proofs.push(self.entry()?) }
        Some(EpochCert { world, epoch, change, round, members, precommits, proofs })
    }
    fn chain(&mut self) -> Option<Vec<Hop>> {
        let n = self.u32()? as usize;
        let mut c = Vec::with_capacity(n.min(64));
        for _ in 0..n { c.push(self.hop()?) }
        Some(c)
    }
    fn attests(&mut self) -> Option<Vec<Attest>> {
        // Review VCR-11: a longer list is refused, never read in part.
        let n = self.u32()? as usize;
        if n > MAX_ATTESTS {
            return None;
        }
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let rel = match self.u8()? { 1 => Rel::Friend, 2 => Rel::Confidant, 3 => Rel::Invited, _ => return None };
            v.push(Attest { rel, with: self.u32()?, sig_with: self.u64()?, sig_joiner: self.u64()? });
        }
        Some(v)
    }
    /// Review VCR-11: a role id is 16 bits; a wider value is refused, never narrowed.
    fn role(&mut self) -> Option<RoleId> {
        RoleId::try_from(self.u32()?).ok()
    }
    fn state(&mut self) -> Option<BodyState> {
        let mut s = [0u32; 16];
        for b in &mut s { *b = self.u32()? }
        Some(BodyState(s))
    }
    fn entry(&mut self) -> Option<Entry> {
        let member = self.u32()?;
        let seq = self.u32()?;
        let prev = self.u64()?;
        let stamp = self.u64()?;
        let payload = match self.u8()? {
            1 => Payload::Touch { obj: self.u32()?, impulse: [self.f32()?, self.f32()?, self.f32()?], grab: self.u8()? != 0 },
            t @ (2 | 21) => {
                let (obj, target_mm, active, span) = (self.u32()?, [self.i32()?, self.i32()?, self.i32()?], self.u8()? != 0, self.u8()?);
                let mut more = Vec::new();
                if t == 21 {
                    // D73(a): a pack holds 2..=PACK_MAX targets; anything else has no canonical encoding.
                    let n = self.u8()? as u64;
                    if n == 0 || n + 1 > PACK_MAX {
                        return None;
                    }
                    let mode = self.u8()?;
                    let mut prev = target_mm;
                    for _ in 0..n {
                        let mut t = [0i32; 3];
                        for (i, c) in t.iter_mut().enumerate() {
                            *c = match mode {
                                1 => prev[i].wrapping_add(self.u8()? as i8 as i32),
                                2 => prev[i].wrapping_add(i16::from_le_bytes(self.take(2)?.try_into().ok()?) as i32),
                                0 => self.i32()?,
                                _ => return None,
                            };
                        }
                        more.push(t);
                        prev = t;
                    }
                }
                Payload::Hand { obj, target_mm, active, span, more }
            }
            3 => {
                let (obj, dest, deadline) = (self.u32()?, self.u32()?, self.u64()?);
                let hop = if self.u8()? == 1 { Some(self.hop()?) } else { None };
                Payload::Depart { obj, dest, deadline, hop }
            }
            4 => {
                let transfer = self.u64()?;
                let record = ObjRecord {
                    global: self.u64()?, kind: self.u8()?, state: self.state()?, src_world: self.u32()?, src_frame: self.u64()?,
                    via: self.u64()?,
                };
                Payload::Arrive { transfer, record, depart: self.u64()?, deadline: self.u64()?, cert: self.cert()?, chain: self.chain()?, proof: self.epoch_proof()? }
            }
            16 => Payload::Provenance { hop: self.hop()? },
            5 => Payload::Verdict { transfer: self.u64()?, accepted: self.u8()? != 0, cert: self.cert()?, frame: self.u64()?, proof: self.epoch_proof()? },
            9 => Payload::Attest { fact: self.u64()?, sig: self.u64()? },
            6 => Payload::Portal { open: self.u8()? != 0 },
            20 => {
                let epoch = self.u32()?;
                let n = self.u32()? as usize;
                if n > MAX_CERT_WORDS { return None; }
                let mut w = Vec::with_capacity(n.min(256));
                for _ in 0..n { w.push(self.u64()?) }
                Payload::Ownership { epoch, cert: Cert::from_words(&w)? }
            }
            tag @ (7 | 14 | 22) => {
                let horizon = self.u64()?;
                let n = self.u32()? as usize;
                let mut heads = Vec::with_capacity(n.min(256));
                for _ in 0..n { heads.push((self.u32()?, self.u32()?, self.u64()?)) }
                let check = if self.u8()? == 1 { Some((self.u64()?, self.u64()?)) } else { None };
                let epoch = if tag != 7 { self.u32()? } else { 0 };
                let fed = if tag == 22 {
                    let anchor = self.u64()?;
                    let k = self.u32()? as usize;
                    let mut echo = Vec::with_capacity(k.min(256));
                    for _ in 0..k { echo.push((self.u32()?, self.u64()?)) }
                    let digest = if k > 0 { self.u64()? } else { 0 };
                    Some(FedAck { anchor, echo, digest })
                } else {
                    None
                };
                Payload::Ack { horizon, heads, check, epoch, fed }
            }
            tag @ (8 | 15) => {
                let (epoch, target, seq, frozen) = (self.u32()?, self.u32()?, self.u32()?, (self.u32()?, self.u64()?));
                let kick = if tag == 15 { self.u64()? } else { 0 };
                Payload::Vote { epoch, target, seq, frozen, kick }
            }
            17 => Payload::Kick { epoch: self.u32()?, target: self.u32()? },
            18 => Payload::SetRole { epoch: self.u32()?, member: self.u32()?, role: self.role()? },
            19 => Payload::DefineRole { epoch: self.u32()?, role: self.role()?, perms: self.u64()? },
            11 => Payload::Propose { epoch: self.u32()?, round: self.u32()?, valid_round: self.u32()?, change: self.change()? },
            12 => Payload::Prevote { epoch: self.u32()?, round: self.u32()?, value: self.u64()? },
            13 => {
                let (epoch, round, value, boundary) = (self.u32()?, self.u32()?, self.u64()?, self.u64()?);
                let n = self.u32()? as usize;
                if n > MAX_POLKA { return None; }
                let mut polka = Vec::with_capacity(n);
                for _ in 0..n { polka.push(self.u64()?) }
                Payload::Precommit { epoch, round, value, boundary, polka }
            }
            _ => return None,
        };
        let flags = self.u8()?;
        if flags > 3 {
            return None;
        }
        let carried = if flags & 1 != 0 { Some((self.u32()?, self.state()?)) } else { None };
        // A link is never 0 on the wire: the flag says there is none.
        let link = if flags & 2 != 0 {
            let l = self.u64()?;
            if l == 0 {
                return None;
            }
            l
        } else {
            0
        };
        Some(Entry { member, seq, prev, stamp, payload, carried, link })
    }
    fn change(&mut self) -> Option<Change> {
        let raw = self.u32()?;
        let (ext, epoch) = (raw & CHANGE_EXT != 0, raw & !CHANGE_EXT);
        let n = self.u32()? as usize;
        let mut remove = Vec::with_capacity(n.min(256));
        for _ in 0..n {
            let member = self.u32()?;
            let k = self.u32()? as usize;
            let mut votes = Vec::with_capacity(k.min(256));
            for _ in 0..k { votes.push(self.u64()?) }
            let cut = (self.u32()?, self.u64()?);
            let kick = if ext { self.u64()? } else { 0 };
            remove.push(Removal { member, votes, cut, kick });
        }
        let n = self.u32()? as usize;
        let mut add = Vec::with_capacity(n.min(256));
        for _ in 0..n { add.push(self.u32()?) }
        let boundary = self.u64()?;
        let n = self.u32()? as usize;
        let mut excluded = Vec::with_capacity(n.min(256));
        for _ in 0..n { excluded.push((self.u32()?, Proof { a: self.u64()?, b: self.u64()? })) }
        let mut admin = Vec::new();
        let mut add_roles = vec![ADMIN; add.len()];
        let mut add_attests = vec![Vec::new(); add.len()];
        let mut add_reqs = Vec::new();
        if ext {
            let n = self.u32()? as usize;
            for _ in 0..n { admin.push((self.u64()?, self.u8()? != 0)) }
            let n = self.u32()? as usize;
            add_roles = Vec::with_capacity(n.min(256));
            for _ in 0..n { add_roles.push(self.role()?) }
            let n = self.u32()? as usize;
            add_attests = Vec::with_capacity(n.min(256));
            for _ in 0..n { add_attests.push(self.attests()?) }
            let n = self.u32()? as usize;
            add_reqs = Vec::with_capacity(n.min(256));
            for _ in 0..n { add_reqs.push((self.u64()?, self.u64()?)) }
        }
        Some(Change { epoch, remove, add, boundary, excluded, admin, add_roles, add_attests, add_reqs })
    }
    pub fn msg(&mut self) -> Option<Msg> {
        Some(match self.u8()? {
            1 => Msg::Entry(self.entry()?),
            3 => Msg::Fetch { author: self.u32()?, from: self.u32()?, to: self.u32()? },
            4 => {
                let n = self.u32()? as usize;
                let mut v = Vec::with_capacity(n.min(256));
                for _ in 0..n { v.push(self.msg()?) }
                Msg::Batch(v)
            }
            5 => Msg::FetchValue { value: self.u64()? },
            tag @ (6 | 7) => {
                let member = self.u32()?;
                let sig = self.u64()?;
                let expires = self.u64()?;
                let attests = if tag == 7 { self.attests()? } else { Vec::new() };
                Msg::Join { member, attests, sig, expires }
            }
            8 => Msg::FetchEntry { id: self.u64()?, back: 0 },
            9 => Msg::FetchEntry { id: self.u64()?, back: self.u8()? },
            11 => {
                let member = self.u32()?;
                let after = if self.u8()? == 1 { Some(self.u64()?) } else { None };
                let n = self.u32()? as usize;
                let mut list = Vec::with_capacity(n.min(256));
                for _ in 0..n { list.push((self.u64()?, self.u32()?, self.u64()?)) }
                let flags = self.u8()?;
                let echo = if flags & 2 != 0 { Some(self.u64()?) } else { None };
                Msg::Ckpts { member, after, list, fed: flags & 1 != 0, echo }
            }
            10 => {
                let frame = self.u64()?;
                let k = self.u8()? as usize;
                let mut fed = Vec::with_capacity(k);
                for _ in 0..k { fed.push(self.u32()?) }
                let n = self.u32()? as usize;
                let mut bodies = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    let obj = self.u32()?;
                    let pos = [self.i32()?, self.i32()?, self.i32()?];
                    let rot = [self.i16()?, self.i16()?, self.i16()?, self.i16()?];
                    let lin = [self.i16()?, self.i16()?, self.i16()?];
                    bodies.push(FeedBody { obj, pos, rot, lin });
                }
                Msg::Feed { frame, fed, bodies }
            }
            _ => return None,
        })
    }
}

pub fn encode_msg(m: &Msg) -> Vec<u8> {
    let mut w = Wr(Vec::with_capacity(128));
    w.msg(m);
    w.0
}
pub fn decode_msg(b: &[u8]) -> Option<Msg> {
    Rd(b, 0).msg()
}

/// Entries that never act on the simulation: acks, attestations and the membership protocol.
pub fn is_protocol(p: &Payload) -> bool {
    matches!(
        p,
        Payload::Ack { .. }
            | Payload::Vote { .. }
            | Payload::Attest { .. }
            | Payload::Provenance { .. }
            | Payload::Propose { .. }
            | Payload::Prevote { .. }
            | Payload::Precommit { .. }
            | Payload::Kick { .. }
            | Payload::SetRole { .. }
            | Payload::DefineRole { .. }
            | Payload::Ownership { .. }
    )
}

/// Same-frame tie-break (user ruling 09:32Z): a per-frame pseudo-random permutation of members.
/// `MemberId`s are assigned in admission order by the membership log, never chosen by the peer,
/// and `seed` comes from the session/epoch genesis, so no key can be ground to win. All 32 bits of
/// the id are hashed (review L7).
pub fn tie_rank(seed: u64, frame: u64, member: MemberId) -> u64 {
    splitmix64(seed ^ frame.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (member as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
}

// ---- R14 proofs (reviews NEW-3..NEW-6) -------------------------------------------------------------

/// Largest linear speed (m/s) and spin (rad/s) an arriving object may carry (review NEW-21).
pub const MAX_ARRIVAL_SPEED: f32 = 200.0;
pub const MAX_ARRIVAL_SPIN: f32 = 2000.0;
/// Furthest deadline, in frames after its arrival, that a destination accepts (round 3, low):
/// it bounds how long a transfer stays open and its id must be kept. World config in production.
pub const MAX_DEADLINE_FRAMES: u64 = 3600;

/// P25-3: the source's certified departure carried by an `Arrive`, as one export of a fork proof,
/// with the source's epoch proof, so a third party checks it from the source's genesis.
#[allow(clippy::too_many_arguments)]
fn arrival_export(src: u32, depart: EntryId, record: &ObjRecord, dest: u32, deadline: u64, chain: u64, cert: &[(MemberId, u64)], proof: &EpochProof) -> Export {
    Export::Departed { src, proof: proof.clone(), depart, record: *record, dest, deadline, chain, cert: cert.to_vec() }
}

/// A transfer this world committed, as its members attest it: (transfer, global id, destination, the
/// transfer the object had entered by, commit frame). The epoch goes into the fact when it is signed.
pub type Commit = (u64, u64, u32, u64, u64);

/// A transfer's id: derived from the source world and its Depart entry, so nobody can squat it.
pub fn transfer_id(src_world: u32, depart: EntryId) -> u64 {
    splitmix64(((src_world as u64) << 40) ^ depart)
}

/// "World `src` confirmed departure `depart` of `record`, bound for world `dest` by `deadline`
/// (a frame of `dest`), with provenance `chain` (its [`chain_hash`]), in its epoch `epoch`." Covers
/// every field of the record (review R3-8), its departure frame `src_frame` included, and the chain
/// (P2-5), so a relayer can neither strip nor swap the chain, nor relabel the epoch.
pub fn fact_departed(src: u32, depart: EntryId, record: &ObjRecord, dest: u32, deadline: u64, chain: u64, epoch: u32) -> u64 {
    fnv_u64s([
        1,
        epoch as u64,
        record.via,
        chain,
        src as u64,
        depart,
        record.global,
        record.kind as u64,
        record.state.hash(),
        record.src_world as u64,
        record.src_frame,
        dest as u64,
        deadline,
    ])
}

/// "World `dst` confirmed transfer `transfer` as accepted / rejected at its frame `frame`, in its
/// epoch `epoch`."
pub fn fact_verdict(dst: u32, transfer: u64, accepted: bool, frame: u64, epoch: u32) -> u64 {
    fnv_u64s([2, dst as u64, transfer, accepted as u64, frame, epoch as u64])
}

/// Spike model of member `member` of world `world` signing `fact`. Production: an Ed25519
/// signature by the member's key, checked against the other world's member keys (D46).
pub fn member_sig(world: u32, member: MemberId, fact: u64) -> u64 {
    splitmix64(fact ^ splitmix64((((world as u64) << 32) | member as u64) ^ 0x5157_4E41_5455_5245))
}

/// True if `cert` holds valid signatures over `fact` from a strict majority of `members`.
pub fn cert_ok(members: &[MemberId], world: u32, fact: u64, cert: &[(MemberId, u64)]) -> bool {
    let signers: BTreeSet<MemberId> =
        cert.iter().filter(|(m, sig)| members.contains(m) && *sig == member_sig(world, *m, fact)).map(|(m, _)| *m).collect();
    !members.is_empty() && signers.len() * 2 > members.len()
}

// ---- scenes --------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum SceneKind {
    /// The phase-1 pile: ground plus side*side*layers boxes/spheres/capsules.
    Pile { side: usize, layers: usize },
    /// A walled pit of small balls plus one kinematic hand per listed member.
    BallPit { balls: usize, hands: usize },
    /// No bodies: a log-only world, for membership measurements at scale (P2-4).
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Obj {
    pub body: BodyId,
    pub global: u64,
    pub kind: u8,
}

const BALL_R: f32 = 0.06;
pub const CLAIM_COOLDOWN: u64 = 10;
const HAND_R: f32 = 0.09;
/// Dead-reckoning speed cap for a hand without an entry (m/s).
const HAND_COAST_VMAX: f32 = 3.0;

fn add_shape(world: &mut World, body: BodyId, kind: u8) {
    let mut sd = default_shape_def();
    sd.density = 1.0;
    match kind {
        0 => {
            let h = make_box_hull(0.4, 0.4, 0.4);
            create_hull_shape(world, body, &sd, &h.base);
        }
        1 => {
            let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: 0.45 };
            create_sphere_shape(world, body, &sd, &s);
        }
        2 => {
            let c = Capsule {
                center1: Vec3 { x: -0.3, y: 0.0, z: 0.0 },
                center2: Vec3 { x: 0.3, y: 0.0, z: 0.0 },
                radius: 0.25,
            };
            create_capsule_shape(world, body, &sd, &c);
        }
        3 => {
            sd.density = 0.2;
            sd.base_material.rolling_resistance = 0.2;
            sd.base_material.friction = 0.6;
            let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: BALL_R };
            create_sphere_shape(world, body, &sd, &s);
        }
        _ => {
            let s = Sphere { center: Vec3 { x: 0.0, y: 0.0, z: 0.0 }, radius: HAND_R };
            create_sphere_shape(world, body, &sd, &s);
        }
    }
}

fn static_box(world: &mut World, p: Pos, hx: f32, hy: f32, hz: f32) {
    let mut d = default_body_def();
    d.type_ = BodyType::Static;
    d.position = p;
    let b = create_body(world, &d);
    let hull = make_box_hull(hx, hy, hz);
    create_hull_shape(world, b, &default_shape_def(), &hull.base);
}

/// Builds the scene. `world_tag` makes global object ids unique across worlds (R14).
pub fn build_scene(kind: SceneKind, world_tag: u32) -> (World, Vec<Option<Obj>>) {
    let g = |i: usize| ((world_tag as u64) << 32) | i as u64;
    match kind {
        SceneKind::Pile { side, layers } => {
            let s = crate::build_pile_scene(side, layers);
            let objs = s.bodies.iter().enumerate().map(|(i, &b)| Some(Obj { body: b, global: g(i), kind: (i % 3) as u8 })).collect();
            (s.world, objs)
        }
        SceneKind::BallPit { balls, hands } => {
            let mut world = World::new(&crate::engine_world_def());
            static_box(&mut world, Pos { x: 0.0, y: -0.5, z: 0.0 }, 2.0, 0.5, 2.0);
            let (w, h) = (1.2f32, 0.6f32);
            static_box(&mut world, pos(w + 0.1, h, 0.0), 0.1, h, w + 0.2);
            static_box(&mut world, pos(-w - 0.1, h, 0.0), 0.1, h, w + 0.2);
            static_box(&mut world, pos(0.0, h, w + 0.1), w + 0.2, h, 0.1);
            static_box(&mut world, pos(0.0, h, -w - 0.1), w + 0.2, h, 0.1);
            let mut objs = Vec::with_capacity(balls + hands);
            let per_row = 16usize;
            let spacing = 2.0 * w / per_row as f32;
            for i in 0..balls {
                let (ix, iz, iy) = (i % per_row, (i / per_row) % per_row, i / (per_row * per_row));
                let jitter = (i as f32 * 0.618_034).fract() * 0.02 - 0.01;
                let mut d = default_body_def();
                d.type_ = BodyType::Dynamic;
                d.linear_damping = 0.1;
                d.angular_damping = 0.3;
                d.position = pos(
                    -w + spacing * (ix as f32 + 0.5) + jitter,
                    BALL_R + 0.01 + iy as f32 * (2.0 * BALL_R + 0.01),
                    -w + spacing * (iz as f32 + 0.5) - jitter,
                );
                let b = create_body(&mut world, &d);
                add_shape(&mut world, b, 3);
                objs.push(Some(Obj { body: b, global: g(i), kind: 3 }));
            }
            for k in 0..hands {
                let mut d = default_body_def();
                d.type_ = BodyType::Kinematic;
                d.position = pos(-0.8 + 1.6 * k as f32, 1.5, 0.0);
                let b = create_body(&mut world, &d);
                add_shape(&mut world, b, 4);
                objs.push(Some(Obj { body: b, global: g(balls + k), kind: 4 }));
            }
            (world, objs)
        }
        SceneKind::Empty => (World::new(&crate::engine_world_def()), Vec::new()),
    }
}

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
    /// Active hands: frames since the hand's last entry, that entry's `span`, and whether it continues
    /// a stream, so the hand may coast past its span (dead reckoning, `SimConfig::hand_predict`).
    pub hand_idle: BTreeMap<u32, (u64, u8, bool)>,
    /// Thin links Part B, rule 2: per object, per author, the carried-state writes applied to it (seq ->
    /// stamp), the last 2 speculation windows of them.
    pub writes: BTreeMap<u32, BTreeMap<MemberId, BTreeMap<u32, u64>>>,
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
    /// Carry-over (measurement): rule 1's verdict passes (`decide_verdicts` calls).
    pub verdict_passes: u64,
    /// Members this replica presumed crashed and voted to remove (review R3-2).
    pub suspicions: u64,
    /// Members this replica voted to demote because their link could not keep up (review R3-9).
    pub demotions: u64,
    /// View changes this replica decided, those that changed no member (resets), and the highest
    /// round any of them needed (P2-4).
    pub view_changes: u64,
    pub resets: u64,
    pub max_round: u32,
    /// D80: kicks and role entries the decided changes applied, and those they refused.
    pub admin_applied: u64,
    pub admin_refused: u64,
    /// Thin links Part B, rule 2: carried-state writes skipped as stale (counted at every simulation of
    /// their frame, re-simulations included).
    pub stale_skips: u64,
    /// Thin links Part B, rule 1's echo: fed members' echoes whose digest matched my version (credited), and
    /// those that didn't (production asks for the full signed heads, 7.6).
    pub echoes_matched: u64,
    pub echo_mismatches: u64,
    /// Thin links Part B, rule 4's exception: fed members this replica voted out below the floors.
    pub floor_removals: u64,
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
    /// Remote-hand dead reckoning (D52): once an active hand's last entry's span has run out, it
    /// keeps its last commanded velocity, at most 3 m/s, for up to this many frames, then holds.
    /// 0 = hold once the span ends. Default 6.
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
    /// Thin links, Part B (thin-link plan revision 17): localized verdicts and stale-write skips for
    /// sessions of 4 or more members. Off keeps today's rules; the spike switches it on per scenario
    /// while Part B is built.
    pub localize: bool,
    /// Part B, rule 1's echo: receivers credit a fed member's echo (on by default; off only to measure what
    /// it carries).
    pub echo: bool,
    /// Part B, section 3, in frames: Q's limits (admin entries in production), the floor F under every
    /// deadline, and an acceptable connection's jitter and delay limits and its re-fetch allowance R.
    pub q_min: u64,
    pub q_max: u64,
    pub floor_f: u64,
    pub j_acc: u64,
    pub y_acc: u64,
    pub refetch_r: u64,
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
            localize: false,
            echo: true,
            q_min: 4,
            q_max: 45,
            floor_f: 32,
            j_acc: 3,
            y_acc: 9,
            refetch_r: 16,
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
    /// Rounds whose proposal this replica already saw a quorum prevote.
    polka_done: BTreeSet<u32>,
    /// Armed timeouts: round -> clock frame armed.
    prevote_timer: BTreeMap<u32, u64>,
    precommit_timer: BTreeMap<u32, u64>,
    /// Review VC3R-2: value -> my present when its proposal by the round's proposer first reached me.
    seen_at: BTreeMap<u64, u64>,
    /// Review VC4R-1: (round, member) -> the id of that member's first prevote this replica holds, so a
    /// precommitter can name the prevote ids of the polka it saw.
    prevote_id: BTreeMap<(u32, MemberId), EntryId>,
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
    /// P25-4): (commits to attest, fork proofs found).
    frame_effects: BTreeMap<u64, (Vec<Commit>, Vec<ForkProof>)>,
    /// Confirmed commits. Each is attested once per epoch label: if a later membership change moves
    /// its frame into another epoch, members attest the fact again with that epoch (VC-7). Kept for
    /// the world's life in the spike; production drops one once its epoch can no longer change [I].
    commits: Vec<Commit>,
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
    /// Part B, section 3: (referrer, author) -> head seq -> (the referrer's first ack that reached it:
    /// its seq and stamp; the head's id). First write wins, where `refs` keeps the latest.
    first_ref: BTreeMap<(MemberId, MemberId), BTreeMap<u32, (u32, u64, EntryId)>>,
    /// Part B, rules 1 and 2: (acker, author) -> ack seq -> the head of the author's chain that ack named,
    /// with its id.
    ack_head_hist: BTreeMap<(MemberId, MemberId), BTreeMap<u32, (u32, EntryId)>>,
    /// Part B, rule 1: per acker, ack stamp -> the highest seq of its acks with that stamp.
    ack_by_stamp: BTreeMap<MemberId, BTreeMap<u64, u32>>,
    /// Part B, rule 1: the verdict of each judged entry, once decided (true: it counts), and the entries
    /// still without one, by stamp.
    pub verdicts: BTreeMap<EntryId, bool>,
    pending: BTreeSet<(u64, EntryId)>,
    /// Part B, rule 1: entries void because q members provably lacked them at their deadline.
    late_void: BTreeSet<EntryId>,
    /// Part B: windows whose values a decision used before they were final (rule 2 at speculation).
    provisional: BTreeMap<u64, WindowVals>,
    /// Part B: an offline replay, which reads the live replica's window values and computes none.
    replaying: bool,
    /// Part B, rule 2: each carried-state entry's decision at its frame's latest simulation (true: skipped).
    pub rule2: BTreeMap<EntryId, bool>,
    /// Part B, section 3: (window, author) -> the seqs of the author's entries stamped in that window.
    win_seqs: BTreeMap<(u64, MemberId), BTreeSet<u32>>,
    /// Part B, section 3: each window's values, once final, and each settled window's data.
    windows: BTreeMap<u64, WindowVals>,
    window_data_cache: BTreeMap<u64, localize::WindowData>,
    /// Part B, rule 5: this replica is on the state feed (set by the harness, never logged).
    fed: Option<feed::Fed>,
    /// Part B (TLP-27): per author, the checkpoint state after the newest entry of its chain I validated: the
    /// newest checkpoint's block and id.
    ckpt: BTreeMap<MemberId, (u64, EntryId)>,
    /// Part B (TLP-27): the checkpoint state after each validated entry, by its id. An entry's expected link
    /// follows from its prev's state, so each version of a forked chain is judged on its own line.
    ckpt_after: BTreeMap<EntryId, (u64, EntryId)>,
    /// Part B (TLP-19): my own chain's checkpoint state, kept at publish (a joiner validates its own chain only
    /// once admitted).
    my_ckpt: Option<(u64, EntryId)>,
    /// Part B (TLP-23): every checkpoint in my version, by author and block: its seq, id and rest-hash.
    ckpt_by_block: BTreeMap<(MemberId, u64), (u32, EntryId, u64)>,
    /// Part B, rule 1's echo: the checkpoints a fed member's echo credits as its references, once its digest
    /// matched my version. Read only by echo-stability (`matched_ref`): never holders, D54 stability or votes.
    echo_refs: BTreeMap<(MemberId, MemberId), BTreeMap<u32, (EntryId, u32)>>,
    /// A fed member's newest echo not yet checked (I don't hold every checkpoint it names yet).
    /// Each fed member's echoes not yet checked, by the seq of the ack that carried each (every one is checked: keeping
    /// only the newest left a replica that caught up in a burst without the older ones, so the final term's references
    /// differed between replicas; leave-s4-rejoin's spectator cut member 2 at 58, the members at 96).
    echo_pending: BTreeMap<MemberId, BTreeMap<u32, (Vec<(MemberId, u64)>, u64)>>,
    /// Members my log shows on the feed (their newest ack carries an anchor), with the block of their echo
    /// of my chain (TLP-23).
    fed_peers: BTreeMap<MemberId, Option<u64>>,
    /// TLP-23: per fed member, the newest of my checkpoint blocks I sent it and the frame I sent it at.
    ckpt_sent: BTreeMap<MemberId, (u64, u64)>,
    /// Rule 4's floor test: the frame I first sent each of my checkpoint blocks to each fed member.
    ckpt_first_sent: BTreeMap<(MemberId, u64), u64>,
    /// Part B at 3 players (S-1): per member, the seqs of its acks that carry an anchor (it was fed).
    anchored_acks: BTreeMap<MemberId, BTreeSet<u32>>,
    /// The fed side (TLP-24): per member, the newest checkpoint of its chain I echo, its block and id. Kept
    /// across leaving and re-entering the feed.
    echo_last: BTreeMap<MemberId, (u64, EntryId)>,
    /// Rev 22 (TLP-24): the seq of each chain's last echoed checkpoint, so a fed member's vote can freeze it.
    echo_seq: BTreeMap<MemberId, u32>,
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
    /// (spike diagnostics) Each Part B removal's cut as finalized here: `localize::CutRecord`.
    pub cut_records: BTreeMap<MemberId, localize::CutRecord>,
    /// Carry-over, hole 2 (rev 22 part 3): each removed member's B, the seq bound its decided removal fixes: the
    /// highest frozen head of its chain among the votes the change lists (a leave lists its own). Its acks past B never
    /// count, so its last epoch's acks end in the log. A seq bound only: the version that counts is the echo-stable one.
    bound: BTreeMap<MemberId, u32>,
    /// Carry-over (measurement): per-epoch reference lookups (`ref_in` calls), each one range search of a
    /// referencer's ack history.
    pub ref_lookups: std::cell::Cell<u64>,
    /// Carry-over: (member, epoch) -> its vouched prefix in that epoch, once that epoch's acks are all in here.
    final_vouch: BTreeMap<(MemberId, u32), u32>,
    /// Revision 22: since when my latency leave has deferred to another fed member (at most one window).
    latency_defer_since: Option<u64>,
    /// (diagnostics) Latency leaves a change I built left out, logged once each.
    latency_left_out: BTreeSet<EntryId>,
    /// The view change of the current epoch.
    vc: Vc,
    /// View-change entries (votes, proposals, prevotes, precommits) of later epochs, processed on
    /// entering their epoch.
    vc_later: BTreeMap<u32, Vec<EntryId>>,
    /// Every proposal entry seen, by value (answers `Msg::FetchValue`).
    proposal_ids: BTreeMap<u64, EntryId>,
    /// Review PF-1 and VC7R-6: proposal entries of later epochs kept by value before their epoch began
    /// (another version, or a proven forker's past its fork), noted on entering it.
    later_proposals: BTreeMap<u32, Vec<EntryId>>,
    /// Review PF-1: every signed precommit held for a value, any version, linked into its author's
    /// chain or not: (epoch, round, value) to the members that signed one. Only proven members are
    /// counted from it (`precommitters`); everyone else counts in chain order.
    pc_held: BTreeMap<(u32, u32, u64), BTreeSet<MemberId>>,
    /// Review VC8R-1: every signed prevote held for a value, any version, linked or not, as `pc_held`
    /// is for precommits. Only proven members are counted from it (`prevoters`).
    pv_held: BTreeMap<(u32, u32, u64), BTreeSet<MemberId>>,
    /// 7.6 reduction 2: the newest stamp of any entry held other than acks, void or not. The member is
    /// idle while it is below `conf` (`idle`).
    busy_stamp: u64,
    /// Decided changes, one per epoch boundary: (change, round, precommit signatures).
    decided: Vec<(Change, u32, Vec<(MemberId, u64)>)>,
    /// Decisions: (epoch reached, my clock frame when I decided, round, members removed).
    pub vc_log: Vec<(u32, u64, u32, Vec<MemberId>)>,
    /// Test fixture: author no vote, proposal, prevote or precommit (a member that withholds them).
    pub withhold_vc: bool,
    /// Rev 22 delta (proposal), a fed member's: the fed set in its feeder's latest packet (contact only); when each
    /// peer's own fed Ckpts last reached it, and the echo of my chain that peer last reported in them.
    feeder_fed: BTreeSet<MemberId>,
    /// Spike measurement (option 2, m9d40b0d9a9d7): when a fed member's next echo ack is due after a Ckpts advanced
    /// its echo (PE_ECHO_ACK=B batches B frames); the harness clears it when it authors an ack.
    pub echo_ack_at: Option<u64>,
    fed_heard: BTreeMap<MemberId, u64>,
    fed_echo_of_me: BTreeMap<MemberId, Option<u64>>,
    /// Non-members this replica sends its entries to: members that asked to join, or followers.
    pub followers: BTreeSet<MemberId>,
    /// Members that asked to join and are not admitted yet.
    join_requests: BTreeSet<MemberId>,
    /// Review VC4R-2: per pending joiner, its signed request (signature, expiry) with the latest expiry.
    join_reqs: BTreeMap<MemberId, (u64, u64)>,
    /// Review VC4R-6: this session's id ([`session_id`]).
    session: u64,
    /// Admitted members: the first frame each counts for.
    pub admitted: BTreeMap<MemberId, u64>,
    /// This epoch: members that precommitted a value adding members, at seq, with its boundary. Their
    /// later acks count only below the boundary, until they reach a later epoch (10.8).
    join_caps: BTreeMap<MemberId, (u32, u64)>,
    /// Per member: the first seq of its chain whose ack declares each epoch after 0.
    ack_epoch_from: BTreeMap<MemberId, BTreeMap<u32, u32>>,
    /// F2 (revision 21 (a)): each removed member's first epoch as a non-member.
    removed_in: BTreeMap<MemberId, u32>,
    /// F2: removed members whose Part B cut isn't final yet (`finalize_cuts`).
    pb_pending: BTreeSet<MemberId>,
    /// Diagnostics: the last second of my present traced (PE_CONF_TRACE).
    dbg_conf_at: u64,
    /// Rev 22: each member's leave request in this epoch, taken by value (a vote on itself without a kick).
    leaves: BTreeMap<MemberId, EntryId>,
    /// Rule 4 (D): bytes reaching me directly from each member, by my present's frame (`note_rx`).
    rx_bytes: BTreeMap<MemberId, BTreeMap<u64, u64>>,
    /// Rule 4 (D): when I first saw each fed member's ack say it is fed (cleared when it leaves the feed).
    fed_seen: BTreeMap<MemberId, u64>,
    /// F2: every validated vote by (target, voter): (its epoch, its seq, its frozen head's seq), the first per epoch.
    vote_hist: BTreeMap<(MemberId, MemberId), Vec<(u32, u32, u32)>>,
    /// Test fixture (D80 bridge): heads this member's acks and votes report at most.
    hidden: Option<BTreeMap<MemberId, (u32, EntryId)>>,
    /// Test fixture (D80 bridge): a faulty member that backs every vote another member casts,
    /// prevotes and precommits every proposal it sees, in every round, echoes the nil votes of
    /// rounds it holds no proposal for, never proposes, and never applies a decision, so it can
    /// back both sides of a partition in one epoch.
    pub byz_ally: bool,
    /// Test fixture (review VCR-7): with `byz_ally`, prevote every proposal but never precommit.
    pub byz_no_precommit: bool,
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
    /// separate from the session's admins. A world without an ownership chain (`book`) lists them
    /// here; one with a chain takes them from it.
    pub owners: BTreeSet<MemberId>,
    /// World ownership (12.5): the world's ownership chain. Governance, like roles: changed only by
    /// the certificates decided view changes commit, never rewound by a rollback.
    pub book: Option<Book>,
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
    /// V3 (user 08:31Z): the members the close removed, every one left; everyone is told (the
    /// `SESSION CLOSED` event) and no further change is made to the world. The spike keeps its member
    /// list for its own bookkeeping and records the removal here; pe-session empties the list.
    pub closed_members: Vec<MemberId>,
    /// Test fixture: review fixes switched off (see [`fix`]).
    pub fixes_off: u64,
    /// Test fixture (review VC-5): the epoch this member's acks declare, whatever epoch it is in.
    pub fake_ack_epoch: Option<u32>,
    /// VC-2: a removed member's chain is being switched to its cut's branch; confirmation waits.
    adopting: bool,
    /// Review VCR-4: per member, the epochs reached by resets that overruled one of its votes or caps.
    pub strikes: BTreeMap<MemberId, Vec<u32>>,
    /// Review VCR-8: admin entries a decided change lists and this replica lacks: (member to ask, id,
    /// how many predecessors too), and every pair asked so far.
    entry_wants: Vec<(MemberId, EntryId, u8)>,
    entry_asked: BTreeSet<(MemberId, EntryId)>,
    /// Review VC4R-4: (holder, id) -> (when last asked, current retry interval), in ms.
    entry_backoff: BTreeMap<(MemberId, EntryId), (u64, u64)>,
    /// Review VC4R-3: per member suspected silent or slow, my present when it first was, so the fetch
    /// exemption that spares it while I chase another member's claim about it is bounded in time.
    suspect_since: BTreeMap<MemberId, u64>,
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
    /// D73(a): this member's own hand poses waiting for their pack, by (frame, hand): applied at their
    /// frames on this replica only (`step_frame`). Set by the harness.
    pub own_pending: BTreeMap<(u64, u32), [i32; 3]>,
    /// D73(a): while a pack is pending, its first frame: this member's acks declare a horizon below
    /// it, so nothing it confirms can miss the pack.
    pub hold_horizon_below: Option<u64>,
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
            commits: Vec::new(),
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
            ack_stamps: BTreeMap::new(),
            refs: BTreeMap::new(),
            stable_hi: BTreeMap::new(),
            first_ref: BTreeMap::new(),
            ack_head_hist: BTreeMap::new(),
            ack_by_stamp: BTreeMap::new(),
            verdicts: BTreeMap::new(),
            pending: BTreeSet::new(),
            late_void: BTreeSet::new(),
            provisional: BTreeMap::new(),
            replaying: false,
            rule2: BTreeMap::new(),
            win_seqs: BTreeMap::new(),
            windows: BTreeMap::new(),
            window_data_cache: BTreeMap::new(),
            fed: None,
            ckpt: BTreeMap::new(),
            ckpt_after: BTreeMap::new(),
            my_ckpt: None,
            ckpt_by_block: BTreeMap::new(),
            echo_refs: BTreeMap::new(),
            echo_pending: BTreeMap::new(),
            fed_peers: BTreeMap::new(),
            ckpt_sent: BTreeMap::new(),
            ckpt_first_sent: BTreeMap::new(),
            anchored_acks: BTreeMap::new(),
            echo_last: BTreeMap::new(),
            echo_seq: BTreeMap::new(),
            heard: BTreeMap::new(),
            lagging: BTreeMap::new(),
            proofs: BTreeMap::new(),
            liars: BTreeMap::new(),
            frozen: BTreeMap::new(),
            votes: BTreeMap::new(),
            evicted: BTreeMap::new(),
            cut_ids: BTreeMap::new(),
            cut_done: BTreeSet::new(),
            cut_records: BTreeMap::new(),
            bound: BTreeMap::new(),
            ref_lookups: std::cell::Cell::new(0),
            final_vouch: BTreeMap::new(),
            latency_defer_since: None,
            latency_left_out: BTreeSet::new(),
            vc: Vc::default(),
            vc_later: BTreeMap::new(),
            proposal_ids: BTreeMap::new(),
            later_proposals: BTreeMap::new(),
            pc_held: BTreeMap::new(),
            pv_held: BTreeMap::new(),
            busy_stamp: 0,
            decided: Vec::new(),
            vc_log: Vec::new(),
            withhold_vc: false,
            feeder_fed: BTreeSet::new(),
            echo_ack_at: None,
            fed_heard: BTreeMap::new(),
            fed_echo_of_me: BTreeMap::new(),
            byz_ally: false,
            byz_no_precommit: false,
            hidden: None,
            followers: BTreeSet::new(),
            join_requests: BTreeSet::new(),
            join_reqs: BTreeMap::new(),
            session: session_id(world_id, members),
            admitted: BTreeMap::new(),
            join_caps: BTreeMap::new(),
            ack_epoch_from: BTreeMap::new(),
            removed_in: BTreeMap::new(),
            pb_pending: BTreeSet::new(),
            dbg_conf_at: u64::MAX,
            rx_bytes: BTreeMap::new(),
            leaves: BTreeMap::new(),
            fed_seen: BTreeMap::new(),
            vote_hist: BTreeMap::new(),
            genesis: members.to_vec(),
            roles: members.iter().map(|&m| (m, ADMIN)).collect(),
            role_defs: BTreeMap::new(),
            join_rules: JoinRules::default(),
            join_attests: BTreeMap::new(),
            owners: BTreeSet::new(),
            book: None,
            admin_pending: BTreeSet::new(),
            own_held: BTreeSet::new(),
            admin_outcomes: BTreeMap::new(),
            closed_at: None,
            closed_members: Vec::new(),
            fixes_off: 0,
            fake_ack_epoch: None,
            adopting: false,
            strikes: BTreeMap::new(),
            entry_wants: Vec::new(),
            entry_asked: BTreeSet::new(),
            entry_backoff: BTreeMap::new(),
            suspect_since: BTreeMap::new(),
            admitted_epoch: BTreeMap::new(),
            vote_hold: BTreeMap::new(),
            hold_n: BTreeMap::new(),
            fake_refs: None,
            byz_change: None,
            quiet_until_member: false,
            value_wants: Vec::new(),
            value_asked: BTreeSet::new(),
            wants: Vec::new(),
            conf: 0,
            now: 0,
            my_seq: 0,
            horizon_lag: 0,
            own_pending: BTreeMap::new(),
            hold_horizon_below: None,
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

    // ---- snapshots -------------------------------------------------------------------------------

    fn save(&mut self) -> Keyframe {
        let t0 = now_us();
        let mut buf = RecBuffer::new();
        serialize_world(&self.world, &mut buf, &mut self.registry);
        if self.registry.entries.len() != self.slot_count {
            self.slots = self.registry.to_slots();
            self.slot_count = self.registry.entries.len();
        }
        self.stats.saves += 1;
        self.stats.save_us += now_us() - t0;
        Keyframe { image: buf.data, t: self.t.clone() }
    }

    fn restore(&mut self, k: &Keyframe, frame: u64) {
        let t0 = now_us();
        assert!(deserialize_into_shell(&k.image, &mut self.world, &mut self.slots), "restore failed");
        self.t = k.t.clone();
        self.frame = frame;
        self.stats.restore_us += now_us() - t0;
    }

    /// Bytes of world image the ring holds now (the memory side of the snapshot knob).
    pub fn ring_bytes(&self) -> u64 {
        self.ring.values().map(|k| k.image.len() as u64).sum()
    }

    /// What the ring would hold as one full base image plus block deltas: (full bytes, bytes if
    /// every later image were stored as a delta against the base, bytes if each were stored as a
    /// delta against the previous image), with 64-byte blocks.
    pub fn ring_delta_bytes(&self) -> (u64, u64, u64) {
        let imgs: Vec<&[u8]> = self.ring.values().map(|k| k.image.as_slice()).collect();
        let Some(base) = imgs.first() else { return (0, 0, 0) };
        let full = imgs.iter().map(|i| i.len() as u64).sum();
        let vs_base = base.len() as u64 + imgs[1..].iter().map(|i| block_delta(base, i, 64)).sum::<u64>();
        let vs_prev = base.len() as u64 + imgs.windows(2).map(|w| block_delta(w[0], w[1], 64)).sum::<u64>();
        (full, vs_base, vs_prev)
    }

    /// The memory/CPU knob (design 6.7): any value, any time. Takes effect at the next keyframe
    /// decision and trim; never changes a result.
    pub fn set_snap(&mut self, p: SnapPolicy) {
        self.cfg.snap = p;
    }

    fn wants_keyframe(&self, f: u64) -> bool {
        if self.ring.contains_key(&f) {
            return false;
        }
        if self.ring.is_empty() {
            return true;
        }
        match self.cfg.snap {
            SnapPolicy::Every(k) => f % k == 0,
            // During a re-simulation, skip images the thinning would drop on arrival.
            SnapPolicy::Log(k) => f % k == 0 && f % log_grid(k, self.resim_to.max(self.frame) - f) == 0,
            SnapPolicy::Base(k) => f % k == 0 && f > self.conf && self.ring.range(self.conf + 1..).next().is_none(),
            SnapPolicy::Window(k) => f % k == 0 && self.resim_to.max(self.frame) - f < 4 * k,
        }
    }

    /// Hash of the full world image plus tables: used at confirmed check frames (review H4).
    pub fn image_hash(&mut self) -> u64 {
        // A check-hash serialization is not a keyframe: keep it out of the snapshot counters.
        let (n, us) = (self.stats.saves, self.stats.save_us);
        let k = self.save();
        (self.stats.saves, self.stats.save_us) = (n, us);
        // Review NEW-7: after a destroy, a world that stepped continuously and one restored from a
        // keyframe hold the same physics but different image bytes. Hash the image of a scratch
        // world restored from this one instead, so both paths hash alike.
        let scratch = self.scratch.get_or_insert_with(|| World::new(&crate::engine_world_def()));
        assert!(deserialize_into_shell(&k.image, scratch, &mut self.slots), "canonical restore failed");
        let mut buf = RecBuffer::new();
        serialize_world(scratch, &mut buf, &mut self.registry);
        fnv_bytes(&buf.data) ^ self.tables_hash().rotate_left(17)
    }

    fn tables_hash(&self) -> u64 {
        let mut v = vec![self.frame];
        for (i, o) in self.t.objs.iter().enumerate() {
            if let Some(o) = o {
                v.extend([i as u64, o.global, o.kind as u64]);
            }
        }
        for (o, c) in &self.t.claims {
            v.extend([*o as u64, c.member as u64, c.until]);
        }
        for (o, (m, u)) in &self.t.cooldown {
            v.extend([*o as u64, *m as u64, *u, 7]);
        }
        for (t, e) in &self.t.escrow {
            v.extend([*t, e.obj as u64, e.status as u64, e.depart_frame, e.record.map_or(0, |r| r.state.hash())]);
        }
        for (t, a) in &self.t.arrivals {
            match a {
                Arrival::Accepted { obj, frame } => v.extend([*t, 1, *obj as u64, *frame]),
                Arrival::Rejected { frame, until } => v.extend([*t, 2, *frame, *until]),
            }
        }
        for (g, s) in &self.t.seen {
            s.hash_into(*g, &mut v);
        }
        v.push(self.t.portal_open as u64);
        for (o, (n, span, stream)) in &self.t.hand_idle {
            v.extend([*o as u64, *n, *span as u64, *stream as u64, 11]);
        }
        // Thin links Part B, rule 2's write index (empty with Part B off).
        for (o, ws) in &self.t.writes {
            for (m, seqs) in ws {
                for (s, st) in seqs {
                    v.extend([*o as u64, *m as u64, *s as u64, *st, 13]);
                }
            }
        }
        fnv_u64s(v)
    }

    /// Per-frame hash: Box3D body transforms + velocities, plus every engine table.
    pub fn state_hash(&self) -> u64 {
        hash_world_state(&self.world) ^ self.tables_hash().rotate_left(29)
    }

    pub fn obj_state(&self, obj: u32) -> Option<BodyState> {
        self.t.objs.get(obj as usize).copied().flatten().map(|o| BodyState::read(&self.world, o.body))
    }

    // ---- log -------------------------------------------------------------------------------------
    //
    // Round-2 protocol (design 10.8; reviews NEW-1, NEW-2; user rule 14:31Z):
    // * every input to a deterministic decision is an entry in its author's hash chain:
    //   perturbations, acks (horizon, holdings, check hash) and eviction votes;
    // * an entry's frame is its stamp. Nothing is ever re-stamped, so an author never sees its own
    //   interaction corrected; lateness is paid by the late receiver's rollback;
    // * validity is chain-local: an entry is void iff its own chain breaks a rule, so every holder
    //   decides the same;
    // * confirmation uses only stable data: a seq of a chain is stable once every member's chain
    //   references it with the same id (Wuu-Bernstein's 2-D time table, over signed hash chains).

    pub fn entry(&self, id: EntryId) -> Option<&Entry> {
        self.entries.get(&id)
    }

    pub fn effective_frame(&self, id: EntryId) -> Option<u64> {
        self.eff.get(&id).copied()
    }

    fn is_void(&self, id: EntryId) -> bool {
        self.violations.contains(&id) || self.disputed.contains(&id) || self.late_void.contains(&id)
    }

    /// Apply or withdraw an entry's effect on the sim according to its current verdict.
    fn set_eff(&mut self, id: EntryId) {
        let e = &self.entries[&id];
        // A member that asked to join acts only once admitted (10.8): until then nothing it signs
        // touches the simulation.
        let counted = e.member == LOGIC || self.members.contains(&e.member) || self.evicted.contains_key(&e.member);
        let acts = counted && !is_protocol(&e.payload);
        let open = self.closed_at.is_none_or(|c| e.stamp < c);
        let new = if acts && open && !self.is_void(id) { Some(e.stamp) } else { None };
        let old = self.eff.get(&id).copied();
        if old == new {
            return;
        }
        // Diagnostics: the entry whose change of effect is about to break the confirmed-frame invariant.
        if old.is_some_and(|o| o < self.conf) || new.is_some_and(|n| n < self.conf) {
            let e = &self.entries[&id];
            eprintln!(
                "DIRTY at m{} (conf {}, frame {}): m{} s{} stamp {} {:?}: eff {:?} -> {:?}; violation {} disputed {} late_void {} verdict {:?} pending {} evicted {:?}",
                self.me,
                self.conf,
                self.frame,
                e.member,
                e.seq,
                e.stamp,
                std::mem::discriminant(&e.payload),
                old,
                new,
                self.violations.contains(&id),
                self.disputed.contains(&id),
                self.late_void.contains(&id),
                self.verdicts.get(&id),
                self.pending.contains(&(e.stamp, id)),
                self.evicted.get(&e.member)
            );
        }
        // D73(a): a hand pack acts at every frame it covers.
        let extra = e.payload.extra_frames();
        if let Some(o) = old {
            for g in o..=o + extra {
                if let Some(s) = self.at_frame.get_mut(&g) {
                    s.remove(&id);
                }
            }
            self.eff.remove(&id);
            self.mark_dirty(o);
        }
        if let Some(n) = new {
            self.eff.insert(id, n);
            for g in n..=n + extra {
                self.at_frame.entry(g).or_default().insert(id);
            }
            self.mark_dirty(n);
        }
    }

    fn mark_dirty(&mut self, f: u64) {
        // Review NEW-2: confirmed frames are final. Getting here below `conf` is a protocol bug,
        // never a rollback.
        assert!(
            f >= self.conf,
            "protocol invariant violated: member {} would roll back to frame {f}, below its confirmed frame {}",
            self.me,
            self.conf
        );
        if f < self.frame {
            self.dirty_from = Some(self.dirty_from.map_or(f, |d| d.min(f)));
        }
    }

    /// The checkpoint state of `m`'s chain in my version (mine: as I authored it): its newest checkpoint's block
    /// and id.
    pub fn chain_ckpt(&self, m: MemberId) -> Option<(u64, EntryId)> {
        if m == self.me {
            self.my_ckpt
        } else {
            self.ckpt.get(&m).copied()
        }
    }

    fn publish(&mut self, mut e: Entry) -> Entry {
        // Thin links Part B: a checkpoint carries the link to this member's previous checkpoint.
        e.link = ckpt_link_for(self.my_ckpt, e.stamp);
        self.my_ckpt = ckpt_step(self.my_ckpt, &e);
        self.my_seq += 1;
        self.my_last = e.id();
        let _ = self.insert(e.clone());
        e
    }

    /// Author an entry at the current frame. With `carry`, the entry carries the touched object's
    /// state after this frame: computed by stepping the frame once on a scratch copy.
    pub fn author(&mut self, payload: Payload, carry: Option<u32>) -> Entry {
        let stamp = if carry.is_some() {
            assert!(self.dirty_from.is_none() && self.frame >= self.now, "carrying state needs a caught-up sim");
            self.frame
        } else {
            self.frame.max(self.now)
        };
        let mut e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None, link: 0 };
        if let Some(obj) = carry {
            let scratch = self.save();
            let tmp_id = e.id();
            self.entries.insert(tmp_id, e.clone());
            self.at_frame.entry(stamp).or_default().insert(tmp_id);
            self.eff.insert(tmp_id, stamp);
            self.step_frame(false);
            let st = self.obj_state(obj);
            self.entries.remove(&tmp_id);
            self.at_frame.get_mut(&stamp).unwrap().remove(&tmp_id);
            self.eff.remove(&tmp_id);
            self.outcomes.remove(&tmp_id);
            self.restore(&scratch, stamp);
            e.carried = st.map(|s| (obj, s));
            self.watched.insert(obj);
        }
        self.publish(e)
    }

    /// Author a state-free entry stamped `ahead` frames after the present (opt-in input delay).
    pub fn author_ahead(&mut self, payload: Payload, ahead: u64) -> Entry {
        let stamp = self.frame.max(self.now) + ahead;
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None, link: 0 };
        self.publish(e)
    }

    /// Author an entry at `stamp` (D73(a): a hand pack, stamped at its first frame, up to k - 1 frames
    /// before the present; its author's horizon has stayed below it).
    pub fn author_stamped(&mut self, payload: Payload, stamp: u64) -> Entry {
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None, link: 0 };
        self.publish(e)
    }

    /// Test fixture (review HO-1, round 2): author `payload` carrying a chosen object state.
    pub fn author_carrying(&mut self, payload: Payload, carried: (u32, BodyState)) -> Entry {
        let stamp = self.frame.max(self.now);
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: Some(carried), link: 0 };
        self.publish(e)
    }

    /// Test fixture for review H5: author with an arbitrary (backdated) stamp.
    pub fn author_backdated(&mut self, payload: Payload, back: u64) -> Entry {
        let stamp = self.frame.max(self.now).saturating_sub(back);
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload, carried: None, link: 0 };
        self.publish(e)
    }

    /// Author this member's ack: a chain entry stating its horizon (it will author nothing stamped
    /// at or before it), the head it holds of every other chain, and its newest confirmed check
    /// hash (reviews NEW-1, H4).
    pub fn author_ack(&mut self) -> Entry {
        let mut heads = Vec::new();
        for (&q, &held) in &self.contiguous {
            // A vote's frozen head is this replica's last word on that chain in the epoch (R3-1).
            let s = match self.frozen.get(&q) {
                Some(&f) if !self.evicted.contains_key(&q) => held.min(f),
                _ => held,
            };
            if s > 0 && q != self.me && self.cfg.ack_heads {
                match (self.hidden.as_ref().and_then(|h| h.get(&q)), self.fake_refs) {
                    (_, Some((f, n))) if f == q => heads.push((q, s + n, fnv_u64s([41, q as u64, (s + n) as u64]))),
                    (Some(&(hs, hid)), _) if hs < s => heads.push((q, hs, hid)),
                    _ => heads.push((q, s, self.by_author[&(q, s)])),
                }
            }
        }
        // Part B, rule 5: a fed replica's state is its view, not the log's, so it signs no check hash.
        let check = if self.dirty_from.is_none() && self.fed.is_none() {
            self.check_hashes.range(..self.conf.min(self.frame)).next_back().map(|(f, h)| (*f, *h))
        } else {
            None
        };
        let stamp = self.frame.max(self.now);
        let mut horizon = self.now.saturating_sub(1 + self.horizon_lag);
        // D73(a): below a pending pack's first frame until the pack is sent.
        if let Some(p0) = self.hold_horizon_below {
            horizon = horizon.min(p0.saturating_sub(1));
        }
        let fed = self.fed_ack();
        let epoch = self.fake_ack_epoch.unwrap_or(self.epoch);
        // Part B: my own echo is my reference too, as every receiver records it once it checks out (`retry_echoes`, with
        // the same R3-1 rule by chain order), so a fed member's copy of the final term reads the references the others'
        // do (leave-s4-rejoin: member 3 cut 2 at 58 from its stale ack heads alone, the others at 96).
        if let Some(fa) = fed.as_ref().filter(|fa| !fa.echo.is_empty()) {
            let aseq = self.my_seq + 1;
            for &(x, _) in &fa.echo {
                if let (Some(&s), Some(&(_, id))) = (self.echo_seq.get(&x), self.echo_last.get(&x)) {
                    if self.vote_on(self.me, x, epoch).is_some_and(|(vs, _)| vs < aseq) {
                        continue;
                    }
                    self.echo_refs.entry((self.me, x)).or_default().entry(s).and_modify(|v| v.1 = v.1.min(aseq)).or_insert((id, aseq));
                }
            }
        }
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp, payload: Payload::Ack { horizon, heads, check, epoch, fed }, carried: None, link: 0 };
        self.publish(e)
    }

    /// Store an entry (received, fetched or my own). Returns entries to broadcast: when it proves
    /// equivocation, both versions and this member's vote.
    pub fn insert(&mut self, e: Entry) -> Vec<Entry> {
        let id = e.id();
        if self.entries.contains_key(&id) || (!self.known.contains(&e.member) && e.member != LOGIC) {
            return vec![];
        }
        if !matches!(e.payload, Payload::Ack { .. }) {
            self.busy_stamp = self.busy_stamp.max(e.stamp);
        }
        // Review PF-1: every signed precommit for a value, any version, linked into its author's chain
        // or not (a proven member's counts from here, `precommitters`).
        if let Payload::Precommit { epoch, round, value, .. } = e.payload {
            if value != 0 && epoch >= self.epoch {
                self.pc_held.entry((epoch, round, value)).or_default().insert(e.member);
            }
        }
        // Review VC8R-1: and every signed prevote for a value, the same way.
        if let Payload::Prevote { epoch, round, value } = e.payload {
            if value != 0 && epoch >= self.epoch {
                self.pv_held.entry((epoch, round, value)).or_default().insert(e.member);
            }
        }
        if let Some(&other) = self.by_author.get(&(e.member, e.seq)) {
            // Two signed entries for the same (member, seq): a transferable proof (review C2).
            self.entries.insert(id, e.clone());
            self.disputed.insert(id);
            // Another version of a proposal still names a value someone may decide (P2-4).
            self.note_proposal(id);
            let mut out = vec![self.entries[&other].clone(), e.clone()];
            out.extend(self.on_proof(e.member, e.seq, other, id));
            // Review VC-2: the version a removed member's cut names may arrive as a duplicate.
            if self.evicted.contains_key(&e.member) && self.fixes_off & fix::VC2 == 0 {
                self.adopt_cut(e.member);
            }
            return out;
        }
        self.by_author.insert((e.member, e.seq), id);
        if e.member != self.me {
            self.heard.insert(e.member, self.now);
        }
        if let Some((obj, _)) = e.carried {
            self.watched.insert(obj);
        }
        if let Payload::Touch { obj, .. } = e.payload {
            self.watched.insert(obj);
        }
        // Below the confirmed frame only a horizon violation can be new (design 10.8): never apply.
        if e.stamp < self.conf && !is_protocol(&e.payload) {
            self.violations.insert(id);
        }
        let disputed = self.proofs.get(&e.member).is_some_and(|&(k, _, _)| e.seq >= k)
            || self.evicted.get(&e.member).is_some_and(|&(cut, _)| e.seq > cut);
        if disputed {
            self.disputed.insert(id);
        }
        let (member, seq) = (e.member, e.seq);
        // Part B, rule 1: a judged entry waits for its verdict from its arrival, held behind a hole in its author's
        // chain or not (rule 1 can decide such an entry void; it counts only once validated and vouched).
        // F2 at arrival too: an entry of an earlier epoch waits for its verdict whatever my membership is now, since it is
        // judged with its own epoch's (`decide_verdicts`: an epoch of 2 keeps D54). Keyed on my current membership, a
        // replica that took an n = 3 epoch's entries only after moving to n = 2 (a fed member replaying the log after a
        // leave) counted them all, while those that held them pending voided some by rule 1 (leave-s4-rejoin: member 1's
        // s107 to s155, void at 1 and 102, counted at 3).
        let judged_now = self.cfg.localize
            && (self.localized() || self.fed_mode_capable() || self.entry_epoch(e.member, e.seq) < self.epoch)
            && localize::judged(&e.payload)
            && (self.members.contains(&e.member) || self.evicted.contains_key(&e.member))
            && !self.verdicts.contains_key(&id);
        if judged_now {
            self.pending.insert((e.stamp, id));
        }
        self.entries.insert(id, e);
        self.set_eff(id);
        // Review PF-1: a proposal of a member proven to have forked may never link here; keep it by
        // value now, as any other version of a proposal is kept.
        if self.fixes_off & fix::PF1 == 0 && self.proofs.contains_key(&member) {
            self.note_proposal(id);
        }
        let mut out = Vec::new();
        // Revision 22: a leave request (its author's vote on itself, no kick) is taken by value by every replica: it is
        // evidence for its author's removal on its own, and it may arrive ahead of its author's queued entries (D-5).
        // It stands until its author is removed: a reset decided meanwhile doesn't clear it (leave-s4: member 2 asked in
        // epoch 0 while the others had just reset into epoch 1, and its request was dropped).
        if let Payload::Vote { epoch, target, kick: 0, seq, .. } = self.entries[&id].payload {
            // Revision 22, plan (A): a latency leave is conditional and counts only in its own epoch; one from an earlier
            // epoch is void (its region may have ended at the decision that came between).
            let conditional = latency_leave_fed(seq).is_some();
            if target == member && (epoch == self.epoch || (epoch < self.epoch && !conditional)) && self.members.contains(&member) {
                self.leaves.entry(member).or_insert(id);
            }
        }
        // Revision 22: a fed member takes part in the view change outside its log. It holds votes, proposals,
        // prevotes and precommits by value (each signed on itself, O2-1), as PF-1 holds precommits, since it can't
        // place them in chain order without the log. A kick needs the kicker's chain segment (D73-1, not built): a
        // fed member gives it its nil.
        if self.fed.is_some() && member != self.me && self.members.contains(&member) {
            let ep = match &self.entries[&id].payload {
                Payload::Vote { epoch, .. }
                | Payload::Propose { epoch, .. }
                | Payload::Prevote { epoch, .. }
                | Payload::Precommit { epoch, .. }
                | Payload::Kick { epoch, .. } => Some(*epoch),
                _ => None,
            };
            match ep {
                Some(ep) if ep > self.epoch => self.vc_later.entry(ep).or_default().push(id),
                Some(ep) if ep == self.epoch => self.record_vc(member, id, &mut out),
                _ => {}
            }
        }
        self.extend_chain(member, &mut out);
        // An entry past a hole in its author's chain shows the hole: ask for it.
        let c = self.contiguous.get(&member).copied().unwrap_or(0);
        if member != self.me && seq > c + 1 {
            self.want_all(member, c + 1, seq - 1);
        }
        // A removed member's chain may still be missing entries up to its cut.
        if self.evicted.contains_key(&member) {
            self.adopt_cut(member);
        }
        out
    }

    /// Extend `m`'s verified contiguous prefix: each next entry must link to the previous by hash.
    fn extend_chain(&mut self, m: MemberId, out: &mut Vec<Entry>) {
        // A member that asked to join is validated once admitted, when its boundary is known.
        if m != LOGIC && !self.members.contains(&m) && !self.evicted.contains_key(&m) {
            return;
        }
        loop {
            let c = self.contiguous.get(&m).copied().unwrap_or(0);
            // A removed member's chain ends at its cut. A vote only caps what the voter references
            // (stable_seq, author_ack): it keeps processing the chain, so a member wrongly
            // suspected can still take part in the reset that keeps it (P2-4).
            if self.evicted.contains_key(&m) && self.frozen.get(&m).is_some_and(|&f| c >= f) {
                break;
            }
            let Some(&id) = self.by_author.get(&(m, c + 1)) else { break };
            let prev = if c == 0 { 0 } else { self.by_author[&(m, c)] };
            if self.entries[&id].prev != prev {
                // The chain continues from a version of seq c we do not hold: fetch it. And the
                // two signed entries already prove that m signed two versions of seq c (P2-4: a
                // member that forks its vote is proven faulty even if it never sends the other).
                self.want_all(m, c, c);
                if c > 0 && !self.evicted.contains_key(&m) && !self.proofs.contains_key(&m) {
                    let held = self.by_author[&(m, c)];
                    out.push(self.entries[&held].clone());
                    out.push(self.entries[&id].clone());
                    out.extend(self.on_proof(m, c, held, id));
                }
                break;
            }
            self.contiguous.insert(m, c + 1);
            self.validate(m, id, out);
        }
    }

    /// Chain-local rules, in chain order (reviews NEW-1, H5; user rule 14:31Z):
    /// * an ack is void if its stamp goes backwards, or its horizon lags its stamp by more than
    ///   `horizon_slack` frames (so no member can hold a backdating window open);
    /// * any other entry is void if stamped at or before a horizon its author declared earlier.
    fn validate(&mut self, m: MemberId, id: EntryId, out: &mut Vec<Entry>) {
        let e = self.entries[&id].clone();
        // Part B, section 3: d(e) is measured for every entry, void or not, so verdicts can't censor it.
        self.win_seqs.entry((e.stamp / WINDOW, m)).or_default().insert(e.seq);
        // Part B, rule 1: a perturbation or hand entry waits for its verdict.
        if (self.localized() || self.fed_mode_capable()) && localize::judged(&e.payload) && !self.verdicts.contains_key(&id) {
            self.pending.insert((e.stamp, id));
        }
        // D80: an admin entry stays pending across epochs until a decision judges it.
        let admin_entry = matches!(e.payload, Payload::Kick { .. } | Payload::SetRole { .. } | Payload::DefineRole { .. } | Payload::Ownership { .. });
        let (max_h, last_ack) = self.chain_state.get(&m).copied().unwrap_or((None, 0));
        let valid = match &e.payload {
            Payload::Ack { horizon, epoch, .. } => {
                // Review VC-5: an author's epoch never falls and rises by at most one per ack. Review
                // VC3R-6: a joiner's first acks may declare the epoch it was admitted into, so it need
                // not have acked every epoch since genesis (its chain is validated only once admitted).
                let prev = self.ack_epoch_from.get(&m).and_then(|a| a.keys().next_back()).copied().unwrap_or(0);
                let adm = if self.fixes_off & fix::VC3R6 == 0 { self.admitted_epoch.get(&m).copied().unwrap_or(0) } else { 0 };
                let epoch_ok = self.fixes_off & fix::VC5 != 0 || (*epoch >= prev && (*epoch <= prev + 1 || *epoch <= adm));
                // D73(a): the room a pending hand pack needs (PACK_MAX - 1 frames more), for every member.
                e.stamp >= last_ack && horizon + self.cfg.horizon_slack + PACK_MAX - 1 >= e.stamp && epoch_ok
            }
            // Review VC-4: a precommit's boundary caps its author's acks, so it must lie ahead of the
            // horizon the author has declared: a cap never takes back what its acks already confirmed.
            // Reviews VCR-12 and VC3R-5: no upper bound. A far cap restrains only its own author, and
            // round 2's bound (two join leads past that horizon) voided a lagging member's precommits.
            Payload::Precommit { boundary, .. } if *boundary > 0 && self.fixes_off & fix::VC4 == 0 => {
                let h = max_h.unwrap_or(0);
                *boundary > h && (self.fixes_off & fix::VCR12 == 0 || *boundary <= h + 2 * self.cfg.join_lead)
            }
            // Review VCR-9: an admin entry's stamp orders it (commit order), so it obeys its author's
            // horizon like any entry: no backdating past what its author has declared.
            // D73(a): and within horizon_slack of its author's last ack stamp, so only packs get the
            // wider window the ack lag allows.
            // Rev 22 (e1): not kicks: a kick is valid on its signature, epoch and the previous decided ACL, never its stamp.
            Payload::SetRole { .. } | Payload::DefineRole { .. } | Payload::Ownership { .. } if self.fixes_off & fix::VCR9 == 0 => {
                max_h.is_none_or(|h| e.stamp > h) && e.stamp + self.cfg.horizon_slack > last_ack
            }
            Payload::Vote { .. }
            | Payload::Propose { .. }
            | Payload::Prevote { .. }
            | Payload::Precommit { .. }
            | Payload::Kick { .. }
            | Payload::SetRole { .. }
            | Payload::DefineRole { .. }
            | Payload::Ownership { .. } => true,
            // A member admitted by a view change acts from its boundary frame on (10.8). D73(a): a
            // perturbation's last frame (stamp + a pack's extra frames) lies within horizon_slack of its
            // author's last ack stamp, and a pack holds at most PACK_MAX targets.
            _ => {
                let extra = e.payload.extra_frames();
                max_h.is_none_or(|h| e.stamp > h)
                    && e.stamp + extra + self.cfg.horizon_slack > last_ack
                    && extra < PACK_MAX
                    && self.admitted.get(&m).is_none_or(|&b| e.stamp >= b)
            }
        };
        // Part B (TLP-27): the checkpoint link, a chain-local rule. The expected link follows from the state after
        // e's prev in my version. A chain I hold from midway (no state for its prev) is taken on trust from its
        // next checkpoint on.
        let before = if e.prev == 0 { Some(None) } else { self.ckpt_after.get(&e.prev).map(|&c| Some(c)) };
        let (link_ok, after) = match before {
            Some(ck) => (e.link == ckpt_link_for(ck, e.stamp), ckpt_step(ck, &e)),
            None => (true, if e.link != 0 { Some((e.stamp / CKPT_FRAMES, id)) } else { None }),
        };
        if let Some(a) = after {
            self.ckpt_after.insert(id, a);
            self.ckpt.insert(m, a);
            if a.1 == id {
                self.ckpt_by_block.insert((m, a.0), (e.seq, id, e.rest_hash()));
                if !self.echo_pending.is_empty() {
                    self.retry_echoes();
                }
            }
        }
        let valid = valid && link_ok;
        if !valid {
            self.stats.entries_rejected_violation += 1;
            self.ev(format!("violation: m{m} s{} stamped {} breaks its chain's horizon rules: void for every member", e.seq, e.stamp));
            self.violations.insert(id);
            self.set_eff(id);
            return;
        }
        // F2: votes by their chains, so an ended epoch's votes stay readable (`vote_on`).
        if let Payload::Vote { epoch, target, frozen, .. } = e.payload {
            if target != m {
                let v = self.vote_hist.entry((target, m)).or_default();
                if !v.iter().any(|r| r.0 == epoch) {
                    v.push((epoch, e.seq, frozen.0));
                }
            }
        }
        match e.payload {
            Payload::Ack { horizon, heads, check, epoch, fed } => {
                if epoch > 0 {
                    self.ack_epoch_from.entry(m).or_default().entry(epoch).or_insert(e.seq);
                }
                let h = max_h.map_or(horizon, |x| x.max(horizon));
                self.chain_state.insert(m, (Some(h), e.stamp));
                self.ack_hist.entry(m).or_default().insert(e.seq, h);
                self.ack_stamps.entry(m).or_default().insert(e.seq, e.stamp);
                self.ack_by_stamp.entry(m).or_default().insert(e.stamp, e.seq);
                for &(q, s, hid) in &heads {
                    if m != self.me {
                        let mine = self.contiguous.get(&q).copied().unwrap_or(0);
                        if q != self.me && s > mine {
                            self.want_from(m, q, mine + 1, s);
                        } else if self.by_author.get(&(q, s)).is_some_and(|&x| x != hid) {
                            self.want_from(m, q, s, s);
                        }
                    }
                    self.ack_head_hist.entry((m, q)).or_default().insert(e.seq, (s, hid));
                    // Review R3-1: once m has voted against q, its vote's frozen head is its last
                    // word on q's chain in this epoch; later references never count toward stability.
                    if self.votes.get(&q).is_some_and(|v| v.contains_key(&m)) {
                        continue;
                    }
                    self.refs.entry((m, q)).or_default().insert(s, (hid, id));
                    self.note_first_ref(m, e.seq, e.stamp, q, s, hid);
                }
                if fed.is_some() {
                    self.anchored_acks.entry(m).or_default().insert(e.seq);
                }
                if m != self.me {
                    if let Some((f, hh)) = check {
                        self.remote_checks.entry(f).or_default().insert(m, hh);
                    }
                    // Part B: whether m is fed (TLP-23), and its echo (rule 1, TLP-15).
                    match fed {
                        Some(fa) if !fa.echo.is_empty() => {
                            self.fed_peers.insert(m, fa.echo.iter().find(|&&(x, _)| x == self.me).map(|&(_, b)| b));
                            let now = self.now;
                            self.fed_seen.entry(m).or_insert(now);
                            self.echo_pending.entry(m).or_default().insert(e.seq, (fa.echo, fa.digest));
                            self.retry_echoes();
                        }
                        // The same echo as m's previous ack.
                        Some(_) => {
                            self.fed_peers.entry(m).or_insert(None);
                            let now = self.now;
                            self.fed_seen.entry(m).or_insert(now);
                        }
                        None => {
                            self.fed_seen.remove(&m);
                            if self.fed_peers.remove(&m).is_some() {
                                // Off the feed: a later stay starts its checkpoint clocks afresh.
                                self.ckpt_sent.remove(&m);
                                self.ckpt_first_sent.retain(|&(f, _), _| f != m);
                            }
                            // (Its echoes still pending stay: each is checked once I hold what it names, whenever that is,
                            // so every replica ends with the same references.)
                        }
                    }
                }
                self.recompute_conf();
                self.evaluate_checks();
            }
            Payload::Vote { epoch, .. }
            | Payload::Propose { epoch, .. }
            | Payload::Prevote { epoch, .. }
            | Payload::Precommit { epoch, .. }
            | Payload::Kick { epoch, .. }
            | Payload::SetRole { epoch, .. }
            | Payload::DefineRole { epoch, .. }
            | Payload::Ownership { epoch, .. } => {
                if epoch > self.epoch {
                    self.vc_later.entry(epoch).or_default().push(id);
                } else if epoch == self.epoch || admin_entry {
                    self.record_vc(m, id, out);
                }
            }
            _ => {}
        }
    }

    /// The highest seq of `m`'s chain that every member is known to hold with identical ids:
    /// each other member's newest reference at or below my own head, checked against my copy.
    /// Monotone: what was once stable stays stable (review R3-1).
    fn stable_seq(&mut self, m: MemberId) -> u32 {
        // After voting against m, this replica references no more of m's chain than its frozen head.
        let held = self.contiguous.get(&m).copied().unwrap_or(0);
        let mine = match self.frozen.get(&m) {
            Some(&f) if !self.evicted.contains_key(&m) => held.min(f),
            _ => held,
        };
        let mut s = mine;
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        let matching = self.fixes_off & fix::VC4R3 == 0;
        for j in members {
            if j == self.me || j == m {
                continue;
            }
            if matching {
                // Review VC5R-2: j's contribution is the newest of m's chain it has referenced that I
                // hold in the version it names. A reference to another version (a fake id, or a fork) is
                // skipped and fetched for the proof, but it does not zero j's contribution, so a faulty
                // member alternating a fake head with a stale real one cannot cap m's stability.
                let Some(r) = self.refs.get(&(j, m)) else {
                    s = 0;
                    break;
                };
                let mut matched = None;
                let unmatched: Vec<u32> = r
                    .range(..=mine)
                    .rev()
                    .take_while(|(rs, (rid, _))| {
                        if self.by_author.get(&(m, **rs)) == Some(rid) {
                            matched = Some(**rs);
                            false
                        } else {
                            true
                        }
                    })
                    .map(|(rs, _)| *rs)
                    .collect();
                for rs in unmatched {
                    self.want_from(j, m, rs, rs);
                }
                // Part B at 3 players (S-2): a fed member's echo counts as its reference, so entries pending across its
                // switch-in complete under D54.
                if self.fed_mode_capable() {
                    let e = self
                        .echo_refs
                        .get(&(j, m))
                        .and_then(|r| r.range(..=mine).rev().find(|(es, (hid, _))| self.by_author.get(&(m, **es)) == Some(hid)))
                        .map(|(es, _)| *es);
                    matched = matched.max(e);
                }
                match matched {
                    Some(rs) => s = s.min(rs),
                    None => {
                        s = 0;
                        break;
                    }
                }
                continue;
            }
            let Some((rs, rid)) = self.refs.get(&(j, m)).and_then(|r| r.range(..=mine).next_back()).map(|(a, b)| (*a, b.0)) else {
                s = 0;
                break;
            };
            if self.by_author.get(&(m, rs)) != Some(&rid) {
                // j holds another version: wait for the proof (fetch it from j).
                self.want_from(j, m, rs, rs);
                s = 0;
                break;
            }
            s = s.min(rs);
        }
        let hi = self.stable_hi.entry(m).or_insert(0);
        *hi = (*hi).max(s);
        *hi
    }

    /// Confirmed frame from stable data only (reviews C1, NEW-1): frame f is confirmed once, for
    /// every member, a stable prefix of its chain declares a horizon at or after f. No decision
    /// depends on it except finality (SCM, ACL, R14 relays) and trimming.
    fn recompute_conf(&mut self) {
        // Part B, rule 5: a fed replica holds no log, so it confirms nothing and computes no window or verdict until
        // it is back on the log. While fed, rule 3's deadline bound moved its conf over entries it never received,
        // and on recovery every one of them stamped below that conf was taken for a horizon violation.
        if self.adopting || self.fed.is_some() {
            return;
        }
        // F2, revision 21 (a): a Part B removal's cut, once final.
        if !self.pb_pending.is_empty() {
            self.finalize_cuts();
        }
        // Thin links Part B, rule 1: decide the verdicts whose statuses are known. Also below 3 members while an entry of
        // an older Part B epoch still waits for its verdict (a removed member's, until its cut is final).
        if self.localized() || self.fed_mode_capable() || (self.cfg.localize && !self.pending.is_empty()) {
            // Windows 0 and 1 need no data (Q = Q_max). They must exist before the loop below: rule 3's
            // deadline bound reads them, and the loop returns early for a member with neither an echo-stable
            // horizon nor a bound. Initialized after it, a member whose first ack took 2.5 s held conf at 0,
            // and every member paused from frame W until that ack was echo-stable.
            if self.windows.is_empty() {
                self.window_vals(1);
            }
            self.decide_verdicts();
        }
        // F2: at 3 players, or with conf in an earlier epoch, frames confirm by their epoch's rules (`walk_windows`).
        let n = self.members.len();
        let walk = self.cfg.localize && n >= 3 && (n == 3 || self.epoch_at(self.conf) < self.epoch);
        let c = if walk {
            // Part B at 3 players (step 2): fed windows close by rule 3, the others by D54.
            let c_b = self.conf_candidate(true);
            let c_d = self.conf_candidate(false);
            // F2: and every judged entry below conf has its verdict, whichever rules its frames confirm by (an entry of
            // an older epoch can be stamped in a D54 window of mine).
            let first = self.pending.iter().next().map(|&(s, _)| s);
            self.walk_windows(c_b, c_d).map(|c| first.map_or(c, |s| c.min(s)))
        } else {
            let localized = self.localized();
            // F2 below 3 members: an entry of an older Part B epoch still waiting for its verdict holds conf too,
            // whichever rules today's frames confirm by. kickliar-fed2's n = 2 tail: D54 confirmed past member 2's
            // s132 (epoch 0, pending until 2's cut was final), and the final cut then voided it below conf.
            let first = if self.cfg.localize { self.pending.iter().next().map(|&(s, _)| s) } else { None };
            self.conf_candidate(localized).map(|c| first.map_or(c, |s| c.min(s)))
        };
        if let Some(c) = c {
            if c > self.conf {
                self.conf = c;
            }
        }
        // Diagnostics (PE_CONF_TRACE=<member>): once a second of my present, what bounds my confirmation.
        if self.now / 60 != self.dbg_conf_at && std::env::var("PE_CONF_TRACE").ok().and_then(|v| v.parse::<MemberId>().ok()) == Some(self.me) {
            self.dbg_conf_at = self.now / 60;
            let first = self.pending.iter().next().map(|&(s, id)| {
                let e = &self.entries[&id];
                (s, e.member, e.seq, e.seq <= self.contiguous.get(&e.member).copied().unwrap_or(0))
            });
            let per: Vec<String> = self
                .members
                .iter()
                .map(|&j| {
                    let es = self.echo_stable(j);
                    let h = self.ack_hist.get(&j).and_then(|a| a.range(..=es).next_back()).map(|(_, h)| *h);
                    format!("m{j}: es {es} held {} h {:?} bound {:?}", self.contiguous.get(&j).copied().unwrap_or(0), h, self.deadline_bound(j))
                })
                .collect();
            eprintln!("CONF m{} now {} conf {} pending {} first {:?} | {}", self.me, self.now, self.conf, self.pending.len(), first, per.join(" | "));
        }
    }

    /// Part B at 3 players (step 2): the combined frontier. From my confirmed frame, window by window: a fed window
    /// closes up to rule 3's candidate, a D54 window up to D54's; a window whose mode isn't known yet stops it.
    fn walk_windows(&self, c_b: Option<u64>, c_d: Option<u64>) -> Option<u64> {
        let mut f = self.conf;
        loop {
            let k = f / WINDOW;
            // F2: segments end at a window's end or at the boundary of the change that ends their epoch. An epoch of
            // 4 or more members confirms by rule 3, one of 3 by its windows' modes (an older one by both rules), one
            // of 2 by D54.
            let e = self.epoch_at(f);
            let mut end = (k + 1) * WINDOW;
            if let Some(d) = self.decided.get(e as usize) {
                end = end.min(d.0.boundary);
            }
            let n_e = if e == self.epoch { self.members.len() } else { self.members_at(e).map_or(0, |m| m.len()) };
            let limit = if n_e >= 4 {
                c_b
            } else if n_e == 3 && e == self.epoch {
                match self.window_mode(k) {
                    None => return Some(f),
                    Some(true) => c_b,
                    Some(false) => c_d,
                }
            } else if n_e == 3 {
                c_b.zip(c_d).map(|(a, b)| a.min(b))
            } else {
                c_d
            };
            let Some(limit) = limit else { return Some(f) };
            if limit < end {
                return Some(f.max(limit));
            }
            f = end;
        }
    }

    /// The confirmed frame one rule set allows: Part B's rule 3 (`part_b`) or D54. None if a member has neither.
    fn conf_candidate(&mut self, localized: bool) -> Option<u64> {
        let mut c = u64::MAX;
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for j in members {
            // Part B, rule 3: a prefix is evidence once echo-stable; D54's stability for 2-3 members.
            let mut s = if localized { self.echo_stable(j) } else { self.stable_seq(j) };
            // An ack from an epoch newer than mine never counts toward my confirmation (10.8): its
            // author may know of members I do not.
            if let Some(&first) = self.ack_epoch_from.get(&j).and_then(|a| a.range(self.epoch + 1..).map(|(_, s)| s).min()) {
                s = s.min(first.saturating_sub(1));
            }
            let mut h = self.ack_hist.get(&j).and_then(|a| a.range(..=s).next_back()).map(|(_, h)| *h);
            // Part B, rule 3: or the frames for which anything j could still author would be past its
            // deadline at q members.
            if localized {
                let mut hb = self.deadline_bound(j);
                // F2: while j's echo-stable prefix hasn't reached its first ack of my epoch, it may still author
                // entries of its older epoch, judged with that epoch's members and quorum: the bound must hold there
                // too (an older epoch of 3 or fewer members gives none: D54 windows void nothing).
                let ej = self.entry_epoch(j, s);
                if ej < self.epoch {
                    hb = match self.ctx(ej) {
                        Some(old) if old.members.len() >= 4 => hb.zip(self.deadline_bound_in(j, &old)).map(|(a, b)| a.min(b)),
                        _ => None,
                    };
                }
                if let Some(hb) = hb {
                    h = Some(h.map_or(hb, |x| x.max(hb)));
                }
            }
            // After precommitting a join, its author's acks count only below the join's boundary.
            if let (Some(hh), Some(&(at, b))) = (h, self.join_caps.get(&j)) {
                if s > at {
                    h = Some(hh.min(b.saturating_sub(1)));
                }
            }
            match (h, self.admitted.get(&j)) {
                (Some(h), Some(&b)) => c = c.min((h + 1).max(b)),
                (None, Some(&b)) => c = c.min(b),
                (Some(h), None) => c = c.min(h + 1),
                (None, None) => return None,
            }
        }
        // P2-4: a removed member's entries up to its cut count, and a replica that did not vote
        // may not hold them all yet. Until it does, it confirms only the frames that the acks it
        // holds of that prefix cover: an entry stamped at or before a declared horizon must come
        // before that ack in the chain (8.2).
        for (&x, &(cut, _)) in &self.evicted {
            // F2, revision 21 (a): a removal whose cut isn't final: what of x isn't vouched yet may still be. Bound by
            // x's echo-stable horizon, or rule 3's bound, with its last epoch's members and quorum.
            if self.pb_pending.contains(&x) {
                let old = self.removed_in.get(&x).and_then(|&e| self.ctx(e - 1));
                let es = old.as_ref().map_or(0, |o| self.es_in(x, o));
                let h = self.ack_hist.get(&x).and_then(|a| a.range(..=es).next_back()).map_or(0, |(_, h)| h + 1);
                let hb = old.as_ref().and_then(|o| self.deadline_bound_in(x, o)).map_or(0, |b| b + 1);
                c = c.min(h.max(hb));
                continue;
            }
            let held = self.contiguous.get(&x).copied().unwrap_or(0);
            let right = self.cut_ids.get(&x).is_none_or(|&cid| cut == 0 || self.by_author.get(&(x, cut)) == Some(&cid));
            if held < cut || !right {
                // Review VC-2: past the prefix verified against every member's references, my copy may
                // be another branch of a fork, so only that prefix's acks bound confirmation; the
                // robust cut contains it.
                // Part B, rule 3: there the verified prefix is the echo-stable one (stable_hi isn't kept).
                let upto = if localized {
                    self.echo_stable(x).min(cut)
                } else if self.fixes_off & fix::VC2 == 0 {
                    self.stable_hi.get(&x).copied().unwrap_or(0).min(cut)
                } else {
                    held.min(cut)
                };
                let h = self.ack_hist.get(&x).and_then(|a| a.range(..=upto).next_back()).map_or(0, |(_, h)| h + 1);
                c = c.min(h);
            }
        }
        // Thin links Part B, section 3: nothing is confirmed in a window before its values are final, so a
        // decision taken with provisional values can still be re-simulated (`finalize_windows`).
        // Part B, rule 3: and every judged entry stamped below conf has its verdict.
        if localized && c != u64::MAX {
            if let Some(&(s, _)) = self.pending.iter().next() {
                c = c.min(s);
            }
            // So does a judged entry I hold but haven't validated yet, behind a hole in its author's chain: it acts
            // already and has no verdict. The rejoin cell: rule 3's bound passed one (member 3's touch, stamped 243,
            // behind a hole), and its verdict later voided it below conf.
            let authors: Vec<MemberId> = self.members.iter().chain(self.evicted.keys()).copied().collect();
            for j in authors {
                let held = self.contiguous.get(&j).copied().unwrap_or(0);
                for (_, id) in self.by_author.range((j, held.saturating_add(1))..=(j, u32::MAX)) {
                    if let Some(e) = self.entries.get(id) {
                        if localize::judged(&e.payload) && !self.verdicts.contains_key(id) {
                            c = c.min(e.stamp);
                        }
                    }
                }
            }
        }
        if localized && c != u64::MAX {
            self.finalize_windows();
            let next = self.windows.keys().next_back().map_or(0, |&k| k + 1);
            c = c.min(next * WINDOW);
            if let Some(&j) = self.provisional.keys().next() {
                c = c.min(j * WINDOW);
            }
        }
        (c != u64::MAX).then_some(c)
    }

    /// Two signed entries of `m` contradict each other (`a` at seq `k`, `b` another version of it
    /// or a successor that names another version): record the proof, withdraw what it disputes,
    /// and vote to remove `m` in this epoch (design 9.3, 10.8).
    fn on_proof(&mut self, m: MemberId, k: u32, a: EntryId, b: EntryId) -> Vec<Entry> {
        if !self.proofs.contains_key(&m) {
            self.stats.equivocations += 1;
            self.ev(format!("EQUIVOCATION proof: member {m} signed two versions of seq {k} ({a:016x} / {b:016x})"));
        }
        let old_k = self.proofs.get(&m).map(|p| p.0);
        let (k0, a0, b0) = match self.proofs.get(&m) {
            Some(&(k1, a1, b1)) if k1 <= k => (k1, a1, b1),
            _ => (k, a, b),
        };
        self.proofs.insert(m, (k0, a0, b0));
        // Review PF-1: m's proposals already held past its fork may never link here: keep them by value.
        // Once per proof, or when it moves earlier (VC7R-6); later ones are kept as they arrive (insert).
        if self.fixes_off & fix::PF1 == 0 && old_k.is_none_or(|ok| k0 < ok) {
            let held: Vec<EntryId> = self.by_author.range((m, k0)..=(m, u32::MAX)).map(|(_, &id)| id).collect();
            for id in held {
                self.note_proposal(id);
            }
        }
        let mut out = Vec::new();
        if m != self.me && self.members.contains(&m) && !self.voted(m) {
            out.extend(self.freeze_and_vote(m, k0, 0));
        }
        self.reconcile(m);
        out
    }

    /// Whether this replica has voted against `m` in the current epoch.
    fn voted(&self, m: MemberId) -> bool {
        self.votes.get(&m).is_some_and(|v| v.contains_key(&self.me))
    }

    /// Carry-over, gap 2 (b) (@coordinator-opus m0813b922e026): a leave's B comes from the others' votes on it, each
    /// frozen at the voter's head of the leaver's chain, which it holds; not from the leaver's own head, which a dead
    /// uplink may never deliver (leave-s4: the members held member 2's chain to 107, its leave froze 166). So with Part
    /// B I vote on every leave I hold, once per epoch (a leave stands across a reset; votes don't).
    pub(super) fn leave_votes_due(&mut self) -> Vec<Entry> {
        if !self.cfg.localize || self.withhold_vc || !self.members.contains(&self.me) {
            return Vec::new();
        }
        let leavers: Vec<MemberId> = self.leaves.keys().copied().filter(|&x| x != self.me && self.members.contains(&x)).collect();
        let mut out = Vec::new();
        for x in leavers {
            if !self.voted(x) {
                out.extend(self.freeze_and_vote(x, 0, 0));
            }
        }
        out
    }

    /// Freeze what this member will ever reference of m's chain in this epoch, and say so in a
    /// vote. `k0` is the disputed seq of a proof, or 0 for a member presumed crashed or too slow.
    fn freeze_and_vote(&mut self, m: MemberId, k0: u32, kick: EntryId) -> Option<Entry> {
        if self.withhold_vc {
            return None;
        }
        let f = self.contiguous.get(&m).copied().unwrap_or(0);
        // Rev 22 (TLP-24): a fed member's frozen head is its last echoed checkpoint of m: it echoed that far, so its
        // held prefix (stale while fed) would drop references it published.
        if self.fed.is_some() {
            if let (Some(&s), Some(&(_, id))) = (self.echo_seq.get(&m), self.echo_last.get(&m)) {
                if s > f {
                    self.frozen.insert(m, s);
                    let e = Entry {
                        member: self.me,
                        seq: self.my_seq + 1,
                        prev: self.my_last,
                        stamp: self.frame.max(self.now),
                        payload: Payload::Vote { epoch: self.epoch, target: m, seq: k0, frozen: (s, id), kick },
                        carried: None,
                        link: 0,
                    };
                    return Some(self.publish(e));
                }
            }
        }
        // A faulty member (fixture) votes without freezing: it keeps processing that chain.
        if !self.byz_ally {
            self.frozen.insert(m, f);
        }
        let (f, fid) = match self.hidden.as_ref().and_then(|h| h.get(&m)) {
            Some(&(hs, hid)) if hs < f => (hs, hid),
            _ => (f, if f == 0 { 0 } else { self.by_author[&(m, f)] }),
        };
        let (f, fid) = if self.lie_in_votes { (0, 0) } else { (f, fid) };
        let v = Entry {
            member: self.me,
            seq: self.my_seq + 1,
            prev: self.my_last,
            stamp: self.frame.max(self.now),
            payload: Payload::Vote { epoch: self.epoch, target: m, seq: k0, frozen: (f, fid), kick },
            carried: None,
            link: 0,
        };
        Some(self.publish(v))
    }

    /// Review R3-2 (a silent member) and R3-9 (a member too slow to keep up), and proofs carried
    /// into a new epoch: vote to remove members. A member none of whose entries has reached this
    /// replica, directly or fetched, for `suspect_after` frames is presumed crashed, unless another
    /// member's acks show more of its chain than this replica holds (then this replica is behind,
    /// and fetches). The view change (10.8) then decides which removals happen and their cuts. A
    /// vote stands until its epoch ends; a member heard from again is kept by a reset (P2-4).
    /// Returns the votes to broadcast.
    pub fn suspects_due(&mut self) -> Vec<Entry> {
        // Part B, rule 5: a fed replica receives no entries; it votes only on what it can check without a log
        // (revision 21, RV20-3).
        // Rev 22 delta (proposal): a member that has asked to leave casts no new votes. Its view is the one it declared
        // unfit (leave-s4: the leaving thin member's R3-9 votes against both others, at its request's frame, gave the
        // full member's marginal vote against the fed member its q at n = 3, and the leave never decided). It still
        // takes part in the view change, and its precommit counts toward its own leave's q.
        if self.leaving() {
            return vec![];
        }
        if self.fed.is_some() {
            return self.fed_suspects();
        }
        if !self.members.contains(&self.me) || self.closed_at.is_some() {
            return vec![];
        }
        let mut out = Vec::new();
        // D80: a pending kick outlives an epoch that did not carry it out: freeze its target again.
        let kicks: Vec<(EntryId, MemberId, MemberId)> = self
            .admin_pending
            .iter()
            .filter_map(|id| {
                let e = self.entries.get(id)?;
                match e.payload {
                    Payload::Kick { target, .. } => Some((*id, e.member, target)),
                    _ => None,
                }
            })
            .collect();
        for (id, author, t) in kicks {
            if self.members.contains(&t) && self.members.contains(&author) && self.admin_run().kick_allowed(author, t) && !self.voted(t) && !self.withhold_vc {
                if t == self.me {
                    out.extend(self.self_kick_vote(id));
                } else {
                    out.extend(self.freeze_and_vote(t, 0, id));
                }
            }
        }
        out.extend(self.leave_votes_due());
        // A proof outlives an epoch that did not remove its member: vote again.
        let proven: Vec<(MemberId, u32)> = self
            .proofs
            .iter()
            .map(|(m, p)| (*m, p.0))
            .chain(self.liars.iter().map(|(m, p)| (*m, self.entries.get(&p.b).map_or(1, |e| e.seq))))
            .collect();
        for (m, k) in proven {
            if m != self.me && self.members.contains(&m) && !self.voted(m) {
                out.extend(self.freeze_and_vote(m, k.max(1), 0));
            }
        }
        if self.byz_ally {
            let targets: Vec<MemberId> = self.votes.keys().copied().filter(|t| *t != self.me && self.members.contains(t)).collect();
            for t in targets {
                if !self.voted(t) {
                    out.extend(self.freeze_and_vote(t, 0, 0));
                }
            }
            return out;
        }
        // Trust survey #4 (user rulings, 2026-11-01): a stale or unexplained cap (VC3R-2, VC4R-1), a
        // kick vote whose kick nobody delivers (VCR-1), a vote whose frozen head nobody delivers
        // (VC4R-5), withheld references (P2-4) and acks declaring an epoch I have not reached (VC-5)
        // no longer get their authors voted out. Each holds confirmation until an admin kicks the
        // member; a kick is a view change, which needs no confirmation, and its decision ends every
        // cap, freeze and vote of the epoch. VC-5's guards stay: an ack's epoch never falls and rises
        // by at most one (chain-local, in `validate`), and an ack from an epoch newer than mine never
        // counts toward my confirmation (`recompute_conf`).
        // Review VC4R-3, and the generator's RG-1: a member whose acks reference more of some chain
        // than I hold, I fetch the difference from. The references that count toward stability
        // (`stable_seq`) are only the verified ones: a seq I hold in the version the reference names.
        // A faulty member that references entries nobody has (a fake head, with or without a small
        // live offset) advances its raw reference but not its verified one, so its fake head never
        // caps confirmation. Here I only issue the fetches; the counting is in `stable_seq`.
        if self.fixes_off & fix::VC4R3 == 0 {
            let members: Vec<MemberId> = self.members.iter().copied().collect();
            let want: Vec<(MemberId, MemberId, u32, u32)> = self
                .refs
                .iter()
                .filter(|((k, y), _)| *k != self.me && *k != *y && members.contains(k) && members.contains(y))
                .filter_map(|((k, y), r)| {
                    let mine = self.contiguous.get(y).copied().unwrap_or(0);
                    r.keys().next_back().filter(|&&top| top > mine).map(|&top| (*k, *y, mine + 1, top))
                })
                .collect();
            for (k, y, from, to) in want {
                self.want_from(k, y, from, to);
            }
        }
        // Review VC4R-4 and VC4R-5: what is still missing is asked for again. A removed member's cut
        // branch is walked again; a standing kick vote's kick and a standing vote's frozen head are
        // fetched by id. A vote whose frozen head nobody delivers within half the window holds back no
        // reset (`propose_change`'s `fresh`); its voter is left to admins (trust survey #4).
        if self.fixes_off & fix::VC4R4 == 0 {
            let pending: Vec<MemberId> = self.evicted.keys().copied().filter(|x| !self.cut_done.contains(x)).collect();
            for x in pending {
                self.adopt_cut(x);
            }
        }
        if self.fixes_off & (fix::VC4R4 | fix::VC4R5) == 0 {
            let standing: Vec<(MemberId, MemberId, VoteRec)> = self.votes.iter().flat_map(|(y, vs)| vs.iter().map(move |(v, r)| (*y, *v, *r))).collect();
            for (y, v, r) in standing {
                if r.kick != 0 && !self.entries.contains_key(&r.kick) {
                    self.want_entry_all(r.kick, 0);
                }
                if r.frozen.0 > 0 && !self.head_held(y, r.frozen) {
                    self.want_entry(v, r.frozen.1, 15);
                    self.want_entry_all(r.frozen.1, 15);
                }
            }
        }
        // Round 3 (VCR-4), kept only to show VC3R-2: a member whose votes or caps resets kept
        // overruling was struck, and voted out at three strikes. Hostile timing by another member
        // could make that an honest one.
        if self.fixes_off & fix::VCR4 == 0 && self.fixes_off & fix::VC3R2 != 0 {
            let struck: Vec<MemberId> = self.members.iter().copied().filter(|&j| j != self.me && !self.voted(j) && self.struck_out(j)).collect();
            for j in struck {
                if !self.withhold_vc {
                    self.ev(format!("member {j}'s votes or caps were overruled in {STRIKES_OUT} of the last {STRIKE_EPOCHS} epochs: vote to remove it"));
                    out.extend(self.freeze_and_vote(j, 0, 0));
                }
            }
        }
        // Removal on silence needs a quorum without the removed member, so a two-member world
        // never suspects: both members pause instead (D7).
        if self.members.len() < 3 {
            return out;
        }
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for j in members {
            if j == self.me || self.voted(j) || self.held(j) {
                continue;
            }
            let heard = self.heard.get(&j).copied().unwrap_or(0);
            // Review R3-9 (demotion, 10.9 item 6): a member whose newest validated horizon trails
            // this replica's present by more than half the speculation limit, for as long as a
            // silent member is given, is voted out the same way. Its link cannot carry the session;
            // it can follow as a spectator. A short congestion (c1: about 2 s) does not qualify.
            let h = self.chain_state.get(&j).and_then(|c| c.0).unwrap_or(0);
            // Rev 22 (@coordinator-opus m4ec1d459d82b, (a)): a fed member's horizon trails by its ack period by design
            // (P <= 32), so that part isn't slowness: its threshold is W/2 + P. R3-9 still applies at n = 3, and a
            // hopeless fed member stays removable.
            let limit = self.cfg.window / 2 + if self.fed_peers.contains_key(&j) { FED_ACK_MAX_FRAMES } else { 0 };
            let since = if self.now > h + limit {
                *self.lagging.entry(j).or_insert(self.now)
            } else {
                self.lagging.remove(&j);
                self.now
            };
            let silent = self.now >= heard + self.cfg.suspect_after;
            // Thin links Part B, rule 4: no demotion for a slow link in sessions of 4 or more; its late actions
            // are void instead (rule 1). Silence is still removed.
            let slow = !self.localized() && self.now >= since + self.cfg.suspect_after;
            // Part B, rule 4's exception (TLP-25, TLP-29): a fed member that can't carry even the echo is treated
            // like a disconnection.
            let floor = if self.localized() { self.floor_breach(j).or_else(|| self.latency_breach(j)) } else { None };
            if !silent && !slow && floor.is_none() {
                self.suspect_since.remove(&j);
                continue;
            }
            let suspect_first = *self.suspect_since.entry(j).or_insert(self.now);
            let mine = self.contiguous.get(&j).copied().unwrap_or(0);
            let ahead: Vec<(MemberId, u32)> = self
                .refs
                .iter()
                .filter(|((k, a), _)| *a == j && *k != self.me && *k != j)
                .filter_map(|((k, _), r)| r.keys().next_back().filter(|&&s| s > mine).map(|&s| (*k, s)))
                .collect();
            if !ahead.is_empty() {
                // Review VC4R-3: another member's reference to more of j's chain than I hold spares j
                // while I fetch the difference, but only for a silence limit from when j first fell
                // behind. Round 4 spared j for as long as any such reference stood, so a faulty member's
                // fake reference could spare a slow colluder for good; the bound is now a fixed time, so
                // references that never deliver stop sparing it.
                let bounded = self.fixes_off & fix::VC4R3 == 0;
                for (k, s) in ahead {
                    self.want_from(k, j, mine + 1, s);
                }
                if !bounded || self.now < suspect_first + self.cfg.suspect_after {
                    continue;
                }
            }
            if self.withhold_vc {
                continue;
            }
            if silent {
                self.stats.suspicions += 1;
                self.ev(format!("member {j} silent for {} frames: presumed crashed; vote to remove it", self.now - heard));
            } else if let Some(why) = floor {
                self.stats.floor_removals += 1;
                self.ev(format!("member {j} is fed and {why}: below the floors; vote to remove it"));
            } else {
                self.stats.demotions += 1;
                self.ev(format!("member {j}'s horizon has trailed the present by over {limit} frames for {} frames: vote to demote it", self.now - since));
            }
            out.extend(self.freeze_and_vote(j, 0, 0));
        }
        out
    }

    /// Which of `m`'s entries count after an equivocation proof (review NEW-2): until a view change
    /// removes it, every entry from the disputed seq on is withdrawn unless it is stable (held
    /// identically by every member, so possibly confirmed somewhere); after it, exactly the cut.
    fn reconcile(&mut self, m: MemberId) {
        let k = self.proofs.get(&m).map(|p| p.0);
        let decided = self.evicted.get(&m).map(|p| p.0);
        let stable = self.stable_seq(m);
        let ids: Vec<(u32, EntryId)> = self.entries.iter().filter(|(_, e)| e.member == m).map(|(id, e)| (e.seq, *id)).collect();
        for (seq, id) in ids {
            let mine = self.by_author.get(&(m, seq)) == Some(&id);
            let keep = match (decided, k) {
                (Some(cut), _) => mine && seq <= cut,
                (None, Some(k)) => mine && (seq < k || seq <= stable),
                (None, None) => mine,
            };
            let changed = if keep { self.disputed.remove(&id) } else { self.disputed.insert(id) };
            if changed {
                self.set_eff(id);
            }
        }
    }

    /// Review R3-1: whether `voter`'s frozen head of `target`'s chain extends every reference to
    /// that chain the voter published before its vote. A vote that drops one is a lie, and the
    /// ack it contradicts plus the vote prove its voter faulty.
    fn vote_check(&mut self, target: MemberId, voter: MemberId, frozen: (u32, EntryId)) -> VoteCheck {
        let refs: Vec<(u32, EntryId, EntryId)> =
            self.refs.get(&(voter, target)).map(|r| r.iter().rev().map(|(s, (id, ack))| (*s, *id, *ack)).collect()).unwrap_or_default();
        let (mut s, mut id) = frozen;
        for (rs, rid, ack) in refs {
            if rs > frozen.0 {
                return VoteCheck::Lie(ack);
            }
            while s > rs {
                let Some(e) = self.entries.get(&id) else {
                    self.want_from(voter, target, s, s);
                    return VoteCheck::Missing;
                };
                id = e.prev;
                s -= 1;
            }
            if id != rid {
                return VoteCheck::Lie(ack);
            }
        }
        VoteCheck::Valid
    }

    /// `voter`'s vote `vote` contradicts its own earlier ack `ack`: it is proven faulty.
    fn on_lie(&mut self, voter: MemberId, ack: EntryId, vote: EntryId) {
        if self.liars.contains_key(&voter) || voter == self.me {
            return;
        }
        self.liars.insert(voter, Proof { a: ack, b: vote });
        self.stats.lying_votes += 1;
        self.ev(format!("LYING VOTE: member {voter}'s vote drops entries its own ack referenced: proven faulty"));
    }

    // ---- view change (P2-4, design 10.8) ---------------------------------------------------------

    /// Record a validated membership entry of the current epoch, in chain order.
    fn record_vc(&mut self, m: MemberId, id: EntryId, out: &mut Vec<Entry>) {
        if !self.members.contains(&m) {
            return;
        }
        let e = self.entries[&id].clone();
        match e.payload {
            Payload::Vote { target, frozen, kick, .. } => {
                if (target == m && kick == 0) || !self.members.contains(&target) || self.votes.get(&target).is_some_and(|v| v.contains_key(&m)) {
                    return;
                }
                // Review VCR-2: a member's vote on its own kick freezes exactly the entry before it. No
                // reference of its own can check that head, so any other head is refused.
                if target == m && self.fixes_off & fix::VCR2 == 0 && frozen != (e.seq - 1, e.prev) {
                    return;
                }
                let rec = VoteRec { frozen, stamp: e.stamp, id, kick, at: self.now };
                self.votes.entry(target).or_default().insert(m, rec);
                // Review VC3R-2: a kick vote names its kick; fetch one not held, from its voter and
                // everyone, so a real kick arrives and can be applied.
                if kick != 0 && !self.entries.contains_key(&kick) && self.fixes_off & fix::VC3R2 == 0 {
                    self.want_entry_all(kick, 0);
                }
                if let VoteCheck::Lie(ack) = self.vote_check(target, m, frozen) {
                    self.on_lie(m, ack, id);
                    if self.members.contains(&self.me) && !self.voted(m) && m != self.me {
                        out.extend(self.freeze_and_vote(m, e.seq, 0));
                    }
                }
            }
            Payload::Propose { round, valid_round, ref change, .. } => {
                self.note_proposal(id);
                if m == self.proposer(self.epoch, round) {
                    self.vc.proposals.entry(round).or_insert((change.value(), valid_round));
                    self.vc.seen_at.entry(change.value()).or_insert(self.now);
                }
            }
            Payload::Prevote { round, value, .. } => {
                self.vc.prevotes.entry(round).or_default().entry(m).or_insert(value);
                self.vc.prevote_id.entry((round, m)).or_insert(id);
            }
            Payload::Precommit { round, value, boundary, .. } => {
                self.vc.precommits.entry(round).or_default().entry(m).or_insert(value);
                if boundary > 0 {
                    // Keep the first precommit's seq and the smallest boundary (review VC-10).
                    let c = self.join_caps.entry(m).or_insert((e.seq, boundary));
                    c.1 = c.1.min(boundary);
                }
            }
            Payload::Kick { target, .. } => {
                // V1: judged by the same rule as in commit order, against the current state, so a kick
                // that would be refused (an admin by a kick-only role, an owner by a non-owner) freezes
                // no one. It can be sent again once the state allows it.
                if self.admin_outcomes.contains_key(&id) || !self.admin_run().kick_allowed(m, target) {
                    return;
                }
                self.admin_pending.insert(id);
                // The kick decides; every member, the target too, only freezes the target's chain
                // so the view change can cut it (D80).
                if self.members.contains(&self.me) && !self.voted(target) && self.closed_at.is_none() {
                    if target == self.me {
                        out.extend(self.self_kick_vote(id));
                    } else {
                        out.extend(self.freeze_and_vote(target, 0, id));
                    }
                }
            }
            Payload::SetRole { .. } | Payload::DefineRole { .. } => {
                if !self.admin_outcomes.contains_key(&id) && self.perms_of(m) & perm::ROLES != 0 {
                    self.admin_pending.insert(id);
                }
            }
            // 12.5: pending only while the certificate would apply to the book as decided (V1's
            // receipt rule): an entry that cannot apply would start view changes for nothing (VC6R-4).
            // Re-sent later, it is a new entry.
            Payload::Ownership { ref cert, .. } => {
                if !self.admin_outcomes.contains_key(&id) && self.closed_at.is_none() && self.members.contains(&m) {
                    match self.ownership_judge(cert) {
                        Ok(Insert::Applied) => {
                            self.admin_pending.insert(id);
                        }
                        // Review OWNY-2: its parent may be a pending certificate; hold it until it is decided.
                        Err(OwnError::MissingParent) if self.fixes_off & fix::OWNY2 == 0 => {
                            let mine = self.own_held.iter().filter(|h| self.entries.get(h).is_some_and(|e| e.member == m)).count();
                            if mine < MAX_HELD_PER_AUTHOR {
                                self.own_held.insert(id);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// Review HO-1: `obj` belongs to a member other than `author` (LOGIC acts for the world). Hands
    /// today; avatar parts and any per-member object later.
    fn not_owner(&self, obj: u32, author: MemberId) -> bool {
        author != LOGIC && self.hand_owners.get(&obj).is_some_and(|&o| o != author)
    }

    /// D80: the permissions member `m` holds in this epoch.
    pub fn perms_of(&self, m: MemberId) -> u64 {
        self.admin_run().perms(m)
    }

    /// Review VC4R-6: this session's id, which a join request must name.
    pub fn session(&self) -> u64 {
        self.session
    }

    /// D80: this epoch's admins.
    pub fn admins(&self) -> Vec<MemberId> {
        self.members.iter().copied().filter(|m| self.roles.get(m) == Some(&ADMIN)).collect()
    }

    /// Rev 22 (e1): the seed of this decision's kick order: the previous decided change's value (the session id before
    /// the first decision).
    pub(super) fn kick_seed(&self) -> u64 {
        self.decided.last().map_or(self.session, |d| d.0.value())
    }

    fn admin_run(&self) -> AdminRun {
        let owners = self.owner_keys();
        AdminRun {
            members: self.members.clone(),
            roles: self.roles.clone(),
            defs: self.role_defs.clone(),
            excluded: BTreeSet::new(),
            owners: owners.clone(),
            book: self.book.clone(),
            lineage: self.world_id,
            acl0: Some(Box::new((self.members.clone(), self.roles.clone(), self.role_defs.clone(), owners))),
        }
    }

    /// The world owners' keys (12.5): from the ownership chain if the world has one, else from the
    /// owners it lists.
    pub fn owner_keys(&self) -> BTreeSet<Key> {
        match &self.book {
            Some(b) => b.view().owners.owners,
            None => self.owners.iter().map(|&m| key_of(m)).collect(),
        }
    }

    /// How `cert` would fare against the book as decided now (12.5): it applies if its owners'
    /// signatures check and it builds on the current heads.
    fn ownership_judge(&self, cert: &Cert) -> Result<Insert, OwnError> {
        match self.book.clone() {
            Some(mut b) => b.commit_local(cert.clone(), self.world_id, self.frame),
            None => Err(OwnError::NotAllowed),
        }
    }

    fn ownership_applies(&self, cert: &Cert) -> bool {
        matches!(self.ownership_judge(cert), Ok(Insert::Applied))
    }

    /// 12.5: publish an ownership certificate (an admin entry); every replica judges it.
    pub fn publish_ownership(&mut self, cert: Cert) -> Entry {
        let epoch = self.epoch;
        self.author_vc(Payload::Ownership { epoch, cert })
    }

    /// The decided view changes, in order (the ownership report replays their admin entries).
    pub fn decided_changes(&self) -> impl Iterator<Item = &Change> {
        self.decided.iter().map(|(c, _, _)| c)
    }

    /// D80: kick `target` (the harness or an admin's UI). Authored whatever this member's role;
    /// every replica judges it.
    /// Revision 22: ask to leave (a member under a floor whose removal needs a fed member's vote leaves on its own,
    /// path 4): a vote on myself without a kick, freezing my own head. Sent first, ahead of queued entries (D-5).
    /// Revision 22: whether I've asked to leave in this epoch.
    pub fn leaving(&self) -> bool {
        self.leaves.contains_key(&self.me)
    }

    /// Carry-over, hole 2: a member that has asked to leave authors no ack from its request on (spike:
    /// PE_LEAVER_ACKS=1 lets it ack as before, to measure what the rule costs).
    pub fn acks_held(&self) -> bool {
        static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        self.cfg.localize && self.leaving() && !*V.get_or_init(|| std::env::var("PE_LEAVER_ACKS").is_ok())
    }

    pub fn request_leave(&mut self) -> Option<Entry> {
        self.request_leave_with(0)
    }

    /// A leave request whose vote carries `seq`: 0 for a plain leave, `latency_leave_mask` for a latency leave.
    fn request_leave_with(&mut self, seq: u32) -> Option<Entry> {
        if !self.members.contains(&self.me) || self.leaves.contains_key(&self.me) {
            return None;
        }
        let e = Entry {
            member: self.me,
            seq: self.my_seq + 1,
            prev: self.my_last,
            stamp: self.frame.max(self.now),
            payload: Payload::Vote { epoch: self.epoch, target: self.me, seq, frozen: (self.my_seq, self.my_last), kick: 0 },
            carried: None,
            link: 0,
        };
        let e = self.publish(e);
        self.leaves.insert(self.me, e.id());
        self.ev(format!("member {} asks to leave", self.me));
        Some(e)
    }

    /// (spike fixture) A latency leave now, carrying the fed set I see, whatever the votes: the race cell has two fed
    /// members ask at once.
    pub fn force_latency_leave(&mut self) -> Option<Entry> {
        let fed: BTreeSet<MemberId> = self.fed_peers.keys().copied().filter(|m| self.members.contains(m)).chain([self.me]).collect();
        self.ev(format!("latency floor (fixture): member {} leaves by request, fed {fed:?}", self.me));
        self.request_leave_with(latency_leave_mask(&fed))
    }

    /// Revision 22, plan (A): the fed set a held leave carries, if it is a latency leave.
    fn leave_fed(&self, id: EntryId) -> Option<BTreeSet<MemberId>> {
        match self.entries.get(&id)?.payload {
            Payload::Vote { seq, kick: 0, .. } => latency_leave_fed(seq),
            _ => None,
        }
    }

    /// Revision 22, plan (A): whether `c`'s latency leaves all apply. With the change's members after its other removals,
    /// highest id first, each needs n − f < q, f the fed set it carries among the members left after the ones before
    /// it, and it must be of this epoch. Every replica (and a fed member, by value) reads the same.
    pub(crate) fn latency_leaves_ok(&self, c: &Change) -> bool {
        let mut lat: Vec<(MemberId, BTreeSet<MemberId>, u32)> = c
            .remove
            .iter()
            .filter(|r| r.kick == 0 && !r.votes.is_empty())
            .filter_map(|r| match self.entries.get(&r.votes[0]).map(|e| (e.member, &e.payload)) {
                Some((m, Payload::Vote { target, seq, epoch, kick: 0, .. })) if m == r.member && *target == r.member => latency_leave_fed(*seq).map(|f| (r.member, f, *epoch)),
                _ => None,
            })
            .collect();
        if lat.is_empty() {
            return true;
        }
        let lat_ids: BTreeSet<MemberId> = lat.iter().map(|x| x.0).collect();
        let mut members: BTreeSet<MemberId> = self.members.iter().copied().filter(|m| !c.remove.iter().any(|r| r.member == *m && !lat_ids.contains(m))).collect();
        lat.sort_by(|a, b| b.0.cmp(&a.0));
        for (x, fed, epoch) in lat {
            if epoch != self.epoch || !latency_region(&members, &fed) {
                return false;
            }
            members.remove(&x);
        }
        true
    }

    /// Revision 22, S-4 on the transition (@coordinator-opus m5c7b7e5c860e): at most one member is fed at n = 3, so
    /// when a removal leaves n = 3 with two fed, one fed member leaves by request at once: the higher id among the fed
    /// members I know of (myself, and the fed peers the TLP-23 contact rule gives me). The spectator path that follows
    /// (item 7, RV16-2's rejoin after 10 s above both floors) isn't built.
    pub fn s4_leave_due(&mut self) -> Option<Entry> {
        // (Not a member any more, e.g. after my own leave decided: nothing to leave. Before this guard the event was logged
        // on every tick, though `request_leave` sent nothing.)
        if !self.cfg.localize || self.members.len() != 3 || self.fed.is_none() || self.leaving() || self.closed_at.is_some() || !self.members.contains(&self.me) {
            return None;
        }
        let fed: Vec<MemberId> = self.fed_peers.keys().copied().filter(|m| self.members.contains(m) && *m != self.me).collect();
        let top = fed.iter().copied().max()?;
        if top > self.me {
            return None;
        }
        self.ev(format!("S-4: n = 3 with members {fed:?} and {} fed: member {} leaves by request", self.me, self.me));
        self.request_leave()
    }

    /// Revision 22, the latency floor's leave (@coordinator-opus m18d29a6faa0b, 2026-11-02 03:40Z): in the two-fed region
    /// (n − f < q) the full members alone are no quorum and a fed member doesn't judge floors, so a vote to remove a fed
    /// member over rule 3's latency floor can't decide. That member leaves by request instead, sent first (D-5), once it
    /// holds removal votes against itself from at least t + 1 distinct full members: never on a liar's vote alone, and a
    /// withholding liar can't keep it in. One fed member leaves (the user's confirmed text, "one of the fed players drops
    /// to watching"): not while another fed member's leave is held, nor if a higher fed id qualifies too (S-4's order).
    /// The fed members are myself and the fed peers I know of (TLP-23); the votes reach me by value (`record_vc`).
    pub fn latency_leave_due(&mut self) -> Option<Entry> {
        let n = self.members.len();
        if !self.cfg.localize || n < 4 || self.fed.is_none() || self.leaving() || self.closed_at.is_some() || !self.members.contains(&self.me) {
            self.latency_defer_since = None;
            return None;
        }
        let fed: BTreeSet<MemberId> = self.fed_peers.keys().copied().filter(|m| self.members.contains(m)).chain([self.me]).collect();
        let (t, q) = quorum(QuorumRule::Bft, n);
        if n - fed.len() >= q {
            self.latency_defer_since = None;
            return None;
        }
        let full_voters = |x: MemberId| -> Vec<MemberId> {
            self.votes.get(&x).map_or(Vec::new(), |v| v.keys().copied().filter(|&y| y != x && self.members.contains(&y) && !fed.contains(&y)).collect())
        };
        let mine = full_voters(self.me);
        if mine.len() < t + 1 {
            self.latency_defer_since = None;
            return None;
        }
        // Deference, bounded (@coordinator-opus me915f2726556): while another fed member's leave is held, or a higher fed id
        // qualifies in my view, I wait at most one window, then ask anyway; the commit rule (`latency_leaves_ok`) keeps one.
        let defer = fed.iter().any(|&o| o != self.me && self.leaves.contains_key(&o)) || fed.iter().any(|&o| o > self.me && full_voters(o).len() >= t + 1);
        if defer {
            let since = *self.latency_defer_since.get_or_insert(self.now);
            if self.now < since + localize::WINDOW {
                return None;
            }
        }
        self.latency_defer_since = None;
        self.ev(format!(
            "latency floor: n = {n}, fed {fed:?} (n - f < q = {q}); removal votes against me from full members {mine:?} (t + 1 = {}){}: member {} leaves by request",
            t + 1,
            if defer { ", after deferring a window" } else { "" },
            self.me
        ));
        let mask = latency_leave_mask(&fed);
        self.request_leave_with(mask)
    }

    pub fn kick(&mut self, target: MemberId) -> Entry {
        let epoch = self.epoch;
        self.author_vc(Payload::Kick { epoch, target })
    }

    /// D80: give `member` the role `role`.
    pub fn set_role(&mut self, member: MemberId, role: RoleId) -> Entry {
        let epoch = self.epoch;
        self.author_vc(Payload::SetRole { epoch, member, role })
    }

    /// D80: define custom role `role`.
    pub fn define_role(&mut self, role: RoleId, perms: u64) -> Entry {
        let epoch = self.epoch;
        self.author_vc(Payload::DefineRole { epoch, role, perms })
    }

    /// A member's freeze of its own chain when a valid kick targets it (D80): it lets a kick go
    /// through where the other members alone are no quorum, as in a session of two. Its head is the
    /// longest, so it never moves the cut past what the other voters froze.
    fn self_kick_vote(&mut self, kick: EntryId) -> Option<Entry> {
        if self.withhold_vc {
            return None;
        }
        let v = Entry {
            member: self.me,
            seq: self.my_seq + 1,
            prev: self.my_last,
            stamp: self.frame.max(self.now),
            payload: Payload::Vote { epoch: self.epoch, target: self.me, seq: 0, frozen: (self.my_seq, self.my_last), kick },
            carried: None,
            link: 0,
        };
        Some(self.publish(v))
    }

    /// Whether member `j` may join now under the join rules, against `run` (the membership the
    /// change leaves before its joins), and with which starting role (D80).
    fn join_role(&self, j: MemberId, attests: &[Attest], run: &AdminRun) -> Option<RoleId> {
        // Row 0, fixed in every world (user 22:48Z): a world owner joins as Admin. Owners, like the
        // other targets, are as the admin entries before the join in its change leave them (review OWNY-1).
        if run.owners.contains(&key_of(j)) {
            return Some(ADMIN);
        }
        let has = |rel: Rel, g: MemberId| match rel {
            Rel::Is => g == j,
            Rel::Named(n) => n == j,
            _ => attests.iter().any(|a| a.rel == rel && a.with == g && attest_ok(a, j, self.world_id)),
        };
        for rule in &self.join_rules.rules {
            let group: Vec<MemberId> = match rule.target {
                Target::Owner => run.owners.iter().copied().map(member_of).collect(),
                Target::Player => run.members.iter().copied().collect(),
                Target::Admin => run.members.iter().copied().filter(|m| run.roles.get(m) == Some(&ADMIN)).collect(),
                Target::Role(r) => run.members.iter().copied().filter(|m| run.roles.get(m) == Some(&r)).collect(),
            };
            let hit = if rule.all { !group.is_empty() && group.iter().all(|&g| has(rule.rel, g)) } else { group.iter().any(|&g| has(rule.rel, g)) };
            if hit {
                return rule.role;
            }
        }
        self.join_rules.fallback
    }

    /// The usable votes against `x` among `base`, for a removal: any vote for a kick's removal, only
    /// votes that answer no kick for a removal on suspicion or proof (D80).
    fn removal_votes(&mut self, x: MemberId, base: &BTreeSet<MemberId>, kick: bool) -> Vec<(MemberId, VoteRec)> {
        let recs: Vec<(MemberId, VoteRec)> = self
            .votes
            .get(&x)
            .map(|v| v.iter().filter(|(v, r)| base.contains(v) && (kick || r.kick == 0)).map(|(v, r)| (*v, *r)).collect())
            .unwrap_or_default();
        recs.into_iter().filter(|(v, r)| self.vote_check(x, *v, r.frozen) == VoteCheck::Valid).collect()
    }

    /// Review VCR-1: whether `kick` is a held, pending kick of `target`, so a vote naming it freezes
    /// the target's chain for that kick's cut.
    fn kick_live(&self, target: MemberId, kick: EntryId) -> bool {
        self.admin_pending.contains(&kick) && matches!(self.entries.get(&kick).map(|e| &e.payload), Some(Payload::Kick { target: t, .. }) if *t == target)
    }

    /// Review VCR-2: whether this replica can walk `x`'s chain down from `head` to the prefix it holds,
    /// as a cut computed from that head needs.
    fn head_held(&self, x: MemberId, head: (u32, EntryId)) -> bool {
        let (mut s, mut id) = head;
        let held = self.contiguous.get(&x).copied().unwrap_or(0);
        loop {
            if s == 0 || (s <= held && self.by_author.get(&(x, s)) == Some(&id)) {
                return true;
            }
            let Some(e) = self.entries.get(&id) else { return false };
            if e.member != x || e.seq != s {
                return false;
            }
            id = e.prev;
            s -= 1;
        }
    }

    /// Review VC3R-2: whether a decision that answered my last suspicion vote against `m` still holds
    /// me from casting another.
    fn held(&self, m: MemberId) -> bool {
        self.fixes_off & (fix::VC3R2 | fix::LAGKEEP) == 0 && self.vote_hold.get(&m).is_some_and(|&t| self.now < t)
    }

    /// Review VCR-4: whether resets overruled `m`'s votes or caps in `STRIKES_OUT` of the last
    /// `STRIKE_EPOCHS` epochs.
    fn struck_out(&self, m: MemberId) -> bool {
        let from = self.epoch.saturating_sub(STRIKE_EPOCHS);
        self.strikes.get(&m).is_some_and(|v| v.iter().filter(|&&e| e > from).count() >= STRIKES_OUT)
    }

    /// D80: whether a change needs its boundary ahead of the present, with precommit caps: it adds
    /// members, or it leaves the session without an admin (it closes it at the boundary).
    fn change_leads(&self, c: &Change) -> bool {
        !c.add.is_empty() || self.change_closes(c)
    }

    /// D80: whether applying `c` leaves no admin among the members.
    fn change_closes(&self, c: &Change) -> bool {
        let mut run = self.admin_run();
        for &(id, ok) in &c.admin {
            if ok {
                if let Some(e) = self.entries.get(&id) {
                    run.apply(e);
                }
            }
        }
        for r in &c.remove {
            run.members.remove(&r.member);
            run.roles.remove(&r.member);
        }
        !run.has_admin() && !c.add_roles.iter().any(|&r| run.role_admin_like(r))
    }

    /// Keep the change a proposal entry carries, by value, if its author proposes its round in the
    /// current epoch; any version counts (an equivocating proposer's other version may be decided).
    fn note_proposal(&mut self, id: EntryId) {
        let e = &self.entries[&id];
        let Payload::Propose { epoch, round, ref change, .. } = e.payload else { return };
        let v = change.value();
        self.proposal_ids.entry(v).or_insert(id);
        if epoch > self.epoch && self.fixes_off & fix::PF1 == 0 {
            self.later_proposals.entry(epoch).or_default().push(id);
        }
        if epoch == self.epoch && change.epoch == epoch && e.member == self.proposer(epoch, round) {
            let c = change.clone();
            self.vc.changes.entry(v).or_insert(c);
        }
    }

    /// The proposer of `round` of the view change that ends `epoch`: a seeded rotation over the
    /// epoch's members, so no member can choose to propose.
    fn proposer(&self, epoch: u32, round: u32) -> MemberId {
        let mut order: Vec<MemberId> = self.members.iter().copied().collect();
        let salt = self.cfg.seed ^ ((self.world_id as u64) << 40) ^ ((epoch as u64) << 8);
        order.sort_by_key(|&m| (splitmix64(salt ^ (m as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)), m));
        order[round as usize % order.len()]
    }

    /// Members this replica holds a proof against.
    fn proven_set(&self) -> BTreeSet<MemberId> {
        self.proofs.keys().chain(self.liars.keys()).copied().filter(|m| self.members.contains(m)).collect()
    }

    fn proof_of(&self, m: MemberId) -> Proof {
        match self.proofs.get(&m) {
            Some(&(_, a, b)) => Proof { a, b },
            None => self.liars[&m],
        }
    }

    /// This epoch's members outside `excluded`, and (t, q) over them.
    fn base(&self, excluded: &BTreeSet<MemberId>) -> (BTreeSet<MemberId>, usize, usize) {
        let b: BTreeSet<MemberId> = self.members.iter().copied().filter(|m| !excluded.contains(m)).collect();
        let (t, q) = quorum(self.cfg.quorum, b.len());
        (b, t, q)
    }

    /// The members a change excludes from its quorums.
    fn excluded_of(&self, v: u64) -> Option<BTreeSet<MemberId>> {
        self.vc.changes.get(&v).map(|c| c.excluded.iter().map(|(m, _)| *m).collect())
    }

    /// The members whose precommit for `v` in `round` counts toward deciding it: those recorded in
    /// chain order, and (review PF-1) every member proven to have forked its chain that signed one
    /// held here, wherever it sits in that chain. A replica keeps the first version of a seq it
    /// receives, so a forker's chain past its fork may never link here; its precommit in a quorum
    /// others decided on would hold this replica in the old epoch for good. Any version counts: a
    /// faulty member's precommit counts once per value, as Tendermint's commit rule counts signatures.
    fn precommitters(&self, round: u32, v: u64) -> BTreeSet<MemberId> {
        let mut ms: BTreeSet<MemberId> = self.vc.precommits.get(&round).map(|r| r.iter().filter(|&(_, &x)| x == v).map(|(&m, _)| m).collect()).unwrap_or_default();
        if self.fixes_off & fix::PF1 == 0 {
            if let Some(held) = self.pc_held.get(&(self.epoch, round, v)) {
                ms.extend(held.iter().filter(|m| self.proofs.contains_key(m)));
            }
        }
        ms
    }

    /// The members whose prevote for `v` in `round` counts toward a polka (a lock, a re-proposal's
    /// valid round): those recorded in chain order, and (review VC8R-1) every member proven to have
    /// forked its chain that signed one held here, wherever it sits in that chain, in any version, as
    /// `precommitters` counts precommits. A faulty member's prevote counts once per value; two polkas
    /// for different values of one round still need an honest member that prevoted both.
    fn prevoters(&self, round: u32, v: u64) -> BTreeSet<MemberId> {
        let mut ms: BTreeSet<MemberId> = self.vc.prevotes.get(&round).map(|r| r.iter().filter(|&(_, &x)| x == v).map(|(&m, _)| m).collect()).unwrap_or_default();
        if self.fixes_off & fix::VC8R1 == 0 {
            if let Some(held) = self.pv_held.get(&(self.epoch, round, v)) {
                ms.extend(held.iter().filter(|m| self.proofs.contains_key(m)));
            }
        }
        ms
    }

    /// Whether a quorum (counted as the change `v` defines it) prevoted `v` in `round`.
    fn polka(&self, round: u32, v: u64) -> bool {
        let Some(xs) = self.excluded_of(v) else { return false };
        let (base, _, q) = self.base(&xs);
        self.prevoters(round, v).iter().filter(|m| base.contains(m)).count() >= q
    }

    fn precommit_quorum(&self, round: u32, v: u64) -> bool {
        let Some(xs) = self.excluded_of(v) else { return false };
        let (base, _, q) = self.base(&xs);
        self.precommitters(round, v).iter().filter(|m| base.contains(m)).count() >= q
    }

    /// Members of `base` that sent anything (`nil_only`: nil) in `round` of `votes`.
    fn count_round(votes: &BTreeMap<u32, BTreeMap<MemberId, u64>>, round: u32, base: &BTreeSet<MemberId>, nil_only: bool) -> usize {
        votes.get(&round).map_or(0, |r| r.iter().filter(|&(m, &x)| base.contains(m) && (!nil_only || x == 0)).count())
    }

    /// The deepest point of `x`'s chain that at least `k` of the given frozen heads extend
    /// (design 10.8). With at most t faulty voters among them and `k` = their number minus t,
    /// every honest head extends it, so it keeps every entry of `x` stable anywhere; and since
    /// they number more than 2t, two branches can never both reach `k`. None while an entry
    /// needed to walk back is missing (it is fetched from the voter).
    fn robust_cut(&mut self, x: MemberId, heads: &[(MemberId, (u32, EntryId))], k: usize) -> Option<(u32, EntryId)> {
        let mut cur: Vec<(MemberId, u32, EntryId)> = heads.iter().map(|(v, (s, id))| (*v, *s, *id)).collect();
        let mut c = cur.iter().map(|h| h.1).max().unwrap_or(0);
        loop {
            if c == 0 || k == 0 {
                return Some((0, 0));
            }
            for h in cur.iter_mut() {
                while h.1 > c {
                    let Some(e) = self.entries.get(&h.2) else {
                        let (voter, s) = (h.0, h.1);
                        // Review VCR-2: the voter may be the faulty target itself; ask everyone.
                        if self.fixes_off & fix::VCR2 == 0 {
                            self.want_all(x, s, s);
                        } else {
                            self.want_from(voter, x, s, s);
                        }
                        return None;
                    };
                    h.2 = e.prev;
                    h.1 -= 1;
                }
            }
            let mut groups: BTreeMap<EntryId, usize> = BTreeMap::new();
            for h in &cur {
                if h.1 == c {
                    *groups.entry(h.2).or_default() += 1;
                }
            }
            if let Some((&id, _)) = groups.iter().find(|&(_, &n)| n >= k) {
                return Some((c, id));
            }
            c -= 1;
        }
    }

    /// Whether `p` proves `m` faulty (see [`Proof`]). None while an entry it names is missing.
    fn verify_proof(&mut self, m: MemberId, p: Proof) -> Option<bool> {
        let (a, b) = (self.entries.get(&p.a)?.clone(), self.entries.get(&p.b)?.clone());
        if a.member != m || b.member != m {
            return Some(false);
        }
        if a.seq == b.seq && p.a != p.b {
            return Some(true);
        }
        if b.seq == a.seq + 1 && b.prev != p.a {
            return Some(true);
        }
        if let (Payload::Ack { heads, .. }, Payload::Vote { target, frozen, .. }) = (&a.payload, &b.payload) {
            if a.seq < b.seq {
                if let Some(&(_, s, rid)) = heads.iter().find(|h| h.0 == *target) {
                    if s > frozen.0 {
                        return Some(true);
                    }
                    let (mut fs, mut fid) = *frozen;
                    while fs > s {
                        fid = self.entries.get(&fid)?.prev;
                        fs -= 1;
                    }
                    return Some(fid != rid);
                }
            }
        }
        Some(false)
    }

    /// What this replica proposes when it is the round's proposer: remove every member that a
    /// quorum of usable votes names (each with its cut), excluding from the quorum the members it
    /// holds proofs against; with nothing to remove, a reset if a vote of this epoch stands
    /// against a member this replica hears from (it was wrongly suspected, or came back).
    fn build_change(&mut self) -> Option<Change> {
        // Only proofs that their two entries show alone, so other worlds can check the certificate.
        let mut proven = BTreeSet::new();
        let mut excluded = Vec::new();
        for m in self.proven_set() {
            let p = self.proof_of(m);
            if let (Some(a), Some(b)) = (self.entries.get(&p.a), self.entries.get(&p.b)) {
                if proof_self_contained(m, p, a, b) {
                    proven.insert(m);
                    excluded.push((m, p));
                }
            }
        }
        let (base, t, q) = self.base(&proven);
        let mut remove = Vec::new();
        // D80: admin entries in commit order, each judged against the state the ones before it leave.
        // A kick waits, unlisted, until a quorum has frozen its target's chain for the cut.
        let mut run = self.admin_run();
        run.excluded = proven.clone();
        let mut order: Vec<((u8, u64, MemberId, u32), EntryId)> =
            self.admin_pending.iter().filter_map(|id| self.entries.get(id).map(|e| (admin_key(e, self.kick_seed()), *id))).collect();
        order.sort_unstable();
        let mut admin = Vec::new();
        for (_, id) in order {
            let e = self.entries[&id].clone();
            // Round 5 (VC3R-3's low): in commit order, up to the first admin entry that is not this
            // member's canonical version at that seq, or is disputed (a fork). A forked version held
            // only as a duplicate is never proposed, so a validator need not weigh it.
            if self.fixes_off & fix::VC3R3S == 0 && (self.by_author.get(&(e.member, e.seq)) != Some(&id) || self.disputed.contains(&id)) {
                break;
            }
            let mut trial = run.clone();
            let ok = trial.apply(&e);
            if let (true, Payload::Kick { target, .. }) = (ok, &e.payload) {
                let mut usable = self.removal_votes(*target, &base, true);
                if self.fixes_off & fix::VCR2 == 0 {
                    usable.retain(|(_, r)| self.head_held(*target, r.frozen));
                }
                // Review VCR-9: commit order holds for the proposer too. A kick that cannot be carried
                // out yet holds back every later admin entry, rather than being overtaken by them.
                let in_order = self.fixes_off & fix::VCR9 == 0;
                if usable.len() < q {
                    if in_order {
                        break;
                    }
                    continue;
                }
                let heads: Vec<(MemberId, (u32, EntryId))> = usable.iter().map(|(v, r)| (*v, r.frozen)).collect();
                let Some(cut) = self.robust_cut(*target, &heads, usable.len() - t) else {
                    if in_order {
                        break;
                    }
                    continue;
                };
                remove.push(Removal { member: *target, votes: usable.iter().map(|(_, r)| r.id).collect(), cut, kick: id });
            }
            if ok {
                run = trial;
            }
            admin.push((id, ok));
        }
        // Carry-over, gap 2 (b): with Part B a leaver is removed by its leave alone (below). The others' votes on it are
        // cast because it asked (`leave_votes_due`); listed as suspicion votes they would skip plan (A)'s order and
        // region check. (A latency leave left out shares its change with the removal that ended its region, and is void
        // once that decides, so a crashed one is then voted out as any other member.)
        let targets: Vec<MemberId> =
            self.votes.keys().copied().filter(|x| run.members.contains(x) && !(self.cfg.localize && self.leaves.contains_key(x))).collect();
        // Review VC3R-1/2 (round 4): a removal a quorum has voted for, which waits only for entries this
        // replica is still fetching, must not be overtaken by a reset.
        let mut waiting = false;
        for x in targets {
            // Review VC4R-5: a vote whose frozen head is still unresolved half a window after it was
            // recorded counts for nothing here, so one faulty vote cannot hold every reset back.
            let fresh = |r: &VoteRec| self.fixes_off & fix::VC4R5 != 0 || self.now < r.at + self.cfg.window / 2 || self.head_held(x, r.frozen);
            let raw = self.votes.get(&x).map_or(0, |v| v.iter().filter(|(m, r)| base.contains(m) && r.kick == 0 && fresh(r)).count());
            let mut usable = self.removal_votes(x, &base, false);
            if self.fixes_off & fix::VCR2 == 0 {
                usable.retain(|(_, r)| self.head_held(x, r.frozen));
            }
            if usable.len() < q {
                waiting |= raw >= q;
                continue;
            }
            let heads: Vec<(MemberId, (u32, EntryId))> = usable.iter().map(|(v, r)| (*v, r.frozen)).collect();
            let Some(cut) = self.robust_cut(x, &heads, usable.len() - t) else {
                waiting = true;
                continue;
            };
            remove.push(Removal { member: x, votes: usable.iter().map(|(_, r)| r.id).collect(), cut, kick: 0 });
            run.members.remove(&x);
            run.roles.remove(&x);
        }
        // Revision 22: a member's own leave request removes it on its own, cut at the head it froze.
        // Revision 22, plan (A): latency leaves after every other removal, highest id first, each only while the two-fed
        // region still holds with the members left and the fed set it carries; one left out is void (`latency_leaves_ok`).
        let mut leavers: Vec<(MemberId, EntryId)> = self.leaves.iter().map(|(&x, &id)| (x, id)).filter(|(x, _)| run.members.contains(x)).collect();
        leavers.sort_by_key(|&(x, id)| match self.leave_fed(id) {
            None => (0, x as i64),
            Some(_) => (1, -(x as i64)),
        });
        for (x, id) in leavers {
            if remove.iter().any(|r: &Removal| r.member == x) {
                continue;
            }
            let Some(Payload::Vote { frozen, epoch, .. }) = self.entries.get(&id).map(|e| e.payload.clone()) else { continue };
            if let Some(fed) = self.leave_fed(id) {
                if epoch != self.epoch || !latency_region(&run.members, &fed) {
                    if self.latency_left_out.insert(id) {
                        self.ev(format!("plan (A): member {x}'s latency leave left out (fed {fed:?}, members then {:?}): the region has ended", run.members));
                    }
                    continue;
                }
            }
            // Carry-over, gap 2 (b): with Part B, and q - 1 of the others' votes on it, whose frozen heads give its B. With
            // the leave they make q; a member both among an echo-stable ack's q - 1 referencers and among the voters froze
            // at or past it (its vote can't drop its own reference, R3-1), and the two sets share 2q - n - 1 >= 1 of the
            // n - 1 others at n >= 4 (both others at n = 3). So a withholding liar still can't block a leave.
            let votes = if self.cfg.localize {
                let mut usable = self.removal_votes(x, &base, false);
                usable.retain(|(v, r)| *v != x && self.head_held(x, r.frozen));
                if usable.len() + 1 < q {
                    let raw = self.votes.get(&x).map_or(0, |v| v.keys().filter(|&&m| m != x && base.contains(&m)).count());
                    waiting |= raw + 1 >= q;
                    continue;
                }
                std::iter::once(id).chain(usable.iter().map(|(_, r)| r.id)).collect()
            } else {
                vec![id]
            };
            remove.push(Removal { member: x, votes, cut: frozen, kick: 0 });
            run.members.remove(&x);
            run.roles.remove(&x);
        }
        // A join's boundary is about now plus a join lead: a request expiring before it is not proposed.
        let lead_at = (self.now + self.cfg.join_lead).max(self.decided.last().map_or(0, |d| d.0.boundary));
        let carry = self.fixes_off & fix::VC4R2 == 0;
        let joins: Vec<(MemberId, RoleId, Vec<Attest>, (u64, u64))> = self
            .join_requests
            .iter()
            .copied()
            .filter(|j| !self.members.contains(j) && !self.evicted.contains_key(j))
            .filter_map(|j| {
                let req = self.join_reqs.get(&j).copied().unwrap_or((0, u64::MAX));
                if carry && req.1 < lead_at {
                    return None;
                }
                let attests = self.join_attests.get(&j).cloned().unwrap_or_default();
                self.join_role(j, &attests, &run).map(|r| (j, r, attests, req))
            })
            .collect();
        let mut add = Vec::new();
        let mut add_roles = Vec::new();
        let mut add_attests = Vec::new();
        let mut add_reqs = Vec::new();
        for (j, r, a, req) in joins {
            add.push(j);
            add_roles.push(r);
            add_attests.push(if self.fixes_off & fix::VCR7 == 0 { a } else { Vec::new() });
            if carry {
                add_reqs.push(req);
            }
        }
        let last = self.decided.last().map_or(0, |d| d.0.boundary);
        let mut c = Change { epoch: self.epoch, remove, add, boundary: 0, excluded, admin, add_roles, add_attests, add_reqs };
        c.boundary = if self.change_leads(&c) { (self.now + self.cfg.join_lead).max(last) } else { self.now.max(last) };
        if c.remove.is_empty() && c.add.is_empty() && c.admin.is_empty() {
            // A reset answers a suspicion vote against a member heard from again, never a kick vote;
            // and caps from precommits whose change this replica cannot propose (review VC-4).
            let now = self.now;
            // Review VC3R-2: stale caps and ghost kick votes are no longer answered by a reset (round 3)
            // but by voting their authors out (`suspects_due`): a reset keeps the member that forced it.
            let round3 = self.fixes_off & fix::VC3R2 != 0;
            let stale_caps = round3 && self.fixes_off & fix::VC4 == 0 && !self.join_caps.is_empty();
            // Review VCR-1: a kick vote naming no held, pending kick of its target, for as long as a
            // silent member is given, is answered like a suspicion vote.
            let ghosts = round3 && self.fixes_off & fix::VCR1 == 0;
            let reset = stale_caps || self.votes.iter().any(|(y, vs)| {
                let heard = self.heard.get(y).copied().unwrap_or(0);
                // Carry-over, gap 2 (b): votes on a member that has asked to leave wait for its leave, not a reset.
                !proven.contains(y)
                    && !(self.cfg.localize && self.leaves.contains_key(y))
                    && now < heard + self.cfg.suspect_after
                    && vs.values().any(|r| r.kick == 0 || (ghosts && !self.kick_live(*y, r.kick) && now >= r.at + self.cfg.suspect_after))
                    && vs.get(&self.me).is_none_or(|r| heard > r.stamp)
            });
            if !reset {
                return None;
            }
            // Review VC3R-2 (round 4): honest members that suspect the same member vote within a few
            // frames of each other. A reset now would clear the first votes before the quorum arrives,
            // and hold those voters off for a silence limit, so it waits until the newest vote is
            // RESET_WAIT frames old (at most three times that from the oldest), and while a removal
            // waits on fetches.
            if self.fixes_off & fix::VC3R2 == 0 {
                let ats: Vec<u64> = self.votes.values().flat_map(|vs| vs.values().map(|r| r.at)).collect();
                let (newest, oldest) = (ats.iter().max().copied().unwrap_or(0), ats.iter().min().copied().unwrap_or(0));
                if waiting || (now < newest + RESET_WAIT && now < oldest + 3 * RESET_WAIT) {
                    return None;
                }
            }
        }
        Some(c)
    }

    /// Whether `c` is a valid change for this epoch. Deterministic once this replica holds every
    /// entry it names (and has validated the voters' chains up to their votes); None until then.
    /// Review VC7R-1: whether admin entry `id` is valid along its own ancestry. It walks back from the
    /// entry to the prefix of its author's chain this replica has validated, replays the chain-local
    /// rules over the rest (only acks change chain state: `validate`), and judges the entry by VCR-9's
    /// horizon rule. The prefix is common to every branch through it, so the answer is the same at every
    /// replica that holds the entry's ancestry, whichever branch it linked. None while an ancestor is
    /// missing (it is fetched).
    fn valid_on_own_branch(&mut self, id: EntryId) -> Option<bool> {
        let e = self.entries.get(&id)?.clone();
        // Rev 22 (e1): a kick is valid without its chain.
        if matches!(e.payload, Payload::Kick { .. }) {
            return Some(true);
        }
        let m = e.member;
        let linked = if m == self.me { self.my_seq } else { self.contiguous.get(&m).copied().unwrap_or(0) };
        let mut seg: Vec<Entry> = Vec::new();
        let (mut cur, mut k) = (e.prev, e.seq.saturating_sub(1));
        while k > 0 && !(k <= linked && self.by_author.get(&(m, k)) == Some(&cur)) {
            let Some(p) = self.entries.get(&cur).cloned() else {
                self.want_entry_all(cur, 0);
                return None;
            };
            cur = p.prev;
            seg.push(p);
            k -= 1;
        }
        // The chain state after seq k of the validated prefix.
        let mut max_h = self.ack_hist.get(&m).and_then(|a| a.range(..=k).next_back()).map(|(_, h)| *h);
        let mut last_ack = self.ack_stamps.get(&m).and_then(|a| a.range(..=k).next_back()).map_or(0, |(_, st)| *st);
        let mut prev_epoch = self.ack_epoch_from.get(&m).and_then(|a| a.iter().filter(|&(_, &sq)| sq <= k).map(|(&ep, _)| ep).max()).unwrap_or(0);
        let adm = if self.fixes_off & fix::VC3R6 == 0 { self.admitted_epoch.get(&m).copied().unwrap_or(0) } else { 0 };
        for p in seg.iter().rev() {
            if let Payload::Ack { horizon, epoch, .. } = p.payload {
                let epoch_ok = self.fixes_off & fix::VC5 != 0 || (epoch >= prev_epoch && (epoch <= prev_epoch + 1 || epoch <= adm));
                if p.stamp >= last_ack && horizon + self.cfg.horizon_slack + PACK_MAX - 1 >= p.stamp && epoch_ok {
                    max_h = Some(max_h.map_or(horizon, |x| x.max(horizon)));
                    last_ack = p.stamp;
                    prev_epoch = prev_epoch.max(epoch);
                }
            }
        }
        // D73(a), review D73-1: the same bound as `validate`'s admin arm, so a listed admin entry stamped
        // in the wider ack gap (more than horizon_slack before its author's last ack) is void on both paths.
        Some(self.fixes_off & fix::VCR9 != 0 || (max_h.is_none_or(|h| e.stamp > h) && e.stamp + self.cfg.horizon_slack > last_ack))
    }

    fn change_valid(&mut self, c: &Change) -> Option<bool> {
        // Revision 22: a fed member checks only what it can from what it holds.
        if self.fed.is_some() {
            return self.fed_change_valid(c);
        }
        let last = self.decided.last().map_or(0, |d| d.0.boundary);
        let timed = self.fixes_off & fix::VC3 != 0;
        if c.epoch != self.epoch || c.boundary < last || (timed && c.boundary > self.now + 2 * self.cfg.join_lead) {
            return Some(false);
        }
        // Joins: requests this replica holds, from non-members, with a boundary still ahead of it
        // and not too far ahead (10.8). A change that closes the session needs the same lead (D80).
        let mut joiners = BTreeSet::new();
        let carried = self.fixes_off & fix::VC4R2 == 0;
        if carried && c.add_reqs.len() != c.add.len() {
            return Some(false);
        }
        for (i, &j) in c.add.iter().enumerate() {
            if self.members.contains(&j) || self.evicted.contains_key(&j) || !joiners.insert(j) {
                return Some(false);
            }
            // Review VC4R-2: the change carries the joiner's signed request, so every validator judges
            // it alike; waiting for a copy of its own let a request that reached only some members wedge
            // the members locked on the join.
            if carried {
                let (sig, expires) = c.add_reqs[i];
                let ok = if self.fixes_off & fix::VC4R6 != 0 { sig == join_sig(self.world_id, j) } else { sig == join_sig_in(self.session, j, expires) };
                if !ok || c.boundary > expires {
                    return Some(false);
                }
            } else if !self.join_requests.contains(&j) {
                return None;
            }
        }
        for &(id, _) in &c.admin {
            let e = self.entries.get(&id)?;
            // Review VC3R-3: an admin entry counts only from a chain prefix this replica has validated,
            // and only if that validation did not void it (a backdated one is horizon-void, VCR-9).
            if self.fixes_off & fix::VC3R3 == 0 {
                if self.fixes_off & fix::VC7R1 == 0 {
                    if !self.valid_on_own_branch(id)? {
                        return Some(false);
                    }
                    continue;
                }
                if e.member != self.me && self.contiguous.get(&e.member).copied().unwrap_or(0) < e.seq {
                    return None;
                }
                if self.violations.contains(&id) {
                    return Some(false);
                }
            }
        }
        // Round 5 (VC3R-3's low), now only with VC6R-3 switched off: a listed admin entry had to be this
        // member's canonical version at that seq and not disputed. Both change as forks arrive, so a
        // value a quorum had locked on could turn invalid at the locked members, and nothing releases a
        // lock (review VC6R-3, `adminwedge`). Validity is now a pure function of the entry and its
        // ancestry (the VC3R-3 checks above), the same at every replica once it holds the entry; whether
        // to list a canonical, undisputed version is the proposer's choice (`build_change`). Every
        // replica applies the version the change lists, with the outcome the change carries, so a fork
        // cannot be committed at some replicas and not others.
        if self.fixes_off & fix::VC3R3S == 0 && self.fixes_off & fix::VC6R3 != 0 {
            for &(id, _) in &c.admin {
                let (m, sq) = match self.entries.get(&id) {
                    Some(e) => (e.member, e.seq),
                    None => return None,
                };
                if self.by_author.get(&(m, sq)) != Some(&id) || self.disputed.contains(&id) {
                    return Some(false);
                }
            }
        }
        if timed && self.change_leads(c) && c.boundary <= self.now + 1 {
            return Some(false);
        }
        for &(m, p) in &c.excluded {
            let (Some(a), Some(b)) = (self.entries.get(&p.a), self.entries.get(&p.b)) else { return None };
            if !proof_self_contained(m, p, a, b) {
                return Some(false);
            }
        }
        let mut xs = BTreeSet::new();
        for &(m, p) in &c.excluded {
            if !self.members.contains(&m) || !xs.insert(m) || !self.verify_proof(m, p)? {
                return Some(false);
            }
        }
        let (base, t, q) = self.base(&xs);
        // D80: the admin entries, in commit order, each applied or refused as the sequence decides.
        let mut run = self.admin_run();
        run.excluded = xs.clone();
        let mut prev = None;
        let mut kicked: BTreeMap<EntryId, MemberId> = BTreeMap::new();
        for &(id, ok) in &c.admin {
            let e = self.entries.get(&id)?.clone();
            let epoch = match e.payload {
                Payload::Kick { epoch, .. } | Payload::SetRole { epoch, .. } | Payload::DefineRole { epoch, .. } | Payload::Ownership { epoch, .. } => epoch,
                _ => return Some(false),
            };
            let key = admin_key(&e, self.kick_seed());
            if epoch > self.epoch || self.admin_outcomes.contains_key(&id) || prev.is_some_and(|p| p >= key) || run.apply(&e) != ok {
                return Some(false);
            }
            prev = Some(key);
            if let (true, Payload::Kick { target, .. }) = (ok, &e.payload) {
                kicked.insert(id, *target);
            }
        }
        // Revision 22, plan (A): every latency leave listed applies (the region holds for it at commit).
        if !self.latency_leaves_ok(c) {
            return Some(false);
        }
        let mut seen = BTreeSet::new();
        for r in &c.remove {
            let x = r.member;
            // Revision 22: a member's own leave request removes it on its own, cut at the head it froze.
            if r.kick == 0 && r.votes.first().is_some_and(|v| self.leaves.get(&x) == Some(v)) && (r.votes.len() == 1 || self.cfg.localize) {
                let frozen = match self.entries.get(&r.votes[0]).map(|e| &e.payload) {
                    Some(Payload::Vote { frozen, .. }) => *frozen,
                    _ => return Some(false),
                };
                if !self.members.contains(&x) || !seen.insert(x) || !run.members.contains(&x) || r.cut != frozen {
                    return Some(false);
                }
                // Carry-over, gap 2 (b): with Part B, q - 1 votes of the others on it, each checked as a removal's vote.
                if self.cfg.localize {
                    if r.votes.len() < q {
                        return Some(false);
                    }
                    let mut voters = BTreeSet::new();
                    for &vid in &r.votes[1..] {
                        let e = self.entries.get(&vid)?.clone();
                        let Payload::Vote { epoch, target, frozen, kick, .. } = e.payload else { return Some(false) };
                        if epoch != self.epoch || target != x || e.member == x || kick != 0 || !base.contains(&e.member) || !voters.insert(e.member) {
                            return Some(false);
                        }
                        match self.votes.get(&x).and_then(|v| v.get(&e.member)) {
                            Some(rec) if rec.id == vid => {}
                            Some(_) => return Some(false),
                            None => return None,
                        }
                        match self.vote_check(x, e.member, frozen) {
                            VoteCheck::Valid => {}
                            VoteCheck::Missing => return None,
                            VoteCheck::Lie(ack) => {
                                self.on_lie(e.member, ack, vid);
                                return Some(false);
                            }
                        }
                        if self.fixes_off & fix::VCR2 == 0 && !self.head_held(x, frozen) {
                            return None;
                        }
                    }
                }
                continue;
            }
            if !self.members.contains(&x) || !seen.insert(x) || r.votes.len() < q {
                return Some(false);
            }
            // A kick's removal needs its kick applied; a removal on suspicion or proof needs a member
            // no kick removed (D80).
            if r.kick != 0 && kicked.get(&r.kick) != Some(&x) || r.kick == 0 && !run.members.contains(&x) {
                return Some(false);
            }
            let mut voters = BTreeSet::new();
            let mut heads = Vec::new();
            for &vid in &r.votes {
                let e = self.entries.get(&vid)?.clone();
                let Payload::Vote { epoch, target, frozen, kick, .. } = e.payload else { return Some(false) };
                if epoch != self.epoch || target != x || !base.contains(&e.member) || !voters.insert(e.member) || (r.kick == 0 && kick != 0) {
                    return Some(false);
                }
                // Review VCR-2: a vote on one's own kick freezes exactly the entry before it.
                if e.member == x && self.fixes_off & fix::VCR2 == 0 && frozen != (e.seq - 1, e.prev) {
                    return Some(false);
                }
                // The voter's first vote against x in this epoch, in the copy of its chain I hold.
                match self.votes.get(&x).and_then(|v| v.get(&e.member)) {
                    Some(rec) if rec.id == vid => {}
                    Some(_) => return Some(false),
                    None => return None,
                }
                match self.vote_check(x, e.member, frozen) {
                    VoteCheck::Valid => {}
                    VoteCheck::Missing => return None,
                    VoteCheck::Lie(ack) => {
                        self.on_lie(e.member, ack, vid);
                        return Some(false);
                    }
                }
                heads.push((e.member, frozen));
            }
            if self.robust_cut(x, &heads, heads.len() - t)? != r.cut {
                return Some(false);
            }
            if r.kick == 0 {
                run.members.remove(&x);
                run.roles.remove(&x);
            }
        }
        if kicked.keys().any(|k| !c.remove.iter().any(|r| r.kick == *k)) {
            return Some(false);
        }
        if c.add_roles.len() != c.add.len() || c.add_attests.len() != c.add.len() {
            return Some(false);
        }
        for ((&j, &role), carried) in c.add.iter().zip(&c.add_roles).zip(&c.add_attests) {
            // Review VCR-7: the change's own attestations decide, the same at every replica.
            let attests = if self.fixes_off & fix::VCR7 == 0 { carried.clone() } else { self.join_attests.get(&j).cloned().unwrap_or_default() };
            if self.join_role(j, &attests, &run) != Some(role) {
                return Some(false);
            }
        }
        Some(true)
    }

    fn author_vc(&mut self, payload: Payload) -> Entry {
        let e = Entry { member: self.me, seq: self.my_seq + 1, prev: self.my_last, stamp: self.frame.max(self.now), payload, carried: None, link: 0 };
        self.publish(e)
    }

    fn start_round(&mut self, r: u32) {
        self.vc.round = r;
        self.vc.step = Step::Propose;
        self.vc.round_at = self.now;
        self.vc.proposed = false;
        self.stats.max_round = self.stats.max_round.max(r);
    }

    fn prevote(&mut self, v: u64, out: &mut Vec<Entry>) {
        self.vc.step = Step::Prevote;
        if !self.withhold_vc {
            let (epoch, round) = (self.epoch, self.vc.round);
            out.push(self.author_vc(Payload::Prevote { epoch, round, value: v }));
        }
    }

    fn precommit(&mut self, v: u64, out: &mut Vec<Entry>) {
        self.precommit_join(v, 0, out);
    }

    fn precommit_join(&mut self, v: u64, boundary: u64, out: &mut Vec<Entry>) {
        self.vc.step = Step::Precommit;
        if !self.withhold_vc {
            let (epoch, round) = (self.epoch, self.vc.round);
            // Review VC4R-1: a capped precommit names the prevote ids of the polka this replica saw for
            // `v` in `round`, so a member that lacks one (an equivocated prevote sent only to me) can
            // fetch it before it holds my cap unjustified.
            // Review VC6R-2: at most a quorum of prevote ids; more is never needed to justify the cap,
            // and listing every prevoting member overran the encoding in a large session. Capped at
            // MAX_POLKA as well (review 6.1 low): a quorum passes MAX_POLKA only past ~768 members (3x
            // the 256-player target), where the proof is truncated rather than overrunning the encoding.
            let polka = if boundary > 0 {
                let (_, q) = quorum(self.cfg.quorum, self.members.len());
                let mut ids: Vec<EntryId> = self
                    .vc
                    .prevotes
                    .get(&round)
                    .map(|ms| ms.iter().filter(|&(_, &x)| x == v).filter_map(|(k, _)| self.vc.prevote_id.get(&(round, *k)).copied()).collect())
                    .unwrap_or_default();
                ids.truncate(q.min(MAX_POLKA));
                ids
            } else {
                Vec::new()
            };
            out.push(self.author_vc(Payload::Precommit { epoch, round, value: v, boundary, polka }));
        }
    }

    /// Run this epoch's view change and return the entries to broadcast. Called every tick:
    /// Tendermint's rules (algorithm 1, lines 11-65) over chain entries, timeouts in clock frames.
    pub fn view_change_due(&mut self) -> Vec<Entry> {
        let mut out = Vec::new();
        if self.closed_at.is_some() {
            return out;
        }
        // Everyone that holds the log follows its decisions, members or not (a joiner learns so).
        if !self.byz_ally && self.try_decide(&mut out) {
            return out;
        }
        if !self.members.contains(&self.me) {
            return out;
        }
        if self.byz_ally {
            // Prevote and precommit every proposal seen, first per round (a linear chain).
            let props: Vec<(u32, u64)> = self.vc.proposals.iter().map(|(r, (v, _))| (*r, *v)).collect();
            for (r, v) in props {
                let me = self.me;
                if !self.vc.prevotes.get(&r).is_some_and(|x| x.contains_key(&me)) {
                    let epoch = self.epoch;
                    out.push(self.author_vc(Payload::Prevote { epoch, round: r, value: v }));
                    if !self.byz_no_precommit {
                        let boundary = self.vc.changes.get(&v).filter(|c| self.change_leads(c)).map_or(0, |c| c.boundary);
                        out.push(self.author_vc(Payload::Precommit { epoch, round: r, value: v, boundary, polka: vec![] }));
                    }
                }
            }
            // Drive every round another member gave up on: echo its nil prevote or precommit where no
            // proposal is held, so a side of a partition that is no quorum alone still reaches a round
            // one of its own members proposes in.
            let (me, epoch) = (self.me, self.epoch);
            let nil_in = |votes: &BTreeMap<u32, BTreeMap<MemberId, u64>>| -> Vec<u32> {
                votes.iter().filter(|(_, ms)| !ms.contains_key(&me) && ms.values().any(|v| *v == 0)).map(|(r, _)| *r).collect()
            };
            for r in nil_in(&self.vc.prevotes) {
                if !self.vc.proposals.contains_key(&r) {
                    out.push(self.author_vc(Payload::Prevote { epoch, round: r, value: 0 }));
                }
            }
            for r in nil_in(&self.vc.precommits) {
                if !self.vc.proposals.contains_key(&r) && !self.byz_no_precommit {
                    out.push(self.author_vc(Payload::Precommit { epoch, round: r, value: 0, boundary: 0, polka: vec![] }));
                }
            }
            return out;
        }
        if self.vc.since.is_none() {
            let joins = self.join_requests.iter().any(|j| !self.members.contains(j) && !self.evicted.contains_key(j));
            let busy = joins
                || (self.fixes_off & fix::VC4 == 0 && !self.join_caps.is_empty())
                || !self.votes.is_empty()
                // Revision 22: a member's leave request starts a view change on its own.
                || self.leaves.keys().any(|m| self.members.contains(m))
                || !self.admin_pending.is_empty()
                || !self.vc.proposals.is_empty()
                || !self.vc.prevotes.is_empty()
                || !self.vc.precommits.is_empty();
            if !busy {
                return out;
            }
            self.vc.since = Some(self.now);
            self.start_round(0);
        }
        for _ in 0..16 {
            let before = (self.vc.round, self.vc.step, self.vc.proposed, self.vc.polka_done.len(), self.vc.prevote_timer.len(), self.vc.precommit_timer.len());
            self.vc_rules(&mut out);
            if self.try_decide(&mut out) {
                return out;
            }
            if before == (self.vc.round, self.vc.step, self.vc.proposed, self.vc.polka_done.len(), self.vc.prevote_timer.len(), self.vc.precommit_timer.len()) {
                break;
            }
        }
        out
    }

    fn vc_rules(&mut self, out: &mut Vec<Entry>) {
        let (e, r) = (self.epoch, self.vc.round);
        let proven = self.proven_set();
        let (any_base, t_any, q_any) = self.base(&proven);
        // Lines 55-56: more than t members are in a later round: go there.
        let mut hi: BTreeMap<MemberId, u32> = BTreeMap::new();
        for votes in [&self.vc.prevotes, &self.vc.precommits] {
            for (&rr, ms) in votes.range(r + 1..) {
                for m in ms.keys().filter(|m| any_base.contains(m)) {
                    let x = hi.entry(*m).or_insert(rr);
                    *x = (*x).max(rr);
                }
            }
        }
        let mut later: Vec<u32> = hi.into_values().collect();
        later.sort_unstable_by(|a, b| b.cmp(a));
        if later.len() > t_any {
            self.start_round(later[t_any]);
            return;
        }
        // Lines 14-19: the round's proposer proposes its valid value, or a new one.
        if self.vc.step == Step::Propose && !self.vc.proposed && !self.withhold_vc && self.proposer(e, r) == self.me {
            let byz = self.byz_change.clone();
            let (vr, change) = match (self.vc.valid, byz) {
                (_, Some(c)) => (NO_ROUND, Some(Change { epoch: e, boundary: c.boundary.max(self.now), ..c })),
                (Some((v, vr)), None) => (vr, self.vc.changes.get(&v).cloned()),
                (None, None) => (NO_ROUND, self.build_change()),
            };
            // Members that fail together are suspected within a few frames of each other: give the
            // votes on every member this replica itself suspects a few frames to arrive, so that one
            // change removes them all.
            let me = self.me;
            let gathering = vr == NO_ROUND
                && self.now < self.vc.round_at + VC_GATHER
                && change.as_ref().is_some_and(|c| self.votes.iter().any(|(y, vs)| vs.contains_key(&me) && !c.remove.iter().any(|x| x.member == *y)));
            if let Some(c) = change.filter(|_| !gathering) {
                self.vc.proposed = true;
                self.ev(format!("view change: propose in round {r}: remove {:?}, excluded {:?}", c.remove.iter().map(|x| (x.member, x.cut.0)).collect::<Vec<_>>(), c.excluded.iter().map(|x| x.0).collect::<Vec<_>>()));
                out.push(self.author_vc(Payload::Propose { epoch: e, round: r, valid_round: vr, change: c }));
            }
        }
        // Lines 22-33: prevote on the round's proposal.
        if self.vc.step == Step::Propose {
            if let Some(&(v, vr)) = self.vc.proposals.get(&r) {
                let c = self.vc.changes.get(&v).cloned();
                let ok = match c {
                    Some(c) => self.change_valid(&c),
                    None => Some(false),
                };
                // Rev 22 (c): a value a fed member can't fully check gets its prevote once t + 1 distinct members (any,
                // fed or not) have prevoted it in this round: the first honest prevote is a full member's, which checked
                // it against the whole chain.
                let ok = if ok.is_none() && self.fed.is_some() {
                    let n = self.vc.prevotes.get(&r).map_or(0, |ms| ms.iter().filter(|&(m, &x)| x == v && any_base.contains(m)).count());
                    if n > t_any { Some(true) } else { None }
                } else {
                    ok
                };
                // Review VC-3: validity never changes with time (a Tendermint lock relies on it); a
                // fresh proposal must also bring its boundary in time, a re-proposal need not.
                let fresh_in_time = match self.vc.changes.get(&v) {
                    Some(c) => {
                        let last = self.decided.last().map_or(0, |d| d.0.boundary);
                        let leads = self.change_leads(c);
                        c.boundary >= last && c.boundary <= self.now + 2 * self.cfg.join_lead && (!leads || c.boundary > self.now + 1)
                    }
                    None => false,
                };
                let decision = match ok {
                    None => None,
                    Some(ok) if vr == NO_ROUND => Some(ok && fresh_in_time && self.vc.locked.is_none_or(|(lv, _)| lv == v)),
                    Some(ok) if vr < r && self.polka(vr, v) => {
                        Some(ok && self.vc.locked.is_none_or(|(lv, lr)| lr <= vr || lv == v))
                    }
                    Some(_) => None,
                };
                if let Some(yes) = decision {
                    self.prevote(if yes { v } else { 0 }, out);
                }
            }
        }
        // Lines 34-35: a quorum of prevotes of any kind arms the prevote timeout.
        if self.vc.step == Step::Prevote && !self.vc.prevote_timer.contains_key(&r) && Self::count_round(&self.vc.prevotes, r, &any_base, false) >= q_any {
            self.vc.prevote_timer.insert(r, self.now);
        }
        // Lines 36-43: a quorum prevoted the round's proposal: lock it and precommit it.
        if self.vc.step != Step::Propose && !self.vc.polka_done.contains(&r) {
            if let Some(&(v, _)) = self.vc.proposals.get(&r) {
                if self.polka(r, v) {
                    let c = self.vc.changes.get(&v).cloned().unwrap_or_default();
                    let leads = self.change_leads(&c);
                    // My precommit caps my acks below the boundary only while it is still ahead of
                    // me. Past it, I may still precommit (without a cap) once some member's acks are
                    // capped at or below it: readers then confirm nothing from the boundary on until
                    // they reach the next epoch, whoever that member is (review VC-3).
                    let ahead = c.boundary > self.now;
                    let capped = self.join_caps.values().any(|&(_, b)| b <= c.boundary);
                    let in_time = !leads || ahead || (self.fixes_off & fix::VC3 == 0 && capped);
                    // Rev 22 (c): a polka (q >= t + 1 prevotes) settles what a fed member can't check.
                    let valid = match self.change_valid(&c) {
                        None if self.fed.is_some() => Some(true),
                        x => x,
                    };
                    if self.vc.changes.contains_key(&v) && in_time && valid == Some(true) {
                        self.vc.polka_done.insert(r);
                        if self.vc.step == Step::Prevote {
                            self.vc.locked = Some((v, r));
                            self.precommit_join(v, if leads && ahead { c.boundary } else { 0 }, out);
                        }
                        self.vc.valid = Some((v, r));
                    }
                }
            }
        }
        // Lines 44-46: a quorum prevoted nil.
        if self.vc.step == Step::Prevote && Self::count_round(&self.vc.prevotes, r, &any_base, true) >= q_any {
            self.precommit(0, out);
        }
        // Lines 47-48: a quorum of precommits of any kind arms the precommit timeout; a quorum of
        // nil precommits ends the round at once (no value can be decided in it).
        if Self::count_round(&self.vc.precommits, r, &any_base, true) >= q_any {
            self.start_round(r + 1);
            return;
        }
        if !self.vc.precommit_timer.contains_key(&r) && Self::count_round(&self.vc.precommits, r, &any_base, false) >= q_any {
            self.vc.precommit_timer.insert(r, self.now);
        }
        // Lines 57-65: timeouts, lengthening with the round.
        let to = self.cfg.vc_timeout + self.cfg.vc_timeout_step * r as u64;
        if self.vc.step == Step::Propose && self.now >= self.vc.round_at + to {
            self.prevote(0, out);
        }
        if self.vc.step == Step::Prevote && self.vc.prevote_timer.get(&r).is_some_and(|&a| self.now >= a + to) {
            self.precommit(0, out);
        }
        if self.vc.precommit_timer.get(&r).is_some_and(|&a| self.now >= a + to) {
            self.start_round(r + 1);
        }
        let _ = t_any;
    }

    /// Lines 49-54: a quorum precommitted a value in some round: decide it and apply it. A value
    /// this replica holds no proposal for is fetched from those that precommitted it.
    fn try_decide(&mut self, out: &mut Vec<Entry>) -> bool {
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for (&rr, ms) in &self.vc.precommits {
            let mut vals: BTreeMap<u64, Vec<MemberId>> = BTreeMap::new();
            for (&m, &v) in ms {
                if v != 0 {
                    vals.entry(v).or_default().push(m);
                }
            }
            for (v, ms) in vals {
                if !self.vc.changes.contains_key(&v) {
                    unknown.push((v, ms));
                } else if self.precommit_quorum(rr, v) {
                    candidates.push((rr, v));
                }
            }
        }
        for (v, ms) in unknown {
            for m in ms {
                if m != self.me && self.value_asked.insert((m, v)) {
                    self.value_wants.push((m, v));
                }
            }
        }
        for (rr, v) in candidates {
            // A change's exclusions set its quorum: decide it only once every one is proved, or a
            // faulty minority could exclude honest members and decide alone (P2-4).
            let ex = self.vc.changes[&v].excluded.clone();
            let mut proved = true;
            for (m, p) in ex {
                if self.verify_proof(m, p) != Some(true) {
                    proved = false;
                }
            }
            // Review VCR-8: applying a change runs its admin entries, so every one must be held first;
            // ask those that precommitted it for the ones missing.
            if proved && self.fixes_off & fix::VCR8 == 0 {
                // Carry-over, hole 2: and with Part B every vote a removal lists, so each replica takes the removed
                // member's B (the highest frozen head among them) from the same votes. A spectator that applied S-4's
                // leave without holding the leave vote took B = 0, ended the leaver's acks there and cut it at 0
                // (leave-s4-rejoin).
                let votes = self.vc.changes[&v].remove.iter().flat_map(|r| r.votes.iter().copied()).filter(|_| self.cfg.localize);
                let missing: Vec<EntryId> =
                    self.vc.changes[&v].admin.iter().map(|(id, _)| *id).chain(votes).filter(|id| !self.entries.contains_key(id)).collect();
                if !missing.is_empty() {
                    let askers: Vec<MemberId> = self.vc.precommits.get(&rr).map(|ms| ms.iter().filter(|&(_, &x)| x == v).map(|(&m, _)| m).collect()).unwrap_or_default();
                    for id in missing {
                        for &m in &askers {
                            self.want_entry(m, id, 0);
                        }
                    }
                    continue;
                }
            }
            if proved {
                let c = self.vc.changes[&v].clone();
                self.apply_change(c, rr, out);
                return true;
            }
        }
        false
    }

    /// Apply a decided change: the next epoch's members, each removed member's cut, a new epoch
    /// with no votes, and the epoch certificate's material.
    fn apply_change(&mut self, c: Change, round: u32, out: &mut Vec<Entry>) {
        let v = c.value();
        let xs: BTreeSet<MemberId> = c.excluded.iter().map(|(m, _)| *m).collect();
        // Review VC7R-5: exactly a quorum of signers, the lowest member ids among the precommits held, so
        // the certificate's size is bounded and replicas holding more than a quorum agree on it. Two
        // replicas that decided on different quorums still sign different (equally valid) certificates:
        // a certificate is checked by its signatures (`epoch_cert_ok`), never compared by its bytes.
        let (_, _, q_sig) = self.base(&xs);
        let sigs: Vec<(MemberId, u64)> =
            self.precommitters(round, v).into_iter().filter(|m| !xs.contains(m)).take(q_sig).map(|m| (m, vc_sig(self.world_id, m, self.epoch, round, v))).collect();
        let removed: Vec<MemberId> = c.remove.iter().map(|r| r.member).collect();
        // D80: role entries in commit order (kicks are among the removals), then the removals, then
        // each joiner's starting role against the membership they leave.
        let closes = self.change_closes(&c);
        for &(id, ok) in &c.admin {
            self.admin_outcomes.insert(id, ok);
            if ok {
                self.stats.admin_applied += 1;
                // Review VCR-8: with the fix, every listed entry is held here (`try_decide`); without
                // it, a missing one panics the replica, as it did before.
                let payload = if self.fixes_off & fix::VCR8 != 0 { Some(self.entries[&id].payload.clone()) } else { self.entries.get(&id).map(|e| e.payload.clone()) };
                match payload {
                    Some(Payload::SetRole { member, role, .. }) => {
                        self.roles.insert(member, role);
                    }
                    Some(Payload::DefineRole { role, perms, .. }) => {
                        self.role_defs.insert(role, perms);
                    }
                    // 12.5: the session commits the certificate at the deciding boundary. Its
                    // validity does not depend on the frame, so it applies here as in the run.
                    Some(Payload::Ownership { cert, .. }) => {
                        let lineage = self.world_id;
                        if let Some(b) = self.book.as_mut() {
                            let r = b.commit_local(cert, lineage, c.boundary);
                            debug_assert!(matches!(r, Ok(Insert::Applied)), "a decided certificate applies: {r:?}");
                        }
                    }
                    _ => {}
                }
            } else {
                self.stats.admin_refused += 1;
            }
        }
        // F2, revision 21 (a): removing from a Part B epoch, the cut is the final vouched prefix (`finalize_cuts`);
        // until then the removed chain keeps linking and nothing of it past its vouched prefix counts.
        let pb_old = self.cfg.localize && self.members.len() >= 3;
        for r in &c.remove {
            self.members.remove(&r.member);
            self.roles.remove(&r.member);
            self.removed_in.insert(r.member, self.epoch + 1);
            // Carry-over, hole 2: B, the highest frozen head of its chain among the listed votes (held, `try_decide`), the
            // voters' only (gap 2 (b)): never its own vote's (a leave's or a self-kick's), its own head, which a dead
            // uplink may never deliver.
            let b = r
                .votes
                .iter()
                .filter_map(|id| match self.entries.get(id).map(|e| (e.member, &e.payload)) {
                    Some((voter, Payload::Vote { frozen, .. })) if voter != r.member => Some(frozen.0),
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            self.bound.insert(r.member, b);
            if pb_old {
                self.frozen.insert(r.member, u32::MAX);
                self.evicted.insert(r.member, (u32::MAX, 0));
                self.pb_pending.insert(r.member);
                continue;
            }
            self.frozen.insert(r.member, r.cut.0);
            self.cut_ids.insert(r.member, r.cut.1);
            self.evicted.insert(r.member, (r.cut.0, 0));
        }
        for (&j, &role) in c.add.iter().zip(&c.add_roles) {
            self.roles.insert(j, role);
        }
        for &j in &c.add {
            self.members.insert(j);
            self.known.insert(j);
            self.admitted.insert(j, c.boundary);
            self.followers.remove(&j);
            self.join_requests.remove(&j);
            self.join_attests.remove(&j);
            self.join_reqs.remove(&j);
        }
        for (id, _) in &c.admin {
            self.admin_pending.remove(id);
        }
        // A pending entry about a member no longer here can never apply: drop it unjudged.
        let gone: Vec<EntryId> = self
            .admin_pending
            .iter()
            .copied()
            .filter(|id| match self.entries.get(id).map(|e| &e.payload) {
                Some(Payload::Kick { target: x, .. }) | Some(Payload::SetRole { member: x, .. }) => !self.members.contains(x),
                _ => false,
            })
            .collect();
        for id in gone {
            self.admin_pending.remove(&id);
        }
        // 12.5: a pending certificate the decision made stale or invalid can never apply: drop it
        // unjudged, as `gone` does (V1's receipt rule).
        let dead: Vec<EntryId> = self
            .admin_pending
            .iter()
            .copied()
            .filter(|id| matches!(self.entries.get(id).map(|e| &e.payload), Some(Payload::Ownership { cert, .. }) if !self.ownership_applies(cert)))
            .collect();
        for id in dead {
            self.admin_pending.remove(&id);
        }
        // Review OWNY-2: a held certificate whose parents the decision brought into the book is judged
        // again: pending if it now applies, dropped if it never can; still held while a parent is missing.
        let held: Vec<EntryId> = self.own_held.iter().copied().collect();
        for id in held {
            let Some(e) = self.entries.get(&id) else { continue };
            let Payload::Ownership { cert, .. } = &e.payload else { continue };
            if self.admin_outcomes.contains_key(&id) || self.closed_at.is_some() || !self.members.contains(&e.member) {
                self.own_held.remove(&id);
                continue;
            }
            if self.book.as_ref().is_some_and(|b| cert.prev.iter().any(|p| b.cert(*p).is_none())) {
                continue;
            }
            match self.ownership_judge(cert) {
                Err(OwnError::MissingParent) => {}
                r => {
                    self.own_held.remove(&id);
                    if r == Ok(Insert::Applied) {
                        self.admin_pending.insert(id);
                    }
                }
            }
        }
        // Review VC-6: a decided epoch restarts the slowness timers, so one borderline view cannot vote
        // again the instant a reset ends. Review VCR-4: the unbacked-epoch timers do not restart (a
        // member climbing one epoch per ack would never be voted out); they end when this replica
        // reaches the epoch they wait for.
        // Review VC3R-2: without strikes, a member forcing resets must not be able to keep a slow member
        // from its demotion by restarting everyone's timers, so they survive decisions. A replica
        // restarts only its own timer on a member its standing vote was against and that the decision
        // kept: that is the vote the decision answered (VC-6: it must not vote again the instant after).
        if self.fixes_off & (fix::VC3R2 | fix::LAGKEEP) != 0 {
            self.lagging.clear();
        } else {
            // VC-6: a member that alone sees another as slow must not force a reset the instant each one
            // ends. A decision answered my standing suspicion vote against y if it kept y, its proposal
            // reached me after I voted, and no other member's vote against y stood with mine. Then I hold
            // off voting against y, twice as long each time in a row, up to a silence limit. A vote that
            // others share, or that the decided proposal never weighed (it reached me first), is cast
            // again at once: a member forcing a reset every epoch cannot keep a quorum of honest votes
            // from standing together, and members whose timers run apart still meet.
            let seen = self.vc.seen_at.get(&v).copied();
            let mine: Vec<(MemberId, bool)> = self
                .votes
                .iter()
                .filter(|(y, _)| !removed.contains(y))
                .filter_map(|(y, vs)| vs.get(&self.me).filter(|r| r.kick == 0).map(|r| (*y, vs.len() == 1 && seen.is_none_or(|s| r.at <= s))))
                .collect();
            for (y, alone) in mine {
                if alone {
                    let n = self.hold_n.entry(y).or_insert(0);
                    *n = (*n + 1).min(8);
                    let wait = (RESET_WAIT << *n).min(self.cfg.suspect_after);
                    self.vote_hold.insert(y, self.now + wait);
                } else {
                    self.hold_n.remove(&y);
                }
            }
            for y in &removed {
                self.hold_n.remove(y);
                self.vote_hold.remove(y);
            }
        }
        // Review VCR-4: a reset overrules every vote and cap of its epoch: count each against its author,
        // so a member that forces one each epoch is voted out (`STRIKES_OUT`). Votes against a member
        // already struck out are not counted against their voters.
        if self.fixes_off & fix::VCR4 == 0 && self.fixes_off & fix::VC3R2 != 0 && c.remove.is_empty() && c.add.is_empty() && c.admin.is_empty() {
            let mut struck: BTreeSet<MemberId> = self.join_caps.keys().copied().collect();
            for (y, vs) in &self.votes {
                if self.struck_out(*y) {
                    continue;
                }
                for (v, r) in vs {
                    if *v != *y && (r.kick == 0 || !self.kick_live(*y, r.kick)) {
                        struck.insert(*v);
                    }
                }
            }
            let reached = self.epoch + 1;
            for m in struck {
                let v = self.strikes.entry(m).or_default();
                if v.last() != Some(&reached) {
                    v.push(reached);
                }
            }
        }
        self.join_caps.clear();
        if self.fixes_off & fix::VC6R4 == 0 {
            // Review VC6R-4: reset only the timers a cleared vote made stale (this decision clears every
            // vote, below): the targets of my own votes looked silent or slow to me (`suspect_since`);
            // new members count from admission. Round 6 cleared every timer at every decision, so a
            // member that forces decisions (a no-op admin entry every 2 s) kept a crashed member's
            // silence spare-window from ever ripening.
            for (y, vs) in &self.votes {
                if vs.contains_key(&self.me) {
                    self.suspect_since.remove(y);
                }
            }
            for &j in &c.add {
                self.suspect_since.remove(&j);
            }
        } else {
            // Review VC5R-4 (round 6): the silence spare-window is per-epoch; a decision clears it so a
            // stale entry does not vote a member out in the new epoch.
            self.suspect_since.clear();
        }
        for &j in &c.add {
            self.admitted_epoch.insert(j, c.epoch + 1);
            // Review VC3R-6: a new member's silence counts from its admission, not from its last entry
            // as a follower (a joiner restored from a state transfer has authored none).
            if self.fixes_off & fix::VC3R6 == 0 {
                let t = self.heard.entry(j).or_insert(0);
                *t = (*t).max(self.now);
            }
        }
        let from = self.epoch;
        self.decided.push((c.clone(), round, sigs));
        self.epoch += 1;
        self.vc_log.push((self.epoch, self.now, round, removed.clone()));
        self.stats.view_changes += 1;
        if removed.is_empty() && c.add.is_empty() && c.admin.is_empty() {
            self.stats.resets += 1;
        }
        self.ev(format!(
            "VIEW CHANGE decided (epoch {from} -> {}, round {round}): removed {:?} with cuts {:?}, excluded by proof {:?}, added {:?} from frame {}; members {:?}",
            self.epoch,
            removed,
            c.remove.iter().map(|r| r.cut.0).collect::<Vec<_>>(),
            xs,
            c.add,
            c.boundary,
            self.members
        ));
        // A new epoch: the old one's votes, freezes and view-change state end.
        self.votes.clear();
        // A leave request stands until its author is removed (it is its author's standing wish, not a vote).
        let members_now = self.members.clone();
        // Revision 22, plan (A): a latency leave the decision didn't apply is void; its sender asks again if it qualifies
        // again.
        let latency: BTreeSet<MemberId> = self.leaves.iter().filter(|(_, id)| self.leave_fed(**id).is_some()).map(|(m, _)| *m).collect();
        self.leaves.retain(|m, _| members_now.contains(m) && !latency.contains(m));
        self.vc = Vc::default();
        self.pc_held.retain(|&(e, _, _), _| e >= self.epoch);
        self.pv_held.retain(|&(e, _, _), _| e >= self.epoch);
        let unfreeze: Vec<MemberId> = self.frozen.keys().copied().filter(|m| !self.evicted.contains_key(m)).collect();
        for m in unfreeze {
            self.frozen.remove(&m);
        }
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        for m in members {
            self.extend_chain(m, out);
        }
        // F2: rule 2 is decided by an entry's own epoch (`part_b_entry`), which I couldn't read for an entry of this epoch
        // that I simulated before deciding it. Re-simulate from the earliest such entry: its verdict waited for the epoch,
        // so it lies above my confirmed frame (cutfix-move: member 6 confirmed member 1's touch at frame 421 simulated
        // without rule 2 and diverged from every other replica). One below my confirmed frame would change a confirmed
        // frame, so it is a hard failure (@coordinator-opus m63b968ceb600), not a log line.
        if self.cfg.localize {
            let first = self.eff.iter().filter(|(id, _)| self.entries.get(id).is_some_and(|e| self.entry_epoch(e.member, e.seq) == self.epoch)).map(|(_, &f)| f).min();
            match first {
                Some(f) if f >= self.conf => self.mark_dirty(f),
                Some(f) => panic!("F2: m{} entered epoch {} with an entry of it acting at frame {f}, below its confirmed frame {}", self.me, self.epoch, self.conf),
                None => {}
            }
        }
        for x in removed {
            self.adopt_cut(x);
        }
        // An admitted member's entries act from now on (those stamped before its boundary are void).
        let joined: Vec<EntryId> = self.entries.iter().filter(|(_, e)| c.add.contains(&e.member)).map(|(id, _)| *id).collect();
        for id in joined {
            self.set_eff(id);
        }
        // D80: no admin is left, so the session closes at the boundary. The precommit caps kept every
        // frame from it on unconfirmed, so nothing confirmed is undone.
        if closes && self.closed_at.is_none() {
            self.closed_at = Some(c.boundary);
            // V3 (user 08:31Z): closing removes every other player, tells everyone, and allows no further
            // change to the world; no longer grace period and no automatic handover.
            self.closed_members = self.members.iter().copied().collect();
            self.ev(format!("SESSION CLOSED (epoch {}): no admin is left; every member {:?} is removed, and nothing stamped at or after frame {} acts", self.epoch, self.closed_members, c.boundary));
            let late: Vec<EntryId> = self.entries.iter().filter(|(_, e)| e.stamp >= c.boundary && !is_protocol(&e.payload)).map(|(id, _)| *id).collect();
            for id in late {
                self.set_eff(id);
            }
        }
        if let Some(ids) = self.vc_later.remove(&self.epoch) {
            for id in ids {
                let m = self.entries[&id].member;
                self.record_vc(m, id, out);
            }
        }
        // Review PF-1: proposals of the new epoch kept by value before it began (another version, or a
        // proven forker's past its fork) name values it may decide. Indexed by epoch (VC7R-6).
        if self.fixes_off & fix::PF1 == 0 {
            let mut keep = self.later_proposals.split_off(&self.epoch);
            let held = keep.remove(&self.epoch).unwrap_or_default();
            self.later_proposals = keep;
            for id in held {
                self.note_proposal(id);
            }
        }
        self.recompute_conf();
        // Review VCR-3: ack in the new epoch at once, so this member's acks never skip an epoch (an
        // ack's epoch rises by at most one, VC-5), however many decisions fall between two acks.
        // Carry-over, hole 2: a member that has asked to leave authors no ack (and so no fed echo) from its request on;
        // a void latency leave was dropped above, so its sender acks again in the new epoch.
        if self.fixes_off & fix::VCR3 == 0 && !(self.quiet_until_member && !self.members.contains(&self.me)) && !self.acks_held() {
            out.push(self.author_ack());
        }
    }

    /// Make the version of removed member `x`'s chain that ends at its cut the one this replica
    /// holds: walk back from the cut id, fetch what is missing, replace any other version (8.2
    /// verdicts are re-derived along it), and keep exactly the cut (P2-4).
    fn adopt_cut(&mut self, x: MemberId) {
        if self.cut_done.contains(&x) {
            return;
        }
        let (Some(&(cut, _)), Some(&cid)) = (self.evicted.get(&x), self.cut_ids.get(&x)) else { return };
        let held = self.contiguous.get(&x).copied().unwrap_or(0);
        if cut > 0 && !(held >= cut && self.by_author.get(&(x, cut)) == Some(&cid)) {
            let mut ids = Vec::with_capacity(cut as usize);
            let (mut s, mut id) = (cut, cid);
            loop {
                let Some(e) = self.entries.get(&id) else {
                    // Review VC3R-7: the walk knows the exact id it lacks, so ask for it and the 15 entries
                    // below it down its prev links: a member holding the other branch answers nothing.
                    if self.fixes_off & fix::VC3R7 == 0 {
                        self.want_entry_all(id, 15);
                        return;
                    }
                    // One request for the whole missing stretch of the cut's branch (review VC-2).
                    let from = if self.fixes_off & fix::VC2 == 0 { self.stable_hi.get(&x).copied().unwrap_or(0) + 1 } else { s };
                    // Review VCR-5: a reply holds at most 16 entries, so ask for the highest missing
                    // stretch (the walk down needs it first) and go lower as the walk does.
                    let from = if self.fixes_off & fix::VCR5 == 0 { from.max(s.saturating_sub(15)) } else { from };
                    self.want_all(x, from.min(s), s);
                    return;
                };
                ids.push((s, id));
                if s == 1 {
                    break;
                }
                id = e.prev;
                s -= 1;
            }
            let mut changed = false;
            for &(s, id) in &ids {
                if self.by_author.get(&(x, s)) != Some(&id) {
                    self.by_author.insert((x, s), id);
                    changed = true;
                }
            }
            if changed {
                self.contiguous.insert(x, 0);
                self.chain_state.remove(&x);
                self.ack_hist.remove(&x);
                self.ack_stamps.remove(&x);
                if self.fixes_off & fix::RECUT == 0 {
                    self.ack_epoch_from.remove(&x);
                }
            }
            // Re-validating the cut's branch replays its acks, and each ack recomputes confirmation.
            // Confirming then, before that branch's entries act, would confirm frames whose
            // entries are about to change (found by fork9, review VC-2): settle first, confirm after.
            let fixed = self.fixes_off & fix::VC2 == 0;
            self.adopting = fixed;
            // Review VCR-6: reconciling before the branch is re-validated would apply its entries before
            // their horizon verdicts exist, a backdated one below the confirmed frame included. The
            // suspension, and the reconcile and recompute after, are enough.
            if fixed && self.fixes_off & fix::VCR6 != 0 {
                self.reconcile(x);
            }
            let mut out = Vec::new();
            self.extend_chain(x, &mut out);
            self.adopting = false;
        }
        self.reconcile(x);
        if self.contiguous.get(&x).copied().unwrap_or(0) >= cut {
            let v = self.ack_hist.get(&x).and_then(|a| a.range(..=cut).next_back()).map_or(0, |(_, h)| h + 1);
            self.evicted.insert(x, (cut, v));
            self.cut_done.insert(x);
            self.ev(format!("member {x} evicted (epoch {}): its chain is cut at seq {cut}; it no longer counts from frame {v}", self.epoch));
        }
        if self.fixes_off & fix::VC2 == 0 {
            self.recompute_conf();
        }
    }

    /// The certificate of the view change that started `epoch` (R3-8b).
    pub fn epoch_cert(&self, epoch: u32) -> Option<EpochCert> {
        let (c, round, sigs) = self.decided.get(epoch.checked_sub(1)? as usize)?;
        let mut proofs = Vec::new();
        for (_, p) in &c.excluded {
            proofs.push(self.entries.get(&p.a)?.clone());
            proofs.push(self.entries.get(&p.b)?.clone());
        }
        Some(EpochCert { world: self.world_id, epoch, change: c.clone(), round: *round, members: self.members_at(epoch)?, precommits: sigs.clone(), proofs })
    }

    /// This world's certificates for epochs 1..=`epoch`: what a fact signed in `epoch` carries so
    /// any world can check it from this world's genesis (P2-5).
    pub fn epoch_certs_upto(&self, epoch: u32) -> Option<Vec<EpochCert>> {
        (1..=epoch).map(|e| self.epoch_cert(e)).collect()
    }

    /// The epoch of this world active at `frame`, by the boundaries of the decided changes.
    pub fn epoch_at(&self, frame: u64) -> u32 {
        self.decided.iter().take_while(|d| d.0.boundary <= frame).count() as u32
    }

    /// A certificate for `fact`, a fact about `frame`: Attest entries of a strict majority of the
    /// members of the epoch active at that frame (P2-5's epoch-at-frame rule). Members that the next
    /// epoch excluded by proof do not count in that majority's size.
    pub fn cert_at(&self, fact: u64, frame: u64) -> Option<(u32, Vec<(MemberId, u64)>)> {
        let e = self.epoch_at(frame);
        let mut members: BTreeSet<MemberId> = self.members_at(e)?.into_iter().collect();
        if let Some((next, _, _)) = self.decided.get(e as usize) {
            for (m, _) in &next.excluded {
                members.remove(m);
            }
        }
        let mut c: BTreeMap<MemberId, u64> = BTreeMap::new();
        for (id, en) in &self.entries {
            if let Payload::Attest { fact: f, sig } = en.payload {
                if f == fact && !self.is_void(*id) && members.contains(&en.member) {
                    c.insert(en.member, sig);
                }
            }
        }
        (c.len() * 2 > members.len()).then(|| (e, c.into_iter().collect()))
    }

    /// The members of `epoch`, replayed from the genesis members and the decided changes.
    pub fn members_at(&self, epoch: u32) -> Option<Vec<MemberId>> {
        if epoch > self.epoch {
            return None;
        }
        let mut m: BTreeSet<MemberId> = self.genesis.iter().copied().collect();
        for (c, _, _) in self.decided.iter().take(epoch as usize) {
            for r in &c.remove {
                m.remove(&r.member);
            }
            for a in &c.add {
                m.insert(*a);
            }
        }
        Some(m.into_iter().collect())
    }

    /// A non-member asks to join (P2-4): keep its request, store its entries, send it the log.
    pub fn on_join(&mut self, j: MemberId, attests: Vec<Attest>, sig: u64, expires: u64) {
        let signed = if self.fixes_off & fix::VC3R4 != 0 {
            true
        } else if self.fixes_off & fix::VC4R6 != 0 {
            sig == join_sig(self.world_id, j)
        } else {
            sig == join_sig_in(self.session, j, expires) && expires > self.now
        };
        if !self.members.contains(&j) && !self.evicted.contains_key(&j) && j != self.me && signed {
            self.join_requests.insert(j);
            let r = self.join_reqs.entry(j).or_insert((sig, expires));
            if expires > r.1 {
                *r = (sig, expires);
            }
            if self.fixes_off & fix::VC3R4 == 0 {
                // Review VC3R-4: only the joiner can make its request, and every valid attestation in
                // any of its requests counts, so no request can hide another's (VCR-7's "the first
                // stands" let any peer pre-empt a joiner with an empty one).
                let have = self.join_attests.entry(j).or_default();
                for a in attests {
                    if attest_ok(&a, j, self.world_id) && !have.contains(&a) && have.len() < MAX_ATTESTS {
                        have.push(a);
                    }
                }
            } else if self.fixes_off & fix::VCR7 == 0 {
                // Review VCR-7 (round 3): the first request a joiner sends stands.
                self.join_attests.entry(j).or_insert(attests);
            } else {
                self.join_attests.insert(j, attests);
            }
            self.follow(j);
        }
    }

    /// Send `j` this replica's entries and store its own (a member about to join, in the spike).
    pub fn follow(&mut self, j: MemberId) {
        if !self.members.contains(&j) && j != self.me {
            self.known.insert(j);
            self.followers.insert(j);
        }
    }

    /// Who this replica sends its entries to: the members and the followers.
    pub fn audience(&self) -> Vec<MemberId> {
        self.members.iter().chain(self.followers.iter()).copied().filter(|m| *m != self.me).collect::<BTreeSet<_>>().into_iter().collect()
    }

    /// Test fixture (D80 bridge): from now on, acks and votes report no more of every other
    /// member's chain than this replica holds now, as a faulty member hiding what it hears. It keeps
    /// processing everything. `unfreeze_others` ends it.
    pub fn freeze_others(&mut self) {
        let heads: BTreeMap<MemberId, (u32, EntryId)> = self
            .contiguous
            .iter()
            .filter(|(m, s)| **m != self.me && **s > 0)
            .map(|(m, s)| (*m, (*s, self.by_author[&(*m, *s)])))
            .collect();
        self.hidden = Some(heads);
    }

    pub fn unfreeze_others(&mut self) {
        self.hidden = None;
    }

    /// Diagnostics (the stuck-kick trace, @coordinator-opus me6b738e13923), without side effects: this replica's
    /// round and step, the round's proposal and what it lists, its own prevote and precommit in the round, the round's
    /// prevote counts, and each vote it holds on `x` (frozen seq, `k` for a kick vote, `unheld` if it can't walk x's
    /// chain down from that head).
    pub fn kick_trace(&self, x: MemberId) -> String {
        let r = self.vc.round;
        let held = self.contiguous.get(&x).copied().unwrap_or(0);
        let kicks: Vec<String> = self
            .admin_pending
            .iter()
            .filter_map(|id| self.entries.get(id).map(|e| (id, e)))
            .filter(|(_, e)| matches!(e.payload, Payload::Kick { target, .. } if target == x))
            .map(|(id, e)| format!("m{} s{} {:x}", e.member, e.seq, id & 0xffff))
            .collect();
        let votes: Vec<String> = self
            .votes
            .get(&x)
            .map(|v| {
                v.iter()
                    .map(|(m, rec)| format!("{m}:{}{}{}", rec.frozen.0, if rec.kick != 0 { "k" } else { "" }, if self.head_held(x, rec.frozen) { "" } else { " unheld" }))
                    .collect()
            })
            .unwrap_or_default();
        let prop = self.vc.proposals.get(&r).map(|&(v, vr)| {
            let c = self.vc.changes.get(&v);
            format!(
                "{:x} vr {} remove {:?} admin {}",
                v & 0xffff,
                if vr == NO_ROUND { "-".to_string() } else { vr.to_string() },
                c.map(|c| c.remove.iter().map(|x| (x.member, x.cut.0, x.votes.len())).collect::<Vec<_>>()),
                c.map_or(0, |c| c.admin.len())
            )
        });
        let mine = |b: &BTreeMap<u32, BTreeMap<MemberId, u64>>| match b.get(&r).and_then(|m| m.get(&self.me)) {
            None => "-".to_string(),
            Some(0) => "nil".to_string(),
            Some(v) => format!("{:x}", v & 0xffff),
        };
        let counts = self.vc.prevotes.get(&r).map(|m| (m.values().filter(|&&v| v != 0).count(), m.values().filter(|&&v| v == 0).count()));
        format!(
            "epoch {} round {r} step {:?} proposer m{} since {:?} locked {:?}; holds x to {held}; kick pending {kicks:?}; votes [{}]; proposal {}; my prevote {} precommit {}; prevotes (value, nil) {:?}",
            self.epoch,
            self.vc.step,
            self.proposer(self.epoch, r),
            self.vc.since,
            self.vc.locked.map(|(v, lr)| (v & 0xffff, lr)),
            votes.join(", "),
            prop.unwrap_or_else(|| "none".to_string()),
            mine(&self.vc.prevotes),
            mine(&self.vc.precommits),
            counts
        )
    }

    /// Diagnostics: this replica's view-change state and how much of each chain it holds.
    pub fn vc_debug(&self) -> String {
        let pv: Vec<String> = self.vc.prevotes.iter().map(|(r, m)| format!("r{r}:{:?}", m.iter().map(|(k, v)| (*k, *v != 0)).collect::<Vec<_>>())).collect();
        let pc: Vec<String> = self.vc.precommits.iter().map(|(r, m)| format!("r{r}:{:?}", m.iter().map(|(k, v)| (*k, *v != 0)).collect::<Vec<_>>())).collect();
        format!(
            "now {} epoch {} round {} step {:?} since {:?} proposals {:?} prevotes [{}] precommits [{}] votes {:?} contiguous {:?} heard {:?}",
            self.now,
            self.epoch,
            self.vc.round,
            self.vc.step,
            self.vc.since,
            self.vc.proposals.keys().collect::<Vec<_>>(),
            pv.join(" "),
            pc.join(" "),
            self.votes.iter().map(|(t, v)| (*t, v.keys().copied().collect::<Vec<_>>())).collect::<Vec<_>>(),
            self.contiguous,
            self.heard
        )
    }

    /// The proposer of `round` of the view change ending `epoch`, if `epoch` is the current one.
    pub fn proposer_of(&self, epoch: u32, round: u32) -> Option<MemberId> {
        (epoch == self.epoch).then(|| self.proposer(epoch, round))
    }

    /// Answer `Msg::FetchEntry` (review VCR-8): the entry with that id, if held, and (review VC3R-7) up
    /// to `back` of its predecessors down its prev links, as far as this replica holds them.
    pub fn on_fetch_entry(&self, id: EntryId, back: u8) -> Vec<Msg> {
        let mut out = Vec::new();
        let mut next = Some(id);
        while let Some(e) = next.and_then(|i| self.entries.get(&i)) {
            out.push(Msg::Entry(e.clone()));
            next = (out.len() <= back.min(15) as usize && e.seq > 1).then_some(e.prev);
        }
        out
    }

    /// Answer `Msg::FetchValue`: the proposal entry carrying `value`, any version.
    pub fn on_fetch_value(&self, value: u64) -> Vec<Msg> {
        self.proposal_ids.get(&value).and_then(|id| self.entries.get(id)).map(|e| vec![Msg::Entry(e.clone())]).unwrap_or_default()
    }

    /// Ask `holder` for entry `id` (and `back` of its predecessors). Review VC4R-4: asked again with
    /// backoff while still missing (`take_fetches`); round 4 asked once, so one lost reply stopped a
    /// cut walk, an admin-entry fetch or a kick fetch for good.
    fn want_entry(&mut self, holder: MemberId, id: EntryId, back: u8) {
        if holder == self.me {
            return;
        }
        if self.fixes_off & fix::VC4R4 == 0 || self.entry_asked.insert((holder, id)) {
            self.entry_wants.push((holder, id, back));
        }
    }

    /// [`Self::want_entry`] from every member this replica knows.
    fn want_entry_all(&mut self, id: EntryId, back: u8) {
        let known: Vec<MemberId> = self.known.iter().copied().collect();
        for k in known {
            self.want_entry(k, id, back);
        }
    }

    fn want_from(&mut self, holder: MemberId, author: MemberId, from: u32, to: u32) {
        if holder != self.me && from <= to {
            self.wants.push((holder, author, from, to));
        }
    }

    fn want_all(&mut self, author: MemberId, from: u32, to: u32) {
        let known: Vec<MemberId> = self.known.iter().copied().collect();
        for j in known {
            self.want_from(j, author, from, to);
        }
    }

    pub fn on_fetch(&self, author: MemberId, from: u32, to: u32) -> Vec<Msg> {
        // At most 16 entries per reply (review R3-9): the requester asks again for the rest.
        (from..=to.min(from + 15))
            .filter_map(|s| self.by_author.get(&(author, s)).and_then(|id| self.entries.get(id)))
            .map(|e| Msg::Entry(e.clone()))
            .collect()
    }

    /// Fetch requests from gap and conflict detection: (who to ask, request), rate-limited.
    pub fn take_fetches(&mut self, now_ms: u64) -> Vec<(MemberId, Msg)> {
        let mut out = Vec::new();
        for (to, author, from, upto) in std::mem::take(&mut self.wants) {
            // Review R3-9: the same range is re-asked with exponential backoff, 60 ms doubling up to
            // 2 s, so a congested link is not flooded with repeats of requests whose replies are
            // still queued (a fixed 60 ms was shorter than a congested round trip).
            let wait = match self.fetch_asked.get(&(to, author)).copied() {
                Some((f, t, w)) if f == from => {
                    if now_ms < t + w {
                        continue;
                    }
                    (w * 2).min(2000)
                }
                _ => 60,
            };
            self.fetch_asked.insert((to, author), (from, now_ms, wait));
            out.push((to, Msg::Fetch { author, from, to: upto }));
        }
        for (to, value) in std::mem::take(&mut self.value_wants) {
            out.push((to, Msg::FetchValue { value }));
        }
        let mut asked_now = BTreeSet::new();
        for (to, id, back) in std::mem::take(&mut self.entry_wants) {
            if self.fixes_off & fix::VC4R4 == 0 {
                // Review VC4R-4: the same 60 ms doubling up to 2 s as range fetches, per (holder, id),
                // for as long as the entry is missing.
                if self.entries.contains_key(&id) || !asked_now.insert((to, id)) {
                    continue;
                }
                let wait = match self.entry_backoff.get(&(to, id)).copied() {
                    Some((t, w)) if now_ms < t + w => continue,
                    Some((_, w)) => (w * 2).min(2000),
                    None => 60,
                };
                self.entry_backoff.insert((to, id), (now_ms, wait));
            }
            out.push((to, Msg::FetchEntry { id, back }));
        }
        out
    }

    /// Compare check hashes only for confirmed frames this member has finished simulating with the
    /// complete log (viz-1: never while a re-simulation is pending).
    fn evaluate_checks(&mut self) {
        if self.dirty_from.is_some() {
            return;
        }
        let fin = self.conf.min(self.frame);
        let frames: Vec<u64> = self.remote_checks.range(..fin).map(|(f, _)| *f).collect();
        for f in frames {
            let Some(&mine) = self.check_hashes.get(&f) else {
                self.remote_checks.remove(&f);
                continue;
            };
            let theirs = self.remote_checks.remove(&f).unwrap_or_default();
            for (m, h) in theirs {
                if !self.evaluated.insert((f, m)) {
                    continue;
                }
                if h == mine {
                    self.stats.checks_ok += 1;
                } else {
                    self.stats.checks_bad += 1;
                    self.events.push(format!(
                        "[w{} m{}] NONDETERMINISM at confirmed frame {f}: mine {mine:016x} member {m} {h:016x}",
                        self.world_id, self.me
                    ));
                }
            }
        }
    }

    // ---- R14 attestations ------------------------------------------------------------------------

    /// Sign, as chain entries, the cross-world facts this replica has confirmed: a pending
    /// departure (for the destination) and an arrival's outcome (for the source). A relay needs a
    /// strict majority of them (reviews NEW-3, NEW-4).
    pub fn attest_due(&mut self) -> Vec<Entry> {
        if self.dirty_from.is_some() || !self.members.contains(&self.me) {
            return vec![];
        }
        let mut facts = Vec::new();
        for esc in self.t.escrow.values() {
            if let (EscrowStatus::Pending, Some(rec)) = (esc.status, esc.record) {
                if esc.depart_frame < self.conf {
                    let chain = self.chain_hash_of(rec.global);
                    facts.push(fact_departed(self.world_id, esc.depart, &rec, esc.dest, esc.deadline, chain, self.epoch_at(rec.src_frame)));
                }
            }
        }
        // P2-5: a committed transfer is the hop the destination's next departure needs. P25-4: each
        // commit is queued once its frame is confirmed. VC-7: the fact names the frame and the epoch
        // active at it in this replica's log.
        self.confirm_effects();
        for &(t, g, dest, via, frame) in &self.commits {
            facts.push(fact_committed(self.world_id, t, g, dest, via, frame, self.epoch_at(frame)));
        }
        for (t, a) in &self.t.arrivals {
            let (frame, accepted) = match a {
                Arrival::Accepted { frame, .. } => (*frame, true),
                Arrival::Rejected { frame, .. } => (*frame, false),
            };
            if frame < self.conf {
                facts.push(fact_verdict(self.world_id, *t, accepted, frame, self.epoch_at(frame)));
            }
        }
        let mut out = Vec::new();
        for fact in facts {
            if self.attested.insert(fact) {
                let e = Entry {
                    member: self.me,
                    seq: self.my_seq + 1,
                    prev: self.my_last,
                    stamp: self.frame.max(self.now),
                    payload: Payload::Attest { fact, sig: member_sig(self.world_id, self.me, fact) },
                    carried: None,
                    link: 0,
                };
                out.push(self.publish(e));
            }
        }
        out
    }

    /// A certificate for `fact` from this world's members, once a strict majority has signed it.
    pub fn cert(&self, fact: u64) -> Option<Vec<(MemberId, u64)>> {
        let members: Vec<MemberId> = self.members.iter().copied().collect();
        let mut c: BTreeMap<MemberId, u64> = BTreeMap::new();
        for (id, e) in &self.entries {
            if let Payload::Attest { fact: f, sig } = e.payload {
                if f == fact && !self.is_void(*id) && members.contains(&e.member) {
                    c.insert(e.member, sig);
                }
            }
        }
        let c: Vec<(MemberId, u64)> = c.into_iter().collect();
        cert_ok(&members, self.world_id, fact, &c).then_some(c)
    }

    // ---- P2-5 provenance ---------------------------------------------------------------------------

    /// Act on the cross-world effects of frames that are now confirmed and simulated in their final
    /// form: queue their commit facts for attestation, and keep their fork proofs.
    fn confirm_effects(&mut self) {
        if self.dirty_from.is_some() {
            return;
        }
        let upto = self.conf.min(self.frame);
        while let Some(e) = self.frame_effects.first_entry() {
            if *e.key() >= upto {
                break;
            }
            let (commits, forks) = e.remove();
            self.commits.extend(commits);
            for p in forks {
                let k = p.key();
                if !self.fork_proofs.contains_key(&k) {
                    self.ev(format!(
                        "P2-5 fork proof: world {} exported global id {:#x} twice from its entry {}: transfers {} and {}",
                        k.0, k.1, k.2, k.3, k.4
                    ));
                    self.fork_proofs.insert(k, p);
                }
            }
        }
    }

    /// What this world knows of global id `g`. An object born here that has no entry yet is here,
    /// with an empty chain.
    pub fn seen_of(&self, g: u64) -> Option<Seen> {
        self.t.seen.get(&g).cloned().or_else(|| {
            (birth_world(g) == self.world_id && (g & 0xFFFF_FFFF) < self.genesis_objects)
                .then(|| Seen::Here { via: 0, from: self.world_id, from_via: 0, chain: Vec::new(), chash: chain_hash(&[]), settled: true, entry: None })
        })
    }

    /// The cached hash of [`Self::chain_of`] (what a departure certificate covers).
    pub fn chain_hash_of(&self, g: u64) -> u64 {
        match self.t.seen.get(&g) {
            Some(Seen::Here { chash, .. }) => *chash,
            _ => chain_hash(&self.chain_of(g)),
        }
    }

    /// The provenance chain of an object here: what its next departure's `Arrive` carries.
    pub fn chain_of(&self, g: u64) -> Vec<Hop> {
        match self.seen_of(g) {
            Some(Seen::Here { chain, .. }) => chain,
            _ => Vec::new(),
        }
    }

    /// The hop a departure of local object `obj` must carry: the certified commit that brought it
    /// here, from a `Provenance` entry in this world's log, if this world does not hold it yet.
    pub fn pending_hop(&self, obj: u32) -> Option<Hop> {
        let g = self.live(obj)?.global;
        let here = self.seen_of(g)?;
        if !matches!(here, Seen::Here { settled: false, .. }) {
            return None;
        }
        // Anyone may publish a Provenance entry: take one that proves the hop.
        self.entries.iter().find_map(|(id, e)| match &e.payload {
            Payload::Provenance { hop } if !self.is_void(*id) && self.hop_in_ok(g, &here, hop) => Some(hop.clone()),
            _ => None,
        })
    }

    /// A certificate from this world's log for a fact about its `frame`, `fact(epoch)` naming the
    /// epoch active there, with the epoch proof a partner checks it by: the certificates up to that
    /// epoch, and the next one once decided, since `cert_at` then leaves out the members it excluded
    /// by proof (VC-8). None until that epoch's majority has attested it.
    pub fn vouch(&self, frame: u64, fact: impl Fn(u32) -> u64) -> Option<(EpochProof, Vec<(MemberId, u64)>)> {
        let (epoch, cert) = self.cert_at(fact(self.epoch_at(frame)), frame)?;
        let certs = self.epoch_certs_upto(epoch + 1).or_else(|| self.epoch_certs_upto(epoch))?;
        Some((EpochProof { epoch, certs }, cert))
    }

    /// The certified hop of transfer `transfer` of object `global` that this world committed, from its
    /// own log: what a relayer copies into the destination (P2-5). Signed by a majority of the epoch
    /// active at the commit frame, with this world's certificates up to it, and the next one once
    /// decided (VC-8). None until that majority has attested it.
    pub fn own_hop(&self, transfer: u64, global: u64) -> Option<Hop> {
        let (dest, via, frame) = match self.t.seen.get(&global) {
            Some(Seen::Left { transfer: t, dest, via, frame, .. }) if *t == transfer => (*dest, *via, *frame),
            _ => self.commits.iter().find(|c| c.0 == transfer && c.1 == global).map(|c| (c.2, c.3, c.4))?,
        };
        let (EpochProof { epoch, certs }, cert) = self.vouch(frame, |e| fact_committed(self.world_id, transfer, global, dest, via, frame, e))?;
        Some(Hop { src: self.world_id, dest, transfer, via, epoch, frame, certs, cert })
    }

    /// `hop` is the certified commit of the transfer that brought object `g` here.
    fn hop_in_ok(&self, g: u64, here: &Seen, hop: &Hop) -> bool {
        let Seen::Here { via, from, from_via, .. } = here else { return false };
        hop.src == *from
            && hop.dest == self.world_id
            && hop.transfer == *via
            && hop.via == *from_via
            && self.epochs.hop_ok(hop, g)
    }

    // ---- simulation ------------------------------------------------------------------------------

    /// Sorted (entry id, entry) for frame `f`: soonest-wins order with the per-frame tie rank.
    /// Set hand `obj`'s target for this frame (a `Hand` entry's, or this member's own pending pose,
    /// D73(a)): reached `span` frames on; `active = false` parks the hand.
    fn hand_target(&mut self, obj: u32, target_mm: [i32; 3], active: bool, span: u8, hands_set: &mut BTreeSet<u32>) -> Outcome {
        let Some(o) = self.live(obj) else { return Outcome::RejectedGone };
        let p = if active {
            pos(target_mm[0] as f32 * 0.001, target_mm[1] as f32 * 0.001, target_mm[2] as f32 * 0.001)
        } else {
            Pos { x: 0.0, y: 1.5, z: 0.0 }
        };
        let dt = self.cfg.dt * span.max(1) as f32;
        body_set_target_transform(&mut self.world, o.body, WorldTransform { p, q: QUAT_IDENTITY }, dt, true);
        hands_set.insert(obj);
        if active {
            // The coast continues only a stream (@coordinator-opus, 2026-10-31; D52): a target that
            // comes more than the previous one's span + `hand_predict` frames after it (a one-shot
            // target, a gesture's start) is followed over its span, then held. The previous target was
            // n + 1 frames ago.
            let stream = self.t.hand_idle.get(&obj).is_some_and(|&(n, sp, _)| n + 1 <= sp as u64 + self.cfg.hand_predict);
            self.t.hand_idle.insert(obj, (0, span, stream));
        } else {
            self.t.hand_idle.remove(&obj);
        }
        Outcome::Accepted
    }

    fn frame_entries(&self, f: u64) -> Vec<(EntryId, Entry)> {
        let mut v: Vec<(EntryId, Entry)> = self
            .at_frame
            .get(&f)
            .map(|s| s.iter().filter(|id| !self.is_void(**id)).map(|id| (*id, self.entries[id].clone())).collect())
            .unwrap_or_default();
        let seed = self.cfg.seed;
        v.sort_by_key(|(_, e)| (tie_rank(seed, f, e.member), e.member, e.seq));
        v
    }

    fn live(&self, obj: u32) -> Option<Obj> {
        self.t.objs.get(obj as usize).copied().flatten()
    }

    #[allow(clippy::too_many_arguments)]
    fn apply(
        &mut self,
        id: EntryId,
        e: &Entry,
        f: u64,
        hands_set: &mut BTreeSet<u32>,
        carry: &mut Vec<(u32, BodyState)>,
        record_effects: bool,
    ) -> Outcome {
        match &e.payload {
            Payload::Touch { obj, impulse, grab } => {
                // Part B, rule 2, by the entry's own epoch (F2).
                let pb = self.part_b_entry(e);
                // Part B, rule 2: this simulation's decision replaces any earlier one; a touch refused below
                // never reaches it.
                if pb {
                    self.rule2.remove(&id);
                }
                // Review HO-1: an object a member owns (a hand) is touched by no one else: refused,
                // so no claim is taken that would block the owner.
                if self.not_owner(*obj, e.member) {
                    return Outcome::RejectedNotOwner;
                }
                // Review HO-1, round 2: a touch carries the touched object's resulting state and no
                // other; a carried state for another object would overwrite it unclaimed (a member's
                // hand included).
                if self.fixes_off & fix::HO1 == 0 && e.carried.is_some_and(|(cobj, _)| cobj != *obj) {
                    return Outcome::RejectedInvalid;
                }
                let Some(o) = self.live(*obj) else { return Outcome::RejectedGone };
                if e.member != LOGIC {
                    if let Some(c) = self.t.claims.get(obj) {
                        if c.member != e.member && c.until > f {
                            return Outcome::RejectedClaimed { by: c.member, until: c.until };
                        }
                    }
                    if let Some(&(m, u)) = self.t.cooldown.get(obj) {
                        if m == e.member && u > f && !self.t.claims.contains_key(obj) {
                            return Outcome::RejectedCooldown { until: u };
                        }
                    }
                }
                let imp = Vec3 { x: impulse[0], y: impulse[1], z: impulse[2] };
                body_apply_linear_impulse_to_center(&mut self.world, o.body, imp, true);
                if e.member != LOGIC {
                    match self.t.claims.get(obj).copied() {
                        Some(c) if c.member == e.member && c.until > f => {
                            if *grab {
                                self.t.claims.insert(*obj, Claim { member: e.member, until: f + CLAIM_FRAMES });
                            }
                        }
                        _ => {
                            self.t.claims.insert(*obj, Claim { member: e.member, until: f + CLAIM_FRAMES });
                        }
                    }
                }
                if let Some((cobj, st)) = e.carried {
                    if self.eff.get(&id) == Some(&e.stamp) {
                        // Thin links Part B, rule 2: a write its author made without having seen an older
                        // one by another member is skipped; the push above still acts.
                        let stale = pb && self.stale_write(e, cobj);
                        if pb {
                            self.rule2.insert(id, stale);
                        }
                        if stale {
                            self.stats.stale_skips += 1;
                            if record_effects {
                                self.ev(format!("rule 2: m{} s{} stamped {}: carried state for {cobj} skipped (stale)", e.member, e.seq, e.stamp));
                            }
                        } else {
                            carry.push((cobj, st));
                            if pb {
                                self.note_write(cobj, e.member, e.seq, e.stamp);
                            }
                        }
                    }
                }
                Outcome::Accepted
            }
            Payload::Hand { obj, target_mm, active, span, more } => {
                // A member moves only its own hands; the author is the signed chain's member.
                if self.hand_owners.get(obj) != Some(&e.member) {
                    return Outcome::RejectedNotOwner;
                }
                // D73(a): a pack acts at every frame it covers, target j at its effective frame + j,
                // each with a span of 1 but the last, which has the entry's.
                let j = f - self.eff.get(&id).copied().unwrap_or(e.stamp);
                let (t, sp) = match j {
                    0 if more.is_empty() => (*target_mm, *span),
                    0 => (*target_mm, 1),
                    j => (more[j as usize - 1], if j as usize == more.len() { *span } else { 1 }),
                };
                self.hand_target(*obj, t, *active, sp, hands_set)
            }
            Payload::Depart { obj, dest, deadline, hop } => {
                // Review HO-1: nobody carries another member's hand through a portal.
                if self.not_owner(*obj, e.member) {
                    return Outcome::RejectedNotOwner;
                }
                let transfer = &transfer_id(self.world_id, id);
                if self.t.escrow.contains_key(transfer) {
                    return Outcome::RejectedDuplicate;
                }
                let gone = Escrow {
                    obj: *obj,
                    record: None,
                    status: EscrowStatus::DepartRejected,
                    depart_frame: f,
                    depart: id,
                    dest: *dest,
                    deadline: *deadline,
                };
                // Reviews R3-4, R3-6: objects leave only for worlds on the agreed route list, so no
                // member can send one to a world it controls alone, or to a world nobody knows.
                if !self.peers_of.contains_key(dest) {
                    self.t.escrow.insert(*transfer, gone);
                    return Outcome::RejectedNoRoute;
                }
                let Some(o) = self.live(*obj) else {
                    self.t.escrow.insert(*transfer, gone);
                    return Outcome::RejectedGone;
                };
                if let Some(c) = self.t.claims.get(obj) {
                    if c.member != e.member && c.until > f {
                        self.t.escrow.insert(*transfer, gone);
                        return Outcome::RejectedClaimed { by: c.member, until: c.until };
                    }
                }
                // P2-5: an object that arrived here leaves again only with the certified hop in, so
                // the next destination can check its whole way from birth.
                let mut via = 0;
                if self.provenance {
                    let here = self.seen_of(o.global);
                    match here.clone() {
                        Some(Seen::Here { settled: true, via: v, .. }) => via = v,
                        Some(Seen::Here { settled: false, via: v, from, from_via, mut chain, entry, .. }) => {
                            let Some(h) = hop.as_ref().filter(|h| self.hop_in_ok(o.global, here.as_ref().unwrap(), h)) else {
                                self.t.escrow.insert(*transfer, gone);
                                return Outcome::RejectedUnsettled;
                            };
                            chain.push(h.clone());
                            via = v;
                            let chash = chain_hash(&chain);
                            self.t.seen.insert(o.global, Seen::Here { via, from, from_via, chain, chash, settled: true, entry });
                        }
                        _ => {
                            self.t.escrow.insert(*transfer, gone);
                            return Outcome::RejectedUnsettled;
                        }
                    }
                }
                let record =
                    ObjRecord { global: o.global, kind: o.kind, state: BodyState::read(&self.world, o.body), src_world: self.world_id, src_frame: f, via };
                destroy_body(&mut self.world, o.body);
                self.t.objs[*obj as usize] = None;
                self.t.claims.remove(obj);
                self.t.escrow.insert(*transfer, Escrow { record: Some(record), status: EscrowStatus::Pending, ..gone });
                Outcome::Accepted
            }
            Payload::Arrive { transfer, record, depart, deadline, cert, chain, proof } => {
                let src = record.src_world;
                // Reviews R3-4, R3-5: only a world on the agreed route list can vouch for an object.
                if !self.peers_of.contains_key(&src) {
                    return Outcome::RejectedNoRoute;
                }
                // R3-8b: signed by the source's epoch at the departure frame, checked from its
                // genesis through the epoch certificates the `Arrive` carries, so a member the source
                // has removed no longer counts.
                let chash = chain_hash(chain);
                let fact = fact_departed(src, *depart, record, self.world_id, *deadline, chash, proof.epoch);
                if *transfer != transfer_id(src, *depart) || !self.epochs.vouched(src, record.src_frame, proof, fact, cert) {
                    return Outcome::RejectedNoProof;
                }
                // Review NEW-21: even a certified record is untrusted input (a bug or a compromised
                // majority in the source world). Refuse non-finite or absurd states.
                let st = record.state;
                let finite = st.is_finite();
                let (q, v, w) = (st.rot(), st.lin(), st.ang());
                let qn = q.v.x * q.v.x + q.v.y * q.v.y + q.v.z * q.v.z + q.s * q.s;
                let sane = finite
                    && (0.99..=1.01).contains(&qn)
                    && v.x * v.x + v.y * v.y + v.z * v.z <= MAX_ARRIVAL_SPEED * MAX_ARRIVAL_SPEED
                    && w.x * w.x + w.y * w.y + w.z * w.z <= MAX_ARRIVAL_SPIN * MAX_ARRIVAL_SPIN;
                if !sane {
                    if !self.t.arrivals.contains_key(transfer) {
                        self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    }
                    return Outcome::RejectedInvalid;
                }
                if self.t.arrivals.contains_key(transfer) {
                    return Outcome::RejectedDuplicate;
                }
                // P2-5: the source's majority vouches for the departure, not for how the object got
                // there. Check the chain back to its birth world against each hop's epoch
                // certificates, and that it passes through this world's own last exit of the
                // object. A refusal is certified like any other, and returns the object to the
                // source (whose majority signed this chain).
                if self.provenance {
                    if check_chain(&self.epochs, record.global, src, record.via, chain).is_err() {
                        self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                        return Outcome::RejectedNoProvenance;
                    }
                    // P25-1: identity is proven; content and birth are the policy's call.
                    if !self.import_policy.allows(record.global, src, record.kind, chain) {
                        self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                        return Outcome::RejectedPolicy;
                    }
                    let known = self.seen_of(record.global);
                    let arrival = arrival_export(src, *depart, record, self.world_id, *deadline, chash, cert, proof);
                    if let Some(p) = known.as_ref().and_then(|k| fork_proof(k, record.global, chain, &arrival)) {
                        if record_effects {
                            self.frame_effects.entry(f).or_default().1.push(p);
                        }
                    }
                    match known {
                        Some(Seen::Here { .. }) => {
                            self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                            return Outcome::RejectedLiveId;
                        }
                        Some(Seen::Left { transfer: out, .. }) if !chain.iter().any(|h| h.transfer == out && h.src == self.world_id) => {
                            self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                            return Outcome::RejectedFork;
                        }
                        _ => {}
                    }
                }
                // Review R3-6: past the deadline the arrival is refused, and that certified refusal
                // returns the object to the source. Nobody can hold it in limbo by never relaying:
                // anyone may relay late, and late means refused.
                if f > *deadline {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    return Outcome::RejectedLate;
                }
                // A deadline far ahead would keep the transfer open, and its id in this table, for
                // as long as it likes (round 3, low).
                if *deadline > f.saturating_add(MAX_DEADLINE_FRAMES) {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    return Outcome::RejectedDeadline;
                }
                if !self.t.portal_open {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    return Outcome::RejectedPortalClosed;
                }
                // Review R3-5: never a second live copy of one global id in a world.
                if self.t.objs.iter().flatten().any(|o| o.global == record.global) {
                    self.t.arrivals.insert(*transfer, Arrival::Rejected { frame: f, until: *deadline });
                    return Outcome::RejectedLiveId;
                }
                let obj = self.spawn(record, Pos { x: 0.0, y: 2.0, z: 0.0 });
                self.t.arrivals.insert(*transfer, Arrival::Accepted { obj, frame: f });
                let entry = Some(Box::new(arrival_export(src, *depart, record, self.world_id, *deadline, chash, cert, proof)));
                let here = Seen::Here { via: *transfer, from: src, from_via: record.via, chain: chain.clone(), chash, settled: false, entry };
                self.t.seen.insert(record.global, here);
                self.watched.insert(obj);
                Outcome::Accepted
            }
            Payload::Verdict { transfer, accepted, cert, frame, proof } => {
                let Some(esc) = self.t.escrow.get(transfer).copied() else { return Outcome::RejectedGone };
                if esc.status != EscrowStatus::Pending {
                    return Outcome::RejectedDuplicate;
                }
                // R3-8b, as for `Arrive`: the destination's epoch at its arrival frame.
                let fact = fact_verdict(esc.dest, *transfer, *accepted, *frame, proof.epoch);
                if !self.peers_of.contains_key(&esc.dest) || !self.epochs.vouched(esc.dest, *frame, proof, fact, cert) {
                    return Outcome::RejectedNoProof;
                }
                if *accepted {
                    let rec = esc.record.expect("pending escrow holds its record");
                    let (from, from_via, entry) = match self.seen_of(rec.global) {
                        Some(Seen::Here { from, from_via, entry, .. }) => (from, from_via, entry),
                        _ => (self.world_id, 0, None),
                    };
                    self.t.seen.insert(rec.global, Seen::Left { transfer: *transfer, dest: esc.dest, via: rec.via, frame: f, from, from_via, entry });
                    if record_effects {
                        self.frame_effects.entry(f).or_default().0.push((*transfer, rec.global, esc.dest, rec.via, f));
                    }
                    self.t.escrow.insert(*transfer, Escrow { status: EscrowStatus::Committed, record: None, ..esc });
                } else {
                    let rec = esc.record.expect("pending escrow holds its record");
                    let back = self.spawn(&rec, rec.state.pos());
                    debug_assert_eq!(back as usize, self.t.objs.len() - 1);
                    self.t.escrow.insert(*transfer, Escrow { status: EscrowStatus::Returned, ..esc });
                }
                Outcome::Accepted
            }
            Payload::Portal { open } => {
                self.t.portal_open = *open;
                Outcome::Accepted
            }
            // Protocol entries never reach the sim (set_eff schedules only perturbations).
            Payload::Ack { .. }
            | Payload::Vote { .. }
            | Payload::Attest { .. }
            | Payload::Provenance { .. }
            | Payload::Propose { .. }
            | Payload::Prevote { .. }
            | Payload::Precommit { .. }
            | Payload::Kick { .. }
            | Payload::SetRole { .. }
            | Payload::DefineRole { .. }
            | Payload::Ownership { .. } => Outcome::Accepted,
        }
    }

    /// Recreate an object from a record at `at`, keeping its rotation and velocities.
    fn spawn(&mut self, r: &ObjRecord, at: Pos) -> u32 {
        let mut d = default_body_def();
        d.type_ = BodyType::Dynamic;
        d.position = at;
        d.rotation = r.state.rot();
        d.linear_velocity = r.state.lin();
        d.angular_velocity = r.state.ang();
        let b = create_body(&mut self.world, &d);
        add_shape(&mut self.world, b, r.kind);
        self.t.objs.push(Some(Obj { body: b, global: r.global, kind: r.kind }));
        (self.t.objs.len() - 1) as u32
    }

    /// Step one frame: keyframe, entries in canonical order, hands, Box3D step, carried states.
    fn step_frame(&mut self, record: bool) {
        let f = self.frame;
        if record {
            // A re-simulation of this frame replaces its cross-world effects (P25-3, P25-4).
            self.frame_effects.remove(&f);
        }
        if record && self.wants_keyframe(f) {
            let k = self.save();
            self.ring.insert(f, k);
            self.stats.max_ring_bytes = self.stats.max_ring_bytes.max(self.ring_bytes());
        }
        // Round 3 (low): forget transfers that can no longer change. A rejected arrival goes once
        // its deadline has passed (any replay is then late, so rejected again, consistently) and
        // MAX_DEADLINE_FRAMES after it arrived (by then it is confirmed and attested: the sim never
        // runs more than the speculation limit past the confirmed frame). A resolved escrow goes
        // MAX_DEADLINE_FRAMES after its departure: a replayed verdict then finds nothing and does
        // nothing. Accepted arrivals stay: forgetting one would let a late replay of its Arrive be
        // certified "rejected" and refund an object that did arrive.
        self.t.arrivals
            .retain(|_, a| !matches!(a, Arrival::Rejected { frame, until } if *until < f && frame.saturating_add(MAX_DEADLINE_FRAMES) < f));
        self.t.escrow.retain(|_, e| e.status == EscrowStatus::Pending || e.depart_frame.saturating_add(MAX_DEADLINE_FRAMES) >= f);
        let mut hands_set = BTreeSet::new();
        let mut carry = Vec::new();
        for (id, e) in self.frame_entries(f) {
            let o = self.apply(id, &e, f, &mut hands_set, &mut carry, record);
            // A pack's outcome is its first frame's (D73(a)).
            if record && self.eff.get(&id).is_none_or(|&s| s == f) {
                self.outcomes.insert(id, (f, o));
            }
        }
        // D73(a): this member's own poses not yet sent in a pack act at their frames here, so its hand
        // follows its controller exactly. Only this replica has them; the pack, once sent, replaces
        // them (a rollback to its first frame re-simulates with the same targets).
        let own: Vec<(u32, [i32; 3])> = self.own_pending.range((f, 0)..=(f, u32::MAX)).map(|(&(_, o), &t)| (o, t)).collect();
        for (obj, t) in own {
            if !hands_set.contains(&obj) && self.hand_owners.get(&obj) == Some(&self.me) {
                self.hand_target(obj, t, true, 1, &mut hands_set);
            }
        }
        // Hands without a target this frame: an active hand follows its last entry over the rest of
        // that entry's span (D53: the target is reached `span` frames after the entry), then, if that
        // entry continues a stream, keeps its velocity for up to `hand_predict` more frames at most
        // 3 m/s (D52's coast), then holds: it moves while n <= (span - 1) + hand_predict frames after
        // the entry. An entry that does not continue a stream is held once its span ends. These are
        // deterministic rules of the sim: a speculative frame that lacks the author's entry predicts
        // with them, and the entry's arrival replaces the prediction by rollback.
        let hands: Vec<(u32, Obj)> =
            self.t.objs.iter().enumerate().filter_map(|(i, o)| o.filter(|o| o.kind == 4).map(|o| (i as u32, o))).collect();
        for (i, o) in hands {
            if hands_set.contains(&i) {
                continue;
            }
            // None: hold. Some(false): inside the entry's span. Some(true): coasting past it.
            let motion = match self.t.hand_idle.get_mut(&i) {
                Some((n, span, stream)) => {
                    *n += 1;
                    let follow = (*span as u64).saturating_sub(1);
                    if *n <= follow {
                        Some(false)
                    } else if *stream && *n <= follow + self.cfg.hand_predict {
                        Some(true)
                    } else {
                        None
                    }
                }
                None => None,
            };
            if motion.is_none() {
                body_set_linear_velocity(&mut self.world, o.body, Vec3 { x: 0.0, y: 0.0, z: 0.0 });
            } else if motion == Some(true) {
                // Never coast faster than a hand moves: a jump (a teleport-like target) would
                // otherwise be extrapolated as a 40 m/s flight.
                let v = body_get_linear_velocity(&self.world, o.body);
                let sp = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
                if sp > HAND_COAST_VMAX {
                    let k = HAND_COAST_VMAX / sp;
                    body_set_linear_velocity(&mut self.world, o.body, Vec3 { x: v.x * k, y: v.y * k, z: v.z * k });
                }
            }
        }
        let t0 = now_us();
        self.world.step(self.cfg.dt, SUBSTEPS);
        if record {
            self.stats.step_us += now_us() - t0;
            self.stats.steps += 1;
        }
        // User ruling 09:30Z: the touched object takes the carried resulting state. On the author's
        // world line the bits are the ones it holds; otherwise it pulls the object onto it. Written
        // always (review SET-1: writing a pose recomputes derived state), and the write resets the
        // body's history (7.3), so every peer does the same thing at this frame; `overwrites` counts
        // the writes that changed bits.
        for (obj, st) in carry {
            if let Some(o) = self.live(obj) {
                if record && BodyState::read(&self.world, o.body) != st {
                    self.stats.overwrites += 1;
                }
                st.write(&mut self.world, o.body);
            }
        }
        // Part B, rule 5: a fed replica writes the feed's states for this frame into its own view.
        if self.fed.is_some() {
            self.write_anchors(f);
        }
        // Expire claims; their holders enter a short cooldown on that object.
        let next = f + 1;
        let expired: Vec<(u32, MemberId)> = self.t.claims.iter().filter(|(_, c)| c.until <= next).map(|(o, c)| (*o, c.member)).collect();
        for (o, m) in expired {
            self.t.claims.remove(&o);
            self.t.cooldown.insert(o, (m, next + CLAIM_COOLDOWN));
        }
        self.t.cooldown.retain(|_, (_, u)| *u > next);
        self.frame = next;
        if record {
            self.hashes.insert(f, self.state_hash());
            for &obj in &self.watched.clone() {
                if let Some(s) = self.obj_state(obj) {
                    self.obj_hist.insert((f, obj), s.hash());
                }
            }
            if f % self.cfg.check_every == 0 {
                let h = self.image_hash();
                self.check_hashes.insert(f, h);
                // Review L2: a release-build finite check at every check frame. NaN payload bits differ
                // across ISAs, so a non-finite state is a divergence waiting to happen: report it.
                let bad = self.t.objs.iter().flatten().find(|o| {
                    let st = BodyState::read(&self.world, o.body);
                    !st.is_finite()
                });
                if let Some(o) = bad {
                    self.stats.nonfinite += 1;
                    let g = o.global;
                    self.ev(format!("NON-FINITE body state at frame {f} (object global id {g:#x})"));
                }
            }
            if self.track {
                let p: Vec<[f32; 3]> = self
                    .t
                    .objs
                    .iter()
                    .map(|o| o.map_or([f32::NAN; 3], |o| {
                        let x = body_get_transform(&self.world, o.body).p;
                        [x.x as f32, x.y as f32, x.z as f32]
                    }))
                    .collect();
                self.pos_hist.insert(f, p);
            }
        }
    }

    /// Roll back if needed, then simulate forward to `target` (exclusive) unless the speculation
    /// limit is reached. Returns the number of new frames stepped.
    pub fn advance(&mut self, target: u64) -> u64 {
        let t_adv = now_us();
        let n = self.advance_inner(target);
        let us = now_us() - t_adv;
        self.stats.advance_us += us;
        self.stats.max_advance_us = self.stats.max_advance_us.max(us);
        n
    }

    fn advance_inner(&mut self, target: u64) -> u64 {
        if let Some(from) = self.dirty_from.take() {
            let now = self.frame;
            self.resim_to = now;
            let (&kf, _) = self.ring.range(..=from).next_back().expect("keyframe at or before rollback frame");
            let k = self.ring[&kf].clone();
            for f in self.ring.range(kf + 1..).map(|(f, _)| *f).collect::<Vec<_>>() {
                self.ring.remove(&f);
            }
            self.restore(&k, kf);
            self.stats.rollbacks += 1;
            self.stats.max_depth = self.stats.max_depth.max(now - from);
            let t0 = now_us();
            while self.frame < now {
                self.step_frame(true);
                self.stats.resim_frames += 1;
            }
            self.stats.resim_us += now_us() - t0;
            self.evaluate_checks();
        }
        self.trim_ring();
        let mut n = 0;
        while self.frame < target {
            // Part B, rule 5: a fed replica can't confirm (it holds no log), and doesn't stall at its own W.
            if self.frame >= self.conf + self.cfg.window && self.fed.is_none() {
                self.stats.paused_frames += 1;
                break;
            }
            self.step_frame(true);
            n += 1;
        }
        n
    }

    /// Drop keyframes this member can never need again: everything before the newest keyframe at
    /// or before min(confirmed frame, simulated frame), once no re-simulation is pending.
    fn trim_ring(&mut self) {
        if self.dirty_from.is_some() {
            return;
        }
        let fin = self.conf.min(self.frame);
        let base = match self.ring.range(..=fin).next_back() {
            Some((&kf, _)) => kf,
            None => return,
        };
        let now = self.frame;
        let drop: Vec<u64> = self
            .ring
            .keys()
            .copied()
            .filter(|&f| {
                f < base
                    || (f > base
                        && match self.cfg.snap {
                            SnapPolicy::Every(k) => f % k != 0,
                            SnapPolicy::Log(k) => f % log_grid(k, now - f) != 0,
                            // Keep only the first candidate after the confirmed frame.
                            SnapPolicy::Base(_) => self.ring.range(base + 1..f).next().is_some(),
                            SnapPolicy::Window(k) => f % k != 0 || now - f >= 4 * k,
                        })
            })
            .collect();
        for f in drop {
            self.ring.remove(&f);
        }
    }

    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    /// Bytes of rollback snapshot data held: every keyframe's world image plus its object table
    /// (ballpit-viz; `ring_bytes` counts the world images only).
    pub fn ring_bytes_with_objs(&self) -> usize {
        self.ring.values().map(|k| k.image.len() + k.t.objs.len() * std::mem::size_of::<Option<Obj>>()).sum()
    }

    pub fn caught_up(&self) -> bool {
        self.dirty_from.is_none()
    }

    /// 7.6 reduction 2: "nothing unconfirmed", judged from this member's own copy. Every entry it holds,
    /// other than acks, is stamped below `conf`, so at or before its last confirmed frame. Acks are left
    /// out, or each ack would keep every member busy. D73(a) (@world-chunks, @coordinator-opus): nor is a
    /// member idle while it holds an unsent pack, since the pack will be stamped at its first frame.
    pub fn idle(&self) -> bool {
        self.busy_stamp < self.conf && self.hold_horizon_below.is_none()
    }

    /// Entries this member holds whose effective frame is confirmed (R14 relays wait for this).
    pub fn is_confirmed(&self, id: EntryId) -> bool {
        self.eff.get(&id).is_some_and(|&f| f < self.conf)
    }

    pub fn all_entries(&self) -> Vec<Entry> {
        self.entries.values().cloned().collect()
    }

    pub fn void_ids(&self) -> BTreeSet<EntryId> {
        self.violations.union(&self.disputed).copied().collect()
    }

    pub fn members_evicted(&self) -> &BTreeMap<MemberId, (u32, u64)> {
        &self.evicted
    }

    /// Give the world an ownership chain from its genesis certificate (part of the genesis
    /// document; before the first frame is simulated).
    pub fn set_ownership(&mut self, genesis: Cert) {
        assert_eq!(self.frame, 0, "ownership is part of the world's genesis");
        self.book = Some(Book::new(genesis).expect("a valid genesis certificate"));
    }

    /// A certificate for `change` on top of this replica's present ownership head(s), signed by
    /// `keys` (spike: anyone can sign as any
    /// key, standing in for a key holder handing the member a signed certificate). A merge joins
    /// every head; any other change builds on the first head.
    pub fn owner_cert(&self, change: OwnChange, keys: &[Key]) -> Option<Cert> {
        let b = self.book.as_ref()?;
        let heads = b.heads();
        let prev = if change == OwnChange::Merge { heads } else { heads.into_iter().take(1).collect() };
        let seq = prev.iter().map(|p| b.cert(*p).map_or(0, |c| c.seq)).max()? + 1;
        Some(Cert { world: b.world, seq, prev, change, sigs: vec![] }.signed(keys))
    }
}

// ---- offline reference ---------------------------------------------------------------------------

/// Re-simulate a world from scratch with a complete, final log and effective frames: the
/// single-world reference used to check that sharing a process with other worlds, rollbacks and
/// throttling changed nothing.
pub fn replay(world_id: u32, scene: SceneKind, cfg: SimConfig, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> BTreeMap<u64, u64> {
    replay_sim(world_id, scene, cfg, members, log, frames).hashes
}

pub fn replay_sim(world_id: u32, scene: SceneKind, cfg: SimConfig, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> WorldSim {
    replay_sim_with(world_id, scene, cfg, members, &BTreeMap::new(), log, frames)
}

/// As [`replay_sim`], for a world that trades objects with others (R14 certificates).
pub fn replay_sim_with(
    world_id: u32,
    scene: SceneKind,
    cfg: SimConfig,
    members: &[MemberId],
    peers_of: &BTreeMap<u32, Vec<MemberId>>,
    log: &[(Entry, u64)],
    frames: u64,
) -> WorldSim {
    let mut s = WorldSim::new(world_id, u32::MAX, members, scene, cfg);
    s.peers_of = peers_of.clone();
    s.run_log(log, frames)
}

/// As [`replay_sim_with`], with `like`'s cross-world config: route list, epoch certificates and
/// whether provenance is checked (P2-5).
pub fn replay_sim_like(like: &WorldSim, scene: SceneKind, members: &[MemberId], log: &[(Entry, u64)], frames: u64) -> WorldSim {
    let mut s = WorldSim::new(like.world_id, u32::MAX, members, scene, like.cfg);
    s.peers_of = like.peers_of.clone();
    s.epochs = like.epochs.clone();
    s.provenance = like.provenance;
    s.import_policy = like.import_policy.clone();
    s.hand_owners = like.hand_owners.clone();
    // Thin links Part B: the replay holds no acks, so rule 2 reads the live replica's window values and the
    // heads its acks named. Every replica derives them from the same log, which the digests compare.
    s.windows = like.windows.clone();
    s.provisional = like.provisional.clone();
    s.ack_head_hist = like.ack_head_hist.clone();
    // F2: and the epochs, so rule 2 applies by each entry's own epoch as it did live.
    s.genesis = like.genesis.clone();
    s.decided = like.decided.clone();
    s.epoch = like.epoch;
    s.ack_epoch_from = like.ack_epoch_from.clone();
    s.admitted_epoch = like.admitted_epoch.clone();
    s.replaying = true;
    s.run_log(log, frames)
}

impl WorldSim {
    /// Offline reference: load a final log with its effective frames and simulate `frames`.
    fn run_log(mut self, log: &[(Entry, u64)], frames: u64) -> WorldSim {
        for (e, eff) in log {
            let id = e.id();
            self.by_author.insert((e.member, e.seq), id);
            self.entries.insert(id, e.clone());
            self.eff.insert(id, *eff);
            for g in *eff..=*eff + e.payload.extra_frames() {
                self.at_frame.entry(g).or_default().insert(id);
            }
        }
        self.conf = u64::MAX / 2;
        while self.frame < frames {
            self.step_frame(true);
        }
        self
    }
}

/// The log this member would hand to [`replay`]: every non-void entry with its effective frame.
pub fn final_log(s: &WorldSim) -> Vec<(Entry, u64)> {
    s.eff.iter().map(|(id, f)| (s.entries[id].clone(), *f)).collect()
}

pub fn digest(h: &BTreeMap<u64, u64>, frames: u64) -> u64 {
    fnv_u64s((0..frames).map(|f| h.get(&f).copied().unwrap_or(0)))
}

#[cfg(test)]
mod round3_tests {
    //! Review VCR-11 (P2-4 round 3): encodings that must be refused, never read in part.
    use super::*;

    #[test]
    fn large_polka_round_trips() {
        // Review VC6R-2: a capped precommit carries up to MAX_POLKA prevote ids. A polka of 300 would
        // have panicked the old encoder (asserted at 64); now it round-trips, and a decode claiming
        // more than MAX_POLKA is refused.
        let polka: Vec<EntryId> = (1..=300).collect();
        let e = Entry { member: 1, seq: 1, prev: 0, stamp: 5, payload: Payload::Precommit { epoch: 0, round: 0, value: 9, boundary: 7, polka }, carried: None, link: 0 };
        assert_eq!(decode_msg(&encode_msg(&Msg::Entry(e.clone()))), Some(Msg::Entry(e)));
        // A precommit payload whose polka count exceeds MAX_POLKA is refused (never read in part).
        let mut w = Wr(Vec::new());
        w.u8(1); // Msg::Entry
        w.u32(1); w.u32(1); w.u64(0); w.u64(5); // member, seq, prev, stamp
        w.u8(13); w.u32(0); w.u32(0); w.u64(9); w.u64(7); // Precommit epoch, round, value, boundary
        w.u32(MAX_POLKA as u32 + 1);
        for i in 0..=MAX_POLKA as u64 { w.u64(i) }
        w.u8(0); // carried: none
        assert_eq!(decode_msg(&w.0), None);
    }

    #[test]
    fn signed_join_and_branch_fetch_round_trip() {
        // Review VC3R-4 and VC3R-7 (round 4): the join's signature and the fetch's depth travel.
        for m in [
            join_msg(session_id(1, &[1, 2, 3]), 5, vec![], 900),
            join_msg(session_id(1, &[1, 2, 3]), 5, vec![attest(Rel::Friend, 5, 1, 1)], u64::MAX),
            Msg::FetchEntry { id: 77, back: 0 },
            Msg::FetchEntry { id: 77, back: 15 },
        ] {
            assert_eq!(decode_msg(&encode_msg(&m)), Some(m));
        }
        assert_ne!(join_sig(1, 5), join_sig(2, 5));
        assert_ne!(join_sig(1, 5), join_sig(1, 6));
        // Review VC4R-6: another session of the same world, or another expiry, is another signature.
        let (a, b) = (session_id(1, &[1, 2, 3]), session_id(1, &[1, 2, 4]));
        assert_ne!(join_sig_in(a, 5, 900), join_sig_in(b, 5, 900));
        assert_ne!(join_sig_in(a, 5, 900), join_sig_in(a, 5, 901));
        // A change that adds a member carries its request, in the extended layout.
        let c = Change { epoch: 2, add: vec![5], add_roles: vec![ADMIN], add_attests: vec![vec![]], add_reqs: vec![(join_sig_in(a, 5, 900), 900)], boundary: 7, ..Default::default() };
        let e = Entry { member: 1, seq: 1, prev: 0, stamp: 5, payload: Payload::Propose { epoch: 2, round: 0, valid_round: NO_ROUND, change: c }, carried: None, link: 0 };
        assert_eq!(decode_msg(&encode_msg(&Msg::Entry(e.clone()))), Some(Msg::Entry(e)));
    }

    fn join_bytes(n: u32) -> Vec<u8> {
        let mut w = Wr(Vec::new());
        w.u8(7);
        w.u32(5);
        w.u64(join_sig(1, 5));
        w.u64(u64::MAX);
        w.u32(n);
        for _ in 0..n {
            w.u8(Rel::Friend.tag());
            w.u32(1);
            w.u64(2);
            w.u64(3);
        }
        w.0
    }

    #[test]
    fn join_with_too_many_attests_is_refused() {
        let ok = decode_msg(&join_bytes(MAX_ATTESTS as u32));
        assert!(matches!(ok, Some(Msg::Join { ref attests, .. }) if attests.len() == MAX_ATTESTS));
        assert_eq!(decode_msg(&join_bytes(MAX_ATTESTS as u32 + 1)), None);
        // Inside a batch, a refused join refuses the batch rather than shifting what follows.
        let mut w = Wr(Vec::new());
        w.u8(4);
        w.u32(2);
        let mut b = w.0;
        b.extend(join_bytes(MAX_ATTESTS as u32 + 1));
        b.extend(encode_msg(&Msg::FetchValue { value: 9 }));
        assert_eq!(decode_msg(&b), None);
    }

    #[test]
    fn wide_role_ids_are_refused() {
        let e = Entry { member: 1, seq: 1, prev: 0, stamp: 5, payload: Payload::SetRole { epoch: 0, member: 2, role: 7 }, carried: None, link: 0 };
        let mut bytes = encode_msg(&Msg::Entry(e.clone()));
        assert_eq!(decode_msg(&bytes), Some(Msg::Entry(e)));
        // The role is the u32 after tag 18, epoch and member: set its high half.
        let at = bytes.iter().position(|&b| b == 18).unwrap() + 1 + 4 + 4;
        bytes[at + 2] = 1;
        assert_eq!(decode_msg(&bytes), None);
    }

    #[test]
    #[should_panic(expected = "does not fit")]
    fn an_epoch_at_the_flag_bit_is_not_encoded() {
        let c = Change { epoch: CHANGE_EXT, ..Default::default() };
        let _ = c.value();
    }

    #[test]
    fn change_attests_round_trip() {
        let c = Change { epoch: 3, add: vec![5], add_roles: vec![1], add_attests: vec![vec![attest(Rel::Friend, 5, 1, 1)]], boundary: 9, ..Default::default() };
        let mut w = Wr(Vec::new());
        w.change(&c);
        assert_eq!(Rd(&w.0, 0).change(), Some(c));
    }
}

#[cfg(test)]
mod ownership_tests {
    //! Review round 10 (ownership on the admin layer): joins read the owners the change's own admin
    //! entries leave (OWNY-1); a certificate built on a pending one's head waits for it (OWNY-2).
    use super::*;
    use crate::ownership::SignRule;

    const GUEST: RoleId = 5;

    /// Member 1 of a session of [1, 2] whose world is owned by `owners` (rule A), where everyone else
    /// joins as GUEST.
    fn world(owners: &[Key]) -> WorldSim {
        let mut s = WorldSim::new(1, 1, &[1, 2], SceneKind::Empty, SimConfig::default());
        s.set_ownership(Cert::genesis(owners, SignRule::All, 7));
        s.join_rules = JoinRules::public(GUEST);
        s
    }

    /// A change listing admin entry `id` (applied) and member 9's join with `role`.
    fn change_with_join(s: &WorldSim, id: EntryId, role: RoleId) -> Change {
        let exp = 1000;
        Change { epoch: s.epoch, admin: vec![(id, true)], add: vec![9], add_roles: vec![role], add_attests: vec![vec![]], add_reqs: vec![(join_sig_in(s.session, 9, exp), exp)], boundary: 10, ..Default::default() }
    }

    #[test]
    fn an_owner_leaving_in_the_change_of_its_join_is_not_row_0() {
        let mut s = world(&[1, 9]);
        let cert = s.owner_cert(OwnChange::Leave { key: 9 }, &[9]).unwrap();
        let id = s.publish_ownership(cert).id();
        assert!(s.admin_pending.contains(&id));
        // The decided book still lists 9; the change's own Leave removes it before the join.
        assert!(s.owner_keys().contains(&9));
        assert_eq!(s.change_valid(&change_with_join(&s, id, ADMIN)), Some(false));
        assert_eq!(s.change_valid(&change_with_join(&s, id, GUEST)), Some(true));
    }

    #[test]
    fn an_owner_added_in_the_change_of_its_join_is_row_0() {
        let mut s = world(&[1]);
        let cert = s.owner_cert(OwnChange::Add { key: 9 }, &[1]).unwrap();
        let id = s.publish_ownership(cert).id();
        assert!(!s.owner_keys().contains(&9));
        assert_eq!(s.change_valid(&change_with_join(&s, id, ADMIN)), Some(true));
        assert_eq!(s.change_valid(&change_with_join(&s, id, GUEST)), Some(false));
    }

    /// Owners [1]: 1 adds 9 (pending), then 1 and 9 add 8 on top of that certificate.
    fn chained(fixes_off: u64) -> (WorldSim, EntryId, EntryId) {
        let mut s = world(&[1]);
        s.fixes_off = fixes_off;
        let c1 = s.owner_cert(OwnChange::Add { key: 9 }, &[1]).unwrap();
        let c2 = Cert { world: c1.world, seq: c1.seq + 1, prev: vec![c1.id()], change: OwnChange::Add { key: 8 }, sigs: vec![] }.signed(&[1, 9]);
        let id1 = s.publish_ownership(c1).id();
        let id2 = s.publish_ownership(c2).id();
        (s, id1, id2)
    }

    #[test]
    fn a_certificate_on_a_pending_head_is_held_until_its_parent_is_decided() {
        let (mut s, id1, id2) = chained(0);
        assert!(s.admin_pending.contains(&id1));
        assert!(!s.admin_pending.contains(&id2) && s.own_held.contains(&id2));
        let c = Change { epoch: s.epoch, admin: vec![(id1, true)], boundary: 10, ..Default::default() };
        assert_eq!(s.change_valid(&c), Some(true));
        s.apply_change(c, 0, &mut Vec::new());
        assert!(s.owner_keys().contains(&9));
        assert!(s.admin_pending.contains(&id2) && s.own_held.is_empty());
        // Its own change then commits it.
        let c = Change { epoch: s.epoch, admin: vec![(id2, true)], boundary: 20, ..Default::default() };
        assert_eq!(s.change_valid(&c), Some(true));
        s.apply_change(c, 0, &mut Vec::new());
        assert_eq!(s.owner_keys(), BTreeSet::from([1, 8, 9]));
    }

    #[test]
    fn without_owny2_a_certificate_on_a_pending_head_is_lost() {
        let (mut s, id1, id2) = chained(fix::OWNY2);
        assert!(!s.admin_pending.contains(&id2) && s.own_held.is_empty());
        let c = Change { epoch: s.epoch, admin: vec![(id1, true)], boundary: 10, ..Default::default() };
        s.apply_change(c, 0, &mut Vec::new());
        assert!(!s.admin_pending.contains(&id2));
    }

    #[test]
    fn an_author_holds_at_most_max_held_per_author() {
        // Review OWNZ-1: certificates naming parents that never exist are held up to the cap only.
        let mut s = world(&[1]);
        let world = s.book.as_ref().unwrap().world;
        for n in 0..MAX_HELD_PER_AUTHOR as u64 + 4 {
            let c = Cert { world, seq: 2, prev: vec![1000 + n], change: OwnChange::Add { key: 9 }, sigs: vec![] }.signed(&[1]);
            s.publish_ownership(c);
        }
        assert_eq!(s.own_held.len(), MAX_HELD_PER_AUTHOR);
        assert!(s.admin_pending.is_empty());
    }

    #[test]
    fn an_oversized_ownership_entry_is_refused() {
        // Review OWNY-3: a tag-20 payload claiming more than MAX_CERT_WORDS words is never read.
        let mut w = Wr(Vec::new());
        w.u8(1); // Msg::Entry
        w.u32(1); w.u32(1); w.u64(0); w.u64(5); // member, seq, prev, stamp
        w.u8(20); w.u32(0); w.u32(MAX_CERT_WORDS as u32 + 1);
        for i in 0..=MAX_CERT_WORDS as u64 { w.u64(i) }
        w.u8(0); // carried: none
        assert_eq!(decode_msg(&w.0), None);
        assert_eq!(Cert::from_words(&vec![0; MAX_CERT_WORDS + 1]), None);
    }
}

#[cfg(test)]
mod echo_tests {
    //! Thin links Part B, rule 1's echo: checkpoint links, the ids a fed member computes, and the encodings.
    use super::*;

    /// A chain of acks of member 1 stamped every `step` frames, with honest checkpoint links.
    fn chain(n: u32, step: u64) -> Vec<Entry> {
        let (mut out, mut ck, mut prev): (Vec<Entry>, Option<(u64, EntryId)>, EntryId) = (Vec::new(), None, 0);
        for s in 1..=n {
            let stamp = 3 + s as u64 * step;
            let mut e = Entry { member: 1, seq: s, prev, stamp, payload: Payload::Ack { horizon: stamp - 1, heads: vec![(2, s, 7)], check: None, epoch: 0, fed: None }, carried: None, link: 0 };
            e.link = ckpt_link_for(ck, stamp);
            ck = ckpt_step(ck, &e);
            prev = e.id();
            out.push(e);
        }
        out
    }

    #[test]
    fn checkpoint_links_round_trip_and_ids_split() {
        let c = chain(40, 7);
        let ckpts: Vec<&Entry> = c.iter().filter(|e| e.link != 0).collect();
        // Blocks of 32 frames at 7 frames per entry: a checkpoint in nearly every block after the first.
        assert!(ckpts.len() >= 7, "{}", ckpts.len());
        for e in &c {
            assert_eq!(decode_msg(&encode_msg(&Msg::Entry(e.clone()))), Some(Msg::Entry(e.clone())));
            assert_eq!(entry_id(e.link, e.rest_hash()), e.id());
        }
        // Each checkpoint links to the previous one.
        for w in ckpts.windows(2) {
            assert_eq!(w[1].link, w[0].id());
        }
    }

    #[test]
    fn a_fed_member_computes_checkpoint_ids_forward() {
        // TLP-22/23: from a checkpoint id it holds and each later checkpoint's rest-hash, a fed member gets
        // every later checkpoint's id; a rest-hash of another fork gives an id no one holds.
        let c = chain(40, 7);
        let mut ck: Option<(u64, EntryId)> = None;
        let mut firsts: Vec<(u64, EntryId, u64)> = Vec::new();
        for e in &c {
            let before = ck;
            ck = ckpt_step(ck, e);
            if ck != before {
                firsts.push((e.stamp / CKPT_FRAMES, e.id(), e.rest_hash()));
            }
        }
        let (_, mut id, _) = firsts[0];
        for &(_, real, rest) in &firsts[1..] {
            id = entry_id(id, rest);
            assert_eq!(id, real);
        }
        let forged = Entry { payload: Payload::Attest { fact: 1, sig: 0 }, ..c[39].clone() };
        assert_ne!(entry_id(firsts[firsts.len() - 2].1, forged.rest_hash()), c[39].id());
    }

    #[test]
    fn fed_acks_and_checkpoint_lists_round_trip() {
        for fed in [
            Some(FedAck { anchor: 900, echo: vec![(1, 12), (2, 11), (4, 12)], digest: 0xabcd }),
            Some(FedAck { anchor: 901, echo: vec![], digest: 0 }),
            None,
        ] {
            for epoch in [0, 3] {
                let e = Entry { member: 3, seq: 9, prev: 5, stamp: 400, payload: Payload::Ack { horizon: 399, heads: vec![(1, 4, 8)], check: Some((300, 6)), epoch, fed: fed.clone() }, carried: None, link: 0 };
                assert_eq!(decode_msg(&encode_msg(&Msg::Entry(e.clone()))), Some(Msg::Entry(e)));
            }
        }
        for m in [
            Msg::Ckpts { member: 2, after: Some(11), list: vec![(12, 40, 5), (13, 41, 6)], fed: false, echo: None },
            Msg::Ckpts { member: 2, after: None, list: vec![(0, 1, 5)], fed: true, echo: Some(9) },
            Msg::Ckpts { member: 2, after: None, list: vec![], fed: true, echo: None },
            Msg::Feed { frame: 7, fed: vec![3, 4], bodies: vec![] },
        ] {
            assert_eq!(decode_msg(&encode_msg(&m)), Some(m));
        }
    }

    #[test]
    fn the_decoder_refuses_unknown_flags_and_a_flagged_zero_link() {
        let e = Entry { member: 1, seq: 1, prev: 0, stamp: 5, payload: Payload::Attest { fact: 1, sig: 0 }, carried: None, link: 0 };
        let mut b = encode_msg(&Msg::Entry(e));
        let flags = b.len() - 1;
        assert_eq!(b[flags], 0);
        b[flags] = 4;
        assert_eq!(decode_msg(&b), None);
        b[flags] = 2;
        b.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(decode_msg(&b), None);
        let n = b.len();
        b[n - 8] = 1;
        assert!(decode_msg(&b).is_some());
    }
}

/// Rev 22 (e1), @coordinator-opus m15aed3d58263 ruling 2: the commit order of one decision's admin entries,
/// deterministically. Stamp-effective entries run first; a kick is judged on the ACL as of the previous decided change
/// (acl0), whatever this decision's role entries changed.
#[cfg(test)]
mod e1_commit_order {
    use super::*;

    const GUEST: RoleId = 1;

    fn entry(member: MemberId, stamp: u64, payload: Payload) -> Entry {
        Entry { member, seq: 5, prev: 0, stamp, payload, carried: None, link: 0 }
    }

    fn run(admins: &[MemberId]) -> AdminRun {
        let members: BTreeSet<MemberId> = [1, 2, 3, 4].into_iter().collect();
        let roles: BTreeMap<MemberId, RoleId> = members.iter().map(|&m| (m, if admins.contains(&m) { ADMIN } else { GUEST })).collect();
        let defs: BTreeMap<RoleId, u64> = [(GUEST, 0)].into_iter().collect();
        AdminRun {
            members: members.clone(),
            roles: roles.clone(),
            defs: defs.clone(),
            excluded: BTreeSet::new(),
            owners: BTreeSet::new(),
            book: None,
            lineage: 1,
            acl0: Some(Box::new((members, roles, defs, BTreeSet::new()))),
        }
    }

    fn decide(r: &mut AdminRun, mut es: Vec<Entry>) -> Vec<(bool, Entry)> {
        es.sort_by_key(|e| admin_key(e, 7));
        es.into_iter().map(|e| (r.apply(&e), e)).collect()
    }

    #[test]
    fn same_decision_revocation_then_kick_stands() {
        let mut r = run(&[1, 2]);
        let out = decide(&mut r, vec![entry(2, 361, Payload::Kick { epoch: 0, target: 4 }), entry(1, 360, Payload::SetRole { epoch: 0, member: 2, role: GUEST })]);
        assert!(matches!(out[0].1.payload, Payload::SetRole { .. }), "stamp-effective entries run first");
        assert_eq!(out.iter().map(|x| x.0).collect::<Vec<_>>(), vec![true, true]);
        assert_eq!(r.roles.get(&2), Some(&GUEST));
        assert!(!r.members.contains(&4), "the kick is judged on acl0, where 2 was an admin");
    }

    #[test]
    fn same_decision_promotion_then_kick_refused() {
        let mut r = run(&[1]);
        let out = decide(&mut r, vec![entry(2, 361, Payload::Kick { epoch: 0, target: 4 }), entry(1, 360, Payload::SetRole { epoch: 0, member: 2, role: ADMIN })]);
        assert_eq!(out.iter().map(|x| x.0).collect::<Vec<_>>(), vec![true, false]);
        assert_eq!(r.roles.get(&2), Some(&ADMIN));
        assert!(r.members.contains(&4), "on acl0 2 was a guest");
    }

    /// F never accepts what a full member rejects (@design-reviewer mf9870e4051a7, m912db9fb4df1): on one decision's
    /// list, the full member's commit-order apply has the demotion apply and the kick, judged on acl0, stand; a fed
    /// member's check of the same change (fed_change_valid) can't check the stamp-effective demotion, so it answers
    /// "can't tell" (None, which falls to rule (c): a prevote once t + 1 have), never Some(true) where the full member
    /// rejects; a change marking the standing kick refused gets its nil (Some(false)).
    #[test]
    fn fed_never_accepts_what_full_rejects() {
        let mut f = WorldSim::new(1, 3, &[1, 2, 3, 4], SceneKind::Empty, SimConfig { localize: true, ..SimConfig::default() });
        f.role_defs.insert(GUEST, 0);
        f.roles.insert(3, GUEST);
        f.roles.insert(4, GUEST);
        f.set_fed(true);
        let demote = entry(1, 360, Payload::SetRole { epoch: 0, member: 2, role: GUEST });
        let kick = entry(2, 361, Payload::Kick { epoch: 0, target: 4 });
        let (did, kid) = (demote.id(), kick.id());
        f.entries.insert(did, demote.clone());
        f.entries.insert(kid, kick.clone());
        // The full member's path.
        let mut full = run(&[1, 2]);
        let applied = decide(&mut full, vec![kick.clone(), demote.clone()]);
        let order: Vec<(EntryId, bool)> = applied.iter().map(|(ok, e)| (e.id(), *ok)).collect();
        assert_eq!(order, vec![(did, true), (kid, true)]);
        // The fed member's path, on the same list.
        let change = |admin: Vec<(EntryId, bool)>, kick_ok: bool| Change {
            epoch: 0,
            remove: if kick_ok { vec![Removal { member: 4, votes: vec![], cut: (0, 0), kick: kid }] } else { vec![] },
            add: vec![],
            boundary: 600,
            excluded: vec![],
            admin,
            add_roles: vec![],
            add_attests: vec![],
            add_reqs: vec![],
        };
        assert_eq!(f.fed_change_valid(&change(order.clone(), true)), None, "agrees, but can't check the demotion: can't tell");
        assert_eq!(f.fed_change_valid(&change(vec![(did, true), (kid, false)], false)), Some(false), "a refused kick that should stand gets its nil");
    }

    #[test]
    fn kick_order_by_hash_not_stamp() {
        // Two admins kick each other in one decision: the order is hash(seed, author), then seq, never the stamp, so a
        // backdated kick doesn't come first by backdating.
        let mut a = run(&[1, 2]);
        let out_a = decide(&mut a, vec![entry(1, 400, Payload::Kick { epoch: 0, target: 2 }), entry(2, 100, Payload::Kick { epoch: 0, target: 1 })]);
        let mut b = run(&[1, 2]);
        let out_b = decide(&mut b, vec![entry(1, 400, Payload::Kick { epoch: 0, target: 2 }), entry(2, 399, Payload::Kick { epoch: 0, target: 1 })]);
        assert_eq!(out_a[0].1.member, out_b[0].1.member, "the stamp doesn't change the order");
        assert_eq!(out_a.iter().map(|x| x.0).collect::<Vec<_>>(), vec![true, false], "the second kick's author is gone");
    }
}
